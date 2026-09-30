//! Requests to the drives a client shares, over IronRDP's RDPDR channel ([MS-RDPEFS]).
//!
//! IronRDP's `RdpdrServer` runs the channel: it initialises it, hears which devices the client
//! shares, gives every request a completion ID and hands each completion to the channel's backend,
//! [`super::DriveBackend`], which passes it on to [`Requests`]. [`RdpdrHandle`] turns what the NFS
//! server needs (attributes, listings, reading, writing, creating, truncating, setting times, renaming
//! and deleting) into device I/O requests, sends them as server events and waits for their
//! completions, each for at most [`REQUEST_TIMEOUT`].
//!
//! The server chooses a request's completion ID as it sends the request, and tells the backend in
//! `on_request_sent`, request after request, in the order their events arrived. [`Requests`] queues a
//! request under the same lock as it sends its event, so the n-th ID the backend hears of belongs to
//! the n-th request in the queue.
//!
//! Adapted from macrdp (<https://github.com/clintcan/macrdp>), Copyright (c) 2026 Clint Christopher
//! Canada, MIT OR Apache-2.0.
//!
//! [MS-RDPEFS]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/34d9de58-b2b5-40b6-b970-f82d4603bdb5

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ironrdp_rdpdr::pdu::efs::{
    Boolean, CreateDisposition, CreateOptions, DesiredAccess, DeviceAnnounceHeader, DeviceIoResponse, FileAttributes,
    FileBasicInformation, FileDispositionInformation, FileEndOfFileInformation, FileInformationClass,
    FileInformationClassLevel, FileRenameInformation, NtStatus,
};
use ironrdp_server::{RdpdrServerMessage, ServerEvent};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// How long a request waits for the client's answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on the file handles kept open per connection. Reads and writes of one file reuse its
/// handle; the least recently used one is closed once more files than this are in use.
const MAX_OPEN_HANDLES: usize = 16;

/// Why a request to the client failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RdpdrError {
    /// The client answered with a failure status.
    Status { op: &'static str, status: NtStatus },
    /// The client did not answer within the request timeout.
    Timeout { op: &'static str },
    /// The channel closed, because the connection ended.
    Closed,
    /// The client's answer did not fit the request.
    Malformed { op: &'static str, reason: String },
}

impl fmt::Display for RdpdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status { op, status } => write!(f, "RDPDR {op} failed with {status:?}"),
            Self::Timeout { op } => write!(f, "RDPDR {op}: the client did not answer in time"),
            Self::Closed => write!(f, "RDPDR channel closed"),
            Self::Malformed { op, reason } => write!(f, "RDPDR {op}: unexpected answer: {reason}"),
        }
    }
}

impl std::error::Error for RdpdrError {}

pub(crate) type RdpdrResult<T> = Result<T, RdpdrError>;

/// A file's size, attributes and times, the times as FILETIMEs: 100-nanosecond intervals since
/// 1601-01-01 UTC.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FileInfo {
    pub(crate) size: u64,
    pub(crate) attributes: FileAttributes,
    pub(crate) creation_time: i64,
    pub(crate) last_access_time: i64,
    pub(crate) last_write_time: i64,
    pub(crate) change_time: i64,
}

impl FileInfo {
    pub(crate) fn is_dir(&self) -> bool {
        self.attributes.contains(FileAttributes::FILE_ATTRIBUTE_DIRECTORY)
    }
}

/// What [`RdpdrHandle::create_file`] does when the file exists already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateMode {
    /// Fail with STATUS_OBJECT_NAME_COLLISION.
    Exclusive,
    /// Open it as it is.
    Open,
    /// Empty it.
    Truncate,
}

/// An entry of a directory listing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DirEntry {
    /// The entry's name, without its directory.
    pub(crate) name: String,
    pub(crate) info: FileInfo,
}

/// The name of a drive the client shares, such as `C` or `Documents`: DeviceData when the client sent
/// it, otherwise PreferredDosName, which holds at most eight characters.
pub(crate) fn drive_name(device: &DeviceAnnounceHeader) -> String {
    name_from_device_data(device.device_data())
        .unwrap_or_else(|| device.preferred_dos_name().trim_end_matches(':').to_owned())
}

/// Reads a drive's full name from DeviceData. MS-RDPEFS 2.2.3.1 asks for a null-terminated UTF-16
/// string, as IronRDP's client sends, but FreeRDP sends 8-bit characters with a single null
/// terminator. Only UTF-16 ends with two zero bytes at an even offset, unless the 8-bit name is empty.
fn name_from_device_data(data: &[u8]) -> Option<String> {
    let utf16 = data.len() >= 4 && data.len().is_multiple_of(2) && data.ends_with(&[0, 0]);
    let name = if utf16 {
        let (pairs, _) = data.as_chunks::<2>();
        let units: Vec<u16> = pairs
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .take_while(|&unit| unit != 0)
            .collect();
        String::from_utf16(&units).ok()?
    } else {
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        String::from_utf8_lossy(&data[..end]).into_owned()
    };
    let name = name.trim().trim_end_matches(':').to_owned();
    if name.is_empty() || name.chars().any(char::is_control) {
        None
    } else {
        Some(name)
    }
}

/// The kinds of request sent here, each answered by a completion of its own layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Create,
    Close,
    Read,
    Write,
    QueryInformation,
    SetInformation,
    QueryDirectory,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Close => "close",
            Self::Read => "read",
            Self::Write => "write",
            Self::QueryInformation => "query information",
            Self::SetInformation => "set information",
            Self::QueryDirectory => "query directory",
        }
    }
}

/// What a completion holds besides its status.
#[derive(Debug)]
pub(crate) enum Answer {
    Create { file_id: u32 },
    Close,
    Read { data: Vec<u8> },
    Write { length: u32 },
    QueryInformation { buffer: Option<FileInformationClass> },
    SetInformation,
    QueryDirectory { buffer: Option<FileInformationClass> },
}

impl Answer {
    fn op(&self) -> Op {
        match self {
            Self::Create { .. } => Op::Create,
            Self::Close => Op::Close,
            Self::Read { .. } => Op::Read,
            Self::Write { .. } => Op::Write,
            Self::QueryInformation { .. } => Op::QueryInformation,
            Self::SetInformation => Op::SetInformation,
            Self::QueryDirectory { .. } => Op::QueryDirectory,
        }
    }
}

type Reply = RdpdrResult<(NtStatus, Answer)>;

/// A request waiting for its completion.
struct Waiter {
    device_id: u32,
    op: Op,
    reply: oneshot::Sender<Reply>,
}

