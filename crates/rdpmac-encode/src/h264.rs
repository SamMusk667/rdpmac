//! H.264 encoding with VideoToolbox for the graphics pipeline's AVC420 and AVC444 codecs.
//!
//! One encoder serves one session size. Frames go in as BGRA and come out as H.264 access units
//! in Annex B form (start codes), with SPS and PPS in front of every key frame so a client can
//! start or recover at any key frame. Encoding is synchronous: each call flushes the session, so
//! a frame's bitstream is ready when [`H264Encoder::encode`] returns. The session is configured
//! for real-time use without frame reordering, which RDP requires.
//!
//! Where the hardware supports it, the session runs in VideoToolbox's low-latency mode and takes
//! a quantiser with every frame from [`QpControl`]; elsewhere VideoToolbox's own rate control
//! runs. Either way [`H264Encoder::refine`] sharpens a picture that has stopped changing.
//!
//! An AVC444 encoder encodes every picture twice, as the main view (the picture in 4:2:0) and the
//! auxiliary view (the chroma the main view leaves out, see [`avc444`]), one after the other in
//! one stream, and the client combines them. The hardware predicts a frame from the frame before
//! it, which for either view is the other view and looks nothing like it. Instead every view is
//! made a long-term reference, and the next view of its kind acknowledges it and refreshes from
//! it, so each view costs what changed since the last one of its kind.

use std::ffi::{c_int, c_void};
use std::fmt;
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, CVPixelBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_EnableLTR, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_ReferenceBufferCount,
    kVTCompressionPropertyKey_SupportsBaseFrameQP, kVTEncodeFrameOptionKey_AcknowledgedLTRTokens,
    kVTEncodeFrameOptionKey_BaseFrameQP, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTEncodeFrameOptionKey_ForceLTRRefresh, kVTProfileLevel_H264_Main_5_0, kVTProfileLevel_H264_Main_AutoLevel,
    kVTSampleAttachmentKey_RequireLTRAcknowledgementToken,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl, VTCompressionSession, VTEncodeInfoFlags,
    VTSessionCopyProperty, VTSessionSetProperty,
};

use crate::avc444;
use crate::color::{Converter, Converter444, Nv12Planes};
use crate::quantiser::QpControl;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// NAL unit header of an access unit delimiter.
const ACCESS_UNIT_DELIMITER: u8 = 0x09;
/// Target bits per pixel per frame while the picture moves.
const BITS_PER_PIXEL: f64 = 0.2;
const MIN_BITRATE: f64 = 2_000_000.0;
const MAX_BITRATE: f64 = 60_000_000.0;
/// Seconds between forced key frames, so a client that lost a frame recovers on its own.
const KEY_FRAME_SECONDS: u32 = 10;
/// How long the picture has to stay unchanged before refinement starts.
const REFINE_AFTER: Duration = Duration::from_millis(200);
/// Reference frames the low-latency session may keep. Left alone it declares twelve, the
/// picture buffer mstsc disconnected on at 1920x1200; limited to one it sends only key frames.
const LOW_LATENCY_REFERENCES: i32 = 2;
/// Reference frames of an AVC444 session. Two chains of long-term references needed four on
/// macOS 26; macOS 27 keeps one fewer, so four leave every third frame without a token and the
/// view after it without a reference, and five are needed. VideoToolbox never declares more
/// than the level allows, see [`LEVEL_5_0_MACROBLOCKS`].
const LTR_REFERENCES: i32 = 5;
/// Pictures up to this many macroblocks fit level 5.0, whose picture buffer holds five of them
/// even at 4096x2160. AVC444 sessions of such sizes ask for it: left to choose, VideoToolbox takes
/// level 4.0 for 1920x1080 and 1680x1050, holds four, and cuts the references to four.
const LEVEL_5_0_MACROBLOCKS: u32 = 22_080;
/// Without quantiser control, refinement re-encodes the picture this many times, each with the
/// bitrate raised by `REFINE_BOOST`; a larger boost measured the same.
const BOOSTED_REFINEMENTS: u32 = 3;
const REFINE_BOOST: u32 = 4;

#[derive(Debug)]
pub struct EncodeError(String);

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EncodeError {}

fn fail(what: &str, status: i32) -> EncodeError {
    EncodeError(format!("{what} failed with status {status}"))
}

/// One encoded frame.
pub struct EncodedFrame {
    /// Annex B byte stream: start code, NAL unit, start code, NAL unit... For AVC444, the main
    /// view's.
    pub data: Vec<u8>,
    /// AVC444's auxiliary view, in the same form. `None` from an AVC420 encoder, and on the rare
    /// frame whose auxiliary view VideoToolbox dropped, which a client then shows in 4:2:0.
    pub auxiliary: Option<Vec<u8>>,
    pub key_frame: bool,
    /// The base quantiser, when the encoder chose it.
    pub qp: Option<i32>,
}

impl EncodedFrame {
    /// The size of the bitstreams.
    pub fn bytes(&self) -> usize {
        self.data.len() + self.auxiliary.as_ref().map_or(0, Vec::len)
    }
}

/// The two views of a picture in AVC444 (MS-RDPEGFX 3.3.8.3.3), each a frame of the stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    Auxiliary,
}

/// Where the output callback leaves its result for the thread that called `encode`.
#[derive(Default)]
struct Sink {
    output: Option<EncodedFrame>,
    /// The long-term reference token VideoToolbox gave the output.
    token: Option<i64>,
    error: Option<i32>,
}

