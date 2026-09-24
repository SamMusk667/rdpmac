//! Cursor position and shape as RDP pointer updates.

use ironrdp_pdu::pointer::PointerPositionAttribute;
use ironrdp_server::{DisplayUpdate, RGBAPointer};
use screenio_core::CursorShape;

/// Largest shape the pointer update in this IronRDP release carries. Larger shapes (the
/// fast-path large pointer update, up to 384x384) wait for the next IronRDP release.
const POINTER_MAX: u32 = 96;
/// Pointer cache slots we cycle through; every client offers at least this many.
const CACHE_SLOTS: u16 = 20;
/// Scale factors this close to 1 are sent as captured.
const SCALE_TOLERANCE: f64 = 0.05;

#[derive(Default)]
pub struct PointerCache {
    next_index: u16,
}

impl PointerCache {
    fn allocate(&mut self) -> u16 {
        let index = self.next_index;
        self.next_index = (self.next_index + 1) % CACHE_SLOTS;
        index
    }

    /// The update that installs `shape` as the client's pointer, resized so it keeps its size
    /// relative to the picture when the session serves `pixels_per_point` pixels per point.
    /// `None` when the shape is inconsistent or still too large after resizing.
    pub fn update_for(&mut self, shape: &CursorShape, pixels_per_point: f64) -> Option<DisplayUpdate> {
        if shape.width == 0 || shape.height == 0 || shape.rgba.len() != (shape.width * shape.height * 4) as usize {
            return None;
        }
        let bitmap_scale = if shape.scale > 0.0 { f64::from(shape.scale) } else { 1.0 };
        let factor = pixels_per_point / bitmap_scale;
        let scaled;
        let shape = if (factor - 1.0).abs() > SCALE_TOLERANCE && factor > 0.0 {
            scaled = resample(shape, factor);
            &scaled
        } else {
            shape
        };
        if shape.width > POINTER_MAX || shape.height > POINTER_MAX {
            return None;
        }
        let width = shape.width as u16;
        let height = shape.height as u16;
        Some(DisplayUpdate::RGBAPointer(RGBAPointer {
            cache_index: self.allocate(),
            width,
            height,
            hot_x: shape.hot_x.clamp(0, i32::from(width) - 1) as u16,
            hot_y: shape.hot_y.clamp(0, i32::from(height) - 1) as u16,
            data: shape.rgba.clone(),
        }))
    }
}

pub fn position_update(x: u16, y: u16) -> DisplayUpdate {
    DisplayUpdate::PointerPosition(PointerPositionAttribute { x, y })
}

/// Resizes an RGBA cursor by `factor` with area averaging over premultiplied colour, which keeps
/// thin outlines visible when shrinking and avoids dark fringes around transparent pixels.
fn resample(shape: &CursorShape, factor: f64) -> CursorShape {
    let (sw, sh) = (shape.width as usize, shape.height as usize);
    let dw = ((sw as f64 * factor).round() as usize).max(1);
    let dh = ((sh as f64 * factor).round() as usize).max(1);
    let (fx, fy) = (sw as f64 / dw as f64, sh as f64 / dh as f64);
    let mut out = vec![0u8; dw * dh * 4];
    for dy in 0..dh {
        let (y0, y1) = (dy as f64 * fy, (dy + 1) as f64 * fy);
        for dx in 0..dw {
            let (x0, x1) = (dx as f64 * fx, (dx + 1) as f64 * fx);
            let mut acc = [0f64; 4];
            let mut area = 0f64;
            for sy in (y0.floor() as usize)..(y1.ceil() as usize).min(sh) {
                let wy = (y1.min(sy as f64 + 1.0) - y0.max(sy as f64)).max(0.0);
                for sx in (x0.floor() as usize)..(x1.ceil() as usize).min(sw) {
                    let wx = (x1.min(sx as f64 + 1.0) - x0.max(sx as f64)).max(0.0);
                    let w = wx * wy;
                    let p = &shape.rgba[(sy * sw + sx) * 4..][..4];
                    let a = f64::from(p[3]) / 255.0;
                    acc[0] += f64::from(p[0]) * a * w;
                    acc[1] += f64::from(p[1]) * a * w;
                    acc[2] += f64::from(p[2]) * a * w;
                    acc[3] += a * w;
                    area += w;
                }
            }
            let px = &mut out[(dy * dw + dx) * 4..][..4];
            if area > 0.0 && acc[3] > 0.0 {
                for c in 0..3 {
                    px[c] = (acc[c] / acc[3]).round().clamp(0.0, 255.0) as u8;
                }
                px[3] = (acc[3] / area * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    CursorShape {
        id: shape.id,
        width: dw as u32,
        height: dh as u32,
        hot_x: (f64::from(shape.hot_x) * dw as f64 / sw as f64).round() as i32,
        hot_y: (f64::from(shape.hot_y) * dh as f64 / sh as f64).round() as i32,
        scale: (f64::from(shape.scale) * factor) as f32,
        rgba: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgba: [u8; 4], scale: f32) -> CursorShape {
        CursorShape {
            id: 1,
            width,
            height,
            hot_x: 2,
            hot_y: 2,
            scale,
            rgba: rgba.repeat((width * height) as usize),
        }
    }

    fn pointer(update: DisplayUpdate) -> RGBAPointer {
        match update {
            DisplayUpdate::RGBAPointer(p) => p,
            _ => panic!("not an RGBA pointer"),
        }
    }

    #[test]
    fn unscaled_when_densities_match() {
        let mut cache = PointerCache::default();
        let p = pointer(cache.update_for(&solid(8, 8, [255, 0, 0, 255], 2.0), 2.0).unwrap());
        assert_eq!((p.width, p.height, p.hot_x), (8, 8, 2));
    }

    #[test]
    fn shrunk_for_a_scaled_down_session() {
        // A 2x bitmap on a session serving half a pixel per point shrinks four times.
        let mut cache = PointerCache::default();
        let p = pointer(cache.update_for(&solid(56, 80, [10, 20, 30, 255], 2.0), 0.5).unwrap());
        assert_eq!((p.width, p.height), (14, 20));
        assert_eq!(&p.data[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn transparency_is_averaged_without_dark_fringes() {
        // Left column opaque white, right column fully transparent black, halved to one pixel.
        let mut s = solid(2, 1, [0, 0, 0, 0], 1.0);
        s.rgba[..4].copy_from_slice(&[255, 255, 255, 255]);
        let r = resample(&s, 0.5);
        assert_eq!(&r.rgba[..], &[255, 255, 255, 128]);
    }

    #[test]
    fn too_large_after_scaling_is_skipped() {
        let mut cache = PointerCache::default();
        assert!(cache.update_for(&solid(64, 64, [0, 0, 0, 255], 1.0), 2.0).is_none());
    }
}
