//! H.264 encoding with VideoToolbox for the graphics pipeline's AVC420 codec.
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

use std::ffi::{c_int, c_void};
use std::fmt;
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
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
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_ReferenceBufferCount, kVTCompressionPropertyKey_SupportsBaseFrameQP, kVTEncodeFrameOptionKey_BaseFrameQP,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl, VTCompressionSession, VTEncodeInfoFlags,
    VTSessionCopyProperty, VTSessionSetProperty,
};

use crate::color::{Converter, Nv12Planes};
use crate::quantiser::QpControl;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
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
    /// Annex B byte stream: start code, NAL unit, start code, NAL unit...
    pub data: Vec<u8>,
    pub key_frame: bool,
    /// The base quantiser, when the encoder chose it.
    pub qp: Option<i32>,
}

/// Where the output callback leaves its result for the thread that called `encode`.
#[derive(Default)]
struct Sink {
    output: Option<EncodedFrame>,
    error: Option<i32>,
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
}

// The session is only driven from the thread that owns the encoder; VideoToolbox calls the
// output callback on its own threads, which only touch the mutex-protected sink.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self, EncodeError> {
        if width == 0 || height == 0 {
            return Err(EncodeError("empty frame size".into()));
        }
        let fps = fps.clamp(1, 120) as i32;
        let sink = Box::new(Mutex::new(Sink::default()));
        let converter = Converter::new().ok_or_else(|| EncodeError("vImage has no BT.709 conversion".into()))?;
        let bitrate = target_bitrate(width, height, fps as u32);
        let now = Instant::now();

        let (session, control) = match quantiser_session(width, height, &sink) {
            Some(session) => (session, Control::Quantiser(QpControl::new(bitrate, now))),
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
            set(&session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Main_AutoLevel.as_ref())?;
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
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
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

    /// Encodes one picture, with `qp` as its base quantiser in low-latency mode.
    fn submit(&mut self, pixels: &CVPixelBuffer, qp: Option<i32>) -> Result<Option<EncodedFrame>, EncodeError> {
        let force_key = self.key_frame_requested;
        let yes = CFBoolean::new(true);
        let quantiser = qp.map(CFNumber::new_i32);
        let mut keys: Vec<&CFString> = Vec::with_capacity(2);
        let mut values: Vec<&CFType> = Vec::with_capacity(2);
        if force_key {
            keys.push(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame });
            values.push(yes.as_ref());
        }
        if let Some(quantiser) = &quantiser {
            keys.push(unsafe { kVTEncodeFrameOptionKey_BaseFrameQP });
            values.push(quantiser.as_ref());
        }
        let options = (!keys.is_empty()).then(|| CFDictionary::<CFString, CFType>::from_slices(&keys, &values));
        let status = unsafe {
            self.session.encode_frame(
                pixels,
                CMTime::new(self.started.elapsed().as_micros() as i64, 1_000_000),
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
        let mut output = {
            let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(status) = sink.error.take() {
                return Err(fail("encoding", status));
            }
            sink.output.take()
        };
        if let Some(frame) = output.as_mut() {
            let now = Instant::now();
            if frame.key_frame {
                self.key_frame_requested = false;
                self.last_key_frame = now;
            }
            if let (Control::Quantiser(control), Some(qp)) = (&mut self.control, qp) {
                control.record(qp, frame.data.len(), now);
            }
            frame.qp = qp;
        }
        Ok(output)
    }

    /// Converts the frame into a full-range BT.709 buffer from the session's pool, which
    /// VideoToolbox reads without a further copy.
    fn pixel_buffer(&self, bgra: &[u8], stride: usize) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        let row = self.width as usize * 4;
        if stride < row || bgra.len() < stride * (self.height as usize - 1) + row {
            return Err(EncodeError("frame smaller than the encoder size".into()));
        }
        let pool = unsafe { self.session.pixel_buffer_pool() }
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
        let usable = !planes.y.is_null()
            && !planes.cbcr.is_null()
            && planes.y_stride >= self.width as usize
            && planes.cbcr_stride >= (self.width as usize).div_ceil(2) * 2;
        let converted = usable
            && unsafe {
                self.converter
                    .convert(bgra, stride, self.width as usize, self.height as usize, &planes)
            };
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
        if !converted {
            return Err(EncodeError("converting the frame to YUV failed".into()));
        }
        Ok(buffer)
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

/// A low-latency session that takes a base quantiser with every frame, or `None` where the
/// hardware offers no such session.
fn quantiser_session(width: u32, height: u32, sink: &Mutex<Sink>) -> Option<CFRetained<VTCompressionSession>> {
    let session = open_session(width, height, true, sink).ok()?;
    let references = CFNumber::new_i32(LOW_LATENCY_REFERENCES);
    let usable = unsafe {
        supports_base_qp(&session)
            && set(&session, kVTCompressionPropertyKey_ReferenceBufferCount, references.as_ref()).is_ok()
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
        Some(frame) => sink.output = Some(frame),
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
    Some(EncodedFrame { data, key_frame, qp: None })
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
        assert_eq!(&first.data[..4], &START_CODE);
        assert_eq!(first.data[4] & 0x1F, 7, "an SPS leads a key frame");
        frame.iter_mut().step_by(7).for_each(|b| *b = 0x80);
        let second = encoder.encode(&frame, (w * 4) as usize).expect("encode").expect("frame");
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
}