/// What an AVC444 encoder keeps besides an AVC420 one.
struct Avc444 {
    converter: Converter444,
    /// The auxiliary view of the encoder's newest picture, whose main view is `latest`.
    auxiliary: Option<CFRetained<CVPixelBuffer>>,
    /// The long-term reference tokens of the last main and auxiliary views.
    main_token: Option<i64>,
    auxiliary_token: Option<i64>,
    /// Frames encoded so far, which are the presentation times, `fps` a second. VideoToolbox
    /// drops a long-term reference still unacknowledged five frame intervals after it, as it
    /// reckons them from presentation times, and in real time a refinement comes that long after
    /// the frame before.
    frames: i64,
    fps: i32,
    /// Threads the colour conversion may use.
    threads: usize,
}

impl Avc444 {
    fn new(fps: i32, threads: usize) -> Result<Self, EncodeError> {
        let converter = Converter444::new().ok_or_else(|| EncodeError("vImage has no BT.709 4:4:4 conversion".into()))?;
        Ok(Self {
            converter,
            auxiliary: None,
            main_token: None,
            auxiliary_token: None,
            frames: 0,
            fps,
            threads,
        })
    }

    fn next_time(&mut self) -> CMTime {
        self.frames += 1;
        // SAFETY: CMTimeMake only builds a value.
        unsafe { CMTime::new(self.frames, self.fps) }
    }

    /// Converts a frame into its main and auxiliary views, a band of rows at a time, so that the
    /// 4:4:4 chroma never needs planes of the full size.
    ///
    /// # Safety
    /// `main` and `auxiliary` must describe writable NV12 pictures of `size`.
    unsafe fn convert(&mut self, bgra: &[u8], stride: usize, size: (usize, usize), main: &Nv12Planes, auxiliary: &Nv12Planes) -> bool {
        self.converter.convert(bgra, stride, size, (main.y, main.y_stride), self.threads, |top, band| {
            // Bands start on even rows, so a band's chroma rows start at half its first row.
            avc444::write_main_chroma(band, main.cbcr.add(top / 2 * main.cbcr_stride), main.cbcr_stride);
            let auxiliary = Nv12Planes {
                y: auxiliary.y.add(top * auxiliary.y_stride),
                y_stride: auxiliary.y_stride,
                cbcr: auxiliary.cbcr.add(top / 2 * auxiliary.cbcr_stride),
                cbcr_stride: auxiliary.cbcr_stride,
            };
            avc444::write_aux(band, &auxiliary);
        })
    }

    fn token(&mut self, view: View) -> &mut Option<i64> {
        match view {
            View::Main => &mut self.main_token,
            View::Auxiliary => &mut self.auxiliary_token,
        }
    }
}

/// Who decides how coarsely a frame is quantised.
enum Control {
    /// We do, for every frame, in VideoToolbox's low-latency mode.
    Quantiser(QpControl),
    /// VideoToolbox's rate control does, aiming at the average bitrate. `refinements` counts
    /// the boosted re-encodes of the current picture.
    Bitrate { refinements: u32, refined_at: Instant },
}

pub struct H264Encoder {
    session: CFRetained<VTCompressionSession>,
    control: Control,
    target_bitrate: u32,
    bitrate: u32,
    // Boxed so its address stays stable; the session's callback reads it through a raw pointer.
    sink: Box<Mutex<Sink>>,
    width: u32,
    height: u32,
    key_frame_requested: bool,
    last_key_frame: Instant,
    /// Presentation times are real time since this instant, so VideoToolbox's rate control
    /// gives a frame that follows a still second a second's worth of bits.
    started: Instant,
    converter: Converter,
    /// The newest picture, kept for refinement, and whether the client still lacks it.
    latest: Option<CFRetained<CVPixelBuffer>>,
    unsent: bool,
    changed_at: Instant,
    avc444: Option<Box<Avc444>>,
    /// Set while an AVC444 encoder makes main views only, which clients take as AVC420 frames.
    main_only: bool,
}

