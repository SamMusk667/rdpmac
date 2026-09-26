//! What the Mac plays, captured through ScreenCaptureKit (macOS 13 and later).
//!
//! ScreenCaptureKit captures audio only as part of a stream with a content filter, so the stream
//! filters a display and asks for a 2x2 picture once a second, which the output drops. Audio
//! arrives as 32-bit floats, usually one buffer per channel; the output interleaves it as 16-bit
//! samples and queues it for [`AudioCapture::read`]. ScreenCaptureKit captures at a few rates only
//! and silently at 48 kHz when asked for another, so 44.1 kHz is resampled from a 48 kHz capture.

use super::{
    capture::{describe, shareable_content, start},
    resampler::Resampler,
};
use crate::{AudioChunk, Error, Result};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::ProtocolObject,
    AnyThread, DefinedClass,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM, AudioBuffer,
    AudioBufferList,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMClock, CMSampleBuffer, CMTime,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};
use std::{
    collections::VecDeque,
    ptr::{self, NonNull},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

/// Rates ScreenCaptureKit captures audio at; asked for any other, it captures at 48 kHz.
const CAPTURE_RATES: [u32; 4] = [8000, 16000, 24000, 48000];
/// Rates served by resampling a 48 kHz capture.
const RESAMPLED_RATES: [u32; 1] = [44_100];
const RESAMPLED_FROM: u32 = 48_000;
/// Audio waiting for a reader beyond this is dropped, oldest first.
const MAX_QUEUED: Duration = Duration::from_secs(1);

struct Chunk {
    samples: Vec<i16>,
    /// The sample's presentation time, in seconds on the capture clock.
    time: f64,
}

#[derive(Default)]
struct Queue {
    chunks: VecDeque<Chunk>,
    /// Frames (samples per channel) in `chunks`.
    frames: usize,
    free: Vec<Vec<i16>>,
    stopped: Option<String>,
    /// The sample rate of the newest audio, as ScreenCaptureKit delivered it.
    rate: Option<f64>,
    /// Converts what ScreenCaptureKit delivers to the rate asked for, when they differ.
    resampler: Option<Resampler>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    channels: usize,
    max_frames: usize,
}

impl Shared {
    fn publish(&self, sample: &CMSampleBuffer) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut samples = queue.free.pop().unwrap_or_default();
        samples.clear();
        // SAFETY: the sample is a valid audio sample buffer for the duration of the callback.
        let Some(rate) = (unsafe { read_samples(sample, self.channels, &mut samples) }) else {
            queue.free.push(samples);
            return;
        };
        queue.rate = Some(rate);
        let converted = match queue.resampler.as_mut() {
            Some(resampler) => match resampler.process(&samples) {
                Ok(resampled) => {
                    samples.clear();
                    samples.extend_from_slice(resampled);
                    true
                }
                Err(_) => false,
            },
            None => true,
        };
        if !converted {
            // The reader gets `Reset` and opens the capture afresh, converter included.
            queue.free.push(samples);
            queue.stopped = Some("sample rate conversion failed".into());
            self.ready.notify_all();
            return;
        }
        let time = unsafe { sample.presentation_time_stamp() };
        let time = if time.timescale > 0 { time.value as f64 / f64::from(time.timescale) } else { 0.0 };
        queue.frames += samples.len() / self.channels;
        queue.chunks.push_back(Chunk { samples, time });
        while queue.frames > self.max_frames {
            let Some(old) = queue.chunks.pop_front() else {
                break;
            };
            queue.frames -= old.samples.len() / self.channels;
            queue.free.push(old.samples);
        }
        self.ready.notify_one();
    }

    fn stop(&self, reason: String) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.stopped = Some(reason);
        self.ready.notify_all();
    }
}

/// Appends the sample's audio to `out` as interleaved 16-bit samples of `channels` channels and
/// returns its sample rate. `None` when it is not 32-bit float PCM of that many channels.
///
/// # Safety
/// `sample` must be a valid audio sample buffer.
unsafe fn read_samples(sample: &CMSampleBuffer, channels: usize, out: &mut Vec<i16>) -> Option<f64> {
    let format = sample.format_description()?;
    let description = CMAudioFormatDescriptionGetStreamBasicDescription(&format).as_ref()?;
    if description.mFormatID != kAudioFormatLinearPCM
        || description.mFormatFlags & kAudioFormatFlagIsFloat == 0
        || description.mBitsPerChannel != 32
        || description.mChannelsPerFrame as usize != channels
    {
        return None;
    }
    let mut needed = 0usize;
    let status = sample.audio_buffer_list_with_retained_block_buffer(
        &mut needed,
        ptr::null_mut(),
        0,
        None,
        None,
        0,
        ptr::null_mut(),
    );
    if status != 0 || needed < std::mem::size_of::<AudioBufferList>() {
        return None;
    }
    // u64 storage keeps the list aligned for its fields.
    let mut storage = vec![0u64; needed.div_ceil(8)];
    let list = storage.as_mut_ptr().cast::<AudioBufferList>();
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    let status = sample.audio_buffer_list_with_retained_block_buffer(
        ptr::null_mut(),
        list,
        needed,
        None,
        None,
        kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
        &mut block,
    );
    // The block buffer holds the samples the list points at until it is released.
    let _block = NonNull::new(block).map(|block| CFRetained::from_raw(block));
    if status != 0 {
        return None;
    }
    let buffers: &[AudioBuffer] =
        std::slice::from_raw_parts((*list).mBuffers.as_ptr(), (*list).mNumberBuffers as usize);
    let floats = |buffer: &AudioBuffer| -> &[f32] {
        if buffer.mData.is_null() {
            return &[];
        }
        std::slice::from_raw_parts(buffer.mData.cast::<f32>(), buffer.mDataByteSize as usize / 4)
    };
    if description.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
        if buffers.len() != channels {
            return None;
        }
        let planes: Vec<&[f32]> = buffers.iter().map(floats).collect();
        interleave(&planes, out);
    } else {
        let [buffer] = buffers else {
            return None;
        };
        out.extend(floats(buffer).iter().map(|&s| to_i16(s)));
    }
    Some(description.mSampleRate)
}

