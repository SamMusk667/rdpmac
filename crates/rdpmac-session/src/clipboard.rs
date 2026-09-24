//! Plain-text clipboard sharing between the Mac and the client (MS-RDPECLIP).
//!
//! Each connection gets a backend and a worker thread that owns the pasteboard. The worker polls
//! the pasteboard's change count and offers new text to the client; text the client copies is
//! fetched right away and written to the pasteboard. Text identical to what was last exchanged
//! is neither offered nor written again, so the two clipboards cannot bounce the same text back
//! and forth.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

const POLL: Duration = Duration::from_millis(500);
/// Larger text is not offered; RDP clients hold the whole transfer in memory.
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

type EventSender = Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn text_format() -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)
}

/// The pasteboard as the sync logic sees it.
pub trait Pasteboard {
    fn change_count(&self) -> isize;
    fn read_text(&self) -> Option<String>;
    fn write_text(&mut self, text: &str) -> bool;
}

/// Requests from the connection to the worker.
#[derive(Debug)]
enum Command {
    /// The client wants our format list.
    Announce,
    /// The client wants the text we announced.
    SendText,
    /// The client copied text; fetch it.
    FetchText,
    /// Text the client sent; put it on the pasteboard.
    Store(String),
}

/// What moves text between the pasteboard and the client; free of threads and I/O.
struct Sync {
    last_count: isize,
    /// Text last offered to or received from the client.
    last_text: Option<String>,
}

impl Sync {
    fn new(pasteboard: &impl Pasteboard) -> Self {
        Self {
            last_count: pasteboard.change_count(),
            last_text: None,
        }
    }

    fn usable(text: Option<String>) -> Option<String> {
        text.filter(|t| !t.is_empty() && t.len() <= MAX_TEXT_BYTES)
    }

    /// Called periodically: offers text copied on the Mac since the last look.
    fn poll(&mut self, pasteboard: &impl Pasteboard) -> Option<ClipboardMessage> {
        let count = pasteboard.change_count();
        if count == self.last_count {
            return None;
        }
        self.last_count = count;
        let text = Self::usable(pasteboard.read_text())?;
        if self.last_text.as_deref() == Some(text.as_str()) {
            return None;
        }
        info!(chars = text.chars().count(), "text copied on the Mac, offering it to the client");
        self.last_text = Some(text);
        Some(ClipboardMessage::SendInitiateCopy(vec![text_format()]))
    }

    fn handle(&mut self, command: Command, pasteboard: &mut impl Pasteboard) -> Option<ClipboardMessage> {
        match command {
            Command::Announce => {
                let text = Self::usable(pasteboard.read_text());
                let formats = if text.is_some() { vec![text_format()] } else { Vec::new() };
                self.last_text = text;
                Some(ClipboardMessage::SendInitiateCopy(formats))
            }
            Command::SendText => Some(ClipboardMessage::SendFormatData(
                match Self::usable(pasteboard.read_text()) {
                    Some(text) => {
                        debug!(chars = text.chars().count(), "sending text to the client");
                        FormatDataResponse::new_unicode_string(&text)
                    }
                    None => FormatDataResponse::new_error(),
                },
            )),
            Command::FetchText => Some(ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT)),
            Command::Store(text) => {
                if text.is_empty() || self.last_text.as_deref() == Some(text.as_str()) {
                    return None;
                }
                if pasteboard.read_text().as_deref() != Some(text.as_str()) {
                    let written = pasteboard.write_text(&text);
                    info!(chars = text.chars().count(), written, "client text copied to the Mac");
                }
                self.last_count = pasteboard.change_count();
                self.last_text = Some(text);
                None
            }
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
    events: EventSenderDebug,
}

/// `EventSender` has no `Debug`; the backend trait requires one.
struct EventSenderDebug(EventSender);

impl std::fmt::Debug for EventSenderDebug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EventSender")
    }
}

ironrdp_core::impl_as_any!(Backend);