// The session is only driven from the thread that owns the encoder; VideoToolbox calls the
// output callback on its own threads, which only touch the mutex-protected sink.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self, EncodeError> {
        Self::open(width, height, fps, None)
    }

    /// An encoder for AVC444v2, whose frames carry both views of the picture, converting colours
    /// on up to `threads` threads. It needs a size that [`avc444::fits`] and the low-latency
    /// session with long-term references; where either is missing, AVC420 is what is left.
    pub fn new_avc444(width: u32, height: u32, fps: u32, threads: usize) -> Result<Self, EncodeError> {
        if !avc444::fits(width, height) {
            return Err(EncodeError(format!("{width}x{height} does not fit the AVC444 layout")));
        }
        Self::open(width, height, fps, Some(threads))
    }

    /// `avc444` holds the conversion threads of an AVC444 encoder.
    fn open(width: u32, height: u32, fps: u32, avc444: Option<usize>) -> Result<Self, EncodeError> {
        if width == 0 || height == 0 {
            return Err(EncodeError("empty frame size".into()));
        }
        let fps = fps.clamp(1, 120) as i32;
        let sink = Box::new(Mutex::new(Sink::default()));
        let converter = Converter::new().ok_or_else(|| EncodeError("vImage has no BT.709 conversion".into()))?;
        let avc444 = match avc444 {
            Some(threads) => Some(Box::new(Avc444::new(fps, threads)?)),
            None => None,
        };
        let bitrate = target_bitrate(width, height, fps as u32);
        let now = Instant::now();

        let (session, control) = match quantiser_session(width, height, &sink, avc444.is_some()) {
            Some(session) => (session, Control::Quantiser(QpControl::new(bitrate, now))),
            None if avc444.is_some() => {
                return Err(EncodeError("no low-latency encoder with long-term references for AVC444".into()))
            }
            None => {
                let session = open_session(width, height, false, &sink)?;
                let control = Control::Bitrate {
                    refinements: 0,
                    refined_at: now,
                };
                (session, control)
            }
        };

        let yes = CFBoolean::new(true);
        let no = CFBoolean::new(false);
        let rate = CFNumber::new_i32(bitrate.min(i32::MAX as u32) as i32);
        let expected_fps = CFNumber::new_i32(fps);
        let key_interval = CFNumber::new_i32(fps * KEY_FRAME_SECONDS as i32);
        unsafe {
            set(&session, kVTCompressionPropertyKey_RealTime, yes.as_ref())?;
            set(&session, kVTCompressionPropertyKey_AllowFrameReordering, no.as_ref())?;
            let macroblocks = width.div_ceil(16) * height.div_ceil(16);
            let level = if avc444.is_some() && macroblocks <= LEVEL_5_0_MACROBLOCKS {
                kVTProfileLevel_H264_Main_5_0
            } else {
                kVTProfileLevel_H264_Main_AutoLevel
            };
            set(&session, kVTCompressionPropertyKey_ProfileLevel, level.as_ref())?;
            set(&session, kVTCompressionPropertyKey_AverageBitRate, rate.as_ref())?;
            set(&session, kVTCompressionPropertyKey_ExpectedFrameRate, expected_fps.as_ref())?;
            set(&session, kVTCompressionPropertyKey_MaxKeyFrameInterval, key_interval.as_ref())?;
            let status = session.prepare_to_encode_frames();
            if status != 0 {
                return Err(fail("VTCompressionSessionPrepareToEncodeFrames", status));
            }
        }
        Ok(Self {
            session,
            control,
            target_bitrate: bitrate,
            bitrate,
            sink,
            width,
            height,
            key_frame_requested: true,
            last_key_frame: now,
            started: now,
            converter,
            latest: None,
            unsent: false,
            changed_at: now,
            avc444,
            main_only: false,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether the encoder makes AVC444's views rather than AVC420 frames.
    pub fn avc444(&self) -> bool {
        self.avc444.is_some()
    }

    /// Whether an AVC444 encoder makes main views only, to be sent as AVC420 frames.
    pub fn main_only(&self) -> bool {
        self.main_only
    }

    /// Makes an AVC444 encoder encode main views only, or both views again. Either way the
    /// next frame is a key frame, since the client decodes the two kinds of stream separately.
    pub fn set_main_only(&mut self, main_only: bool) {
        if self.avc444.is_some() && self.main_only != main_only {
            self.main_only = main_only;
            self.key_frame_requested = true;
        }
    }

    /// The bitrate chosen for this size and frame rate, in bits per second.
    pub fn target_bitrate(&self) -> u32 {
        self.target_bitrate
    }

    /// Whether the encoder chooses every frame's quantiser, which makes refinement exact.
    pub fn controls_quantiser(&self) -> bool {
        matches!(self.control, Control::Quantiser(_))
    }

    /// Changes the average bitrate; takes effect from the next frame.
    pub fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError> {
        match &mut self.control {
            Control::Quantiser(control) => control.set_bitrate(bits_per_second),
            Control::Bitrate { .. } => self.apply_bitrate(bits_per_second)?,
        }
        self.bitrate = bits_per_second;
        Ok(())
    }

    fn apply_bitrate(&self, bits_per_second: u32) -> Result<(), EncodeError> {
        let rate = CFNumber::new_i32(bits_per_second.min(i32::MAX as u32) as i32);
        unsafe { set(&self.session, kVTCompressionPropertyKey_AverageBitRate, rate.as_ref()) }
    }

    /// Makes the next encoded frame a key frame, for a client that joins or lost a frame.
    pub fn request_key_frame(&mut self) {
        self.key_frame_requested = true;
    }

    /// Sends the newest picture again as a key frame, for a client that asked for the whole
    /// picture: from [`H264Encoder::refine`] while the screen is still, else with the next frame.
    pub fn resend(&mut self) {
        self.key_frame_requested = true;
        self.unsent = self.latest.is_some();
    }

    /// Encodes one BGRA frame of the encoder's size. `Ok(None)` when VideoToolbox dropped the
    /// frame, which it may do under load.
    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Option<EncodedFrame>, EncodeError> {
        let pixels = self.pixel_buffer(bgra, stride)?;
        self.encode_changed(pixels)
    }

    /// Keeps a frame the client is too far behind to take now; [`H264Encoder::refine`] sends it
    /// once the screen is still, unless a newer frame replaces it first.
    pub fn stage(&mut self, bgra: &[u8], stride: usize) -> Result<(), EncodeError> {
        self.latest = Some(self.pixel_buffer(bgra, stride)?);
        self.unsent = true;
        Ok(())
    }

    /// Called while the screen is not changing: sends a picture the client has not had yet,
    /// or encodes the current one again, sharper, once it has been still for a moment.
    /// `Ok(None)` when there is nothing to do yet.
    pub fn refine(&mut self) -> Result<Option<EncodedFrame>, EncodeError> {
        let Some(pixels) = self.latest.clone() else {
            return Ok(None);
        };
        if self.unsent {
            return self.encode_changed(pixels);
        }
        let now = Instant::now();
        if now.duration_since(self.changed_at) < REFINE_AFTER {
            return Ok(None);
        }
        match &mut self.control {
            Control::Quantiser(control) => match control.refinement(now) {
                Some(qp) => self.submit(&pixels, Some(qp)),
                None => Ok(None),
            },
            Control::Bitrate {
                refinements,
                refined_at,
            } => {
                if *refinements >= BOOSTED_REFINEMENTS || now.duration_since(*refined_at) < REFINE_AFTER {
                    return Ok(None);
                }
                *refinements += 1;
                *refined_at = now;
                self.apply_bitrate(self.bitrate.saturating_mul(REFINE_BOOST))?;
                let encoded = self.submit(&pixels, None);
                self.apply_bitrate(self.bitrate)?;
                encoded
            }
        }
    }

    fn encode_changed(&mut self, pixels: CFRetained<CVPixelBuffer>) -> Result<Option<EncodedFrame>, EncodeError> {
        let now = Instant::now();
        let qp = match &mut self.control {
            Control::Quantiser(control) => {
                let Some(qp) = control.changed(now) else {
                    // Over the bitrate: hold the picture until there is room to send it.
                    self.latest = Some(pixels);
                    self.unsent = true;
                    return Ok(None);
                };
                // Low-latency mode never inserts key frames of its own.
                if now.duration_since(self.last_key_frame) >= Duration::from_secs(KEY_FRAME_SECONDS.into()) {
                    self.key_frame_requested = true;
                }
                Some(qp)
            }
            Control::Bitrate { refinements, .. } => {
                *refinements = 0;
                None
            }
        };
        let encoded = self.submit(&pixels, qp);
        self.unsent = !matches!(encoded, Ok(Some(_)));
        self.latest = Some(pixels);
        self.changed_at = now;
        encoded
    }

    /// Encodes one picture, with `qp` as its base quantiser in low-latency mode; for AVC444,
    /// both of its views.
    fn submit(&mut self, pixels: &CVPixelBuffer, qp: Option<i32>) -> Result<Option<EncodedFrame>, EncodeError> {
        let Some(mut frame) = self.encode_view(pixels, qp, View::Main)? else {
            return Ok(None);
        };
        if let Some(auxiliary) = self.avc444.as_ref().filter(|_| !self.main_only).and_then(|a| a.auxiliary.clone()) {
            frame.auxiliary = self.encode_view(&auxiliary, qp, View::Auxiliary)?.map(|view| view.data);
        }
        let now = Instant::now();
        if frame.key_frame {
            self.key_frame_requested = false;
            self.last_key_frame = now;
        }
        if let (Control::Quantiser(control), Some(qp)) = (&mut self.control, qp) {
            control.record(qp, frame.bytes(), now);
        }
        frame.qp = qp;
        Ok(Some(frame))
    }

    /// Encodes one frame of the stream: a picture, or one of an AVC444 picture's views, which
    /// refreshes from the last view of its kind.
    fn encode_view(&mut self, pixels: &CVPixelBuffer, qp: Option<i32>, view: View) -> Result<Option<EncodedFrame>, EncodeError> {
        let force_key = self.key_frame_requested && view == View::Main;
        let reference = self.avc444.as_mut().and_then(|a| *a.token(view)).filter(|_| !force_key);
        let yes = CFBoolean::new(true);
        let quantiser = qp.map(CFNumber::new_i32);
        let acknowledged = reference.map(|token| CFArray::from_retained_objects(&[CFNumber::new_i64(token)]));
        let mut keys: Vec<&CFString> = Vec::with_capacity(4);
        let mut values: Vec<&CFType> = Vec::with_capacity(4);
        if force_key {
            keys.push(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame });
            values.push(yes.as_ref());
        }
        if let Some(quantiser) = &quantiser {
            keys.push(unsafe { kVTEncodeFrameOptionKey_BaseFrameQP });
            values.push(quantiser.as_ref());
        }
        if let Some(acknowledged) = &acknowledged {
            // Acknowledged just before it is needed, the reference is the newest acknowledged
            // one, which is what a refresh predicts from.
            keys.push(unsafe { kVTEncodeFrameOptionKey_AcknowledgedLTRTokens });
            values.push(acknowledged.as_ref());
            keys.push(unsafe { kVTEncodeFrameOptionKey_ForceLTRRefresh });
            values.push(yes.as_ref());
        }
        let options = (!keys.is_empty()).then(|| CFDictionary::<CFString, CFType>::from_slices(&keys, &values));
        let time = match self.avc444.as_mut() {
            Some(avc444) => avc444.next_time(),
            None => unsafe { CMTime::new(self.started.elapsed().as_micros() as i64, 1_000_000) },
        };
        let status = unsafe {
            self.session.encode_frame(
                pixels,
                time,
                kCMTimeInvalid,
                options.as_deref().map(|o| o.as_opaque()),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(fail("VTCompressionSessionEncodeFrame", status));
        }
        let status = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        if status != 0 {
            return Err(fail("VTCompressionSessionCompleteFrames", status));
        }
        let (output, token) = {
            let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(status) = sink.error.take() {
                return Err(fail("encoding", status));
            }
            (sink.output.take(), sink.token.take())
        };
        if let (Some(frame), Some(avc444)) = (&output, self.avc444.as_mut()) {
            // A key frame leaves nothing to refresh from.
            if frame.key_frame {
                avc444.main_token = None;
                avc444.auxiliary_token = None;
            }
            *avc444.token(view) = token;
        }
        Ok(output)
    }

    /// Converts the frame into a full-range BT.709 buffer from the session's pool, which
    /// VideoToolbox reads without a further copy. For AVC444 that is the picture's main view,
    /// and its auxiliary view is kept beside it.
    fn pixel_buffer(&mut self, bgra: &[u8], stride: usize) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        let (width, height) = (self.width as usize, self.height as usize);
        if stride < width * 4 || bgra.len() < stride * (height - 1) + width * 4 {
            return Err(EncodeError("frame smaller than the encoder size".into()));
        }
        let main = Locked::new(&self.session, width)?;
        let converted = match self.avc444.as_deref_mut() {
            Some(avc444) => {
                let auxiliary = Locked::new(&self.session, width)?;
                let converted = unsafe { avc444.convert(bgra, stride, (width, height), &main.planes, &auxiliary.planes) };
                // A failed conversion keeps the last picture's views together.
                if converted {
                    avc444.auxiliary = Some(auxiliary.into_buffer());
                }
                converted
            }
            None => unsafe { self.converter.convert(bgra, stride, width, height, &main.planes) },
        };
        if !converted {
            return Err(EncodeError("converting the frame to YUV failed".into()));
        }
        Ok(main.into_buffer())
    }
}

/// A buffer from the session's pool, locked for writing until it is dropped.
struct Locked {
    buffer: CFRetained<CVPixelBuffer>,
    planes: Nv12Planes,
}

impl Locked {
    /// A buffer with planes wide enough for `width` samples.
    fn new(session: &VTCompressionSession, width: usize) -> Result<Self, EncodeError> {
        let pool = unsafe { session.pixel_buffer_pool() }
            .ok_or_else(|| EncodeError("the compression session has no pixel buffer pool".into()))?;
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        let status = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            return Err(fail("CVPixelBufferPoolCreatePixelBuffer", status));
        };
        let buffer = unsafe { CFRetained::from_raw(raw) };
        let status = unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
        if status != 0 {
            return Err(fail("CVPixelBufferLockBaseAddress", status));
        }
        let planes = Nv12Planes {
            y: CVPixelBufferGetBaseAddressOfPlane(&buffer, 0) as *mut u8,
            y_stride: CVPixelBufferGetBytesPerRowOfPlane(&buffer, 0),
            cbcr: CVPixelBufferGetBaseAddressOfPlane(&buffer, 1) as *mut u8,
            cbcr_stride: CVPixelBufferGetBytesPerRowOfPlane(&buffer, 1),
        };
        // From here on dropping it unlocks the buffer.
        let locked = Self { buffer, planes };
        let usable = !locked.planes.y.is_null()
            && !locked.planes.cbcr.is_null()
            && locked.planes.y_stride >= width
            && locked.planes.cbcr_stride >= width.div_ceil(2) * 2;
        if !usable {
            return Err(EncodeError("the pixel buffer is smaller than the encoder size".into()));
        }
        Ok(locked)
    }

    /// The buffer, unlocked for VideoToolbox to read.
    fn into_buffer(self) -> CFRetained<CVPixelBuffer> {
        self.buffer.clone()
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        unsafe { CVPixelBufferUnlockBaseAddress(&self.buffer, CVPixelBufferLockFlags::empty()) };
    }
}

