//! Clipboard sharing between the Mac and the client (MS-RDPECLIP): text, pictures and files.
//!
//! Each connection gets a backend and a worker thread that owns the pasteboard. The worker polls
//! the pasteboard's change count and offers what is new to the client; what the client copies is
//! fetched right away, one format after another since the channel takes one request at a time,
//! and written to the pasteboard together. What is identical to what was last exchanged is
//! neither offered nor written again, so the two clipboards cannot bounce it back and forth.
//!
//! Pictures go to the client as CF_DIB and as PNG, which keeps transparency; from the client the
//! PNG is taken if offered, else the DIB. On the Mac they are PNG and TIFF (see [`clip_image`]).
//!
//! Files copied on the Mac are offered as a file list whose contents the client reads piece by
//! piece; files copied on the client are fetched into a folder under ~/Library/Caches/rdpmac right
//! away and put on the pasteboard once all have arrived (see [`clip_files`]).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FileDescriptor, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::clip_files::{self, Incoming, Outgoing, Step};
use crate::clip_image::{self, Bitmap};

const POLL: Duration = Duration::from_millis(500);
/// Larger text is not offered; RDP clients hold the whole transfer in memory.
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// The id this side gives the PNG format it offers; registered formats take ids from 0xC000.
const PNG_FORMAT: ClipboardFormatId = ClipboardFormatId(0xC0A0);
/// Windows registers PNG under this name.
const PNG_NAME: &str = "PNG";

type EventSender = Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A picture to put on the pasteboard, as it came from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picture {
    Png(Vec<u8>),
    Bitmap(Bitmap),
}

/// The pasteboard as the sync logic sees it.
pub trait Pasteboard {
    fn change_count(&self) -> isize;
    fn read_text(&self) -> Option<String>;
    /// Something that changes with the picture on the pasteboard; `None` without a picture.
    fn picture_id(&self) -> Option<u64>;
    fn read_bitmap(&self) -> Option<Bitmap>;
    fn read_png(&self) -> Option<Vec<u8>>;
    /// Replaces the pasteboard's contents with what is given.
    fn write(&mut self, text: Option<&str>, picture: Option<&Picture>) -> bool;
    /// The files and folders on the pasteboard, as copied in Finder.
    fn read_files(&self) -> Vec<PathBuf>;
    /// Replaces the pasteboard's contents with these files and folders.
    fn write_files(&mut self, paths: &[PathBuf]) -> bool;
    fn clear(&mut self);
}

/// What the client can be asked for, in the order it is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    /// A file list; its answer arrives as [`Command::RemoteFiles`].
    Files(ClipboardFormatId),
    Text,
    Png(ClipboardFormatId),
    Dib(ClipboardFormatId),
}

impl Wanted {
    fn format(self) -> ClipboardFormatId {
        match self {
            Wanted::Text => ClipboardFormatId::CF_UNICODETEXT,
            Wanted::Files(id) | Wanted::Png(id) | Wanted::Dib(id) => id,
        }
    }

    /// What to ask the client for among the formats it offers: files alone, else text and one
    /// picture format.
    fn from_offer(formats: &[ClipboardFormat]) -> VecDeque<Wanted> {
        let has = |id: ClipboardFormatId| formats.iter().any(|f| f.id == id);
        let named = |name: &str| formats.iter().find(|f| f.name.as_ref().is_some_and(|n| n.value().eq_ignore_ascii_case(name)));
        if let Some(files) = named(ClipboardFormatName::FILE_LIST.value()) {
            return VecDeque::from([Wanted::Files(files.id)]);
        }
        let png = named(PNG_NAME).map(|f| Wanted::Png(f.id));
        let dib = [ClipboardFormatId::CF_DIBV5, ClipboardFormatId::CF_DIB]
            .into_iter()
            .find(|&id| has(id))
            .map(Wanted::Dib);
        has(ClipboardFormatId::CF_UNICODETEXT)
            .then_some(Wanted::Text)
            .into_iter()
            .chain(png.or(dib))
            .collect()
    }
}

/// Requests from the connection to the worker.
#[derive(Debug)]
enum Command {
    /// The client wants our format list.
    Announce,
    /// The client wants our data in this format.
    Send(ClipboardFormatId),
    /// The client copied something and offers these formats.
    RemoteCopy(Vec<ClipboardFormat>),
    /// The client's answer to our last request; `None` when it could not give the data.
    Received(Option<Vec<u8>>),
    /// The client's file list, the answer to a request for it.
    RemoteFiles(Vec<FileDescriptor>),
    /// The client wants a size or a piece of a file we offered.
    FileRequest(FileContentsRequest),
    /// The client's answer to a File Contents Request: its stream id and the data, if any.
    FileData(u32, Option<Vec<u8>>),
    /// What the client agreed to: whether files can go to it.
    Capabilities(ClipboardGeneralCapabilityFlags),
    /// The client locked or unlocked our file list under this clipDataId.
    Lock(u32),
    Unlock(u32),
}

