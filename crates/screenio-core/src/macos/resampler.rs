//! Sample-rate conversion with AudioToolbox's AudioConverter, for sound wanted at a rate
//! ScreenCaptureKit does not capture at.

use crate::{Error, Result};
use objc2_core_audio_types::{
    kAudioFormatFlagIsPacked, kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM, AudioBuffer, AudioBufferList,
    AudioStreamBasicDescription, AudioStreamPacketDescription,
};
use std::{ffi::c_void, ptr};

#[repr(C)]
struct OpaqueAudioConverter {
    _private: [u8; 0],
}
type AudioConverterRef = *mut OpaqueAudioConverter;
type InputProc = unsafe extern "C" fn(
    converter: AudioConverterRef,
    packets: *mut u32,
    data: *mut AudioBufferList,
    descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32;

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioConverterNew(
        source: *const AudioStreamBasicDescription,
        destination: *const AudioStreamBasicDescription,
        converter: *mut AudioConverterRef,
    ) -> i32;
    fn AudioConverterDispose(converter: AudioConverterRef) -> i32;
    fn AudioConverterFillComplexBuffer(
        converter: AudioConverterRef,
        input: InputProc,
        user: *mut c_void,
        packets: *mut u32,
        data: *mut AudioBufferList,
        descriptions: *mut AudioStreamPacketDescription,
    ) -> i32;
}

/// What [`supply`] returns once the call's input is used up. The converter then returns what it
/// made so far and asks for more on the next call; any status it does not know does this.
const INPUT_USED_UP: i32 = i32::from_be_bytes(*b"used");

/// Packed 16-bit PCM at `rate` with `channels`.
fn pcm(rate: u32, channels: u32) -> AudioStreamBasicDescription {
    let bytes_per_frame = 2 * channels;
    AudioStreamBasicDescription {
        mSampleRate: f64::from(rate),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 16,
        mReserved: 0,
    }
}

/// The input of one [`Resampler::process`] call, handed out as the converter asks for it.
struct Pending {
    samples: *const i16,
    frames: u32,
    channels: u32,
}

unsafe extern "C" fn supply(
    _converter: AudioConverterRef,
    packets: *mut u32,
    data: *mut AudioBufferList,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    let pending = &mut *user.cast::<Pending>();
    if pending.frames == 0 {
        *packets = 0;
        return INPUT_USED_UP;
    }
    let frames = (*packets).clamp(1, pending.frames);
    let list = &mut *data;
    list.mNumberBuffers = 1;
    list.mBuffers[0] = AudioBuffer {
        mNumberChannels: pending.channels,
        mDataByteSize: frames * pending.channels * 2,
        mData: pending.samples.cast_mut().cast::<c_void>(),
    };
    pending.samples = pending.samples.add((frames * pending.channels) as usize);
    pending.frames -= frames;
    *packets = frames;
    0
}

/// Converts interleaved 16-bit sound from one rate to another, a chunk at a time.
pub(super) struct Resampler {
    converter: AudioConverterRef,
    channels: u32,
    /// Output frames per input frame.
    ratio: f64,
    out: Vec<i16>,
}

// The converter is only used from whichever thread holds the resampler, one call at a time.
unsafe impl Send for Resampler {}

impl Resampler {
    pub(super) fn new(from: u32, to: u32, channels: u32) -> Result<Self> {
        let mut converter = ptr::null_mut();
        let status = unsafe { AudioConverterNew(&pcm(from, channels), &pcm(to, channels), &mut converter) };
        if status != 0 || converter.is_null() {
            return Err(Error::Os);
        }
        Ok(Self {
            converter,
            channels,
            ratio: f64::from(to) / f64::from(from),
            out: Vec::new(),
        })
    }

    /// What `samples`, interleaved frames at the input rate, become at the output rate, valid until
    /// the next call. The converter keeps a few frames back between calls for its filter, so the
    /// first chunk comes out a little short and later ones carry the difference.
    pub(super) fn process(&mut self, samples: &[i16]) -> Result<&[i16]> {
        let channels = self.channels as usize;
        let mut pending = Pending {
            samples: samples.as_ptr(),
            frames: u32::try_from(samples.len() / channels).map_err(|_| Error::Invalid)?,
            channels: self.channels,
        };
        let mut produced = 0usize;
        loop {
            // Room for what is left plus what the converter may have kept back.
            let room = (pending.frames as f64 * self.ratio) as usize + 64;
            self.out.resize((produced + room) * channels, 0);
            let mut packets = room as u32;
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: self.channels,
                    mDataByteSize: (room * channels * 2) as u32,
                    mData: self.out[produced * channels..].as_mut_ptr().cast::<c_void>(),
                }],
            };
            // SAFETY: the list points into `out` and `pending` into `samples`, both of which
            // outlive the call, and the callback only reads `pending` during it.
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter,
                    supply,
                    ptr::from_mut(&mut pending).cast::<c_void>(),
                    &mut packets,
                    &mut list,
                    ptr::null_mut(),
                )
            };
            produced += packets as usize;
            match status {
                INPUT_USED_UP => break,
                // The output was full first; there may be more.
                0 if packets > 0 => continue,
                0 => break,
                _ => return Err(Error::Os),
            }
        }
        self.out.truncate(produced * channels);
        Ok(&self.out)
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    /// `frames` frames of a 1 kHz tone at `rate` from frame `from` on, in `channels` channels.
    fn tone(rate: u32, channels: usize, from: usize, frames: usize) -> Vec<i16> {
        (from..from + frames)
            .flat_map(|frame| {
                let sample = ((frame as f64 * 1000.0 / f64::from(rate) * TAU).sin() * 10_000.0) as i16;
                std::iter::repeat_n(sample, channels)
            })
            .collect()
    }

    /// Upward zero crossings of the first channel: one per period of the tone.
    fn periods(samples: &[i16], channels: usize) -> usize {
        let first: Vec<i16> = samples.chunks(channels).map(|frame| frame[0]).collect();
        first.windows(2).filter(|pair| pair[0] < 0 && pair[1] >= 0).count()
    }

    #[test]
    fn a_second_at_48_khz_becomes_a_second_at_44_1_khz_at_the_same_pitch() {
        let mut resampler = Resampler::new(48_000, 44_100, 2).expect("a converter");
        let mut out = Vec::new();
        for chunk in 0..50 {
            let resampled = resampler.process(&tone(48_000, 2, chunk * 960, 960)).expect("resampled");
            assert_eq!(resampled.len() % 2, 0, "whole frames");
            out.extend_from_slice(resampled);
        }
        let frames = out.len() / 2;
        assert!((43_900..=44_100).contains(&frames), "{frames} frames for a second");
        let periods = periods(&out, 2);
        assert!((985..=1000).contains(&periods), "{periods} periods of 1 kHz in a second");
    }

    #[test]
    fn chunks_of_any_size_come_out_at_the_ratio() {
        let mut resampler = Resampler::new(48_000, 44_100, 1).expect("a converter");
        let (mut from, mut frames) = (0, 0);
        for _ in 0..5 {
            for size in [1, 7, 480, 4800, 960] {
                frames += resampler.process(&tone(48_000, 1, from, size)).expect("resampled").len();
                from += size;
            }
        }
        let expected = (from as f64 * 44_100.0 / 48_000.0) as usize;
        assert!((expected - 64..=expected + 1).contains(&frames), "{frames} frames of {expected}");
    }
}