/// A hardware H.264 session taking full-range 4:2:0 frames, which we convert ourselves because
/// VideoToolbox would turn BGRA into limited range. `low_latency` asks for the mode that accepts
/// a quantiser with every frame.
fn open_session(
    width: u32,
    height: u32,
    low_latency: bool,
    sink: &Mutex<Sink>,
) -> Result<CFRetained<VTCompressionSession>, EncodeError> {
    let yes = CFBoolean::new(true);
    let spec = unsafe {
        if low_latency {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                ],
                &[yes.as_ref(), yes.as_ref()],
            )
        } else {
            CFDictionary::<CFString, CFType>::from_slices(
                &[kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder],
                &[yes.as_ref()],
            )
        }
    };
    let format = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange as i32);
    let w = CFNumber::new_i32(width as i32);
    let h = CFNumber::new_i32(height as i32);
    let iosurface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
    let attributes = unsafe {
        CFDictionary::<CFString, CFType>::from_slices(
            &[
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            ],
            &[format.as_ref(), w.as_ref(), h.as_ref(), iosurface.as_ref()],
        )
    };
    let mut raw: *mut VTCompressionSession = ptr::null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            None,
            width as i32,
            height as i32,
            kCMVideoCodecType_H264,
            Some(spec.as_opaque()),
            Some(attributes.as_opaque()),
            None,
            Some(on_encoded),
            sink as *const Mutex<Sink> as *mut c_void,
            NonNull::from(&mut raw),
        )
    };
    let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
        return Err(fail("VTCompressionSessionCreate", status));
    };
    Ok(unsafe { CFRetained::from_raw(raw) })
}