/// What the client is being asked for, and what it sent so far.
#[derive(Default)]
struct Fetch {
    queue: VecDeque<Wanted>,
    current: Option<Wanted>,
    /// Answers still due for requests made before the client copied something else.
    stale: usize,
    text: Option<String>,
    picture: Option<Picture>,
}

/// What moves text and pictures between the pasteboard and the client; free of threads and I/O.
struct Sync {
    last_count: isize,
    /// Text last offered to or received from the client.
    last_text: Option<String>,
    /// The picture last offered to or received from the client, by [`Pasteboard::picture_id`].
    last_picture: Option<u64>,
    /// Files last offered to or received from the client.
    last_files: Vec<PathBuf>,
    fetch: Fetch,
    outgoing: Outgoing,
    incoming: Option<Incoming>,
    /// Where files from the client go, a folder for each transfer.
    cache: PathBuf,
    /// The last File Contents stream id used.
    stream: u32,
    /// Whether the client negotiated file copy; without it copied files go as their names.
    files_enabled: bool,
}

impl Sync {
    fn new(pasteboard: &impl Pasteboard) -> Self {
        Self::with_cache(pasteboard, cache_folder())
    }

    fn with_cache(pasteboard: &impl Pasteboard, cache: PathBuf) -> Self {
        Self {
            last_count: pasteboard.change_count(),
            last_text: None,
            last_picture: None,
            last_files: Vec::new(),
            fetch: Fetch::default(),
            outgoing: Outgoing::default(),
            incoming: None,
            cache,
            stream: 0,
            files_enabled: false,
        }
    }

    /// Offers the files on the pasteboard, if there are any.
    fn offer_files(&mut self, files: Vec<PathBuf>) -> Option<ClipboardMessage> {
        let descriptors = self.outgoing.offer(&files);
        if descriptors.is_empty() {
            return None;
        }
        info!(files = files.len(), entries = descriptors.len(), "files copied on the Mac, offering them to the client");
        self.last_files = files;
        self.last_text = None;
        self.last_picture = None;
        Some(ClipboardMessage::SendInitiateFileCopy(descriptors))
    }

    /// Gives up a transfer from the client, for a newer copy on either side or a silent client.
    fn cancel_incoming(&mut self, why: &str) {
        if self.incoming.take().is_some() {
            warn!(why, "stopped fetching files copied on the client");
        }
    }

    fn usable(text: Option<String>) -> Option<String> {
        text.filter(|t| !t.is_empty() && t.len() <= MAX_TEXT_BYTES)
    }

    fn formats(text: bool, picture: bool) -> Vec<ClipboardFormat> {
        let mut formats = Vec::new();
        if text {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
        }
        if picture {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
            formats.push(ClipboardFormat::new(PNG_FORMAT).with_name(ClipboardFormatName::new_static(PNG_NAME)));
        }
        formats
    }

    /// Called periodically: offers what was copied on the Mac since the last look.
    fn poll(&mut self, pasteboard: &impl Pasteboard) -> Option<ClipboardMessage> {
        if self.incoming.as_ref().is_some_and(|i| i.last_activity.elapsed() > clip_files::STALL) {
            self.cancel_incoming("the client stopped sending the files");
        }
        let count = pasteboard.change_count();
        if count == self.last_count {
            return None;
        }
        self.last_count = count;
        self.cancel_incoming("something else was copied on the Mac");
        let files = pasteboard.read_files();
        if !files.is_empty() && self.files_enabled {
            if files == self.last_files {
                return None;
            }
            return self.offer_files(files);
        }
        self.last_files.clear();
        let text = Self::usable(pasteboard.read_text());
        let picture = pasteboard.picture_id();
        if (text.is_none() && picture.is_none()) || (text == self.last_text && picture == self.last_picture) {
            return None;
        }
        info!(
            chars = text.as_ref().map(|t| t.chars().count()),
            picture = picture.is_some(),
            "copied on the Mac, offering it to the client"
        );
        let formats = Self::formats(text.is_some(), picture.is_some());
        self.last_text = text;
        self.last_picture = picture;
        Some(ClipboardMessage::SendInitiateCopy(formats))
    }