impl Backend {
    fn spawn(events: EventSender) -> Self {
        let (commands, rx) = mpsc::channel();
        let worker_events = events.clone();
        if let Err(e) = thread::Builder::new()
            .name("rdpmac-clipboard".into())
            .spawn(move || worker(rx, worker_events))
        {
            warn!(%e, "clipboard worker could not start; clipboard sharing is off");
        }
        Self {
            commands,
            events: EventSenderDebug(events),
        }
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
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        self.command(Command::Announce);
    }

    fn on_request_format_list(&mut self) {
        self.command(Command::Announce);
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        if available_formats.iter().any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT) {
            self.command(Command::FetchText);
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        if request.format == ClipboardFormatId::CF_UNICODETEXT {
            self.command(Command::SendText);
        } else {
            debug!(format = ?request.format, "unsupported clipboard format requested");
            post(&self.events.0, ClipboardMessage::SendFormatData(FormatDataResponse::new_error()));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            debug!("client could not provide its clipboard text");
            return;
        }
        match response.to_unicode_string() {
            Ok(text) => self.command(Command::Store(text)),
            Err(e) => warn!(%e, "client clipboard text could not be decoded"),
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

fn worker(rx: Receiver<Command>, events: EventSender) {
    #[cfg(target_os = "macos")]
    let mut pasteboard = mac::MacPasteboard;
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
    fn write_text(&mut self, _text: &str) -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    use super::Pasteboard;

    /// The general pasteboard, looked up on every call so a restarted pasteboard server is fine.
    pub struct MacPasteboard;

    impl Pasteboard for MacPasteboard {
        fn change_count(&self) -> isize {
            NSPasteboard::generalPasteboard().changeCount()
        }

        fn read_text(&self) -> Option<String> {
            let text = NSPasteboard::generalPasteboard().stringForType(unsafe { NSPasteboardTypeString })?;
            Some(text.to_string())
        }

        fn write_text(&mut self, text: &str) -> bool {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            pasteboard.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
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
    }

    impl FakePasteboard {
        fn copy(&mut self, text: &str) {
            self.count += 1;
            self.text = Some(text.to_owned());
        }
    }

    impl Pasteboard for FakePasteboard {
        fn change_count(&self) -> isize {
            self.count
        }
        fn read_text(&self) -> Option<String> {
            self.text.clone()
        }
        fn write_text(&mut self, text: &str) -> bool {
            self.copy(text);
            true
        }
    }

    fn offered(message: Option<ClipboardMessage>) -> Option<usize> {
        match message {
            Some(ClipboardMessage::SendInitiateCopy(formats)) => Some(formats.len()),
            _ => None,
        }
    }

    #[test]
    fn mac_copy_is_offered_once() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        assert!(sync.poll(&pb).is_none());
        pb.copy("hello");
        assert_eq!(offered(sync.poll(&pb)), Some(1));
        assert!(sync.poll(&pb).is_none());
        // The same text copied again is not offered again.
        pb.copy("hello");
        assert!(sync.poll(&pb).is_none());
    }

    #[test]
    fn client_text_lands_on_the_pasteboard_without_echo() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        assert!(sync.handle(Command::Store("from windows".into()), &mut pb).is_none());
        assert_eq!(pb.text.as_deref(), Some("from windows"));
        // Our own write is not offered back to the client.
        assert!(sync.poll(&pb).is_none());
        // Receiving the same text again does not rewrite the pasteboard.
        let count = pb.count;
        assert!(sync.handle(Command::Store("from windows".into()), &mut pb).is_none());
        assert_eq!(pb.count, count);
    }

    #[test]
    fn requests_are_answered() {
        let mut pb = FakePasteboard::default();
        let mut sync = Sync::new(&pb);
        assert_eq!(offered(sync.handle(Command::Announce, &mut pb)), Some(0));
        pb.copy("x");
        assert_eq!(offered(sync.handle(Command::Announce, &mut pb)), Some(1));
        match sync.handle(Command::SendText, &mut pb) {
            Some(ClipboardMessage::SendFormatData(data)) => {
                assert_eq!(data.to_unicode_string().unwrap(), "x");
            }
            _ => panic!("expected format data"),
        }
        assert!(matches!(
            sync.handle(Command::FetchText, &mut pb),
            Some(ClipboardMessage::SendInitiatePaste(_))
        ));
    }
}