/// A low-latency session that takes a base quantiser with every frame, and with `ltr` makes
/// long-term references, or `None` where the hardware offers no such session.
fn quantiser_session(width: u32, height: u32, sink: &Mutex<Sink>, ltr: bool) -> Option<CFRetained<VTCompressionSession>> {
    let session = open_session(width, height, true, sink).ok()?;
    let references = CFNumber::new_i32(if ltr { LTR_REFERENCES } else { LOW_LATENCY_REFERENCES });
    let yes = CFBoolean::new(true);
    let usable = unsafe {
        supports_base_qp(&session)
            && set(&session, kVTCompressionPropertyKey_ReferenceBufferCount, references.as_ref()).is_ok()
            && (!ltr || set(&session, kVTCompressionPropertyKey_EnableLTR, yes.as_ref()).is_ok())
    };
    if !usable {
        unsafe { session.invalidate() };
        return None;
    }
    Some(session)
}

/// Whether the session takes a base quantiser with every frame.
unsafe fn supports_base_qp(session: &VTCompressionSession) -> bool {
    let mut value: *const CFType = ptr::null();
    let status = VTSessionCopyProperty(
        session,
        kVTCompressionPropertyKey_SupportsBaseFrameQP,
        None,
        &mut value as *mut *const CFType as *mut c_void,
    );
    let Some(value) = NonNull::new(value as *mut CFType).filter(|_| status == 0) else {
        return false;
    };
    let value = CFRetained::from_raw(value);
    value.downcast_ref::<CFBoolean>().is_some_and(|b| b.as_bool())
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

/// Threads for AVC444's colour conversion when it may use several. At 4K on the M4, 4
/// performance and 6 efficiency cores, six cut the conversion from about 15 to 8.5 ms a frame;
/// more measured no faster.
pub fn conversion_threads() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get).min(6)
}