    fn handle(&mut self, command: Command, pasteboard: &mut impl Pasteboard) -> Option<ClipboardMessage> {
        match command {
            Command::Announce => {
                let files = pasteboard.read_files();
                if !files.is_empty() && self.files_enabled {
                    if let Some(offer) = self.offer_files(files) {
                        return Some(offer);
                    }
                }
                let text = Self::usable(pasteboard.read_text());
                let picture = pasteboard.picture_id();
                let formats = Self::formats(text.is_some(), picture.is_some());
                self.last_text = text;
                self.last_picture = picture;
                Some(ClipboardMessage::SendInitiateCopy(formats))
            }
            Command::Send(format) => Some(ClipboardMessage::SendFormatData(Self::data(format, pasteboard))),
            Command::RemoteCopy(formats) => {
                self.cancel_incoming("the client copied something else");
                if self.fetch.current.is_some() {
                    self.fetch.stale += 1;
                }
                self.fetch = Fetch {
                    queue: Wanted::from_offer(&formats),
                    stale: self.fetch.stale,
                    ..Fetch::default()
                };
                self.next(pasteboard)
            }
            Command::Received(data) => {
                if self.fetch.stale > 0 {
                    self.fetch.stale -= 1;
                    return None;
                }
                let wanted = self.fetch.current.take()?;
                match (wanted, data) {
                    (_, None) => debug!(format = ?wanted.format(), "client could not provide its clipboard data"),
                    (Wanted::Files(_), Some(_)) => warn!("the client's file list could not be read"),
                    (Wanted::Text, Some(data)) => match FormatDataResponse::new_data(data).to_unicode_string() {
                        Ok(text) => self.fetch.text = Some(text).filter(|t| !t.is_empty()),
                        Err(e) => warn!(%e, "client clipboard text could not be decoded"),
                    },
                    (Wanted::Png(_), Some(data)) => self.fetch.picture = Some(Picture::Png(data)),
                    (Wanted::Dib(_), Some(data)) => match clip_image::from_dib(&data) {
                        Some(bitmap) => self.fetch.picture = Some(Picture::Bitmap(bitmap)),
                        None => warn!(bytes = data.len(), "client clipboard picture could not be read"),
                    },
                }
                self.next(pasteboard)
            }
            Command::RemoteFiles(files) => {
                if self.fetch.stale > 0 {
                    self.fetch.stale -= 1;
                    return None;
                }
                if !matches!(self.fetch.current.take(), Some(Wanted::Files(_))) {
                    return None;
                }
                self.fetch.queue.clear();
                info!(entries = files.len(), "files copied on the client, fetching them");
                let incoming = clip_files::transfer_folder(&self.cache)
                    .and_then(|folder| Incoming::new(folder, files, self.stream));
                match incoming {
                    Ok(incoming) => {
                        // Until the files are here, a paste on the Mac must not paste what was there.
                        pasteboard.clear();
                        self.last_count = pasteboard.change_count();
                        self.last_text = None;
                        self.last_picture = None;
                        self.last_files.clear();
                        let mut incoming = incoming;
                        let step = incoming.advance();
                        self.incoming = Some(incoming);
                        self.step(step, pasteboard)
                    }
                    Err(e) => {
                        warn!(%e, folder = %self.cache.display(), "no folder for files copied on the client");
                        None
                    }
                }
            }
            Command::FileData(stream, data) => {
                let step = self.incoming.as_mut()?.received(stream, data.as_deref());
                self.step(step, pasteboard)
            }
            Command::FileRequest(request) => Some(ClipboardMessage::SendFileContentsResponse(self.outgoing.answer(&request))),
            Command::Capabilities(flags) => {
                self.files_enabled = flags.contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED);
                debug!(?flags, "clipboard capabilities agreed with the client");
                None
            }
            Command::Lock(id) => {
                self.outgoing.lock(id);
                None
            }
            Command::Unlock(id) => {
                self.outgoing.unlock(id);
                None
            }
        }
    }

    /// Carries a transfer from the client one step further.
    fn step(&mut self, step: Step, pasteboard: &mut impl Pasteboard) -> Option<ClipboardMessage> {
        if let Some(incoming) = &self.incoming {
            self.stream = incoming.stream();
        }
        match step {
            Step::Request(request) => Some(ClipboardMessage::SendFileContentsRequest(request)),
            Step::Wait => None,
            Step::Done(tops) => {
                self.incoming = None;
                let written = pasteboard.write_files(&tops);
                info!(files = tops.len(), written, "files copied on the client, now on the Mac");
                self.last_count = pasteboard.change_count();
                self.last_files = pasteboard.read_files();
                None
            }
            Step::Failed => {
                self.incoming = None;
                warn!("fetching files copied on the client failed");
                None
            }
        }
    }

    /// Asks the client for the next format, or stores what it sent once there is nothing left.
    fn next(&mut self, pasteboard: &mut impl Pasteboard) -> Option<ClipboardMessage> {
        if let Some(wanted) = self.fetch.queue.pop_front() {
            self.fetch.current = Some(wanted);
            return Some(ClipboardMessage::SendInitiatePaste(wanted.format()));
        }
        let (text, picture) = (self.fetch.text.take(), self.fetch.picture.take());
        if text.is_none() && picture.is_none() {
            return None;
        }
        if picture.is_none() && text == self.last_text {
            return None;
        }
        let unchanged = picture.is_none() && text.is_some() && pasteboard.read_text() == text && pasteboard.picture_id().is_none();
        if !unchanged {
            let written = pasteboard.write(text.as_deref(), picture.as_ref());
            info!(
                chars = text.as_ref().map(|t| t.chars().count()),
                picture = picture.is_some(),
                written,
                "copied on the client, now on the Mac"
            );
        }
        self.last_count = pasteboard.change_count();
        self.last_text = text;
        self.last_picture = pasteboard.picture_id();
        None
    }

    /// The pasteboard's contents in `format`, for the client.
    fn data(format: ClipboardFormatId, pasteboard: &impl Pasteboard) -> FormatDataResponse<'static> {
        let data = match format {
            ClipboardFormatId::CF_UNICODETEXT => {
                return match Self::usable(pasteboard.read_text()) {
                    Some(text) => {
                        debug!(chars = text.chars().count(), "sending text to the client");
                        FormatDataResponse::new_unicode_string(&text)
                    }
                    None => FormatDataResponse::new_error(),
                };
            }
            ClipboardFormatId::CF_DIB => pasteboard.read_bitmap().map(|bitmap| clip_image::to_dib(&bitmap)),
            PNG_FORMAT => pasteboard.read_png(),
            other => {
                debug!(format = ?other, "unsupported clipboard format requested");
                None
            }
        };
        match data {
            Some(data) => {
                debug!(format = ?format, bytes = data.len(), "sending a picture to the client");
                FormatDataResponse::new_data(data)
            }
            None => FormatDataResponse::new_error(),
        }
    }
}