/// One 16-bit sample from a float one, full scale being ±1.
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

/// Appends one sample from each plane in turn; planes of different lengths stop at the shortest.
fn interleave(planes: &[&[f32]], out: &mut Vec<i16>) {
    let frames = planes.iter().map(|p| p.len()).min().unwrap_or(0);
    out.reserve(frames * planes.len());
    for frame in 0..frames {
        out.extend(planes.iter().map(|plane| to_i16(plane[frame])));
    }
}

struct OutputIvars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and AudioOutput has no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "ScreenioAudioOutput"]
    #[ivars = OutputIvars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            // The picture the stream has to have is dropped.
            if kind.0 == SCStreamOutputType::Audio.0 {
                self.ivars().shared.publish(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().shared.stop(describe(error));
        }
    }
);

impl AudioOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { shared });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct AudioCapture {
    stream: Retained<SCStream>,
    _output: Retained<AudioOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    sample_rate: u32,
    /// The rate ScreenCaptureKit captures at: `sample_rate`, or what it is resampled from.
    capture_rate: u32,
    channels: u32,
    /// The capture clock's time of the first chunk read, which timestamps count from.
    first: Option<f64>,
    samples: Vec<i16>,
}

// As for `Capturer`: the stream runs on its own queues, and this handle only issues calls from
// whichever thread holds it.
unsafe impl Send for AudioCapture {}

impl AudioCapture {
    pub fn open(sample_rate: u32, channels: u32) -> Result<Self> {
        let capture_rate = if CAPTURE_RATES.contains(&sample_rate) {
            sample_rate
        } else if RESAMPLED_RATES.contains(&sample_rate) {
            RESAMPLED_FROM
        } else {
            return Err(Error::Invalid);
        };
        if !(1..=2).contains(&channels) {
            return Err(Error::Invalid);
        }
        let resampler = (capture_rate != sample_rate)
            .then(|| Resampler::new(capture_rate, sample_rate, channels))
            .transpose()?;
        let content = shareable_content()?;
        let displays = unsafe { content.displays() };
        let display = displays.firstObject().ok_or(Error::Os)?;
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new())
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            config.setQueueDepth(1);
            config.setCapturesAudio(true);
            config.setSampleRate(capture_rate as isize);
            config.setChannelCount(channels as isize);
            config.setExcludesCurrentProcessAudio(true);
        }
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                resampler,
                ..Queue::default()
            }),
            ready: Condvar::new(),
            channels: channels as usize,
            max_frames: (MAX_QUEUED.as_secs_f64() * f64::from(sample_rate)) as usize,
        });
        let output = AudioOutput::new(shared.clone());
        let queue = DispatchQueue::new("screenio.audio", DispatchQueueAttr::SERIAL);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
            unsafe {
                stream.addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    kind,
                    Some(&queue),
                )
            }
            .map_err(|_| Error::Os)?;
        }
        start(&stream)?;
        Ok(Self {
            stream,
            _output: output,
            _queue: queue,
            shared,
            sample_rate,
            capture_rate,
            channels,
            first: None,
            samples: Vec::new(),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    pub fn source_rate(&self) -> Option<f64> {
        let rate = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).rate?;
        // Resampled sound is off by the same ratio as the capture it comes from.
        Some(rate * f64::from(self.sample_rate) / f64::from(self.capture_rate))
    }

    pub fn read(&mut self, timeout: Duration) -> Result<AudioChunk<'_>> {
        let deadline = Instant::now() + timeout;
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        let chunk = loop {
            if queue.stopped.is_some() {
                return Err(Error::Reset);
            }
            if let Some(chunk) = queue.chunks.pop_front() {
                queue.frames -= chunk.samples.len() / self.channels as usize;
                break chunk;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout);
            }
            queue = self
                .shared
                .ready
                .wait_timeout(queue, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        };
        let previous = std::mem::replace(&mut self.samples, chunk.samples);
        queue.free.push(previous);
        drop(queue);
        let first = *self.first.get_or_insert(chunk.time);
        // Sample times are on the host clock, which counts from start-up.
        let now = unsafe { CMClock::host_time_clock().time() };
        let now = if now.timescale > 0 { now.value as f64 / f64::from(now.timescale) } else { chunk.time };
        Ok(AudioChunk {
            samples: &self.samples,
            timestamp: Duration::from_secs_f64((chunk.time - first).max(0.0)),
            played: Duration::from_secs_f64(chunk.time.max(0.0)),
            age: Duration::from_secs_f64((now - chunk.time).max(0.0)),
        })
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planes_interleave_as_16_bit_samples() {
        let left = [0.0f32, 0.5, 1.0, -1.0];
        let right = [0.25f32, -0.5, 2.0, -2.0];
        let mut out = Vec::new();
        interleave(&[&left, &right], &mut out);
        assert_eq!(out, [0, 8192, 16384, -16384, 32767, 32767, -32767, -32767]);
    }

    #[test]
    fn planes_of_different_lengths_stop_at_the_shortest() {
        let mut out = vec![7];
        interleave(&[&[0.5, 0.5, 0.5], &[-0.5]], &mut out);
        assert_eq!(out, [7, 16384, -16384]);
    }
}