/// Bits per second for a session size and frame rate.
pub fn target_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    (f64::from(width) * f64::from(height) * f64::from(fps.max(1)) * BITS_PER_PIXEL).clamp(MIN_BITRATE, MAX_BITRATE) as u32
}

unsafe fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) -> Result<(), EncodeError> {
    let status = VTSessionSetProperty(session, key, Some(value));
    if status != 0 {
        return Err(EncodeError(format!("setting {key} failed with status {status}")));
    }
    Ok(())
}

unsafe extern "C-unwind" fn on_encoded(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let Some(sink) = (refcon as *const Mutex<Sink>).as_ref() else {
        return;
    };
    let mut sink = sink.lock().unwrap_or_else(|e| e.into_inner());
    if status != 0 {
        sink.error = Some(status);
        return;
    }
    if flags.contains(VTEncodeInfoFlags::FrameDropped) {
        return;
    }
    let Some(sample) = sample.as_ref() else {
        return;
    };
    match annex_b(sample) {
        Some(frame) => {
            sink.output = Some(frame);
            sink.token = ltr_token(sample);
        }
        None => sink.error = Some(-1),
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
}

/// A sample is a key frame unless its attachments mark it as not a sync sample.
fn is_key_frame(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    unsafe {
        let array = &*attachments as *const _ as *const c_void;
        if CFArrayGetCount(array) < 1 {
            return true;
        }
        let dict = CFArrayGetValueAtIndex(array, 0);
        let key = kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void;
        let not_sync = CFDictionaryGetValue(dict, key);
        not_sync.is_null() || CFBooleanGetValue(not_sync) == 0
    }
}

/// The token VideoToolbox wants acknowledged once the client has the frame, which makes the
/// frame a long-term reference to refresh from.
fn ltr_token(sample: &CMSampleBuffer) -> Option<i64> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    unsafe {
        let array = &*attachments as *const _ as *const c_void;
        if CFArrayGetCount(array) < 1 {
            return None;
        }
        let dict = CFArrayGetValueAtIndex(array, 0);
        let key = kVTSampleAttachmentKey_RequireLTRAcknowledgementToken as *const CFString as *const c_void;
        let token = CFDictionaryGetValue(dict, key) as *const CFType;
        token.as_ref()?.downcast_ref::<CFNumber>()?.as_i64()
    }
}

/// The sample's bitstream converted from length-prefixed NAL units to Annex B, with the
/// parameter sets in front when it is a key frame; the SPS gets our VUI (see `sps`).
fn annex_b(sample: &CMSampleBuffer) -> Option<EncodedFrame> {
    let key_frame = is_key_frame(sample);
    let description = unsafe { sample.format_description() }?;
    let mut count = 0usize;
    let mut length_size: c_int = 4;
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            &description,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            &mut length_size,
        )
    };
    if status != 0 {
        return None;
    }
    let block = unsafe { sample.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    let status = unsafe { block.copy_data_bytes(0, len, NonNull::new(avcc.as_mut_ptr().cast())?) };
    if status != 0 {
        return None;
    }

    let mut data = Vec::with_capacity(len + 128);
    // An access unit delimiter marks where each picture starts. H.264 leaves it optional; it is
    // here for decoders that go by it (see docs/refresh.md). Its payload says which slice types
    // follow: I only, or I and P.
    data.extend_from_slice(&START_CODE);
    data.extend_from_slice(&[ACCESS_UNIT_DELIMITER, if key_frame { 0x10 } else { 0x30 }]);
    if key_frame {
        for index in 0..count {
            let mut set: *const u8 = ptr::null();
            let mut size = 0usize;
            let status = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &description,
                    index,
                    &mut set,
                    &mut size,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if status != 0 || set.is_null() {
                return None;
            }
            let set = unsafe { std::slice::from_raw_parts(set, size) };
            data.extend_from_slice(&START_CODE);
            match crate::sps::with_vui(set) {
                Some(sps) => data.extend_from_slice(&sps),
                None => data.extend_from_slice(set),
            }
        }
    }
    avcc_to_annex_b(&avcc, length_size as usize, &mut data)?;
    Some(EncodedFrame {
        data,
        auxiliary: None,
        key_frame,
        qp: None,
    })
}