pub struct ClipboardFactory {
    events: EventSender,
}

impl ClipboardFactory {
    pub fn new() -> Self {
        Self {
            events: Arc::default(),
        }
    }
}

impl Default for ClipboardFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerEventSender for ClipboardFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *lock(&self.events) = Some(sender);
    }
}

impl CliprdrBackendFactory for ClipboardFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend::spawn(self.events.clone()))
    }
}

impl CliprdrServerFactory for ClipboardFactory {}

fn post(events: &EventSender, message: ClipboardMessage) {
    if let Some(tx) = lock(events).as_ref() {
        let _ = tx.send(ServerEvent::Clipboard(message));
    }
}

/// One connection's clipboard channel; the worker thread ends when this is dropped.
#[derive(Debug)]
struct Backend {
    commands: Sender<Command>,
}

ironrdp_core::impl_as_any!(Backend);

impl Backend {
    fn spawn(events: EventSender) -> Self {
        let (commands, rx) = mpsc::channel();
        if let Err(e) = thread::Builder::new()
            .name("rdpmac-clipboard".into())
            .spawn(move || worker(rx, events))
        {
            warn!(%e, "clipboard worker could not start; clipboard sharing is off");
        }
        Self { commands }
    }

    fn command(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        ".rdpmac-clipboard"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
    }

    fn on_ready(&mut self) {
        self.command(Command::Announce);
    }

    fn on_request_format_list(&mut self) {
        self.command(Command::Announce);
    }

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        self.command(Command::Capabilities(capabilities));
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.command(Command::RemoteCopy(available_formats.to_vec()));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.command(Command::Send(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let data = (!response.is_error()).then(|| response.data().to_vec());
        self.command(Command::Received(data));
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        self.command(Command::FileRequest(request));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let data = (!response.is_error()).then(|| response.data().to_vec());
        self.command(Command::FileData(response.stream_id(), data));
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        self.command(Command::Lock(data_id.0));
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        self.command(Command::Unlock(data_id.0));
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], _clip_data_id: Option<u32>) {
        self.command(Command::RemoteFiles(files.to_vec()));
    }
}

/// ~/Library/Caches/rdpmac/clipboard, where files copied on the client are put.
fn cache_folder() -> PathBuf {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Caches/rdpmac/clipboard"))
        .unwrap_or_else(|| std::env::temp_dir().join("rdpmac-clipboard"))
}

fn worker(rx: Receiver<Command>, events: EventSender) {
    #[cfg(target_os = "macos")]
    let mut pasteboard = mac::MacPasteboard::general();
    #[cfg(not(target_os = "macos"))]
    let mut pasteboard = NoPasteboard;
    let mut sync = Sync::new(&pasteboard);
    loop {
        let message = match rx.recv_timeout(POLL) {
            Ok(command) => with_pool(|| sync.handle(command, &mut pasteboard)),
            Err(RecvTimeoutError::Timeout) => with_pool(|| sync.poll(&pasteboard)),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(message) = message {
            post(&events, message);
        }
    }
    debug!("clipboard worker stopped");
}

/// AppKit hands out autoreleased objects; a worker thread has no pool of its own.
fn with_pool<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(target_os = "macos")]
    return objc2::rc::autoreleasepool(|_| f());
    #[cfg(not(target_os = "macos"))]
    f()
}

#[cfg(not(target_os = "macos"))]
struct NoPasteboard;

#[cfg(not(target_os = "macos"))]
impl Pasteboard for NoPasteboard {
    fn change_count(&self) -> isize {
        0
    }
    fn read_text(&self) -> Option<String> {
        None
    }
    fn picture_id(&self) -> Option<u64> {
        None
    }
    fn read_bitmap(&self) -> Option<Bitmap> {
        None
    }
    fn read_png(&self) -> Option<Vec<u8>> {
        None
    }
    fn write(&mut self, _text: Option<&str>, _picture: Option<&Picture>) -> bool {
        false
    }
    fn read_files(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn write_files(&mut self, _paths: &[PathBuf]) -> bool {
        false
    }
    fn clear(&mut self) {}
}

#[cfg(target_os = "macos")]
mod mac {
    use std::hash::{Hash, Hasher};
    use std::path::PathBuf;

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeString,
        NSPasteboardTypeTIFF, NSPasteboardWriting,
    };
    use objc2_foundation::{NSArray, NSData, NSString, NSURL};

