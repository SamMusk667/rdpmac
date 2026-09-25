//! Frame to RDP update conversion.
//!
//! Milestone 1 hands whole frames to IronRDP as [`BitmapUpdate`]s and lets its RemoteFX and RDP6
//! encoders do the work. Milestone 2 adds dirty rectangles through [`region_update`] and a
//! VideoToolbox H.264 path that feeds the graphics pipeline instead of this module.

#[cfg(target_os = "macos")]
pub mod avc444;
#[cfg(target_os = "macos")]
pub mod color;
#[cfg(target_os = "macos")]
pub mod h264;
#[cfg(target_os = "macos")]
mod quantiser;
pub mod rate;
#[cfg(target_os = "macos")]
mod sps;

use core::num::{NonZeroU16, NonZeroUsize};

use bytes::Bytes;
use ironrdp_server::{BitmapUpdate, PixelFormat};
use screenio_core::Frame;

/// The whole frame as one update. Returns `None` for a frame RDP cannot describe.
pub fn full_frame_update(frame: &Frame<'_>) -> Option<BitmapUpdate> {
    region_update(frame, 0, 0, frame.width, frame.height)
}

/// The frame as one update, cut to at most `max_width` x `max_height`. A capture pipeline may
/// round the output size; the session size is what the client was promised.
pub fn frame_update(frame: &Frame<'_>, max_width: u32, max_height: u32) -> Option<BitmapUpdate> {
    region_update(frame, 0, 0, frame.width.min(max_width), frame.height.min(max_height))
}

/// A sub-rectangle of the frame, copied to a tightly packed buffer.
pub fn region_update(frame: &Frame<'_>, x: u32, y: u32, width: u32, height: u32) -> Option<BitmapUpdate> {
    if x + width > frame.width || y + height > frame.height {
        return None;
    }
    let w = NonZeroU16::new(u16::try_from(width).ok()?)?;
    let h = NonZeroU16::new(u16::try_from(height).ok()?)?;
    let src_stride = frame.stride as usize;
    let row_bytes = width as usize * 4;
    let stride = NonZeroUsize::new(row_bytes)?;
    let mut data = Vec::with_capacity(row_bytes * height as usize);
    for row in y..y + height {
        let start = row as usize * src_stride + x as usize * 4;
        data.extend_from_slice(frame.data.get(start..start + row_bytes)?);
    }
    Some(BitmapUpdate {
        x: u16::try_from(x).ok()?,
        y: u16::try_from(y).ok()?,
        width: w,
        height: h,
        format: PixelFormat::BgrA32,
        data: Bytes::from(data),
        stride,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_is_copied_with_tight_stride() {
        let width = 4u32;
        let stride = 32u32; // padded rows
        let mut buf = vec![0u8; (stride * 2) as usize];
        buf[stride as usize + 4] = 7; // row 1, pixel 1, byte 0
        let frame = Frame {
            data: &buf,
            width,
            height: 2,
            stride,
            format: screenio_core::PixelFormat::Bgra,
        };
        let update = region_update(&frame, 1, 1, 2, 1).expect("valid region");
        assert_eq!(update.stride.get(), 8);
        assert_eq!(update.data.len(), 8);
        assert_eq!(update.data[0], 7);
        assert_eq!((update.x, update.y), (1, 1));
    }
}
