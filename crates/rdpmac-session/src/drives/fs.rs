//! The NFSv3 file system that serves one redirected drive (ADR-0003).
//!
//! macOS's NFS client asks for files by file handle and by name; the client drive answers RDPDR
//! requests by Windows path. [`DriveFs`] gives every path it meets a stable file ID, and turns each
//! NFS call into as few round trips over the RDP connection as it can:
//!
//! - A directory is listed once per enumeration and the pages macOS asks for come from that listing.
//!   The client returns one entry per request, so listing again for every page would multiply the
//!   round trips by the number of pages.
//! - A name is looked up by asking for that one path, which costs the same four round trips (open,
//!   two queries, close) whatever the size of the directory.
//! - Attributes come from the client with their real times and are asked for again once they are
//!   older than [`ATTR_TTL`]. macOS decides from the modification time and the size whether what it
//!   cached of a file or directory is still valid, so fixed or stale values would hide changes made on
//!   the client.
//! - A renamed file or folder keeps its file ID, as NFS requires: programs that save by writing a
//!   temporary file and renaming it over the original go on using the handle they hold.
//!
//! The names macOS uses for its own purposes (`._*` files holding extended attributes, `.DS_Store`
//! and the like) never reach the client. Their contents are kept here, in memory, for the session:
//! the client's disk gets none of them, and extended attributes last as long as the session.
//!
//! Only the mount this server was made for can reach it. The server accepts a single MOUNT, puts a
//! random secret into every file handle and refuses handles without it, so another process that finds
//! the loopback port can neither mount the drive nor forge a handle. Files are reported as the user's,
//! with modes 700 and 600.
//!
//! Adapted from macrdp's `src/rdpdr/surface.rs` (<https://github.com/clintcan/macrdp>), Copyright (c)
//! 2026 Clint Christopher Canada, MIT OR Apache-2.0.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ironrdp_rdpdr::pdu::efs::{FileAttributes, NtStatus};
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfs_fh3, nfspath3, nfsstat3, nfstime3, sattr3, set_atime, set_mtime,
    set_size3, specdata3,
};
use nfsserve::vfs::{DirEntry as NfsDirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tracing::{debug, warn};

use super::rdpdr::{CreateMode, DirEntry, FileInfo, RdpdrError, RdpdrHandle, RdpdrResult};

/// The root directory's file ID.
const ROOT_ID: fileid3 = 1;

/// How long attributes from the client are used before they are asked for again. macOS keeps them
/// for `actimeo` seconds itself; this only spares the client the bursts of calls for one file that
/// opening it or listing its folder cause.
pub(crate) const ATTR_TTL: Duration = Duration::from_secs(1);

/// The most of macOS's own files kept for one drive. A `._` file is usually 4 KiB and a `.DS_Store`
/// a few, so this holds thousands.
const LOCAL_LIMIT: usize = 64 * 1024 * 1024;

/// Names macOS uses for its own purposes on a volume, which a Windows drive has no use for. Names
/// starting with `._` (AppleDouble files holding extended attributes) belong here too.
/// `.TemporaryItems` does not: programs save documents on a volume by writing into it and renaming
/// the result into place, which only works on the drive itself.
const MAC_NAMES: &[&str] = &[
    ".DS_Store",
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    ".VolumeIcon.icns",
    ".localized",
    ".metadata_never_index",
    ".metadata_never_index_unless_rootfs",
    ".metadata_direct_scope_only",
    "Icon\r",
];

pub(crate) fn is_mac_name(name: &str) -> bool {
    name.starts_with("._") || MAC_NAMES.contains(&name)
}

/// What [`DriveFs`] needs from the client's drive. Paths are Windows paths relative to the drive's
/// root, `\` being the root.
#[async_trait]
pub(crate) trait Remote: Send + Sync + 'static {
    async fn stat(&self, path: &str) -> RdpdrResult<FileInfo>;
    async fn list_dir(&self, path: &str) -> RdpdrResult<Vec<DirEntry>>;
    async fn read(&self, path: &str, offset: u64, length: u32) -> RdpdrResult<Vec<u8>>;
    async fn write(&self, path: &str, offset: u64, data: &[u8]) -> RdpdrResult<u32>;
    async fn create_file(&self, path: &str, mode: CreateMode) -> RdpdrResult<()>;
    async fn create_dir(&self, path: &str) -> RdpdrResult<()>;
    async fn set_len(&self, path: &str, size: u64) -> RdpdrResult<()>;
    async fn set_times(&self, path: &str, last_access_time: Option<i64>, last_write_time: Option<i64>)
        -> RdpdrResult<()>;
    async fn remove(&self, path: &str, is_dir: bool) -> RdpdrResult<()>;
    async fn rename(&self, from: &str, to: &str) -> RdpdrResult<()>;
}

/// One drive of the client, reached through the connection's RDPDR channel.
#[derive(Clone)]
pub(crate) struct ClientDrive {
    pub(crate) handle: RdpdrHandle,
    pub(crate) device_id: u32,
}

#[async_trait]
impl Remote for ClientDrive {
    async fn stat(&self, path: &str) -> RdpdrResult<FileInfo> {
        self.handle.stat(self.device_id, path).await
    }

    async fn list_dir(&self, path: &str) -> RdpdrResult<Vec<DirEntry>> {
        self.handle.list_dir(self.device_id, path).await
    }

    async fn read(&self, path: &str, offset: u64, length: u32) -> RdpdrResult<Vec<u8>> {
        self.handle.read_file(self.device_id, path, offset, length).await
    }

    async fn write(&self, path: &str, offset: u64, data: &[u8]) -> RdpdrResult<u32> {
        self.handle.write_file(self.device_id, path, offset, data).await
    }

    async fn create_file(&self, path: &str, mode: CreateMode) -> RdpdrResult<()> {
        self.handle.create_file(self.device_id, path, mode).await
    }

    async fn create_dir(&self, path: &str) -> RdpdrResult<()> {
        self.handle.create_dir(self.device_id, path).await
    }

    async fn set_len(&self, path: &str, size: u64) -> RdpdrResult<()> {
        self.handle.set_len(self.device_id, path, size).await
    }

    async fn set_times(
        &self,
        path: &str,
        last_access_time: Option<i64>,
        last_write_time: Option<i64>,
    ) -> RdpdrResult<()> {
        self.handle
            .set_times(self.device_id, path, last_access_time, last_write_time)
            .await
    }

    async fn remove(&self, path: &str, is_dir: bool) -> RdpdrResult<()> {
        self.handle.remove(self.device_id, path, is_dir).await
    }

    async fn rename(&self, from: &str, to: &str) -> RdpdrResult<()> {
        // NFS renames replace what is at the destination.
        self.handle.rename(self.device_id, from, to, true).await
    }
}

