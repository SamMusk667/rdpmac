//! H.264 encoding with VideoToolbox for the graphics pipeline's AVC420 codec.
//!
//! One encoder serves one session size. Frames go in as BGRA and come out as H.264 access units
//! in Annex B form (start codes), with SPS and PPS in front of every key frame so a client can
//! start or recover at any key frame. Encoding is synchronous: each call flushes the session, so
//! a frame's bitstream is ready when [`H264Encoder::encode`] returns. The session is configured
//! for real-time use without frame reordering, which RDP requires.

use std::ffi::{c_int, c_void};
use std::fmt;
use std::ptr::{self, NonNull};
use std::sync::Mutex;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder, VTCompressionSession, VTEncodeInfoFlags,
    VTSessionSetProperty,
};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// Target bits per pixel per frame; screen content compresses well, so this is modest.
const BITS_PER_PIXEL: f64 = 0.1;
const MIN_BITRATE: f64 = 2_000_000.0;
const MAX_BITRATE: f64 = 60_000_000.0;
/// Seconds between forced key frames, so a client that lost a frame recovers on its own.
const KEY_FRAME_SECONDS: u32 = 10;

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
}

/// Where the output callback leaves its result for the thread that called `encode`.
#[derive(Default)]
struct Sink {
    output: Option<EncodedFrame>,
    error: Option<i32>,
}

pub struct H264Encoder {
    session: CFRetained<VTCompressionSession>,
    target_bitrate: u32,
    // Boxed so its address stays stable; the session's callback reads it through a raw pointer.
    sink: Box<Mutex<Sink>>,
    width: u32,
    height: u32,
    fps: i32,
    frame: i64,
    key_frame_requested: bool,
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

        let spec = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder],
                &[CFBoolean::new(true).as_ref()],
            )
        };
        let format = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
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
                &*sink as *const Mutex<Sink> as *mut c_void,
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            return Err(fail("VTCompressionSessionCreate", status));
        };
        let session = unsafe { CFRetained::from_raw(raw) };

        let bitrate = target_bitrate(width, height, fps as u32) as i32;
        let yes = CFBoolean::new(true);
        let no = CFBoolean::new(false);
        let rate = CFNumber::new_i32(bitrate);
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
            target_bitrate: bitrate as u32,
            sink,
            width,
            height,
            fps,
            frame: 0,
            key_frame_requested: true,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The bitrate chosen for this size and frame rate, in bits per second.
    pub fn target_bitrate(&self) -> u32 {
        self.target_bitrate
    }

    /// Changes the average bitrate; takes effect from the next frame.
    pub fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError> {
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
        let row = self.width as usize * 4;
        if stride < row || bgra.len() < stride * (self.height as usize - 1) + row {
            return Err(EncodeError("frame smaller than the encoder size".into()));
        }
        let pixels = self.pixel_buffer(bgra, stride)?;
        let force_key = self.key_frame_requested;
        let options = force_key.then(|| unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                &[CFBoolean::new(true).as_ref()],
            )
        });
        let status = unsafe {
            self.session.encode_frame(
                &pixels,
                CMTime::new(self.frame, self.fps),
                CMTime::new(1, self.fps),
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
        self.frame += 1;
        let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(status) = sink.error.take() {
            return Err(fail("encoding", status));
        }
        let output = sink.output.take();
        if output.as_ref().is_some_and(|f| f.key_frame) {
            self.key_frame_requested = false;
        }
        Ok(output)
    }

    /// Copies the frame into a buffer from the session's pool, which VideoToolbox can read
    /// without a further copy.
    fn pixel_buffer(&self, bgra: &[u8], stride: usize) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
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
        let base = CVPixelBufferGetBaseAddress(&buffer) as *mut u8;
        let dst_stride = CVPixelBufferGetBytesPerRow(&buffer);
        let row = self.width as usize * 4;
        if !base.is_null() && dst_stride >= row {
            for y in 0..self.height as usize {
                let src = &bgra[y * stride..y * stride + row];
                unsafe { ptr::copy_nonoverlapping(src.as_ptr(), base.add(y * dst_stride), row) };
            }
        }
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
        if base.is_null() || dst_stride < row {
            return Err(EncodeError("pixel buffer has no usable memory".into()));
        }
        Ok(buffer)
    }
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
/// parameter sets in front when it is a key frame.
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
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(set, size) });
        }
    }
    avcc_to_annex_b(&avcc, length_size as usize, &mut data)?;
    Some(EncodedFrame { data, key_frame })
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
}