    use super::{Pasteboard, Picture};
    use crate::clip_image::{codec, Bitmap};

    /// The general pasteboard, looked up on every call so a restarted pasteboard server is fine;
    /// tests use a private one instead.
    pub struct MacPasteboard {
        private: Option<Retained<NSPasteboard>>,
    }

    impl MacPasteboard {
        pub fn general() -> Self {
            Self { private: None }
        }

        fn board(&self) -> Retained<NSPasteboard> {
            self.private.clone().unwrap_or_else(NSPasteboard::generalPasteboard)
        }

        fn data(&self, kind: &NSPasteboardType) -> Option<Vec<u8>> {
            Some(self.board().dataForType(kind)?.to_vec())
        }

        fn has_files(&self) -> bool {
            self.data(unsafe { NSPasteboardTypeFileURL }).is_some()
        }

        /// The picture's file as the pasteboard holds it, PNG preferred. Files copied in Finder
        /// come with their icons as TIFF, which is no picture to paste.
        fn picture_file(&self) -> Option<Vec<u8>> {
            if self.has_files() {
                return None;
            }
            self.data(unsafe { NSPasteboardTypePNG }).or_else(|| self.data(unsafe { NSPasteboardTypeTIFF }))
        }
    }

    impl Pasteboard for MacPasteboard {
        fn change_count(&self) -> isize {
            self.board().changeCount()
        }

        fn read_text(&self) -> Option<String> {
            let text = self.board().stringForType(unsafe { NSPasteboardTypeString })?;
            Some(text.to_string())
        }

        fn picture_id(&self) -> Option<u64> {
            let file = self.picture_file()?;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            file.hash(&mut hasher);
            Some(hasher.finish())
        }

        fn read_bitmap(&self) -> Option<Bitmap> {
            codec::decode(&self.picture_file()?)
        }

        fn read_png(&self) -> Option<Vec<u8>> {
            if self.has_files() {
                return None;
            }
            self.data(unsafe { NSPasteboardTypePNG }).or_else(|| codec::png(&self.read_bitmap()?))
        }

        fn write(&mut self, text: Option<&str>, picture: Option<&Picture>) -> bool {
            // PNG for today's apps and TIFF for older ones; both from one decoded picture, so
            // that a PNG from the client that ImageIO cannot read writes nothing.
            let files = match picture {
                Some(Picture::Png(png)) => codec::decode(png).and_then(|b| Some((png.clone(), codec::tiff(&b)?))),
                Some(Picture::Bitmap(bitmap)) => codec::png(bitmap).zip(codec::tiff(bitmap)),
                None => None,
            };
            if text.is_none() && files.is_none() {
                return false;
            }
            let pasteboard = self.board();
            pasteboard.clearContents();
            let mut written = true;
            if let Some(text) = text {
                written &= pasteboard.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
            }
            if let Some((png, tiff)) = files {
                written &= pasteboard.setData_forType(Some(&NSData::with_bytes(&png)), unsafe { NSPasteboardTypePNG });
                written &= pasteboard.setData_forType(Some(&NSData::with_bytes(&tiff)), unsafe { NSPasteboardTypeTIFF });
            }
            written
        }

        fn read_files(&self) -> Vec<PathBuf> {
            let Some(items) = self.board().pasteboardItems() else {
                return Vec::new();
            };
            items
                .iter()
                .filter_map(|item| {
                    let url = item.stringForType(unsafe { NSPasteboardTypeFileURL })?;
                    // Finder copies file reference URLs (file:///.file/id=...); resolve them to paths.
                    let path = NSURL::URLWithString(&url)?.filePathURL()?.path()?;
                    Some(PathBuf::from(path.to_string()))
                })
                .collect()
        }

        fn write_files(&mut self, paths: &[PathBuf]) -> bool {
            let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
                .iter()
                .filter_map(|path| path.to_str())
                .map(|path| ProtocolObject::from_retained(NSURL::fileURLWithPath(&NSString::from_str(path))))
                .collect();
            let pasteboard = self.board();
            pasteboard.clearContents();
            pasteboard.writeObjects(&NSArray::from_retained_slice(&urls))
        }