struct Queue {
    /// Where requests go, as server events; `None` once the connection has ended.
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    /// Requests sent as server events, oldest first, whose completion IDs the server has not told yet.
    queued: VecDeque<Waiter>,
    /// Requests the server has sent to the client, by completion ID.
    sent: HashMap<u32, Waiter>,
}

/// The requests of one connection that wait for the client. [`RdpdrHandle`] queues them; the
/// channel's backend pairs them with their completion IDs and hands them their completions.
#[derive(Clone)]
pub(crate) struct Requests(Arc<Mutex<Queue>>);

impl Requests {
    /// Requests that go to `sender`, or that fail at once without one.
    pub(crate) fn new(sender: Option<mpsc::UnboundedSender<ServerEvent>>) -> Self {
        Self(Arc::new(Mutex::new(Queue {
            sender,
            queued: VecDeque::new(),
            sent: HashMap::new(),
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        // A panic while the lock was held cannot leave the queue inconsistent.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sends `message` as a server event and queues the request, under one lock: the server tells the
    /// completion IDs in the order the events were sent, which is then the order of the queue.
    fn send(&self, device_id: u32, op: Op, message: RdpdrServerMessage) -> RdpdrResult<oneshot::Receiver<Reply>> {
        let (reply, receiver) = oneshot::channel();
        let mut queue = self.lock();
        let sender = queue.sender.as_ref().ok_or(RdpdrError::Closed)?;
        sender
            .send(ServerEvent::Rdpdr(message))
            .map_err(|_| RdpdrError::Closed)?;
        queue.queued.push_back(Waiter { device_id, op, reply });
        Ok(receiver)
    }

    /// The server sent the oldest queued request under `completion_id`.
    pub(crate) fn sent(&self, completion_id: u32) {
        let mut queue = self.lock();
        // Requests that gave up waiting leave: the client may never answer them. Queued ones stay,
        // since their place in the queue is what pairs the requests behind them with their IDs.
        queue.sent.retain(|_, waiter| !waiter.reply.is_closed());
        match queue.queued.pop_front() {
            Some(waiter) => {
                queue.sent.insert(completion_id, waiter);
            }
            None => warn!(completion_id, "RDPDR request sent that was not queued"),
        }
    }

    /// Hands a completion to the request it answers.
    pub(crate) fn complete(&self, reply: &DeviceIoResponse, answer: Answer) {
        let Some(waiter) = self.lock().sent.remove(&reply.completion_id) else {
            debug!(
                completion_id = reply.completion_id,
                "RDPDR completion nobody waits for (timed out?)"
            );
            return;
        };
        let result = if waiter.device_id != reply.device_id || waiter.op != answer.op() {
            warn!(
                completion_id = reply.completion_id,
                device_id = reply.device_id,
                asked = waiter.op.name(),
                answered = answer.op().name(),
                "RDPDR completion does not fit its request"
            );
            Err(RdpdrError::Malformed {
                op: waiter.op.name(),
                reason: format!(
                    "a {} answer from device {} for a request to device {}",
                    answer.op().name(),
                    reply.device_id,
                    waiter.device_id
                ),
            })
        } else {
            Ok((reply.io_status, answer))
        };
        let _ = waiter.reply.send(result);
    }

    /// Fails every waiting request, and every later one: the connection has ended. Later requests
    /// would reach no client, or the channel of the next connection.
    pub(crate) fn close(&self) {
        let mut queue = self.lock();
        queue.sender = None;
        queue.queued.clear();
        queue.sent.clear();
    }
}

/// Which access a cached handle was opened with: a file can be open for reading and for writing at
/// once, and a read-only file cannot be opened for writing.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum AccessKind {
    Read,
    Write,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct HandleKey {
    device_id: u32,
    path: String,
    kind: AccessKind,
}

struct CachedHandle {
    file_id: u32,
    last_used: u64,
}

/// Open file handles, least recently used first out. Consecutive reads or writes of a file reuse one
/// handle instead of an open and a close for every chunk, so a large transfer costs one round trip
/// per chunk instead of three.
#[derive(Default)]
struct HandleCache {
    map: Mutex<HashMap<HandleKey, CachedHandle>>,
    tick: AtomicU64,
}

impl HandleCache {
    fn map(&self) -> MutexGuard<'_, HashMap<HandleKey, CachedHandle>> {
        self.map.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn stamp(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed)
    }
}

/// Sends device I/O requests to the client and waits for their completions. Clones share the
/// requests and the open handles.
///
/// Paths are relative to the drive's root, with backslashes: `\` is the root and `\docs\a.txt` a file
/// in the folder `docs`.
#[derive(Clone)]
pub(crate) struct RdpdrHandle {
    requests: Requests,
    cache: Arc<HandleCache>,
    timeout: Duration,
}

impl fmt::Debug for RdpdrHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpdrHandle")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl RdpdrHandle {
    pub(crate) fn new(requests: Requests) -> Self {
        Self {
            requests,
            cache: Arc::new(HandleCache::default()),
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// Returns a handle whose requests give up after `timeout` instead of [`REQUEST_TIMEOUT`].
    #[cfg(test)]
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The size, attributes and times of the file or folder at `path`.
    pub(crate) async fn stat(&self, device_id: u32, path: &str) -> RdpdrResult<FileInfo> {
        let file_id = self
            .open(
                device_id,
                path,
                DesiredAccess::FILE_READ_ATTRIBUTES | DesiredAccess::SYNCHRONIZE,
                CreateDisposition::FILE_OPEN,
                CreateOptions::empty(),
            )
            .await?;
        let info = self.query_info(device_id, file_id).await;
        self.close(device_id, file_id).await;
        info
    }

    /// The entries of the folder at `dir`, without `.` and `..`. The client returns one entry per
    /// request, so a listing costs a round trip for each entry.
    pub(crate) async fn list_dir(&self, device_id: u32, dir: &str) -> RdpdrResult<Vec<DirEntry>> {
        let file_id = self
            .open(
                device_id,
                dir,
                DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY
                    | DesiredAccess::FILE_READ_ATTRIBUTES
                    | DesiredAccess::SYNCHRONIZE,
                CreateDisposition::FILE_OPEN,
                CreateOptions::FILE_DIRECTORY_FILE | CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT,
            )
            .await?;
        let entries = self.query_directory(device_id, file_id, dir).await;
        self.close(device_id, file_id).await;
        entries
    }

    /// Reads up to `length` bytes at `offset` of the file at `path`, through a cached handle. Fewer
    /// bytes than asked for mean the end of the file.
    pub(crate) async fn read_file(&self, device_id: u32, path: &str, offset: u64, length: u32) -> RdpdrResult<Vec<u8>> {
        let file_id = self.acquire(device_id, path, AccessKind::Read).await?;
        let result = self.read(device_id, file_id, offset, length).await;
        if result.is_err() {
            // The handle may have gone stale; the next read opens a new one.
            self.invalidate(device_id, path, AccessKind::Read);
        }
        result
    }

    /// Writes `data` at `offset` of the existing file at `path`, through a cached handle, and returns
    /// how many bytes the client wrote.
    pub(crate) async fn write_file(&self, device_id: u32, path: &str, offset: u64, data: &[u8]) -> RdpdrResult<u32> {
        let file_id = self.acquire(device_id, path, AccessKind::Write).await?;
        let result = self.write(device_id, file_id, offset, data).await;
        if result.is_err() {
            self.invalidate(device_id, path, AccessKind::Write);
        }
        result
    }

    /// Creates a file at `path`, or does what `mode` says with one that exists.
    pub(crate) async fn create_file(&self, device_id: u32, path: &str, mode: CreateMode) -> RdpdrResult<()> {
        let disposition = match mode {
            CreateMode::Exclusive => CreateDisposition::FILE_CREATE,
            CreateMode::Open => CreateDisposition::FILE_OPEN_IF,
            CreateMode::Truncate => CreateDisposition::FILE_OVERWRITE_IF,
        };
        // A file being emptied may be open for reading or writing through a cached handle, whose view
        // of it would be stale.
        if mode == CreateMode::Truncate {
            self.evict_path(device_id, path).await;
        }
        let file_id = self
            .open(
                device_id,
                path,
                file_write_access(),
                disposition,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE,
            )
            .await?;
        self.close(device_id, file_id).await;
        Ok(())
    }

    /// Creates a folder at `path`, failing if something exists there.
    pub(crate) async fn create_dir(&self, device_id: u32, path: &str) -> RdpdrResult<()> {
        let file_id = self
            .open(
                device_id,
                path,
                file_write_access(),
                CreateDisposition::FILE_CREATE,
                CreateOptions::FILE_DIRECTORY_FILE | CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT,
            )
            .await?;
        self.close(device_id, file_id).await;
        Ok(())
    }

    /// Deletes the file, or the empty folder, at `path`: it is opened for deletion, marked delete
    /// pending, and removed when the handle closes.
    pub(crate) async fn remove(&self, device_id: u32, path: &str, is_dir: bool) -> RdpdrResult<()> {
        // A handle still open would postpone the deletion until it closed.
        self.evict_path(device_id, path).await;
        let options = if is_dir {
            CreateOptions::FILE_DIRECTORY_FILE | CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
        } else {
            CreateOptions::FILE_NON_DIRECTORY_FILE | CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
        };
        let file_id = self
            .open(
                device_id,
                path,
                DesiredAccess::DELETE | DesiredAccess::SYNCHRONIZE,
                CreateDisposition::FILE_OPEN,
                options,
            )
            .await?;
        let result = self
            .set_information(
                device_id,
                file_id,
                FileInformationClass::Disposition(FileDispositionInformation { delete_pending: 1 }),
            )
            .await;
        self.close(device_id, file_id).await;
        result
    }

    /// Renames or moves the file or folder at `from` to `to`, replacing what is at `to` if `replace`.
    pub(crate) async fn rename(&self, device_id: u32, from: &str, to: &str, replace: bool) -> RdpdrResult<()> {
        // A handle open on the source can block the rename, and one on the destination goes stale.
        self.evict_path(device_id, from).await;
        self.evict_path(device_id, to).await;
        let file_id = self
            .open(
                device_id,
                from,
                DesiredAccess::DELETE | DesiredAccess::SYNCHRONIZE,
                CreateDisposition::FILE_OPEN,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT,
            )
            .await?;
        let result = self
            .set_information(
                device_id,
                file_id,
                FileInformationClass::Rename(FileRenameInformation {
                    replace_if_exists: if replace { Boolean::True } else { Boolean::False },
                    file_name: to.to_owned(),
                }),
            )
            .await;
        self.close(device_id, file_id).await;
        result
    }

    /// Truncates or extends the file at `path` to `size` bytes.
    pub(crate) async fn set_len(&self, device_id: u32, path: &str, size: u64) -> RdpdrResult<()> {
        let file_id = self
            .open(
                device_id,
                path,
                file_write_access(),
                CreateDisposition::FILE_OPEN,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE,
            )
            .await?;
        let end_of_file = i64::try_from(size).unwrap_or(i64::MAX);
        let result = self
            .set_information(
                device_id,
                file_id,
                FileInformationClass::EndOfFile(FileEndOfFileInformation { end_of_file }),
            )
            .await;
        self.close(device_id, file_id).await;
        result
    }

    /// Sets the last access and last write times of the file or folder at `path`, as FILETIMEs;
    /// `None` leaves a time as it is.
    pub(crate) async fn set_times(
        &self,
        device_id: u32,
        path: &str,
        last_access_time: Option<i64>,
        last_write_time: Option<i64>,
    ) -> RdpdrResult<()> {
        let file_id = self
            .open(
                device_id,
                path,
                DesiredAccess::FILE_WRITE_ATTRIBUTES | DesiredAccess::SYNCHRONIZE,
                CreateDisposition::FILE_OPEN,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT,
            )
            .await?;
        // Zero leaves a time, and the attributes, unchanged (MS-FSCC 2.4.7).
        let result = self
            .set_information(
                device_id,
                file_id,
                FileInformationClass::Basic(FileBasicInformation {
                    creation_time: 0,
                    last_access_time: last_access_time.unwrap_or(0),
                    last_write_time: last_write_time.unwrap_or(0),
                    change_time: 0,
                    file_attributes: FileAttributes::empty(),
                }),
            )
            .await;
        self.close(device_id, file_id).await;
        result
    }

    /// Forgets the cached handles of a drive the client no longer shares; they died with it.
    pub(crate) fn forget_device(&self, device_id: u32) {
        self.cache.map().retain(|key, _| key.device_id != device_id);
    }

    /// Sends a request and waits for its completion, or gives up after the timeout.
    async fn request(&self, device_id: u32, op: Op, message: RdpdrServerMessage) -> Reply {
        let receiver = self.requests.send(device_id, op, message)?;
        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(RdpdrError::Closed),
            Err(_) => {
                warn!(op = op.name(), timeout = ?self.timeout, "RDPDR request timed out");
                Err(RdpdrError::Timeout { op: op.name() })
            }
        }
    }

    /// IRP_MJ_CREATE: opens or creates `path` and returns the client's file ID.
    async fn open(
        &self,
        device_id: u32,
        path: &str,
        desired_access: DesiredAccess,
        create_disposition: CreateDisposition,
        create_options: CreateOptions,
    ) -> RdpdrResult<u32> {
        let message = RdpdrServerMessage::Create {
            device_id,
            path: path.to_owned(),
            desired_access,
            create_disposition,
            create_options,
        };
        let (status, answer) = self.request(device_id, Op::Create, message).await?;
        check(Op::Create, status)?;
        match answer {
            Answer::Create { file_id } => Ok(file_id),
            other => Err(misfit(Op::Create, &other)),
        }
    }

    async fn close(&self, device_id: u32, file_id: u32) {
        let result = self
            .request(device_id, Op::Close, RdpdrServerMessage::Close { device_id, file_id })
            .await
            .and_then(|(status, _)| check(Op::Close, status));
        if let Err(error) = result {
            debug!(%error, "RDPDR close failed");
        }
    }

    /// Closes a handle without waiting: for handles the cache lets go of.
    fn close_in_background(&self, device_id: u32, file_id: u32) {
        let this = self.clone();
        tokio::spawn(async move { this.close(device_id, file_id).await });
    }

    async fn read(&self, device_id: u32, file_id: u32, offset: u64, length: u32) -> RdpdrResult<Vec<u8>> {
        let message = RdpdrServerMessage::Read {
            device_id,
            file_id,
            length,
            offset,
        };
        let (status, answer) = self.request(device_id, Op::Read, message).await?;
        check(Op::Read, status)?;
        match answer {
            Answer::Read { data } => Ok(data),
            other => Err(misfit(Op::Read, &other)),
        }
    }

    async fn write(&self, device_id: u32, file_id: u32, offset: u64, data: &[u8]) -> RdpdrResult<u32> {
        let message = RdpdrServerMessage::Write {
            device_id,
            file_id,
            data: data.to_vec(),
            offset,
        };
        let (status, answer) = self.request(device_id, Op::Write, message).await?;
        check(Op::Write, status)?;
        match answer {
            Answer::Write { length } => Ok(length),
            other => Err(misfit(Op::Write, &other)),
        }
    }

    async fn set_information(&self, device_id: u32, file_id: u32, set_buffer: FileInformationClass) -> RdpdrResult<()> {
        let message = RdpdrServerMessage::SetInformation {
            device_id,
            file_id,
            set_buffer,
        };
        let (status, _) = self.request(device_id, Op::SetInformation, message).await?;
        check(Op::SetInformation, status)
    }

    async fn query_information(
        &self,
        device_id: u32,
        file_id: u32,
        info_class: FileInformationClassLevel,
    ) -> RdpdrResult<FileInformationClass> {
        const OP: Op = Op::QueryInformation;
        let message = RdpdrServerMessage::QueryInformation {
            device_id,
            file_id,
            info_class,
        };
        let (status, answer) = self.request(device_id, OP, message).await?;
        check(OP, status)?;
        match answer {
            Answer::QueryInformation { buffer: Some(buffer) } => Ok(buffer),
            Answer::QueryInformation { buffer: None } => Err(RdpdrError::Malformed {
                op: OP.name(),
                reason: "no information in a successful answer".to_owned(),
            }),
            other => Err(misfit(OP, &other)),
        }
    }

    async fn query_info(&self, device_id: u32, file_id: u32) -> RdpdrResult<FileInfo> {
        let basic = self
            .query_information(device_id, file_id, FileInformationClassLevel::FILE_BASIC_INFORMATION)
            .await?;
        let standard = self
            .query_information(device_id, file_id, FileInformationClassLevel::FILE_STANDARD_INFORMATION)
            .await?;
        match (basic, standard) {
            (FileInformationClass::Basic(basic), FileInformationClass::Standard(standard)) => {
                let mut attributes = basic.file_attributes;
                // Some clients leave the directory attribute out of FileBasicInformation.
                if standard.directory == Boolean::True {
                    attributes |= FileAttributes::FILE_ATTRIBUTE_DIRECTORY;
                }
                Ok(FileInfo {
                    size: u64::try_from(standard.end_of_file).unwrap_or(0),
                    attributes,
                    creation_time: basic.creation_time,
                    last_access_time: basic.last_access_time,
                    last_write_time: basic.last_write_time,
                    change_time: basic.change_time,
                })
            }
            _ => Err(RdpdrError::Malformed {
                op: Op::QueryInformation.name(),
                reason: "an answer of another class than asked for".to_owned(),
            }),
        }
    }

    async fn query_directory(&self, device_id: u32, file_id: u32, dir: &str) -> RdpdrResult<Vec<DirEntry>> {
        const OP: Op = Op::QueryDirectory;
        let pattern = query_pattern(dir);
        let mut entries = Vec::new();
        let mut initial = true;
        loop {
            let message = RdpdrServerMessage::QueryDirectory {
                device_id,
                file_id,
                info_class: FileInformationClassLevel::FILE_DIRECTORY_INFORMATION,
                // Later queries continue the enumeration and carry no path.
                path: if initial { pattern.clone() } else { String::new() },
                initial_query: initial,
            };
            initial = false;
            let (status, answer) = self.request(device_id, OP, message).await?;
            if status == NtStatus::NO_MORE_FILES {
                break;
            }
            // An empty folder may answer the first query with STATUS_NO_SUCH_FILE.
            if status == NtStatus::NO_SUCH_FILE && entries.is_empty() {
                break;
            }
            check(OP, status)?;
            let entry = match answer {
                Answer::QueryDirectory {
                    buffer: Some(FileInformationClass::Directory(entry)),
                } => entry,
                Answer::QueryDirectory { .. } => break,
                other => return Err(misfit(OP, &other)),
            };
            if entry.file_name == "." || entry.file_name == ".." {
                continue;
            }
            entries.push(DirEntry {
                name: entry.file_name,
                info: FileInfo {
                    size: u64::try_from(entry.end_of_file).unwrap_or(0),
                    attributes: entry.file_attributes,
                    creation_time: entry.creation_time,
                    last_access_time: entry.last_access_time,
                    last_write_time: entry.last_write_time,
                    change_time: entry.change_time,
                },
            });
        }
        Ok(entries)
    }

    /// A cached handle for `path` opened for `kind`, opened and cached if there is none.
    async fn acquire(&self, device_id: u32, path: &str, kind: AccessKind) -> RdpdrResult<u32> {
        let key = HandleKey {
            device_id,
            path: path.to_owned(),
            kind,
        };
        if let Some(handle) = self.cache.map().get_mut(&key) {
            handle.last_used = self.cache.stamp();
            return Ok(handle.file_id);
        }
        let access = match kind {
            AccessKind::Read => file_read_access(),
            AccessKind::Write => file_write_access(),
        };
        let file_id = self
            .open(
                device_id,
                path,
                access,
                CreateDisposition::FILE_OPEN,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE,
            )
            .await?;

        let mut to_close = Vec::new();
        let file_id = {
            let mut map = self.cache.map();
            if let Some(handle) = map.get_mut(&key) {
                // Another request opened the same file meanwhile: use its handle and close ours.
                handle.last_used = self.cache.stamp();
                to_close.push((device_id, file_id));
                handle.file_id
            } else {
                map.insert(
                    key,
                    CachedHandle {
                        file_id,
                        last_used: self.cache.stamp(),
                    },
                );
                if map.len() > MAX_OPEN_HANDLES {
                    let oldest = map.iter().min_by_key(|(_, h)| h.last_used).map(|(k, _)| k.clone());
                    if let Some(evicted) = oldest.and_then(|k| map.remove(&k).map(|h| (k.device_id, h.file_id))) {
                        to_close.push(evicted);
                    }
                }
                file_id
            }
        };
        for (device_id, file_id) in to_close {
            self.close_in_background(device_id, file_id);
        }
        Ok(file_id)
    }

    /// Drops a cached handle after a failed request and closes it.
    fn invalidate(&self, device_id: u32, path: &str, kind: AccessKind) {
        let key = HandleKey {
            device_id,
            path: path.to_owned(),
            kind,
        };
        let file_id = self.cache.map().remove(&key).map(|h| h.file_id);
        if let Some(file_id) = file_id {
            self.close_in_background(device_id, file_id);
        }
    }

    /// Closes the cached handles of `path`, of both kinds, and waits for the client to confirm.
    async fn evict_path(&self, device_id: u32, path: &str) {
        let file_ids: Vec<u32> = {
            let mut map = self.cache.map();
            let keys: Vec<HandleKey> = map
                .keys()
                .filter(|k| k.device_id == device_id && k.path == path)
                .cloned()
                .collect();
            keys.iter().filter_map(|k| map.remove(k)).map(|h| h.file_id).collect()
        };
        for file_id in file_ids {
            self.close(device_id, file_id).await;
        }
    }
}

fn check(op: Op, status: NtStatus) -> RdpdrResult<()> {
    if status == NtStatus::SUCCESS {
        Ok(())
    } else {
        Err(RdpdrError::Status { op: op.name(), status })
    }
}

/// The error for an answer of another kind than the request, which [`Requests::complete`] already
/// turns away.
fn misfit(op: Op, answer: &Answer) -> RdpdrError {
    RdpdrError::Malformed {
        op: op.name(),
        reason: format!("a {} answer", answer.op().name()),
    }
}

/// What `CreateFile(GENERIC_READ)` expands to. mstsc honours the rights strictly: without SYNCHRONIZE
/// the synchronous read that follows fails with STATUS_ACCESS_DENIED, which FreeRDP lets pass.
fn file_read_access() -> DesiredAccess {
    DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY
        | DesiredAccess::FILE_READ_ATTRIBUTES
        | DesiredAccess::FILE_READ_EA
        | DesiredAccess::READ_CONTROL
        | DesiredAccess::SYNCHRONIZE
}

/// Read and write rights, so that a handle opened for writing can also be read.
fn file_write_access() -> DesiredAccess {
    file_read_access()
        | DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE
        | DesiredAccess::FILE_APPEND_DATA_OR_FILE_ADD_SUBDIRECTORY
        | DesiredAccess::FILE_WRITE_ATTRIBUTES
        | DesiredAccess::FILE_WRITE_EA
}

/// The search pattern of the first query of a folder: its path followed by `\*`.
fn query_pattern(dir: &str) -> String {
    if dir.is_empty() || dir == "\\" {
        "\\*".to_owned()
    } else if dir.ends_with('\\') {
        format!("{dir}*")
    } else {
        format!("{dir}\\*")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ironrdp_core::{encode_vec, ReadCursor};
    use ironrdp_rdpdr::pdu::efs::{
        CapabilityMessage, ClientDeviceListAnnounce, ClientDriveQueryDirectoryResponse,
        ClientDriveQueryInformationResponse, ClientDriveSetInformationResponse, ClientNameRequest,
        ClientNameRequestUnicodeFlag, CoreCapability, CoreCapabilityKind, DeviceCloseResponse, DeviceCreateResponse,
        DeviceIoRequest, DeviceReadResponse, DeviceWriteResponse, Devices, FileDirectoryInformation,
        FileStandardInformation, Information, ServerDriveIoRequest, SharedAccess, VersionAndIdPdu, VersionAndIdPduKind,
    };
    use ironrdp_rdpdr::pdu::{RdpdrPdu, SharedHeader};
    use ironrdp_rdpdr::server::RdpdrServer;
    use ironrdp_svc::{SvcMessage, SvcProcessor};
    use tokio::task::JoinHandle;

    use super::super::DriveBackend;
    use super::*;

    /// IronRDP's RDPDR channel with the drives' backend, as a session runs it, and the server events
    /// the backend's handle sends.
    struct Channel {
        server: RdpdrServer,
        events: mpsc::UnboundedReceiver<ServerEvent>,
        handle: RdpdrHandle,
    }

    impl Channel {
        /// A channel through its initialisation, as a client goes through it.
        fn new() -> Self {
            let (sender, events) = mpsc::unbounded_channel();
            let backend = DriveBackend::new(PathBuf::from("/nonexistent/rdpmac-drives-test"), Some(sender));
            let handle = backend.handle.clone();
            let mut channel = Self {
                server: RdpdrServer::new(Box::new(backend)),
                events,
                handle,
            };
            assert_eq!(channel.server.start().unwrap().len(), 1, "Server Announce");
            channel.client_sends(RdpdrPdu::VersionAndIdPdu(VersionAndIdPdu {
                version_major: 1,
                version_minor: 12,
                client_id: 1,
                kind: VersionAndIdPduKind::ClientAnnounceReply,
            }));
            let capabilities = channel.client_sends(RdpdrPdu::ClientNameRequest(ClientNameRequest::new(
                "DESKTOP-01".to_owned(),
                ClientNameRequestUnicodeFlag::Unicode,
            )));
            assert_eq!(capabilities.len(), 2, "capabilities and Client ID Confirm");
            let logged_on = channel.client_sends(RdpdrPdu::CoreCapability(CoreCapability {
                capabilities: vec![CapabilityMessage::new_general(0), CapabilityMessage::new_drive()],
                kind: CoreCapabilityKind::ClientCoreCapabilityResponse,
            }));
            assert_eq!(logged_on.len(), 1, "User Logged On, without which clients announce no drives");
            channel
        }

        fn client_sends(&mut self, pdu: RdpdrPdu) -> Vec<SvcMessage> {
            self.server.process(&encode_vec(&pdu).unwrap()).unwrap()
        }

        fn backend(&self) -> &DriveBackend {
            self.server.downcast_backend::<DriveBackend>().unwrap()
        }

        /// Sends the requests the handle asked for, as IronRDP's server does with its events, and
        /// returns them as the client decodes them.
        fn requests(&mut self) -> Vec<ServerDriveIoRequest> {
            let mut requests = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                let ServerEvent::Rdpdr(message) = event else {
                    panic!("a server event other than RDPDR");
                };
                for sent in dispatch(&mut self.server, message) {
                    requests.push(decode(&sent));
                }
            }
            requests
        }

        /// Runs `task`, answering every request it sends with `answer`, and returns what the task
        /// returned and the requests it sent.
        async fn run<T>(
            &mut self,
            task: JoinHandle<T>,
            mut answer: impl FnMut(&ServerDriveIoRequest) -> RdpdrPdu,
        ) -> (T, Vec<ServerDriveIoRequest>) {
            let mut sent = Vec::new();
            for _ in 0..1000 {
                tokio::task::yield_now().await;
                for request in self.requests() {
                    let reply = answer(&request);
                    assert!(self.client_sends(reply).is_empty());
                    sent.push(request);
                }
                if task.is_finished() {
                    return (task.await.unwrap(), sent);
                }
            }
            panic!("the task did not finish; requests so far: {sent:?}");
        }
    }

    /// What IronRDP's server does with an RDPDR server event.
    fn dispatch(server: &mut RdpdrServer, message: RdpdrServerMessage) -> Vec<SvcMessage> {
        match message {
            RdpdrServerMessage::Create {
                device_id,
                path,
                desired_access,
                create_disposition,
                create_options,
            } => server.drive_create(device_id, path, desired_access, create_disposition, create_options),
            RdpdrServerMessage::Read {
                device_id,
                file_id,
                length,
                offset,
            } => server.drive_read(device_id, file_id, length, offset),
            RdpdrServerMessage::Write {
                device_id,
                file_id,
                data,
                offset,
            } => server.drive_write(device_id, file_id, data, offset),
            RdpdrServerMessage::Close { device_id, file_id } => server.drive_close(device_id, file_id),
            RdpdrServerMessage::QueryInformation {
                device_id,
                file_id,
                info_class,
            } => server.drive_query_information(device_id, file_id, info_class),
            RdpdrServerMessage::SetInformation {
                device_id,
                file_id,
                set_buffer,
            } => server.drive_set_information(device_id, file_id, set_buffer),
            RdpdrServerMessage::QueryDirectory {
                device_id,
                file_id,
                info_class,
                path,
                initial_query,
            } => server.drive_query_directory(device_id, file_id, info_class, path, initial_query),
            other => panic!("the handle sent {other:?}"),
        }
        .unwrap()
    }

    fn decode(message: &SvcMessage) -> ServerDriveIoRequest {
        let bytes = message.encode_unframed_pdu().unwrap();
        let mut src = ReadCursor::new(&bytes);
        SharedHeader::decode(&mut src).unwrap();
        let io = DeviceIoRequest::decode(&mut src).unwrap();
        ServerDriveIoRequest::decode(io, &mut src).unwrap()
    }

    fn header(request: &ServerDriveIoRequest) -> &DeviceIoRequest {
        match request {
            ServerDriveIoRequest::ServerCreateDriveRequest(r) => &r.device_io_request,
            ServerDriveIoRequest::ServerDriveQueryInformationRequest(r) => &r.device_io_request,
            ServerDriveIoRequest::DeviceCloseRequest(r) => &r.device_io_request,
            ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(r) => &r.device_io_request,
            ServerDriveIoRequest::DeviceReadRequest(r) => &r.device_io_request,
            ServerDriveIoRequest::DeviceWriteRequest(r) => &r.device_io_request,
            ServerDriveIoRequest::ServerDriveSetInformationRequest(r) => &r.device_io_request,
            other => panic!("unexpected request {other:?}"),
        }
    }

    fn reply(request: &ServerDriveIoRequest, io_status: NtStatus) -> DeviceIoResponse {
        let header = header(request);
        DeviceIoResponse {
            device_id: header.device_id,
            completion_id: header.completion_id,
            io_status,
        }
    }

    /// A client's successful answer, to the requests that need nothing more than a file ID or a
    /// status.
    fn success(request: &ServerDriveIoRequest, file_id: u32) -> RdpdrPdu {
        let io = reply(request, NtStatus::SUCCESS);
        match request {
            ServerDriveIoRequest::ServerCreateDriveRequest(_) => RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
                device_io_reply: io,
                file_id,
                information: Information::FILE_OPENED,
            }),
            ServerDriveIoRequest::DeviceCloseRequest(_) => {
                RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse { device_io_response: io })
            }
            ServerDriveIoRequest::ServerDriveSetInformationRequest(set) => RdpdrPdu::ClientDriveSetInformationResponse(
                ClientDriveSetInformationResponse::new(set, NtStatus::SUCCESS).unwrap(),
            ),
            ServerDriveIoRequest::DeviceWriteRequest(write) => RdpdrPdu::DeviceWriteResponse(DeviceWriteResponse {
                device_io_reply: io,
                length: u32::try_from(write.write_data.len()).unwrap(),
            }),
            other => panic!("no answer for {other:?}"),
        }
    }

    /// The answer to a query of a file's basic or standard information, for a file of `size` bytes.
    fn information(request: &ServerDriveIoRequest, size: i64) -> RdpdrPdu {
        let ServerDriveIoRequest::ServerDriveQueryInformationRequest(query) = request else {
            panic!("expected a query of information, got {request:?}");
        };
        let buffer = if query.file_info_class_lvl == FileInformationClassLevel::FILE_BASIC_INFORMATION {
            FileInformationClass::Basic(FileBasicInformation {
                creation_time: 1,
                last_access_time: 2,
                last_write_time: 3,
                change_time: 4,
                file_attributes: FileAttributes::FILE_ATTRIBUTE_ARCHIVE,
            })
        } else {
            FileInformationClass::Standard(FileStandardInformation {
                allocation_size: 4096,
                end_of_file: size,
                number_of_links: 1,
                delete_pending: Boolean::False,
                directory: Boolean::False,
            })
        };
        RdpdrPdu::ClientDriveQueryInformationResponse(ClientDriveQueryInformationResponse {
            device_io_response: reply(request, NtStatus::SUCCESS),
            buffer: Some(buffer),
        })
    }

    #[tokio::test]
    async fn stat_opens_queries_and_closes() {
        let mut channel = Channel::new();
        let handle = channel.handle.clone();
        let task = tokio::spawn(async move { handle.stat(1, "\\docs\\a.txt").await });
        let (info, requests) = channel
            .run(task, |request| match request {
                ServerDriveIoRequest::ServerDriveQueryInformationRequest(_) => information(request, 1234),
                _ => success(request, 5),
            })
            .await;
        assert_eq!(
            info.unwrap(),
            FileInfo {
                size: 1234,
                attributes: FileAttributes::FILE_ATTRIBUTE_ARCHIVE,
                creation_time: 1,
                last_access_time: 2,
                last_write_time: 3,
                change_time: 4,
            }
        );
        let ServerDriveIoRequest::ServerCreateDriveRequest(create) = &requests[0] else {
            panic!("expected a create, got {:?}", requests[0]);
        };
        assert_eq!(create.path, "\\docs\\a.txt");
        assert_eq!(create.create_disposition, CreateDisposition::FILE_OPEN);
        // mstsc refuses any other share bit with STATUS_INVALID_PARAMETER, which macOS shows as
        // error -50 on every file.
        assert_eq!(
            create.shared_access,
            SharedAccess::FILE_SHARE_READ | SharedAccess::FILE_SHARE_WRITE | SharedAccess::FILE_SHARE_DELETE
        );
        assert_eq!(requests.len(), 4, "open, two queries, close: {requests:?}");
        assert!(
            matches!(&requests[3], ServerDriveIoRequest::DeviceCloseRequest(c) if c.device_io_request.file_id == 5)
        );
    }

    #[tokio::test]
    async fn listing_stops_at_no_more_files() {
        let mut channel = Channel::new();
        let handle = channel.handle.clone();
        let task = tokio::spawn(async move { handle.list_dir(1, "\\").await });
        let mut names = [".", "..", "a.txt", "sub"].into_iter();
        let mut patterns = Vec::new();
        let (listed, _) = channel
            .run(task, |request| {
                let ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(query) = request else {
                    return success(request, 8);
                };
                patterns.push(query.path.clone());
                let (status, buffer) = match names.next() {
                    Some(name) => {
                        let attributes = if name == "sub" {
                            FileAttributes::FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            FileAttributes::FILE_ATTRIBUTE_ARCHIVE
                        };
                        let entry = FileDirectoryInformation::new(0, 0, 7, 0, 42, attributes, name.to_owned());
                        (NtStatus::SUCCESS, Some(FileInformationClass::Directory(entry)))
                    }
                    None => (NtStatus::NO_MORE_FILES, None),
                };
                RdpdrPdu::ClientDriveQueryDirectoryResponse(ClientDriveQueryDirectoryResponse {
                    device_io_reply: reply(request, status),
                    buffer,
                })
            })
            .await;
        let listed = listed.unwrap();
        let names: Vec<_> = listed.iter().map(|e| (e.name.as_str(), e.info.is_dir())).collect();
        assert_eq!(names, [("a.txt", false), ("sub", true)]);
        assert_eq!(listed[0].info.size, 42);
        assert_eq!(listed[0].info.last_write_time, 7);
        // Only the first query carries the pattern.
        assert_eq!(patterns, ["\\*", "", "", "", ""]);
    }

    #[tokio::test]
    async fn create_modes_and_times_reach_the_client() {
        let mut channel = Channel::new();
        for (mode, disposition) in [
            (CreateMode::Exclusive, CreateDisposition::FILE_CREATE),
            (CreateMode::Open, CreateDisposition::FILE_OPEN_IF),
            (CreateMode::Truncate, CreateDisposition::FILE_OVERWRITE_IF),
        ] {
            let handle = channel.handle.clone();
            let task = tokio::spawn(async move { handle.create_file(1, "\\new.txt", mode).await });
            let (result, requests) = channel.run(task, |request| success(request, 9)).await;
            result.unwrap();
            let ServerDriveIoRequest::ServerCreateDriveRequest(create) = &requests[0] else {
                panic!("expected a create, got {:?}", requests[0]);
            };
            assert_eq!(create.create_disposition, disposition, "{mode:?}");
        }

        let handle = channel.handle.clone();
        let task = tokio::spawn(async move {
            handle
                .set_times(1, "\\new.txt", None, Some(133_000_000_000_000_000))
                .await
        });
        let (result, requests) = channel.run(task, |request| success(request, 9)).await;
        result.unwrap();
        let ServerDriveIoRequest::ServerDriveSetInformationRequest(set) = &requests[1] else {
            panic!("expected a set information, got {:?}", requests[1]);
        };
        assert_eq!(
            set.set_buffer,
            FileInformationClass::Basic(FileBasicInformation {
                creation_time: 0,
                last_access_time: 0,
                last_write_time: 133_000_000_000_000_000,
                change_time: 0,
                file_attributes: FileAttributes::empty(),
            })
        );
    }

    #[tokio::test]
    async fn reads_and_writes_reuse_one_handle() {
        let mut channel = Channel::new();
        let handle = channel.handle.clone();
        let task = tokio::spawn(async move {
            let first = handle.read_file(1, "\\a.bin", 0, 4).await?;
            let second = handle.read_file(1, "\\a.bin", 4, 4).await?;
            let written = handle.write_file(1, "\\a.bin", 8, b"more").await?;
            RdpdrResult::Ok((first, second, written))
        });
        let (result, requests) = channel
            .run(task, |request| match request {
                ServerDriveIoRequest::DeviceReadRequest(read) => RdpdrPdu::DeviceReadResponse(DeviceReadResponse {
                    device_io_reply: reply(request, NtStatus::SUCCESS),
                    read_data: if read.offset == 0 { b"abcd".to_vec() } else { b"ef".to_vec() },
                }),
                _ => success(request, 3),
            })
            .await;
        let (first, second, written) = result.unwrap();
        assert_eq!((first.as_slice(), second.as_slice(), written), (&b"abcd"[..], &b"ef"[..], 4));
        let kinds: Vec<&str> = requests
            .iter()
            .map(|r| match r {
                ServerDriveIoRequest::ServerCreateDriveRequest(_) => "create",
                ServerDriveIoRequest::DeviceReadRequest(_) => "read",
                ServerDriveIoRequest::DeviceWriteRequest(_) => "write",
                _ => "other",
            })
            .collect();
        // One open for reading serves both reads; writing needs a handle of its own.
        assert_eq!(kinds, ["create", "read", "read", "create", "write"]);
    }

    #[tokio::test]
    async fn a_failure_status_is_reported() {
        let mut channel = Channel::new();
        let handle = channel.handle.clone();
        let task = tokio::spawn(async move { handle.stat(1, "\\missing").await });
        let (result, _) = channel
            .run(task, |request| {
                RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
                    device_io_reply: reply(request, NtStatus::OBJECT_NAME_NOT_FOUND),
                    file_id: 0,
                    information: Information::empty(),
                })
            })
            .await;
        assert_eq!(
            result,
            Err(RdpdrError::Status {
                op: "create",
                status: NtStatus::OBJECT_NAME_NOT_FOUND
            })
        );
    }

    /// Clients may answer requests in another order than they got them; each answer still reaches
    /// the request it answers.
    #[tokio::test]
    async fn completions_reach_their_requests_in_any_order() {
        let mut channel = Channel::new();
        let (a, b) = (channel.handle.clone(), channel.handle.clone());
        let task = tokio::spawn(async move { tokio::join!(a.stat(1, "\\a"), b.stat(1, "\\b")) });

        let mut creates = Vec::new();
        for _ in 0..100 {
            tokio::task::yield_now().await;
            creates.extend(channel.requests());
            if creates.len() == 2 {
                break;
            }
        }
        assert_eq!(creates.len(), 2, "both stats open their file first");
        // The later request is answered first. The file IDs follow the paths: 1 for \a, 2 for \b.
        for request in creates.iter().rev() {
            let ServerDriveIoRequest::ServerCreateDriveRequest(create) = request else {
                panic!("expected a create, got {request:?}");
            };
            let file_id = if create.path == "\\a" { 1 } else { 2 };
            assert!(channel.client_sends(success(request, file_id)).is_empty());
        }
        // Each file's size is a hundred times its ID.
        let ((a, b), _) = channel
            .run(task, |request| match request {
                ServerDriveIoRequest::ServerDriveQueryInformationRequest(query) => {
                    information(request, i64::from(query.device_io_request.file_id) * 100)
                }
                _ => success(request, 0),
            })
            .await;
        assert_eq!((a.unwrap().size, b.unwrap().size), (100, 200));
    }

    /// A request that gave up keeps its place in the queue, so the requests behind it still get
    /// their own completions, and a late answer to it goes nowhere.
    #[tokio::test(start_paused = true)]
    async fn a_request_without_an_answer_times_out() {
        let mut channel = Channel::new();
        let slow = channel.handle.clone().with_timeout(Duration::from_secs(5));
        assert_eq!(slow.stat(1, "\\slow").await, Err(RdpdrError::Timeout { op: "create" }));

        let handle = channel.handle.clone();
        let task = tokio::spawn(async move { handle.stat(1, "\\fast").await });
        let (info, requests) = channel
            .run(task, |request| match request {
                ServerDriveIoRequest::ServerCreateDriveRequest(create) if create.path == "\\slow" => {
                    success(request, 666)
                }
                ServerDriveIoRequest::ServerDriveQueryInformationRequest(_) => information(request, 7),
                _ => success(request, 7),
            })
            .await;
        assert_eq!(info.unwrap().size, 7);
        assert!(
            requests.iter().all(|r| header(r).file_id != 666),
            "nothing used the late answer: {requests:?}"
        );
    }

    #[tokio::test]
    async fn waiting_requests_fail_when_the_connection_ends() {
        let channel = Channel::new();
        let handle = channel.handle.clone();
        let task = tokio::spawn({
            let handle = handle.clone();
            async move { handle.stat(1, "\\a").await }
        });
        tokio::task::yield_now().await;
        drop(channel);
        assert_eq!(task.await.unwrap(), Err(RdpdrError::Closed));
        // And later requests fail at once, without an event that could reach the next connection.
        assert_eq!(handle.stat(1, "\\b").await, Err(RdpdrError::Closed));
    }

    #[tokio::test]
    async fn only_drives_are_accepted() {
        let mut channel = Channel::new();
        let mut devices = Devices::new();
        devices.add_drive(1, "C".to_owned());
        devices.add_smartcard(2);
        devices.add_printer(3, "Printer".to_owned());
        let replies = channel.client_sends(RdpdrPdu::ClientDeviceListAnnounce(ClientDeviceListAnnounce {
            device_list: devices.clone_inner(),
        }));
        let results: Vec<(u32, NtStatus)> = replies
            .iter()
            .map(|message| {
                let bytes = message.encode_unframed_pdu().unwrap();
                (
                    u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                    NtStatus::from(u32::from_le_bytes(bytes[8..12].try_into().unwrap())),
                )
            })
            .collect();
        assert_eq!(
            results,
            [
                (1, NtStatus::SUCCESS),
                (2, NtStatus::ACCESS_DENIED),
                (3, NtStatus::ACCESS_DENIED)
            ]
        );
        let mounting: Vec<u32> = channel.backend().drives.keys().copied().collect();
        assert_eq!(mounting, [1]);
    }

    /// A Client Device List Announce with one drive, as the client encodes it.
    fn announced(dos_name: &[u8; 8], data: &[u8]) -> DeviceAnnounceHeader {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes()); // DeviceCount
        bytes.extend_from_slice(&0x08u32.to_le_bytes()); // RDPDR_DTYP_FILESYSTEM
        bytes.extend_from_slice(&1u32.to_le_bytes()); // DeviceId
        bytes.extend_from_slice(dos_name);
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(data);
        let mut announce = ClientDeviceListAnnounce::decode(&mut ReadCursor::new(&bytes)).unwrap();
        announce.device_list.remove(0)
    }

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn drive_names_are_read_in_either_encoding() {
        assert_eq!(drive_name(&announced(b"C\0\0\0\0\0\0\0", &utf16z("C on DESKTOP"))), "C on DESKTOP");
        // FreeRDP: 8-bit, and PreferredDosName cut to eight characters.
        assert_eq!(drive_name(&announced(b"Document", b"Documents_2026\0")), "Documents_2026");
        assert_eq!(drive_name(&announced(b"ignored\0", b"Photos\0")), "Photos");
        // No DeviceData: PreferredDosName, without its colon.
        assert_eq!(drive_name(&announced(b"D:\0\0\0\0\0\0", b"")), "D");
        // "文档" (documents), in both encodings.
        assert_eq!(name_from_device_data(&utf16z("\u{6587}\u{6863}")).as_deref(), Some("\u{6587}\u{6863}"));
        assert_eq!(name_from_device_data("\u{6587}\u{6863}\0".as_bytes()).as_deref(), Some("\u{6587}\u{6863}"));
        assert_eq!(name_from_device_data(b"abc\0").as_deref(), Some("abc"));
        assert_eq!(name_from_device_data(b"\0"), None);
        assert_eq!(name_from_device_data(b""), None);
    }
}