/// The NFS error closest to why a request to the client failed.
pub(crate) fn nfs_error(error: &RdpdrError) -> nfsstat3 {
    let RdpdrError::Status { status, .. } = error else {
        // A timeout, a closed channel or an undecodable answer: nothing more precise to say.
        return nfsstat3::NFS3ERR_IO;
    };
    match *status {
        NtStatus::NO_SUCH_FILE | NtStatus::OBJECT_NAME_NOT_FOUND | NtStatus::OBJECT_PATH_NOT_FOUND => {
            nfsstat3::NFS3ERR_NOENT
        }
        NtStatus::ACCESS_DENIED | NtStatus::CANNOT_DELETE | NtStatus::SHARING_VIOLATION | NtStatus::DELETE_PENDING => {
            nfsstat3::NFS3ERR_ACCES
        }
        NtStatus::OBJECT_NAME_COLLISION => nfsstat3::NFS3ERR_EXIST,
        NtStatus::NOT_A_DIRECTORY => nfsstat3::NFS3ERR_NOTDIR,
        NtStatus::FILE_IS_A_DIRECTORY => nfsstat3::NFS3ERR_ISDIR,
        NtStatus::DIRECTORY_NOT_EMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        NtStatus::DISK_FULL => nfsstat3::NFS3ERR_NOSPC,
        NtStatus::MEDIA_WRITE_PROTECTED => nfsstat3::NFS3ERR_ROFS,
        NtStatus::NAME_TOO_LONG => nfsstat3::NFS3ERR_NAMETOOLONG,
        NtStatus::OBJECT_NAME_INVALID | NtStatus::INVALID_PARAMETER => nfsstat3::NFS3ERR_INVAL,
        NtStatus::NOT_SUPPORTED | NtStatus::NOT_IMPLEMENTED => nfsstat3::NFS3ERR_NOTSUPP,
        // The client no longer shares the drive.
        NtStatus::NO_SUCH_DEVICE => nfsstat3::NFS3ERR_STALE,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

/// Seconds between 1601-01-01 (FILETIME's epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

/// A FILETIME as NFS time. Times before 1970, or unknown (0), become 1970-01-01; NFSv3 has no room
/// for times after 2106.
pub(crate) fn nfs_time(filetime: i64) -> nfstime3 {
    let seconds = filetime.div_euclid(10_000_000) - FILETIME_UNIX_OFFSET;
    let nanos = filetime.rem_euclid(10_000_000) * 100;
    match u32::try_from(seconds) {
        Ok(seconds) => nfstime3 {
            seconds,
            nseconds: u32::try_from(nanos).unwrap_or(0),
        },
        Err(_) if seconds < 0 => nfstime3 { seconds: 0, nseconds: 0 },
        Err(_) => nfstime3 {
            seconds: u32::MAX,
            nseconds: 0,
        },
    }
}

/// An NFS time as a FILETIME.
pub(crate) fn filetime(time: nfstime3) -> i64 {
    (i64::from(time.seconds) + FILETIME_UNIX_OFFSET) * 10_000_000 + i64::from(time.nseconds / 100)
}

fn filetime_now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = i64::try_from(since.as_secs()).unwrap_or(0);
    (seconds + FILETIME_UNIX_OFFSET) * 10_000_000 + i64::from(since.subsec_nanos() / 100)
}

/// The attributes of a file or folder made just now.
fn new_info(dir: bool) -> FileInfo {
    let now = filetime_now();
    FileInfo {
        size: 0,
        attributes: if dir {
            FileAttributes::FILE_ATTRIBUTE_DIRECTORY
        } else {
            FileAttributes::FILE_ATTRIBUTE_ARCHIVE
        },
        creation_time: now,
        last_access_time: now,
        last_write_time: now,
        change_time: now,
    }
}

/// Joins `name` onto the Windows path `dir`.
fn join(dir: &str, name: &str) -> String {
    if dir == "\\" {
        format!("\\{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// A name macOS asks for, if it can exist on a Windows drive: backslashes separate its path and it
/// has no other names for "." and "..".
fn usable_name(name: &filename3) -> Option<&str> {
    let name = std::str::from_utf8(name).ok()?;
    (!name.is_empty() && !name.contains('\\') && name != "." && name != "..").then_some(name)
}

struct Node {
    /// The Windows path, relative to the drive's root.
    path: String,
    parent: fileid3,
    info: Option<FileInfo>,
    fetched: Option<Instant>,
    /// The contents of one of macOS's own files, kept here instead of on the client.
    local: Option<Vec<u8>>,
}

impl Node {
    fn fresh_info(&self, ttl: Duration) -> Option<&FileInfo> {
        match (&self.info, self.fetched) {
            _ if self.local.is_some() => self.info.as_ref(),
            (Some(info), Some(fetched)) if fetched.elapsed() < ttl => Some(info),
            _ => None,
        }
    }
}

/// A directory's entries, as one enumeration saw them. Every page of that enumeration comes from it,
/// so the pages stay consistent however the client's contents change meanwhile.
struct Listing {
    ids: Vec<fileid3>,
    fetched: Instant,
}

struct State {
    next_id: fileid3,
    nodes: HashMap<fileid3, Node>,
    ids: HashMap<String, fileid3>,
    listings: HashMap<fileid3, Listing>,
    /// The bytes of macOS's own files kept here.
    local_bytes: usize,
}

impl State {
    fn new() -> Self {
        let root = Node {
            path: "\\".to_owned(),
            parent: ROOT_ID,
            info: None,
            fetched: None,
            local: None,
        };
        Self {
            next_id: ROOT_ID + 1,
            nodes: HashMap::from([(ROOT_ID, root)]),
            ids: HashMap::from([("\\".to_owned(), ROOT_ID)]),
            listings: HashMap::new(),
            local_bytes: 0,
        }
    }

    /// The file ID of `path`, a child of `parent`, with the attributes just learnt.
    fn intern(&mut self, parent: fileid3, path: String, info: FileInfo, now: Instant) -> fileid3 {
        if let Some(&id) = self.ids.get(&path) {
            if let Some(node) = self.nodes.get_mut(&id) {
                node.info = Some(info);
                node.fetched = Some(now);
            }
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.ids.insert(path.clone(), id);
        self.nodes.insert(
            id,
            Node {
                path,
                parent,
                info: Some(info),
                fetched: Some(now),
                local: None,
            },
        );
        id
    }

    /// `id` and everything below it.
    fn tree(&self, id: fileid3) -> Vec<fileid3> {
        let Some(node) = self.nodes.get(&id) else {
            return Vec::new();
        };
        let prefix = format!("{}\\", node.path);
        let mut ids: Vec<fileid3> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.path.starts_with(&prefix))
            .map(|(&i, _)| i)
            .collect();
        ids.push(id);
        ids
    }

    /// Forgets a file, or a folder with everything in it, that no longer exists.
    fn forget(&mut self, id: fileid3) {
        if id == ROOT_ID {
            return;
        }
        for i in self.tree(id) {
            if let Some(node) = self.nodes.remove(&i) {
                self.ids.remove(&node.path);
                if let Some(data) = node.local {
                    self.local_bytes = self.local_bytes.saturating_sub(data.len());
                }
            }
            self.listings.remove(&i);
        }
    }

    /// Moves a file, or a folder with everything in it, to `to` in `new_parent`, keeping file IDs.
    fn rename(&mut self, id: fileid3, to: &str, new_parent: fileid3) {
        let Some(from) = self.nodes.get(&id).map(|n| n.path.clone()) else {
            return;
        };
        for i in self.tree(id) {
            let Some(node) = self.nodes.get_mut(&i) else {
                continue;
            };
            let new_path = format!("{to}{}", &node.path[from.len()..]);
            self.ids.remove(&node.path);
            self.ids.insert(new_path.clone(), i);
            node.path = new_path;
            if i == id {
                node.parent = new_parent;
            }
        }
    }

    /// Marks a folder's listing and attributes as out of date, after its contents changed.
    fn changed(&mut self, dirid: fileid3) {
        self.listings.remove(&dirid);
        if let Some(node) = self.nodes.get_mut(&dirid) {
            node.fetched = None;
        }
    }
}

/// The NFS file system of one redirected drive.
pub(crate) struct DriveFs<R: Remote> {
    remote: R,
    uid: u32,
    gid: u32,
    fsid: u64,
    /// Put into every file handle, and required back.
    secret: [u8; 16],
    /// Set by the one MOUNT the server accepts.
    mounted: AtomicBool,
    attr_ttl: Duration,
    local_limit: usize,
    state: Mutex<State>,
}

impl<R: Remote> DriveFs<R> {
    pub(crate) fn new(remote: R, uid: u32, gid: u32, fsid: u64) -> Self {
        Self {
            remote,
            uid,
            gid,
            fsid,
            secret: random_secret(),
            mounted: AtomicBool::new(false),
            attr_ttl: ATTR_TTL,
            local_limit: LOCAL_LIMIT,
            state: Mutex::new(State::new()),
        }
    }

    #[cfg(test)]
    fn with_attr_ttl(mut self, ttl: Duration) -> Self {
        self.attr_ttl = ttl;
        self
    }

    #[cfg(test)]
    fn with_local_limit(mut self, limit: usize) -> Self {
        self.local_limit = limit;
        self
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while the lock was held cannot leave the maps inconsistent.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn attr(&self, id: fileid3, info: &FileInfo) -> fattr3 {
        let dir = info.is_dir();
        let change_time = if info.change_time != 0 {
            info.change_time
        } else {
            info.last_write_time
        };
        fattr3 {
            ftype: if dir { ftype3::NF3DIR } else { ftype3::NF3REG },
            mode: if dir { 0o700 } else { 0o600 },
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            size: if dir { 0 } else { info.size },
            used: if dir { 0 } else { info.size },
            rdev: specdata3::default(),
            fsid: self.fsid,
            fileid: id,
            atime: nfs_time(info.last_access_time),
            mtime: nfs_time(info.last_write_time),
            ctime: nfs_time(change_time),
        }
    }

    /// The path and parent of a known file ID.
    fn node(&self, id: fileid3) -> Result<(String, fileid3), nfsstat3> {
        let state = self.state();
        let node = state.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        Ok((node.path.clone(), node.parent))
    }

    /// The path of `name` in the folder `dirid`.
    fn child_path(&self, dirid: fileid3, name: &str) -> Result<String, nfsstat3> {
        let state = self.state();
        let dir = state.nodes.get(&dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if dir.info.as_ref().is_some_and(|info| !info.is_dir()) {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        Ok(join(&dir.path, name))
    }

    /// Attributes of `id`, from the client when what is known is older than the TTL.
    async fn info(&self, id: fileid3) -> Result<FileInfo, nfsstat3> {
        let path = {
            let state = self.state();
            let node = state.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            if let Some(info) = node.fresh_info(self.attr_ttl) {
                return Ok(info.clone());
            }
            node.path.clone()
        };
        match self.remote.stat(&path).await {
            Ok(info) => {
                let mut state = self.state();
                if let Some(node) = state.nodes.get_mut(&id) {
                    node.info = Some(info.clone());
                    node.fetched = Some(Instant::now());
                }
                Ok(info)
            }
            Err(error) if id == ROOT_ID => {
                // A client that cannot open its drive's root still lists it; show a plain folder.
                debug!(%error, "drive root has no attributes; using a plain folder");
                Ok(FileInfo {
                    size: 0,
                    attributes: FileAttributes::FILE_ATTRIBUTE_DIRECTORY,
                    creation_time: 0,
                    last_access_time: 0,
                    last_write_time: 0,
                    change_time: 0,
                })
            }
            Err(error) => {
                let status = nfs_error(&error);
                if matches!(status, nfsstat3::NFS3ERR_NOENT) {
                    // Deleted on the client: the handle macOS holds no longer names anything.
                    self.state().forget(id);
                    return Err(nfsstat3::NFS3ERR_STALE);
                }
                debug!(%error, path, "stat failed");
                Err(status)
            }
        }
    }

    /// Lists `dirid` from the client and keeps the listing for the pages of this enumeration.
    async fn relist(&self, dirid: fileid3) -> Result<(), nfsstat3> {
        let (path, _) = self.node(dirid)?;
        let entries = self.remote.list_dir(&path).await.map_err(|error| {
            debug!(%error, path, "listing failed");
            nfs_error(&error)
        })?;
        let now = Instant::now();
        let mut state = self.state();
        let ids = entries
            .into_iter()
            .filter(|entry| !is_mac_name(&entry.name) && !entry.name.contains('\\'))
            .map(|entry| state.intern(dirid, join(&path, &entry.name), entry.info, now))
            .collect();
        state.listings.insert(dirid, Listing { ids, fetched: now });
        Ok(())
    }

    /// Makes one of macOS's own files, kept here: opens it if it exists, unless `exclusive`, and
    /// empties it if `truncate`.
    fn create_local(&self, dirid: fileid3, path: String, exclusive: bool, truncate: bool) -> Result<fileid3, nfsstat3> {
        let mut state = self.state();
        if let Some(&id) = state.ids.get(&path) {
            if exclusive {
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
            if truncate {
                let freed = state
                    .nodes
                    .get_mut(&id)
                    .and_then(|node| {
                        if let Some(info) = node.info.as_mut() {
                            info.size = 0;
                        }
                        node.local.as_mut().map(|data| core::mem::take(data).len())
                    })
                    .unwrap_or(0);
                state.local_bytes = state.local_bytes.saturating_sub(freed);
            }
            return Ok(id);
        }
        let id = state.intern(dirid, path, new_info(false), Instant::now());
        if let Some(node) = state.nodes.get_mut(&id) {
            node.local = Some(Vec::new());
        }
        Ok(id)
    }

    /// Whether `id` is a folder, from what is known or else from the client.
    async fn is_dir(&self, id: fileid3) -> Result<bool, nfsstat3> {
        let known = {
            let state = self.state();
            state.nodes.get(&id).and_then(|n| n.info.as_ref()).map(FileInfo::is_dir)
        };
        match known {
            Some(dir) => Ok(dir),
            None => self.info(id).await.map(|info| info.is_dir()),
        }
    }

    /// The file ID of `name` in `dirid`, looking it up if it is not known yet.
    async fn resolve(&self, dirid: fileid3, name: &filename3) -> Result<fileid3, nfsstat3> {
        let known = {
            let state = self.state();
            let dir = state.nodes.get(&dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
            usable_name(name).and_then(|n| state.ids.get(&join(&dir.path, n)).copied())
        };
        match known {
            Some(id) => Ok(id),
            None => self.lookup(dirid, name).await,
        }
    }
}

fn random_secret() -> [u8; 16] {
    let mut secret = [0u8; 16];
    // SAFETY: `secret` is a writable buffer of exactly the length passed.
    unsafe { libc::arc4random_buf(secret.as_mut_ptr().cast(), secret.len()) };
    secret
}

#[async_trait]
impl<R: Remote> NFSFileSystem for DriveFs<R> {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    /// The MOUNT request resolves the export's path here. Only the first one succeeds: that is the
    /// mount rdpmacd makes itself. Any later one comes from someone else.
    async fn path_to_id(&self, path: &[u8]) -> Result<fileid3, nfsstat3> {
        if path != b"/" {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        if self.mounted.swap(true, Ordering::SeqCst) {
            warn!("refused a second MOUNT of a redirected drive");
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        Ok(ROOT_ID)
    }

    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&self.secret);
        data.extend_from_slice(&id.to_le_bytes());
        nfs_fh3 { data }
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        let Some((secret, id)) = fh.data.split_first_chunk::<16>() else {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        };
        let id: [u8; 8] = id.try_into().map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?;
        // Compare without an early exit, so that the time taken says nothing about the secret.
        let differs = secret.iter().zip(&self.secret).fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if differs != 0 {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        let id = u64::from_le_bytes(id);
        if self.state().nodes.contains_key(&id) {
            Ok(id)
        } else {
            Err(nfsstat3::NFS3ERR_STALE)
        }
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        match &filename[..] {
            b"." => return Ok(dirid),
            b".." => return self.node(dirid).map(|(_, parent)| parent),
            _ => {}
        }
        let Some(name) = usable_name(filename) else {
            return Err(nfsstat3::NFS3ERR_NOENT);
        };
        let path = self.child_path(dirid, name)?;
        {
            let state = self.state();
            if is_mac_name(name) {
                // Only what macOS made here itself.
                return state
                    .ids
                    .get(&path)
                    .copied()
                    .filter(|id| state.nodes.get(id).is_some_and(|n| n.local.is_some()))
                    .ok_or(nfsstat3::NFS3ERR_NOENT);
            }
            // A listing that fresh already says whether the name exists.
            let listed = state.listings.get(&dirid);
            if listed.is_some_and(|listing| listing.fetched.elapsed() < self.attr_ttl) {
                return state.ids.get(&path).copied().ok_or(nfsstat3::NFS3ERR_NOENT);
            }
            let known = state.ids.get(&path).copied();
            if let Some(id) = known.filter(|id| state.nodes.get(id).and_then(|n| n.fresh_info(self.attr_ttl)).is_some()) {
                return Ok(id);
            }
        }
        match self.remote.stat(&path).await {
            Ok(info) => Ok(self.state().intern(dirid, path, info, Instant::now())),
            Err(error) => {
                let status = nfs_error(&error);
                if matches!(status, nfsstat3::NFS3ERR_NOENT) {
                    let mut state = self.state();
                    if let Some(id) = state.ids.get(&path).copied() {
                        state.forget(id);
                    }
                } else {
                    debug!(%error, path, "lookup failed");
                }
                Err(status)
            }
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let info = self.info(id).await?;
        Ok(self.attr(id, &info))
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        if id == ROOT_ID {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        let (path, known_size) = {
            let state = self.state();
            let node = state.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            if let Some(data) = &node.local {
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(data.len());
                let end = start.saturating_add(usize::try_from(count).unwrap_or(usize::MAX)).min(data.len());
                return Ok((data[start..end].to_vec(), end == data.len()));
            }
            let fresh = node.fresh_info(self.attr_ttl).map(|info| info.size);
            (node.path.clone(), (fresh, node.info.as_ref().map(|info| info.size)))
        };
        // macOS reads a whole block, in several requests, even of a file shorter than the block: the
        // parts past the end are answered here.
        if known_size.0.is_some_and(|size| offset >= size) {
            return Ok((Vec::new(), true));
        }
        let data = match self.remote.read(&path, offset, count).await {
            Ok(data) => data,
            // Reading at or past the end: Windows says STATUS_END_OF_FILE, FreeRDP STATUS_UNSUCCESSFUL.
            Err(RdpdrError::Status {
                status: NtStatus::END_OF_FILE,
                ..
            }) => Vec::new(),
            Err(RdpdrError::Status { .. }) if known_size.1.is_some_and(|size| offset >= size) => Vec::new(),
            Err(error) => {
                debug!(%error, path, offset, count, "read failed");
                return Err(nfs_error(&error));
            }
        };
        // A short read is the end of the file, and says exactly where it is.
        let end = u32::try_from(data.len()).is_ok_and(|len| len < count);
        if !data.is_empty() {
            if let Some(info) = self.state().nodes.get_mut(&id).and_then(|node| node.info.as_mut()) {
                let reached = offset.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                if end || reached > info.size {
                    info.size = reached;
                }
            }
        }
        Ok((data, end))
    }

    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<ReadDirResult, nfsstat3> {
        let continuing = start_after != 0 && self.state().listings.contains_key(&dirid);
        if !continuing {
            self.relist(dirid).await?;
        }
        let state = self.state();
        let listing = state.listings.get(&dirid).ok_or(nfsstat3::NFS3ERR_IO)?;
        let start = if start_after == 0 {
            0
        } else {
            // The cookie is the file ID of the last entry sent; one this listing does not hold
            // belongs to an enumeration that has been replaced.
            listing
                .ids
                .iter()
                .position(|&id| id == start_after)
                .map(|p| p + 1)
                .ok_or(nfsstat3::NFS3ERR_BAD_COOKIE)?
        };
        let mut entries = Vec::new();
        for &id in listing.ids.iter().skip(start).take(max_entries) {
            let Some(node) = state.nodes.get(&id) else { continue };
            let Some(info) = &node.info else { continue };
            let name = node.path.rsplit('\\').next().unwrap_or_default();
            entries.push(NfsDirEntry {
                fileid: id,
                name: name.as_bytes().into(),
                attr: self.attr(id, info),
            });
        }
        let end = start + entries.len() >= listing.ids.len();
        Ok(ReadDirResult { entries, end })
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let size = match setattr.size {
            set_size3::size(size) => Some(size),
            set_size3::Void => None,
        };
        let access = match setattr.atime {
            set_atime::DONT_CHANGE => None,
            set_atime::SET_TO_SERVER_TIME => Some(filetime_now()),
            set_atime::SET_TO_CLIENT_TIME(time) => Some(filetime(time)),
        };
        let write = match setattr.mtime {
            set_mtime::DONT_CHANGE => None,
            set_mtime::SET_TO_SERVER_TIME => Some(filetime_now()),
            set_mtime::SET_TO_CLIENT_TIME(time) => Some(filetime(time)),
        };
        // Modes and owners have no counterpart on a Windows drive: they are accepted and dropped.
        let (path, local) = {
            let state = self.state();
            let node = state.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            (node.path.clone(), node.local.is_some())
        };
        if local {
            let mut state = self.state();
            let total = state.local_bytes;
            let node = state.nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            let mut new_total = total;
            if let (Some(size), Some(data)) = (size, node.local.as_mut()) {
                let size = usize::try_from(size).map_err(|_| nfsstat3::NFS3ERR_FBIG)?;
                new_total = total.saturating_sub(data.len()) + size;
                if size > data.len() && new_total > self.local_limit {
                    return Err(nfsstat3::NFS3ERR_NOSPC);
                }
                data.resize(size, 0);
            }
            let info = node.info.get_or_insert_with(|| new_info(false));
            if let Some(size) = size {
                info.size = size;
            }
            if let Some(time) = access {
                info.last_access_time = time;
            }
            if let Some(time) = write {
                info.last_write_time = time;
            }
            let info = info.clone();
            state.local_bytes = new_total;
            return Ok(self.attr(id, &info));
        }

        if let Some(size) = size {
            if !self.is_dir(id).await? {
                self.remote.set_len(&path, size).await.map_err(|error| {
                    debug!(%error, path, size, "truncate failed");
                    nfs_error(&error)
                })?;
                if let Some(info) = self.state().nodes.get_mut(&id).and_then(|n| n.info.as_mut()) {
                    info.size = size;
                }
            }
        }
        if access.is_some() || write.is_some() {
            match self.remote.set_times(&path, access, write).await {
                Ok(()) => {
                    if let Some(info) = self.state().nodes.get_mut(&id).and_then(|n| n.info.as_mut()) {
                        if let Some(time) = access {
                            info.last_access_time = time;
                        }
                        if let Some(time) = write {
                            info.last_write_time = time;
                        }
                    }
                }
                // FreeRDP cannot set a folder's times. They matter little, and failing the call would
                // tell macOS the folder is gone.
                Err(error) if self.is_dir(id).await.unwrap_or(false) => {
                    debug!(%error, path, "the client did not set a folder's times");
                }
                Err(error) => {
                    debug!(%error, path, "setting times failed");
                    return Err(nfs_error(&error));
                }
            }
        }
        let info = self.info(id).await?;
        Ok(self.attr(id, &info))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let len = u64::try_from(data.len()).map_err(|_| nfsstat3::NFS3ERR_FBIG)?;
        let end = offset.checked_add(len).ok_or(nfsstat3::NFS3ERR_FBIG)?;
        let (path, local) = {
            let state = self.state();
            let node = state.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            if node.info.as_ref().is_some_and(FileInfo::is_dir) {
                return Err(nfsstat3::NFS3ERR_ISDIR);
            }
            (node.path.clone(), node.local.is_some())
        };
        if local {
            let mut state = self.state();
            let total = state.local_bytes;
            let node = state.nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            let Some(contents) = node.local.as_mut() else {
                return Err(nfsstat3::NFS3ERR_IO);
            };
            let start = usize::try_from(offset).map_err(|_| nfsstat3::NFS3ERR_FBIG)?;
            let end = usize::try_from(end).map_err(|_| nfsstat3::NFS3ERR_FBIG)?;
            let growth = end.saturating_sub(contents.len());
            if total + growth > self.local_limit {
                warn!(limit = self.local_limit, "no room left for macOS's own files of a drive");
                return Err(nfsstat3::NFS3ERR_NOSPC);
            }
            if contents.len() < end {
                contents.resize(end, 0);
            }
            contents[start..end].copy_from_slice(data);
            let size = u64::try_from(contents.len()).unwrap_or(u64::MAX);
            let info = node.info.get_or_insert_with(|| new_info(false));
            info.size = size;
            info.last_write_time = filetime_now();
            let info = info.clone();
            state.local_bytes = total + growth;
            return Ok(self.attr(id, &info));
        }

        let written = self.remote.write(&path, offset, data).await.map_err(|error| {
            debug!(%error, path, offset, "write failed");
            nfs_error(&error)
        })?;
        if u64::from(written) != len {
            warn!(path, written, len, "the client wrote less than it was sent");
            return Err(nfsstat3::NFS3ERR_IO);
        }
        let info = {
            let mut state = self.state();
            let node = state.nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            let info = node.info.get_or_insert_with(|| new_info(false));
            info.size = info.size.max(end);
            info.last_write_time = filetime_now();
            info.clone()
        };
        Ok(self.attr(id, &info))
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = usable_name(filename).ok_or(nfsstat3::NFS3ERR_INVAL)?;
        let path = self.child_path(dirid, name)?;
        let truncate = matches!(attr.size, set_size3::size(0));
        if is_mac_name(name) {
            let id = self.create_local(dirid, path, false, truncate)?;
            let info = self.info(id).await?;
            return Ok((id, self.attr(id, &info)));
        }
        let mode = if truncate { CreateMode::Truncate } else { CreateMode::Open };
        self.remote.create_file(&path, mode).await.map_err(|error| {
            debug!(%error, path, "create failed");
            nfs_error(&error)
        })?;
        let id = {
            let mut state = self.state();
            state.changed(dirid);
            let id = state.intern(dirid, path, new_info(false), Instant::now());
            if !truncate {
                // A file that existed keeps its contents: its size is asked for below.
                if let Some(node) = state.nodes.get_mut(&id) {
                    node.fetched = None;
                }
            }
            id
        };
        let info = self.info(id).await?;
        Ok((id, self.attr(id, &info)))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = usable_name(filename).ok_or(nfsstat3::NFS3ERR_INVAL)?;
        let path = self.child_path(dirid, name)?;
        if is_mac_name(name) {
            return self.create_local(dirid, path, true, true);
        }
        self.remote.create_file(&path, CreateMode::Exclusive).await.map_err(|error| {
            debug!(%error, path, "exclusive create failed");
            nfs_error(&error)
        })?;
        let mut state = self.state();
        state.changed(dirid);
        Ok(state.intern(dirid, path, new_info(false), Instant::now()))
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = usable_name(dirname).ok_or(nfsstat3::NFS3ERR_INVAL)?;
        if is_mac_name(name) {
            // .Trashes, .fseventsd and the like: macOS copes with a volume that has none.
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let path = self.child_path(dirid, name)?;
        self.remote.create_dir(&path).await.map_err(|error| {
            debug!(%error, path, "mkdir failed");
            nfs_error(&error)
        })?;
        let info = new_info(true);
        let id = {
            let mut state = self.state();
            state.changed(dirid);
            state.intern(dirid, path, info.clone(), Instant::now())
        };
        Ok((id, self.attr(id, &info)))
    }

    /// REMOVE and RMDIR both end here.
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = usable_name(filename).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let id = self.resolve(dirid, filename).await?;
        if is_mac_name(name) {
            self.state().forget(id);
            return Ok(());
        }
        let (path, _) = self.node(id)?;
        let is_dir = self.is_dir(id).await?;
        self.remote.remove(&path, is_dir).await.map_err(|error| {
            debug!(%error, path, "remove failed");
            nfs_error(&error)
        })?;
        let mut state = self.state();
        // A folder takes macOS's own files in it along.
        state.forget(id);
        state.changed(dirid);
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_name = usable_name(from_filename).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let to_name = usable_name(to_filename).ok_or(nfsstat3::NFS3ERR_INVAL)?;
        let to_path = self.child_path(to_dirid, to_name)?;
        let id = self.resolve(from_dirid, from_filename).await?;
        match (is_mac_name(from_name), is_mac_name(to_name)) {
            (true, true) => {
                let mut state = self.state();
                if let Some(replaced) = state.ids.get(&to_path).copied().filter(|&r| r != id) {
                    state.forget(replaced);
                }
                state.rename(id, &to_path, to_dirid);
                return Ok(());
            }
            // One of macOS's own files kept here cannot become a file on the drive, nor the reverse.
            (true, false) | (false, true) => return Err(nfsstat3::NFS3ERR_INVAL),
            (false, false) => {}
        }
        let (from_path, _) = self.node(id)?;
        self.remote.rename(&from_path, &to_path).await.map_err(|error| {
            debug!(%error, from_path, to_path, "rename failed");
            nfs_error(&error)
        })?;
        let mut state = self.state();
        if let Some(replaced) = state.ids.get(&to_path).copied().filter(|&r| r != id) {
            state.forget(replaced);
        }
        state.rename(id, &to_path, to_dirid);
        state.changed(from_dirid);
        state.changed(to_dirid);
        Ok(())
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use nfsserve::nfs::{set_gid3, set_mode3, set_uid3};

    use super::*;

    /// A drive in memory that counts the requests made of it. Where clients differ it answers as
    /// FreeRDP does: a read that starts past the end fails.
    #[derive(Default)]
    pub(crate) struct FakeDrive {
        pub(crate) files: Mutex<HashMap<String, (FileInfo, Vec<u8>)>>,
        pub(crate) stats: AtomicUsize,
        pub(crate) listings: AtomicUsize,
        pub(crate) reads: AtomicUsize,
        pub(crate) writes: AtomicUsize,
    }

    pub(crate) fn info(dir: bool, size: u64, write_time: i64) -> FileInfo {
        FileInfo {
            size,
            attributes: if dir {
                FileAttributes::FILE_ATTRIBUTE_DIRECTORY
            } else {
                FileAttributes::FILE_ATTRIBUTE_ARCHIVE
            },
            creation_time: write_time,
            last_access_time: write_time,
            last_write_time: write_time,
            change_time: write_time,
        }
    }

    fn status(status: NtStatus) -> RdpdrError {
        RdpdrError::Status { op: "fake", status }
    }

    fn parent(path: &str) -> &str {
        match path.rsplit_once('\\') {
            Some(("", _)) | None => "\\",
            Some((dir, _)) => dir,
        }
    }

    impl FakeDrive {
        pub(crate) fn with(files: &[(&str, Option<&[u8]>)]) -> Arc<Self> {
            let drive = Self::default();
            {
                let mut map = drive.files.lock().unwrap();
                map.insert("\\".to_owned(), (info(true, 0, 0), Vec::new()));
                for (path, content) in files {
                    let entry = match content {
                        Some(data) => (
                            info(false, u64::try_from(data.len()).unwrap(), 133_000_000_000_000_000),
                            data.to_vec(),
                        ),
                        None => (info(true, 0, 133_000_000_000_000_000), Vec::new()),
                    };
                    map.insert((*path).to_owned(), entry);
                }
            }
            Arc::new(drive)
        }

        pub(crate) fn contents(&self, path: &str) -> Option<Vec<u8>> {
            self.files.lock().unwrap().get(path).map(|(_, data)| data.clone())
        }

        pub(crate) fn paths(&self) -> Vec<String> {
            let mut paths: Vec<String> = self.files.lock().unwrap().keys().cloned().collect();
            paths.sort();
            paths
        }
    }

    #[async_trait]
    impl Remote for Arc<FakeDrive> {
        async fn stat(&self, path: &str) -> RdpdrResult<FileInfo> {
            self.stats.fetch_add(1, Ordering::SeqCst);
            let files = self.files.lock().unwrap();
            files
                .get(path)
                .map(|(i, _)| i.clone())
                .ok_or_else(|| status(NtStatus::OBJECT_NAME_NOT_FOUND))
        }

        async fn list_dir(&self, path: &str) -> RdpdrResult<Vec<DirEntry>> {
            self.listings.fetch_add(1, Ordering::SeqCst);
            let files = self.files.lock().unwrap();
            let prefix = if path == "\\" { "\\".to_owned() } else { format!("{path}\\") };
            let mut entries: Vec<DirEntry> = files
                .iter()
                .filter_map(|(p, (i, _))| {
                    let rest = p.strip_prefix(&prefix)?;
                    (!rest.is_empty() && !rest.contains('\\')).then(|| DirEntry {
                        name: rest.to_owned(),
                        info: i.clone(),
                    })
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        }

        async fn read(&self, path: &str, offset: u64, length: u32) -> RdpdrResult<Vec<u8>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let files = self.files.lock().unwrap();
            let (_, data) = files.get(path).ok_or_else(|| status(NtStatus::OBJECT_NAME_NOT_FOUND))?;
            if usize::try_from(offset).unwrap() > data.len() {
                // FreeRDP's answer to a read past the end.
                return Err(status(NtStatus::UNSUCCESSFUL));
            }
            let start = usize::try_from(offset).unwrap();
            let end = (start + usize::try_from(length).unwrap()).min(data.len());
            Ok(data[start..end].to_vec())
        }

        async fn write(&self, path: &str, offset: u64, data: &[u8]) -> RdpdrResult<u32> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            let mut files = self.files.lock().unwrap();
            let (info, contents) = files.get_mut(path).ok_or_else(|| status(NtStatus::OBJECT_NAME_NOT_FOUND))?;
            if info.is_dir() {
                return Err(status(NtStatus::FILE_IS_A_DIRECTORY));
            }
            let start = usize::try_from(offset).unwrap();
            let end = start + data.len();
            if contents.len() < end {
                contents.resize(end, 0);
            }
            contents[start..end].copy_from_slice(data);
            info.size = u64::try_from(contents.len()).unwrap();
            Ok(u32::try_from(data.len()).unwrap())
        }

        async fn create_file(&self, path: &str, mode: CreateMode) -> RdpdrResult<()> {
            let mut files = self.files.lock().unwrap();
            if !files.contains_key(parent(path)) {
                return Err(status(NtStatus::OBJECT_PATH_NOT_FOUND));
            }
            match (files.get_mut(path), mode) {
                (Some(_), CreateMode::Exclusive) => Err(status(NtStatus::OBJECT_NAME_COLLISION)),
                (Some((info, data)), CreateMode::Truncate) => {
                    data.clear();
                    info.size = 0;
                    Ok(())
                }
                (Some(_), CreateMode::Open) => Ok(()),
                (None, _) => {
                    files.insert(path.to_owned(), (info(false, 0, 1), Vec::new()));
                    Ok(())
                }
            }
        }

        async fn create_dir(&self, path: &str) -> RdpdrResult<()> {
            let mut files = self.files.lock().unwrap();
            if files.contains_key(path) {
                return Err(status(NtStatus::OBJECT_NAME_COLLISION));
            }
            files.insert(path.to_owned(), (info(true, 0, 1), Vec::new()));
            Ok(())
        }

        async fn set_len(&self, path: &str, size: u64) -> RdpdrResult<()> {
            let mut files = self.files.lock().unwrap();
            let (info, data) = files.get_mut(path).ok_or_else(|| status(NtStatus::OBJECT_NAME_NOT_FOUND))?;
            data.resize(usize::try_from(size).unwrap(), 0);
            info.size = size;
            Ok(())
        }

        async fn set_times(&self, path: &str, access: Option<i64>, write: Option<i64>) -> RdpdrResult<()> {
            let mut files = self.files.lock().unwrap();
            let (info, _) = files.get_mut(path).ok_or_else(|| status(NtStatus::OBJECT_NAME_NOT_FOUND))?;
            if info.is_dir() {
                // FreeRDP's answer: it keeps no handle to a folder to set times through.
                return Err(status(NtStatus::NO_SUCH_FILE));
            }
            if let Some(time) = access {
                info.last_access_time = time;
            }
            if let Some(time) = write {
                info.last_write_time = time;
            }
            Ok(())
        }

        async fn remove(&self, path: &str, is_dir: bool) -> RdpdrResult<()> {
            let mut files = self.files.lock().unwrap();
            let (info, _) = files.get(path).ok_or_else(|| status(NtStatus::OBJECT_NAME_NOT_FOUND))?;
            match (info.is_dir(), is_dir) {
                (true, false) => return Err(status(NtStatus::FILE_IS_A_DIRECTORY)),
                (false, true) => return Err(status(NtStatus::NOT_A_DIRECTORY)),
                _ => {}
            }
            let prefix = format!("{path}\\");
            if files.keys().any(|p| p.starts_with(&prefix)) {
                return Err(status(NtStatus::DIRECTORY_NOT_EMPTY));
            }
            files.remove(path);
            Ok(())
        }

        async fn rename(&self, from: &str, to: &str) -> RdpdrResult<()> {
            let mut files = self.files.lock().unwrap();
            if !files.contains_key(from) {
                return Err(status(NtStatus::OBJECT_NAME_NOT_FOUND));
            }
            let prefix = format!("{from}\\");
            let moved: Vec<String> = files
                .keys()
                .filter(|p| p.as_str() == from || p.starts_with(&prefix))
                .cloned()
                .collect();
            files.remove(to);
            for old in moved {
                let entry = files.remove(&old).unwrap();
                files.insert(format!("{to}{}", &old[from.len()..]), entry);
            }
            Ok(())
        }
    }

    fn fs(drive: &Arc<FakeDrive>) -> DriveFs<Arc<FakeDrive>> {
        DriveFs::new(Arc::clone(drive), 501, 20, 7)
    }

    fn name(s: &str) -> filename3 {
        s.as_bytes().into()
    }

    /// nfsstat3 cannot be compared; its code can.
    fn code<T>(result: Result<T, nfsstat3>) -> Result<T, u32> {
        result.map_err(|status| status as u32)
    }

    fn err(status: nfsstat3) -> u32 {
        status as u32
    }

    fn sattr(size: Option<u64>, mtime: Option<nfstime3>) -> sattr3 {
        sattr3 {
            mode: set_mode3::Void,
            uid: set_uid3::Void,
            gid: set_gid3::Void,
            size: size.map_or(set_size3::Void, set_size3::size),
            atime: set_atime::DONT_CHANGE,
            mtime: mtime.map_or(set_mtime::DONT_CHANGE, set_mtime::SET_TO_CLIENT_TIME),
        }
    }

    #[tokio::test]
    async fn lookups_ask_for_one_path_and_mac_names_stay_on_the_mac() {
        let drive = FakeDrive::with(&[("\\a.txt", Some(b"hello")), ("\\docs", None)]);
        let fs = fs(&drive);

        let id = fs.lookup(ROOT_ID, &name("a.txt")).await.unwrap();
        assert_eq!(drive.stats.load(Ordering::SeqCst), 1);
        assert_eq!(drive.listings.load(Ordering::SeqCst), 0);
        // Fresh attributes answer the next lookup without asking again.
        assert_eq!(fs.lookup(ROOT_ID, &name("a.txt")).await.unwrap(), id);
        assert_eq!(drive.stats.load(Ordering::SeqCst), 1);

        assert_eq!(code(fs.lookup(ROOT_ID, &name("missing")).await), Err(err(nfsstat3::NFS3ERR_NOENT)));
        for mac in ["._a.txt", ".DS_Store", ".Spotlight-V100", "Icon\r"] {
            assert_eq!(code(fs.lookup(ROOT_ID, &name(mac)).await), Err(err(nfsstat3::NFS3ERR_NOENT)));
        }
        assert_eq!(code(fs.lookup(ROOT_ID, &name("a\\b")).await), Err(err(nfsstat3::NFS3ERR_NOENT)));
        assert_eq!(drive.stats.load(Ordering::SeqCst), 2, "only `missing` reached the client");

        let docs = fs.lookup(ROOT_ID, &name("docs")).await.unwrap();
        assert_eq!(fs.lookup(docs, &name("..")).await.unwrap(), ROOT_ID);
    }

    #[tokio::test]
    async fn a_directory_is_listed_once_per_enumeration() {
        let mut files: Vec<(String, Option<&[u8]>)> = (0..10).map(|i| (format!("\\f{i:02}"), Some(&b"x"[..]))).collect();
        files.push(("\\.DS_Store".to_owned(), Some(b"junk")));
        files.push(("\\._f00".to_owned(), Some(b"junk")));
        let refs: Vec<(&str, Option<&[u8]>)> = files.iter().map(|(p, c)| (p.as_str(), *c)).collect();
        let drive = FakeDrive::with(&refs);
        let fs = fs(&drive);

        let mut names = Vec::new();
        let mut cookie = 0;
        loop {
            let page = fs.readdir(ROOT_ID, cookie, 3).await.unwrap();
            names.extend(page.entries.iter().map(|e| String::from_utf8(e.name.to_vec()).unwrap()));
            if page.end {
                break;
            }
            cookie = page.entries.last().unwrap().fileid;
        }
        assert_eq!(names, (0..10).map(|i| format!("f{i:02}")).collect::<Vec<_>>());
        assert_eq!(drive.listings.load(Ordering::SeqCst), 1, "four pages, one listing");

        // The listing answers lookups while it is fresh, without a stat.
        fs.lookup(ROOT_ID, &name("f05")).await.unwrap();
        assert_eq!(code(fs.lookup(ROOT_ID, &name("nope")).await), Err(err(nfsstat3::NFS3ERR_NOENT)));
        assert_eq!(drive.stats.load(Ordering::SeqCst), 0);

        // A new enumeration lists again.
        fs.readdir(ROOT_ID, 0, 100).await.unwrap();
        assert_eq!(drive.listings.load(Ordering::SeqCst), 2);
        // A cookie from no current listing is refused rather than guessed at.
        assert_eq!(code(fs.readdir(ROOT_ID, 999, 3).await).err(), Some(err(nfsstat3::NFS3ERR_BAD_COOKIE)));
    }

    #[tokio::test]
    async fn attributes_carry_real_times_and_are_refreshed() {
        let drive = FakeDrive::with(&[("\\a.txt", Some(b"hello"))]);
        let fs = fs(&drive).with_attr_ttl(Duration::ZERO);
        let id = fs.lookup(ROOT_ID, &name("a.txt")).await.unwrap();

        let attr = fs.getattr(id).await.unwrap();
        assert_eq!(attr.size, 5);
        assert_eq!((attr.uid, attr.gid, attr.mode), (501, 20, 0o600));
        // 133000000000000000 is 2022-06-18T10:13:20Z.
        assert_eq!(attr.mtime.seconds, 1_655_526_400);

        // The file changes on the client; macOS must see it.
        drive.files.lock().unwrap().get_mut("\\a.txt").unwrap().0 = info(false, 9, 133_000_000_010_000_000);
        let attr = fs.getattr(id).await.unwrap();
        assert_eq!(attr.size, 9);
        assert_eq!(attr.mtime.seconds, 1_655_526_401);

        // And once it is gone, the handle is stale.
        drive.files.lock().unwrap().remove("\\a.txt");
        assert_eq!(code(fs.getattr(id).await).err(), Some(err(nfsstat3::NFS3ERR_STALE)));

        let root = fs.getattr(ROOT_ID).await.unwrap();
        assert_eq!((root.ftype as u32, root.mode), (ftype3::NF3DIR as u32, 0o700));
    }

    #[tokio::test]
    async fn reads_past_the_end_are_the_end_not_an_error() {
        let drive = FakeDrive::with(&[("\\a.txt", Some(b"hello"))]);
        let fs = fs(&drive);
        let id = fs.lookup(ROOT_ID, &name("a.txt")).await.unwrap();
        // Parts of a block past a short file's end, as macOS asks for them: answered here.
        assert_eq!(fs.read(id, 126_976, 126_976).await.unwrap(), (Vec::new(), true));
        assert_eq!(drive.reads.load(Ordering::SeqCst), 0);
        // With attributes too old to trust, the client is asked, and its error means the end.
        let fs = fs.with_attr_ttl(Duration::ZERO);
        assert_eq!(fs.read(id, 253_952, 8192).await.unwrap(), (Vec::new(), true));
        assert_eq!(drive.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reads_end_at_a_short_read() {
        let drive = FakeDrive::with(&[("\\a.txt", Some(b"hello world"))]);
        let fs = fs(&drive);
        let id = fs.lookup(ROOT_ID, &name("a.txt")).await.unwrap();
        assert_eq!(fs.read(id, 0, 5).await.unwrap(), (b"hello".to_vec(), false));
        assert_eq!(fs.read(id, 6, 100).await.unwrap(), (b"world".to_vec(), true));
        assert_eq!(code(fs.read(ROOT_ID, 0, 5).await).err(), Some(err(nfsstat3::NFS3ERR_ISDIR)));
    }

    #[tokio::test]
    async fn only_the_first_mount_succeeds() {
        let drive = FakeDrive::with(&[]);
        let fs = fs(&drive);
        assert_eq!(code(fs.path_to_id(b"/").await), Ok(ROOT_ID));
        assert_eq!(code(fs.path_to_id(b"/").await), Err(err(nfsstat3::NFS3ERR_ACCES)));
        assert_eq!(code(fs.path_to_id(b"/other").await), Err(err(nfsstat3::NFS3ERR_NOENT)));
    }

    #[test]
    fn handles_need_the_secret() {
        let drive = FakeDrive::with(&[]);
        let fs = fs(&drive);
        let fh = fs.id_to_fh(ROOT_ID);
        assert_eq!(fh.data.len(), 24);
        assert_eq!(code(fs.fh_to_id(&fh)), Ok(ROOT_ID));

        let mut forged = fh.clone();
        forged.data[3] ^= 1;
        assert_eq!(code(fs.fh_to_id(&forged)), Err(err(nfsstat3::NFS3ERR_BADHANDLE)));
        // nfsserve's own format: a generation number and the file ID.
        let mut plain = 0u64.to_le_bytes().to_vec();
        plain.extend_from_slice(&ROOT_ID.to_le_bytes());
        assert_eq!(code(fs.fh_to_id(&nfs_fh3 { data: plain })), Err(err(nfsstat3::NFS3ERR_BADHANDLE)));
        // A file ID the server never gave out.
        assert_eq!(code(fs.fh_to_id(&fs.id_to_fh(99))), Err(err(nfsstat3::NFS3ERR_STALE)));
        // Each server has its own secret.
        assert_ne!(fs.secret, DriveFs::new(Arc::clone(&drive), 501, 20, 7).secret);
    }

    #[tokio::test]
    async fn files_are_created_written_and_read_back() {
        let drive = FakeDrive::with(&[("\\docs", None)]);
        let fs = fs(&drive);
        let docs = fs.lookup(ROOT_ID, &name("docs")).await.unwrap();

        let (id, attr) = fs.create(docs, &name("new.txt"), sattr(Some(0), None)).await.unwrap();
        assert_eq!(attr.size, 0);
        fs.write(id, 0, b"hello ").await.unwrap();
        let attr = fs.write(id, 6, b"world").await.unwrap();
        assert_eq!(attr.size, 11);
        assert_eq!(drive.contents("\\docs\\new.txt").as_deref(), Some(&b"hello world"[..]));
        assert_eq!(fs.read(id, 0, 100).await.unwrap(), (b"hello world".to_vec(), true));

        // An exclusive create of a file that exists fails; one of a new file does not.
        assert_eq!(
            code(fs.create_exclusive(docs, &name("new.txt")).await),
            Err(err(nfsstat3::NFS3ERR_EXIST))
        );
        let other = fs.create_exclusive(docs, &name("other.txt")).await.unwrap();
        assert_ne!(other, id);

        // Creating with a size of 0 empties an existing file.
        fs.create(docs, &name("new.txt"), sattr(Some(0), None)).await.unwrap();
        assert_eq!(drive.contents("\\docs\\new.txt").as_deref(), Some(&b""[..]));

        let (dir, attr) = fs.mkdir(ROOT_ID, &name("made")).await.unwrap();
        assert_eq!(attr.ftype as u32, ftype3::NF3DIR as u32);
        assert!(drive.paths().contains(&"\\made".to_owned()));
        fs.create(dir, &name("inside"), sattr(Some(0), None)).await.unwrap();
        assert!(drive.paths().contains(&"\\made\\inside".to_owned()));
    }

    #[tokio::test]
    async fn a_rename_keeps_file_ids_and_moves_what_is_inside() {
        let drive = FakeDrive::with(&[("\\a", None), ("\\a\\x.txt", Some(b"x")), ("\\b", None), ("\\b\\y.txt", Some(b"y"))]);
        let fs = fs(&drive);
        let a = fs.lookup(ROOT_ID, &name("a")).await.unwrap();
        let x = fs.lookup(a, &name("x.txt")).await.unwrap();
        let y = fs.lookup(fs.lookup(ROOT_ID, &name("b")).await.unwrap(), &name("y.txt")).await.unwrap();

        // A folder moves with its contents, and every file ID stays.
        fs.rename(ROOT_ID, &name("a"), ROOT_ID, &name("renamed")).await.unwrap();
        assert!(drive.paths().contains(&"\\renamed\\x.txt".to_owned()));
        assert_eq!(fs.lookup(ROOT_ID, &name("renamed")).await.unwrap(), a);
        assert_eq!(fs.lookup(a, &name("x.txt")).await.unwrap(), x);
        assert_eq!(fs.read(x, 0, 10).await.unwrap(), (b"x".to_vec(), true));

        // Renaming over a file replaces it: the old one's handle is stale, the moved one keeps its ID.
        let b = fs.lookup(ROOT_ID, &name("b")).await.unwrap();
        fs.rename(a, &name("x.txt"), b, &name("y.txt")).await.unwrap();
        assert_eq!(drive.contents("\\b\\y.txt").as_deref(), Some(&b"x"[..]));
        assert_eq!(fs.lookup(b, &name("y.txt")).await.unwrap(), x);
        assert_eq!(code(fs.fh_to_id(&fs.id_to_fh(y))), Err(err(nfsstat3::NFS3ERR_STALE)));
    }

    #[tokio::test]
    async fn files_and_folders_are_removed() {
        let drive = FakeDrive::with(&[("\\d", None), ("\\d\\f", Some(b"f")), ("\\g", Some(b"g"))]);
        let fs = fs(&drive);
        let d = fs.lookup(ROOT_ID, &name("d")).await.unwrap();

        assert_eq!(code(fs.remove(ROOT_ID, &name("d")).await), Err(err(nfsstat3::NFS3ERR_NOTEMPTY)));
        fs.remove(d, &name("f")).await.unwrap();
        fs.remove(ROOT_ID, &name("d")).await.unwrap();
        fs.remove(ROOT_ID, &name("g")).await.unwrap();
        assert_eq!(drive.paths(), ["\\"]);
        assert_eq!(code(fs.fh_to_id(&fs.id_to_fh(d))), Err(err(nfsstat3::NFS3ERR_STALE)));
        assert_eq!(code(fs.remove(ROOT_ID, &name("g")).await), Err(err(nfsstat3::NFS3ERR_NOENT)));
    }

    #[tokio::test]
    async fn setattr_truncates_and_sets_times_on_the_client() {
        let drive = FakeDrive::with(&[("\\a.txt", Some(b"hello world"))]);
        let fs = fs(&drive);
        let id = fs.lookup(ROOT_ID, &name("a.txt")).await.unwrap();

        let attr = fs.setattr(id, sattr(Some(5), None)).await.unwrap();
        assert_eq!(attr.size, 5);
        assert_eq!(drive.contents("\\a.txt").as_deref(), Some(&b"hello"[..]));

        let when = nfstime3 {
            seconds: 1_577_836_800, // 2020-01-01
            nseconds: 0,
        };
        let attr = fs.setattr(id, sattr(None, Some(when))).await.unwrap();
        assert_eq!(attr.mtime.seconds, when.seconds);
        let (info, _) = drive.files.lock().unwrap()["\\a.txt"].clone();
        assert_eq!(nfs_time(info.last_write_time).seconds, when.seconds);

        // A folder whose times the client cannot set is not reported as gone.
        let (dir, _) = fs.mkdir(ROOT_ID, &name("d")).await.unwrap();
        fs.setattr(dir, sattr(None, Some(when))).await.unwrap();
    }

    #[tokio::test]
    async fn macs_own_files_stay_on_the_mac() {
        let drive = FakeDrive::with(&[("\\a.txt", Some(b"a"))]);
        let fs = fs(&drive);

        // What macOS writes to an AppleDouble file and to .DS_Store stays here and reads back.
        let (double, _) = fs.create(ROOT_ID, &name("._a.txt"), sattr(Some(0), None)).await.unwrap();
        fs.write(double, 0, b"xattrs").await.unwrap();
        let ds = fs.create_exclusive(ROOT_ID, &name(".DS_Store")).await.unwrap();
        fs.write(ds, 0, b"view").await.unwrap();
        assert_eq!(fs.read(double, 0, 100).await.unwrap(), (b"xattrs".to_vec(), true));
        assert_eq!(fs.lookup(ROOT_ID, &name("._a.txt")).await.unwrap(), double);
        assert_eq!(fs.getattr(ds).await.unwrap().size, 4);
        assert_eq!(drive.paths(), ["\\", "\\a.txt"]);
        assert_eq!(drive.writes.load(Ordering::SeqCst), 0);

        // They are not listed; they follow their file's rename and go with its deletion.
        let names: Vec<_> = fs.readdir(ROOT_ID, 0, 100).await.unwrap().entries.iter().map(|e| e.name.to_vec()).collect();
        assert_eq!(names, [b"a.txt".to_vec()]);
        fs.rename(ROOT_ID, &name("._a.txt"), ROOT_ID, &name("._b.txt")).await.unwrap();
        assert_eq!(fs.lookup(ROOT_ID, &name("._b.txt")).await.unwrap(), double);
        fs.remove(ROOT_ID, &name("._b.txt")).await.unwrap();
        assert_eq!(code(fs.lookup(ROOT_ID, &name("._b.txt")).await), Err(err(nfsstat3::NFS3ERR_NOENT)));

        // No folders of macOS's own, and no moving between here and the drive.
        assert_eq!(code(fs.mkdir(ROOT_ID, &name(".Trashes")).await).err(), Some(err(nfsstat3::NFS3ERR_ACCES)));
        assert_eq!(
            code(fs.rename(ROOT_ID, &name(".DS_Store"), ROOT_ID, &name("view.txt")).await),
            Err(err(nfsstat3::NFS3ERR_INVAL))
        );
        // .TemporaryItems is a real folder on the drive, for saving documents.
        fs.mkdir(ROOT_ID, &name(".TemporaryItems")).await.unwrap();
        assert!(drive.paths().contains(&"\\.TemporaryItems".to_owned()));
    }

    #[tokio::test]
    async fn the_room_for_macs_own_files_is_bounded() {
        let drive = FakeDrive::with(&[]);
        let fs = fs(&drive).with_local_limit(8);
        let (id, _) = fs.create(ROOT_ID, &name("._x"), sattr(Some(0), None)).await.unwrap();
        fs.write(id, 0, b"12345678").await.unwrap();
        assert_eq!(code(fs.write(id, 8, b"9").await).err(), Some(err(nfsstat3::NFS3ERR_NOSPC)));
        // Freeing room makes room.
        fs.remove(ROOT_ID, &name("._x")).await.unwrap();
        let (id, _) = fs.create(ROOT_ID, &name("._y"), sattr(Some(0), None)).await.unwrap();
        fs.write(id, 0, b"87654321").await.unwrap();
    }

    #[test]
    fn statuses_map_to_the_closest_nfs_error() {
        let cases = [
            (NtStatus::OBJECT_NAME_NOT_FOUND, nfsstat3::NFS3ERR_NOENT),
            (NtStatus::OBJECT_PATH_NOT_FOUND, nfsstat3::NFS3ERR_NOENT),
            (NtStatus::NO_SUCH_FILE, nfsstat3::NFS3ERR_NOENT),
            (NtStatus::ACCESS_DENIED, nfsstat3::NFS3ERR_ACCES),
            (NtStatus::SHARING_VIOLATION, nfsstat3::NFS3ERR_ACCES),
            (NtStatus::OBJECT_NAME_COLLISION, nfsstat3::NFS3ERR_EXIST),
            (NtStatus::DIRECTORY_NOT_EMPTY, nfsstat3::NFS3ERR_NOTEMPTY),
            (NtStatus::DISK_FULL, nfsstat3::NFS3ERR_NOSPC),
            (NtStatus::MEDIA_WRITE_PROTECTED, nfsstat3::NFS3ERR_ROFS),
            (NtStatus::NO_SUCH_DEVICE, nfsstat3::NFS3ERR_STALE),
            (NtStatus::from(0xC000_0185), nfsstat3::NFS3ERR_IO),
        ];
        for (status, expected) in cases {
            let error = RdpdrError::Status { op: "create", status };
            assert_eq!(nfs_error(&error) as u32, expected as u32, "{status:?}");
        }
        assert_eq!(nfs_error(&RdpdrError::Timeout { op: "read" }) as u32, nfsstat3::NFS3ERR_IO as u32);
        assert_eq!(nfs_error(&RdpdrError::Closed) as u32, nfsstat3::NFS3ERR_IO as u32);
    }

    #[test]
    fn filetimes_become_nfs_times_and_back() {
        assert_eq!(nfs_time(116_444_736_000_000_000).seconds, 0);
        let t = nfs_time(133_000_000_001_234_567);
        assert_eq!((t.seconds, t.nseconds), (1_655_526_400, 123_456_700));
        assert_eq!(filetime(t), 133_000_000_001_234_567);
        assert_eq!(nfs_time(0).seconds, 0);
        assert_eq!(nfs_time(i64::MAX).seconds, u32::MAX);
    }
}
