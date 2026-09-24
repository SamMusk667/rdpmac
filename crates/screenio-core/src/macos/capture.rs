//! Display capture through ScreenCaptureKit (macOS 12.3 and later).
//!
//! An `SCStream` delivers `CMSampleBuffer`s on a private dispatch queue. The output object
//! copies each complete BGRA frame into a buffer that [`Capturer::frame`] hands out, so the
//! caller never touches CoreVideo memory and the C binding can keep a stable pointer.

use super::{display::pixel_size, session};
use crate::{Error, Frame, PixelFormat, Result};
use block2::RcBlock;
use core_graphics::display::CGDisplay;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    AnyThread, DefinedClass,
};
use objc2_core_foundation::CFArray;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use std::{
    sync::{mpsc, Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest output side accepted for scaled capture; RDP itself stops at 8192.
const MAX_OUTPUT_SIDE: u32 = 16384;
const MAX_FPS: i32 = 60;
const QUEUE_DEPTH: isize = 3;
/// SCStreamErrorUserDeclined: screen recording was not granted to this process.
const SC_ERROR_USER_DECLINED: isize = -3801;

struct Captured {
    data: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Default)]
struct Slot {
    latest: Option<Captured>,
    free: Vec<Vec<u8>>,
    stopped: Option<String>,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    ready: Condvar,
    described: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn publish(&self, sample: &CMSampleBuffer) {
        if frame_status(sample) != Some(SCFrameStatus::Complete.0) {
            return;
        }
        if !self.described.swap(true, std::sync::atomic::Ordering::Relaxed) {
            if let Some(info) = frame_info(sample) {
                log::debug!("first frame info: {}", info.description());
            }
        }
        let Some(pixels) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        if CVPixelBufferGetPixelFormatType(&pixels) != kCVPixelFormatType_32BGRA {
            return;
        }
        if unsafe { CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            return;
        }
        let width = CVPixelBufferGetWidth(&pixels);
        let height = CVPixelBufferGetHeight(&pixels);
        let stride = CVPixelBufferGetBytesPerRow(&pixels);
        let base = CVPixelBufferGetBaseAddress(&pixels) as *const u8;
        if !base.is_null() && width > 0 && height > 0 {
            let len = stride * height;
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            let mut data = slot.free.pop().unwrap_or_default();
            data.clear();
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(base, len) });
            // Latest wins: a consumer that fell behind gets the newest frame, not a backlog.
            if let Some(old) = slot.latest.take() {
                slot.free.push(old.data);
            }
            slot.latest = Some(Captured {
                data,
                width: width as u32,
                height: height as u32,
                stride: stride as u32,
            });
            self.ready.notify_one();
        }
        unsafe { CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) };
    }

    fn stop(&self, reason: String) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.stopped = Some(reason);
        self.ready.notify_all();
    }
}

/// The sample's frame info dictionary (status, content rect, scale factors, dirty rects).
fn frame_info(sample: &CMSampleBuffer) -> Option<Retained<NSDictionary<NSString, AnyObject>>> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    let array: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(&*attachments as *const CFArray as *const NSArray<_>) };
    array.firstObject()
}

/// Reads `SCStreamFrameInfoStatus` out of the sample's attachment dictionary.
fn frame_status(sample: &CMSampleBuffer) -> Option<isize> {
    let info = frame_info(sample)?;
    let status = info.objectForKey(unsafe { SCStreamFrameInfoStatus })?;
    Some(status.downcast_ref::<NSNumber>()?.integerValue())
}

struct OutputIvars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and StreamOutput has no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "ScreenioStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind.0 == SCStreamOutputType::Screen.0 {
                self.ivars().shared.publish(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().shared.stop(describe(error));
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { shared });
        unsafe { msg_send![super(this), init] }
    }
}

fn describe(error: &NSError) -> String {
    format!("{} ({})", error.localizedDescription(), error.code())
}

