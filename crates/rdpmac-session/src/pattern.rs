//! Synthetic frames for testing the pipeline where the screen cannot be captured, and for
//! measuring encoder cost at a chosen resolution and rate.

use core::num::{NonZeroU16, NonZeroUsize};

use bytes::Bytes;
use ironrdp_server::{BitmapUpdate, PixelFormat};

pub struct TestPattern {
    width: u32,
    height: u32,
    frame: u64,
    buffer: Vec<u8>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame: 0,
            buffer: vec![0; (width * height * 4) as usize],
        }
    }

    /// A gradient with a moving vertical bar, so every frame differs from the last but most
    /// of the picture stays the same, roughly like a desktop with a moving window.
    pub fn next_frame(&mut self) -> Option<BitmapUpdate> {
        let (w, h) = (self.width as usize, self.height as usize);
        let bar = (self.frame as usize * 8) % w;
        let bar_width = (w / 16).max(8);
        for y in 0..h {
            let row = &mut self.buffer[y * w * 4..(y + 1) * w * 4];
            let g = (y * 255 / h.max(1)) as u8;
            for x in 0..w {
                let px = &mut row[x * 4..x * 4 + 4];
                let in_bar = x >= bar && x < bar + bar_width;
                px[0] = if in_bar { 0x20 } else { (x * 255 / w.max(1)) as u8 }; // B
                px[1] = if in_bar { 0xE0 } else { g }; // G
                px[2] = if in_bar { 0xFF } else { 0x40 }; // R
                px[3] = 0xFF;
            }
        }
        self.frame += 1;
        Some(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(u16::try_from(self.width).ok()?)?,
            height: NonZeroU16::new(u16::try_from(self.height).ok()?)?,
            format: PixelFormat::BgrA32,
            data: Bytes::copy_from_slice(&self.buffer),
            stride: NonZeroUsize::new(w * 4)?,
        })
    }
}