        fn clear(&mut self) {
            self.board().clearContents();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A private pasteboard, released when the test ends, so the user's clipboard is untouched.
        struct Private(MacPasteboard);

        impl Private {
            fn new() -> Self {
                Self(MacPasteboard {
                    private: Some(NSPasteboard::pasteboardWithUniqueName()),
                })
            }
        }

        impl Drop for Private {
            fn drop(&mut self) {
                if let Some(board) = &self.0.private {
                    // Not bound by objc2; frees the unique pasteboard in the pasteboard server.
                    let _: () = unsafe { objc2::msg_send![&**board, releaseGlobally] };
                }
            }
        }

        fn sample() -> Bitmap {
            Bitmap {
                width: 3,
                height: 2,
                bgra: [[200, 100, 50, 255], [0, 0, 0, 0], [10, 20, 30, 255]].repeat(2).concat(),
            }
        }

        #[test]
        fn text_and_a_picture_are_written_as_png_and_tiff_and_read_back() {
            let mut pb = Private::new();
            let before = pb.0.change_count();
            assert!(pb.0.write(Some("caption"), Some(&Picture::Bitmap(sample()))));
            assert_ne!(pb.0.change_count(), before);
            assert_eq!(pb.0.read_text().as_deref(), Some("caption"));
            assert!(pb.0.data(unsafe { NSPasteboardTypeTIFF }).is_some(), "TIFF for older apps");
            let png = pb.0.read_png().expect("PNG");
            assert_eq!(codec::decode(&png).map(|b| (b.width, b.height)), Some((3, 2)));
            let back = pb.0.read_bitmap().expect("bitmap");
            assert!(back.bgra.iter().zip(&sample().bgra).all(|(a, b)| a.abs_diff(*b) <= 1));
            let id = pb.0.picture_id().expect("a picture");
            assert!(pb.0.write(None, Some(&Picture::Png(png))));
            assert_eq!(pb.0.picture_id(), Some(id), "the same picture from a PNG");
            assert_eq!(pb.0.read_text(), None, "replaced, not added to");
        }

        #[test]
        fn a_picture_that_cannot_be_read_writes_nothing() {
            let mut pb = Private::new();
            assert!(pb.0.write(Some("kept"), None));
            let count = pb.0.change_count();
            assert!(!pb.0.write(None, Some(&Picture::Png(b"not a png".to_vec()))));
            assert_eq!(pb.0.change_count(), count);
            assert_eq!(pb.0.read_text().as_deref(), Some("kept"));
        }

        #[test]
        fn files_are_written_as_urls_and_read_back_as_paths() {
            let mut pb = Private::new();
            let dir = std::env::temp_dir().join(format!("rdpmac-pasteboard-files-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("folder")).unwrap();
            std::fs::write(dir.join("a file.txt"), b"x").unwrap();
            let paths = [dir.join("a file.txt"), dir.join("folder")];
            assert!(pb.0.write_files(&paths));
            let back: Vec<PathBuf> = pb.0.read_files().iter().map(|p| p.canonicalize().unwrap()).collect();
            let want: Vec<PathBuf> = paths.iter().map(|p| p.canonicalize().unwrap()).collect();
            assert_eq!(back, want);
            assert_eq!(pb.0.picture_id(), None);
            pb.0.clear();
            assert!(pb.0.read_files().is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn copied_files_offer_no_picture() {
            let pb = Private::new();
            let board = pb.0.board();
            board.clearContents();
            let url = NSString::from_str("file:///Applications/");
            board.setString_forType(&url, unsafe { NSPasteboardTypeFileURL });
            let tiff = codec::tiff(&sample()).expect("TIFF");
            board.setData_forType(Some(&NSData::with_bytes(&tiff)), unsafe { NSPasteboardTypeTIFF });
            assert_eq!(pb.0.picture_id(), None);
            assert_eq!(pb.0.read_png(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakePasteboard {
        count: isize,
        text: Option<String>,
        picture: Option<Picture>,
        files: Vec<PathBuf>,
    }

    impl FakePasteboard {
        fn copy(&mut self, text: Option<&str>, picture: Option<Picture>) {
            self.count += 1;
            self.text = text.map(str::to_owned);
            self.picture = picture;
            self.files.clear();
        }
    }

    fn bitmap(shade: u8) -> Bitmap {
        Bitmap {
            width: 1,
            height: 1,
            bgra: vec![shade, shade, shade, 255],
        }
    }

    impl Pasteboard for FakePasteboard {
        fn change_count(&self) -> isize {
            self.count
        }
        fn read_text(&self) -> Option<String> {
            self.text.clone()
        }
        fn picture_id(&self) -> Option<u64> {
            match self.picture.as_ref()? {
                Picture::Png(data) => Some(data.len() as u64),
                Picture::Bitmap(b) => Some(1000 + u64::from(b.bgra[0])),
            }
        }
        fn read_bitmap(&self) -> Option<Bitmap> {
            match self.picture.as_ref()? {
                Picture::Bitmap(b) => Some(b.clone()),
                Picture::Png(_) => None,
            }
        }
        fn read_png(&self) -> Option<Vec<u8>> {
            match self.picture.as_ref()? {
                Picture::Png(data) => Some(data.clone()),
                Picture::Bitmap(_) => None,
            }
        }
        fn write(&mut self, text: Option<&str>, picture: Option<&Picture>) -> bool {
            self.copy(text, picture.cloned());
            true
        }
        fn read_files(&self) -> Vec<PathBuf> {
            self.files.clone()
        }
        fn write_files(&mut self, paths: &[PathBuf]) -> bool {
            self.copy(None, None);
            self.files = paths.to_vec();
            true
        }
        fn clear(&mut self) {
            self.copy(None, None);
        }
    }

    fn offered(message: Option<ClipboardMessage>) -> Option<Vec<ClipboardFormatId>> {
        match message {
            Some(ClipboardMessage::SendInitiateCopy(formats)) => Some(formats.iter().map(|f| f.id).collect()),
            _ => None,
        }
    }

    fn asked(message: Option<ClipboardMessage>) -> Option<ClipboardFormatId> {
        match message {
            Some(ClipboardMessage::SendInitiatePaste(format)) => Some(format),
            _ => None,
        }
    }

    fn text_data(text: &str) -> Option<Vec<u8>> {
        Some(FormatDataResponse::new_unicode_string(text).data().to_vec())
    }

    fn remote_copy(sync: &mut Sync, pb: &mut FakePasteboard) -> Option<ClipboardMessage> {
        sync.handle(Command::RemoteCopy(vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]), pb)
    }

    #[test]
    fn mac_copy_is_offered_once() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        assert!(sync.poll(&pb).is_none());
        pb.copy(Some("hello"), None);
        assert_eq!(offered(sync.poll(&pb)), Some(vec![ClipboardFormatId::CF_UNICODETEXT]));
        assert!(sync.poll(&pb).is_none());
        // The same text copied again is not offered again.
        pb.copy(Some("hello"), None);
        assert!(sync.poll(&pb).is_none());
    }

    #[test]
    fn a_mac_picture_is_offered_as_dib_and_png_and_sent_in_either() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        pb.copy(None, Some(Picture::Bitmap(bitmap(7))));
        assert_eq!(offered(sync.poll(&pb)), Some(vec![ClipboardFormatId::CF_DIB, PNG_FORMAT]));
        match sync.handle(Command::Send(ClipboardFormatId::CF_DIB), &mut pb) {
            Some(ClipboardMessage::SendFormatData(data)) => {
                assert_eq!(clip_image::from_dib(data.data()), Some(bitmap(7)));
            }
            _ => panic!("expected a DIB"),
        }
        // No PNG on this pasteboard, and no text: errors, not empty data.
        for format in [PNG_FORMAT, ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_HDROP] {
            match sync.handle(Command::Send(format), &mut pb) {
                Some(ClipboardMessage::SendFormatData(data)) => assert!(data.is_error(), "{format:?}"),
                _ => panic!("expected an answer"),
            }
        }
        // Text beside a picture: all three formats.
        pb.copy(Some("caption"), Some(Picture::Bitmap(bitmap(8))));
        assert_eq!(offered(sync.poll(&pb)).map(|f| f.len()), Some(3));
    }

    #[test]
    fn client_text_lands_on_the_pasteboard_without_echo() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        assert_eq!(asked(remote_copy(&mut sync, &mut pb)), Some(ClipboardFormatId::CF_UNICODETEXT));
        assert!(sync.handle(Command::Received(text_data("from windows")), &mut pb).is_none());
        assert_eq!(pb.text.as_deref(), Some("from windows"));
        // Our own write is not offered back to the client.
        assert!(sync.poll(&pb).is_none());
        // Receiving the same text again does not rewrite the pasteboard.
        let count = pb.count;
        remote_copy(&mut sync, &mut pb);
        assert!(sync.handle(Command::Received(text_data("from windows")), &mut pb).is_none());
        assert_eq!(pb.count, count);
    }

    #[test]
    fn client_text_and_picture_are_fetched_in_turn_and_written_together() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        let offer = vec![
            ClipboardFormat::new(ClipboardFormatId::CF_DIB),
            ClipboardFormat::new(ClipboardFormatId(0xC123)).with_name(ClipboardFormatName::new_static("PNG")),
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            ClipboardFormat::new(ClipboardFormatId::CF_DIBV5),
        ];
        assert_eq!(asked(sync.handle(Command::RemoteCopy(offer), &mut pb)), Some(ClipboardFormatId::CF_UNICODETEXT));
        assert_eq!(asked(sync.handle(Command::Received(text_data("alt")), &mut pb)), Some(ClipboardFormatId(0xC123)));
        assert!(pb.text.is_none(), "nothing written before everything arrived");
        assert!(sync.handle(Command::Received(Some(vec![1, 2, 3])), &mut pb).is_none());
        assert_eq!(pb.text.as_deref(), Some("alt"));
        assert_eq!(pb.picture, Some(Picture::Png(vec![1, 2, 3])));
        assert!(sync.poll(&pb).is_none(), "not offered back");
    }

    #[test]
    fn a_dib_is_taken_when_there_is_no_png() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        let offer = vec![ClipboardFormat::new(ClipboardFormatId::CF_DIB), ClipboardFormat::new(ClipboardFormatId::CF_BITMAP)];
        assert_eq!(asked(sync.handle(Command::RemoteCopy(offer), &mut pb)), Some(ClipboardFormatId::CF_DIB));
        let dib = clip_image::to_dib(&bitmap(9));
        assert!(sync.handle(Command::Received(Some(dib)), &mut pb).is_none());
        assert_eq!(pb.picture, Some(Picture::Bitmap(bitmap(9))));
        // A picture the client cannot give leaves the pasteboard alone.
        let count = pb.count;
        let offer = vec![ClipboardFormat::new(ClipboardFormatId::CF_DIB)];
        sync.handle(Command::RemoteCopy(offer), &mut pb);
        assert!(sync.handle(Command::Received(None), &mut pb).is_none());
        assert_eq!(pb.count, count);
    }

    #[test]
    fn an_answer_to_an_earlier_copy_is_dropped() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        remote_copy(&mut sync, &mut pb);
        // The client copies again before answering; the late answer belongs to the first copy.
        assert_eq!(asked(remote_copy(&mut sync, &mut pb)), Some(ClipboardFormatId::CF_UNICODETEXT));
        assert!(sync.handle(Command::Received(text_data("old")), &mut pb).is_none());
        assert!(pb.text.is_none());
        assert!(sync.handle(Command::Received(text_data("new")), &mut pb).is_none());
        assert_eq!(pb.text.as_deref(), Some("new"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdpmac-clipboard-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn mac_files_are_offered_and_their_contents_served() {
        let dir = scratch("offer");
        std::fs::write(dir.join("notes.txt"), b"hello").unwrap();
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::with_cache(&pb, dir.join("cache"));
        let mut no_files = Sync::with_cache(&pb, dir.join("cache"));
        pb.copy(Some("notes.txt"), None);
        pb.files = vec![dir.join("notes.txt")];
        assert_eq!(
            offered(no_files.poll(&pb)),
            Some(vec![ClipboardFormatId::CF_UNICODETEXT]),
            "a client without file copy gets the names"
        );
        sync.handle(Command::Capabilities(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED), &mut pb);
        match sync.poll(&pb) {
            Some(ClipboardMessage::SendInitiateFileCopy(files)) => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].name, "notes.txt");
                assert_eq!(files[0].file_size, Some(5));
            }
            other => panic!("expected a file offer, got {other:?}"),
        }
        assert!(sync.poll(&pb).is_none());
        let request = FileContentsRequest {
            stream_id: 3,
            index: 0,
            flags: ironrdp_cliprdr::pdu::FileContentsFlags::RANGE,
            position: 1,
            requested_size: 3,
            data_id: None,
        };
        match sync.handle(Command::FileRequest(request), &mut pb) {
            Some(ClipboardMessage::SendFileContentsResponse(response)) => {
                assert_eq!((response.stream_id(), response.data()), (3, &b"ell"[..]));
            }
            other => panic!("expected file contents, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_files_are_fetched_then_put_on_the_pasteboard() {
        let dir = scratch("fetch");
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::with_cache(&pb, dir.join("cache"));
        pb.copy(Some("before"), None);
        let offer = vec![
            ClipboardFormat::new(ClipboardFormatId(0xC0FE)).with_name(ClipboardFormatName::FILE_LIST),
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
        ];
        assert_eq!(asked(sync.handle(Command::RemoteCopy(offer), &mut pb)), Some(ClipboardFormatId(0xC0FE)));
        let files = vec![FileDescriptor::new("hi.txt").with_file_size(2)];
        let request = match sync.handle(Command::RemoteFiles(files), &mut pb) {
            Some(ClipboardMessage::SendFileContentsRequest(r)) => r,
            other => panic!("expected a file request, got {other:?}"),
        };
        assert!(pb.text.is_none() && pb.files.is_empty(), "the old contents are gone meanwhile");
        assert!(sync.handle(Command::FileData(request.stream_id, Some(b"hi".to_vec())), &mut pb).is_none());
        assert_eq!(pb.files.len(), 1);
        assert_eq!(std::fs::read(&pb.files[0]).unwrap(), b"hi");
        assert!(sync.poll(&pb).is_none(), "not offered back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_copy_stops_fetching_files() {
        let dir = scratch("cancel");
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::with_cache(&pb, dir.join("cache"));
        let offer = vec![ClipboardFormat::new(ClipboardFormatId(0xC0FE)).with_name(ClipboardFormatName::FILE_LIST)];
        sync.handle(Command::RemoteCopy(offer), &mut pb);
        let files = vec![FileDescriptor::new("big.bin").with_file_size(10)];
        let Some(ClipboardMessage::SendFileContentsRequest(request)) = sync.handle(Command::RemoteFiles(files), &mut pb) else {
            panic!("expected a file request");
        };
        remote_copy(&mut sync, &mut pb);
        assert!(sync.handle(Command::FileData(request.stream_id, Some(vec![0; 10])), &mut pb).is_none());
        assert!(pb.files.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_are_answered() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        assert_eq!(offered(sync.handle(Command::Announce, &mut pb)), Some(vec![]));
        pb.copy(Some("x"), None);
        assert_eq!(offered(sync.handle(Command::Announce, &mut pb)).map(|f| f.len()), Some(1));
        match sync.handle(Command::Send(ClipboardFormatId::CF_UNICODETEXT), &mut pb) {
            Some(ClipboardMessage::SendFormatData(data)) => {
                assert_eq!(data.to_unicode_string().unwrap(), "x");
            }
            _ => panic!("expected format data"),
        }
    }
}