fn map_error(error: Option<&NSError>) -> Error {
    match error {
        Some(e) if e.code() == SC_ERROR_USER_DECLINED => Error::Permission,
        _ if !session::can_capture() => Error::Permission,
        _ => Error::Os,
    }
}

// SCShareableContent and SCDisplay are immutable snapshots that ScreenCaptureKit hands to an
// arbitrary queue; moving them to the caller's thread is what the framework expects.
struct SendRetained<T>(Retained<T>);
unsafe impl<T> Send for SendRetained<T> {}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel::<std::result::Result<SendRetained<SCShareableContent>, Error>>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = match unsafe { Retained::retain(content) } {
            Some(content) => Ok(SendRetained(content)),
            None => Err(map_error(unsafe { error.as_ref() })),
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &handler,
        );
    }
    match rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(Ok(content)) => Ok(content.0),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Timeout),
    }
}

fn start(stream: &SCStream) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Option<Error>>();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let failed = unsafe { error.as_ref() }.map(|e| map_error(Some(e)));
        let _ = tx.send(failed);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(e),
        Err(_) => Err(Error::Timeout),
    }
}

pub struct Capturer {
    stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    width: u32,
    height: u32,
    stride: u32,
    buf: Vec<u8>,
}

// SCStream is driven from its own queues; this handle only issues calls from whichever thread
// holds it, which is what `&mut self` already guarantees.
unsafe impl Send for Capturer {}

impl Capturer {
    pub fn open(display_id: u32) -> Result<Self> {
        Self::open_with(display_id, None)
    }

    /// Captures the display scaled to `width` x `height`. ScreenCaptureKit does the scaling on
    /// the GPU; when the aspect ratios differ the picture is letterboxed and centred.
    pub fn open_scaled(display_id: u32, width: u32, height: u32) -> Result<Self> {
        if !(1..=MAX_OUTPUT_SIDE).contains(&width) || !(1..=MAX_OUTPUT_SIDE).contains(&height) {
            return Err(Error::Invalid);
        }
        Self::open_with(display_id, Some((width, height)))
    }

    fn open_with(display_id: u32, output: Option<(u32, u32)>) -> Result<Self> {
        let content = shareable_content()?;
        let displays = unsafe { content.displays() };
        let display = displays
            .iter()
            .find(|d| unsafe { d.displayID() } == display_id)
            .ok_or(Error::Invalid)?;
        let (width, height) = output.unwrap_or_else(|| pixel_size(&CGDisplay::new(display_id)));

        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setMinimumFrameInterval(CMTime::new(1, MAX_FPS));
            config.setQueueDepth(QUEUE_DEPTH);
            // The cursor is reported separately through `cursor_shape`, as a remote desktop
            // protocol draws it on the client side.
            config.setShowsCursor(false);
            if output.is_some() {
                config.setScalesToFit(true);
                config.setPreservesAspectRatio(true);
            }
        }

        let shared = Arc::new(Shared::default());
        let output = StreamOutput::new(shared.clone());
        let queue = DispatchQueue::new("screenio.capture", DispatchQueueAttr::SERIAL);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|_| Error::Os)?;
        start(&stream)?;

        Ok(Self {
            stream,
            _output: output,
            _queue: queue,
            shared,
            width,
            height,
            stride: 0,
            buf: Vec::new(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn frame(&mut self, timeout: Duration) -> Result<Frame<'_>> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if slot.stopped.is_some() {
                return Err(Error::Reset);
            }
            if let Some(captured) = slot.latest.take() {
                let previous = std::mem::replace(&mut self.buf, captured.data);
                slot.free.push(previous);
                self.width = captured.width;
                self.height = captured.height;
                self.stride = captured.stride;
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout);
            }
            slot = self
                .shared
                .ready
                .wait_timeout(slot, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(Frame {
            data: &self.buf,
            width: self.width,
            height: self.height,
            stride: self.stride,
            format: PixelFormat::Bgra,
        })
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}