/// Rewrites NAL units carrying a big-endian length prefix of `length_size` bytes as Annex B.
fn avcc_to_annex_b(avcc: &[u8], length_size: usize, out: &mut Vec<u8>) -> Option<()> {
    if !(1..=4).contains(&length_size) {
        return None;
    }
    let mut rest = avcc;
    while !rest.is_empty() {
        if rest.len() < length_size {
            return None;
        }
        let len = rest[..length_size].iter().fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        rest = &rest[length_size..];
        if rest.len() < len {
            return None;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&rest[..len]);
        rest = &rest[len..];
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixed_units_become_annex_b() {
        let avcc = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x41];
        let mut out = Vec::new();
        avcc_to_annex_b(&avcc, 4, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn truncated_input_is_rejected() {
        let mut out = Vec::new();
        assert!(avcc_to_annex_b(&[0, 0, 0, 5, 1, 2], 4, &mut out).is_none());
    }

    /// Needs the hardware encoder, which every supported Mac has; no permission involved.
    #[test]
    fn encodes_a_key_frame_then_a_delta_frame() {
        let (w, h) = (320u32, 240u32);
        let mut encoder = H264Encoder::new(w, h, 30).expect("encoder");
        let mut frame = vec![0u8; (w * h * 4) as usize];
        let first = encoder.encode(&frame, (w * 4) as usize).expect("encode").expect("frame");
        assert!(first.key_frame);
        assert_eq!(&first.data[..6], &[0, 0, 0, 1, ACCESS_UNIT_DELIMITER, 0x10], "a delimiter leads every picture");
        assert_eq!(&first.data[6..10], &START_CODE);
        assert_eq!(first.data[10] & 0x1F, 7, "then the SPS of a key frame");
        frame.iter_mut().step_by(7).for_each(|b| *b = 0x80);
        let second = encoder.encode(&frame, (w * 4) as usize).expect("encode").expect("frame");
        assert_eq!(&second.data[..6], &[0, 0, 0, 1, ACCESS_UNIT_DELIMITER, 0x30]);
        assert!(!second.key_frame);
        encoder.set_bitrate(1_000_000).expect("bitrate change mid-stream");
        assert!(encoder.encode(&frame, (w * 4) as usize).expect("encode").is_some());
    }

    #[test]
    fn refines_a_still_picture_and_sends_a_held_one() {
        let (w, h) = (320u32, 240u32);
        let stride = (w * 4) as usize;
        let mut encoder = H264Encoder::new(w, h, 30).expect("encoder");
        assert!(encoder.refine().expect("refine").is_none(), "nothing to refine yet");
        let mut frame: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
        encoder.encode(&frame, stride).expect("encode").expect("frame");
        assert!(encoder.refine().expect("refine").is_none(), "not still for long enough");
        std::thread::sleep(REFINE_AFTER + Duration::from_millis(50));
        let refined = encoder.refine().expect("refine").expect("a sharper frame once still");
        assert!(!refined.key_frame);
        if encoder.controls_quantiser() {
            assert!(refined.qp.is_some_and(|qp| qp < 24), "finer than new content: {:?}", refined.qp);
        }
        frame.iter_mut().step_by(5).for_each(|b| *b = 0);
        encoder.stage(&frame, stride).expect("stage");
        assert!(encoder.refine().expect("refine").is_some(), "a held frame goes out at once");
    }

    #[test]
    fn resends_the_still_picture_as_a_key_frame() {
        let (w, h) = (320usize, 240usize);
        let mut encoder = H264Encoder::new_avc444(w as u32, h as u32, 30, 1).expect("AVC444 encoder");
        encoder.resend();
        assert!(encoder.refine().expect("refine").is_none(), "no picture to resend yet");
        let frame = coloured_text(w, h);
        encoder.encode(&frame, w * 4).expect("encode").expect("frame");
        std::thread::sleep(REFINE_AFTER + Duration::from_millis(50));
        while encoder.refine().expect("refine").is_some() {
            std::thread::sleep(REFINE_AFTER + Duration::from_millis(50));
        }
        encoder.resend();
        let again = encoder.refine().expect("refine").expect("the picture again at once");
        assert!(again.key_frame && again.auxiliary.is_some());
    }

    /// Lines of red, green and blue text on white, whose colour 4:2:0 smears.
    fn coloured_text(width: usize, height: usize) -> Vec<u8> {
        let mut bgra = vec![255u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let glyph = ((x / 7) * 31 + (y / 14) * 17) % 5 != 0 && (x % 7) < 5 && (y % 14) < 10 && ((x ^ y) & 3) != 0;
                if glyph {
                    let colour = [[0, 0, 220], [0, 150, 0], [200, 0, 0]][(y / 14) % 3];
                    bgra[(y * width + x) * 4..][..3].copy_from_slice(&colour);
                }
            }
        }
        bgra
    }

    /// Both views of a frame, checking that each costs little when little changed, which holds
    /// only if it predicts from the last view of its kind.
    fn views_of_a_small_change(encoder: &mut H264Encoder, frame: &mut [u8], width: usize, x: usize) -> (usize, usize) {
        frame[(100 * width + x) * 4..][..24].copy_from_slice(&[0, 0, 220, 255].repeat(6));
        let typed = encoder.encode(frame, width * 4).expect("encode").expect("frame");
        assert!(!typed.key_frame);
        (typed.data.len(), typed.auxiliary.expect("an auxiliary view").len())
    }

    /// Needs the hardware encoder's low-latency mode, which Apple silicon has.
    #[test]
    fn avc444_frames_carry_both_views_each_predicted_from_its_kind() {
        let (w, h) = (320usize, 240usize);
        let mut encoder = H264Encoder::new_avc444(w as u32, h as u32, 30, 2).expect("AVC444 encoder");
        assert!(encoder.avc444());
        let mut frame = coloured_text(w, h);
        let first = encoder.encode(&frame, w * 4).expect("encode").expect("frame");
        assert!(first.key_frame);
        let (main, auxiliary) = (first.data.len(), first.auxiliary.as_ref().expect("an auxiliary view").len());
        let typed = views_of_a_small_change(&mut encoder, &mut frame, w, 50);
        assert!(typed.0 * 10 < main && typed.1 * 10 < auxiliary, "{typed:?} after {main} and {auxiliary} bytes");
        // A pause between keystrokes, many frame intervals long, keeps both chains.
        std::thread::sleep(REFINE_AFTER + Duration::from_millis(50));
        let typed = views_of_a_small_change(&mut encoder, &mut frame, w, 60);
        assert!(typed.0 * 10 < main && typed.1 * 10 < auxiliary, "{typed:?} after a pause");

        std::thread::sleep(REFINE_AFTER + Duration::from_millis(50));
        let refined = encoder.refine().expect("refine").expect("a sharper frame once still");
        assert!(refined.auxiliary.is_some(), "refinement sharpens both views");

        // A key frame starts both chains afresh.
        encoder.request_key_frame();
        frame[..4].copy_from_slice(&[0, 0, 0, 255]);
        let key = encoder.encode(&frame, w * 4).expect("encode").expect("frame");
        assert!(key.key_frame && key.auxiliary.is_some());
        let typed = views_of_a_small_change(&mut encoder, &mut frame, w, 80);
        assert!(typed.0 * 10 < main && typed.1 * 10 < auxiliary, "{typed:?} after a key frame");
    }

    #[test]
    fn main_views_only_while_asked_each_switch_a_key_frame() {
        let (w, h) = (320usize, 240usize);
        let mut encoder = H264Encoder::new_avc444(w as u32, h as u32, 30, 1).expect("AVC444 encoder");
        let mut frame = coloured_text(w, h);
        let first = encoder.encode(&frame, w * 4).expect("encode").expect("frame");
        let main = first.data.len();
        views_of_a_small_change(&mut encoder, &mut frame, w, 50);

        encoder.set_main_only(true);
        assert!(encoder.main_only());
        frame[(120 * w + 40) * 4..][..24].copy_from_slice(&[0, 150, 0, 255].repeat(6));
        let switched = encoder.encode(&frame, w * 4).expect("encode").expect("frame");
        assert!(switched.key_frame && switched.auxiliary.is_none(), "main view only, from a key frame");
        frame[(130 * w + 40) * 4..][..24].copy_from_slice(&[0, 150, 0, 255].repeat(6));
        let moving = encoder.encode(&frame, w * 4).expect("encode").expect("frame");
        assert!(!moving.key_frame && moving.auxiliary.is_none());
        assert!(moving.data.len() * 10 < main, "main views keep predicting from each other");

        encoder.set_main_only(false);
        frame[(140 * w + 40) * 4..][..24].copy_from_slice(&[0, 150, 0, 255].repeat(6));
        let back = encoder.encode(&frame, w * 4).expect("encode").expect("frame");
        assert!(back.key_frame && back.auxiliary.is_some(), "both views again, from a key frame");
        let typed = views_of_a_small_change(&mut encoder, &mut frame, w, 80);
        assert!(typed.0 * 10 < main, "{typed:?}");
    }

    /// macOS 27 keeps one long-term reference fewer than macOS 26: with four reference frames
    /// every third view came without a token and the next view of its kind cost nearly a key frame.
    #[test]
    fn every_view_of_a_long_run_predicts_from_its_kind() {
        let (w, h) = (320usize, 240usize);
        let mut encoder = H264Encoder::new_avc444(w as u32, h as u32, 30, 1).expect("AVC444 encoder");
        let mut frame = coloured_text(w, h);
        let first = encoder.encode(&frame, w * 4).expect("encode").expect("frame");
        let (main, auxiliary) = (first.data.len(), first.auxiliary.as_ref().expect("an auxiliary view").len());
        for i in 0..12 {
            let typed = views_of_a_small_change(&mut encoder, &mut frame, w, 20 + i * 10);
            assert!(typed.0 * 10 < main && typed.1 * 10 < auxiliary, "change {i}: {typed:?}");
        }
    }

    #[test]
    fn avc444_needs_whole_macroblock_widths() {
        assert!(H264Encoder::new_avc444(1366, 768, 30, 1).is_err());
    }
}
