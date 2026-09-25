//! The YUV444v2 layout of MS-RDPEGFX 3.3.8.3.3: a 4:4:4 picture as two 4:2:0 views that one H.264
//! stream carries. The main view is the luma with the chroma of each 2x2 block averaged, an
//! ordinary 4:2:0 picture; the auxiliary view carries the chroma samples the average leaves out:
//!
//! - Y, left half: Cb of the odd columns (B4); right half: Cr of the odd columns (B5).
//! - Cb, left half: Cb of the odd rows at columns 4x (B6); right half: Cr there (B7).
//! - Cr, left half: Cb of the odd rows at columns 4x+2 (B8); right half: Cr there (B9).
//!
//! A client rebuilds the sample at the even row and column from the average and the other three.

use crate::color::{Chroma444, Nv12Planes};

/// Whether a picture of this size can take the layout. It splits rows into halves and quarters,
/// which only line up with the macroblocks a client decodes when the width is a multiple of 16.
pub fn fits(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && width.is_multiple_of(16) && height.is_multiple_of(2)
}

/// Writes the main view's chroma: each pair averages a 2x2 block of the 4:4:4 picture.
///
/// # Safety
/// `cbcr` and `stride` must describe a writable NV12 chroma plane of the picture's size.
pub unsafe fn write_main_chroma(chroma: &Chroma444, cbcr: *mut u8, stride: usize) {
    let w = chroma.width;
    // Written over whole rows of fixed-size chunks, which the compiler turns into vector code.
    let average = |top: &[u8; 2], bottom: &[u8; 2]| {
        let sum = u16::from(top[0]) + u16::from(top[1]) + u16::from(bottom[0]) + u16::from(bottom[1]);
        ((sum + 2) / 4) as u8
    };
    for cy in 0..chroma.height / 2 {
        let (top, bottom) = (2 * cy * w, (2 * cy + 1) * w);
        let (cb0, cb1) = (chroma.cb[top..][..w].as_chunks().0, chroma.cb[bottom..][..w].as_chunks().0);
        let (cr0, cr1) = (chroma.cr[top..][..w].as_chunks().0, chroma.cr[bottom..][..w].as_chunks().0);
        let out = std::slice::from_raw_parts_mut(cbcr.add(cy * stride), w).as_chunks_mut::<2>().0;
        for ((pair, (cb0, cb1)), (cr0, cr1)) in out.iter_mut().zip(cb0.iter().zip(cb1)).zip(cr0.iter().zip(cr1)) {
            *pair = [average(cb0, cb1), average(cr0, cr1)];
        }
    }
}

/// Writes the auxiliary view.
///
/// # Safety
/// `view` must describe a writable NV12 picture of the chroma's size.
pub unsafe fn write_aux(chroma: &Chroma444, view: &Nv12Planes) {
    let (w, h) = (chroma.width, chroma.height);
    let (half, quarter) = (w / 2, w / 4);
    for y in 0..h {
        let (cb, cr) = (&chroma.cb[y * w..][..w], &chroma.cr[y * w..][..w]);
        let out = std::slice::from_raw_parts_mut(view.y.add(y * view.y_stride), w);
        let (left, right) = out.split_at_mut(half);
        for (out, pair) in left.iter_mut().zip(cb.as_chunks::<2>().0) {
            *out = pair[1];
        }
        for (out, pair) in right.iter_mut().zip(cr.as_chunks::<2>().0) {
            *out = pair[1];
        }
    }
    for cy in 0..h / 2 {
        let row = (2 * cy + 1) * w;
        let (cb, cr) = (&chroma.cb[row..][..w], &chroma.cr[row..][..w]);
        // Pairs of the view's Cb and Cr samples; Cb holds columns 4x, Cr columns 4x+2.
        let out = std::slice::from_raw_parts_mut(view.cbcr.add(cy * view.cbcr_stride), w);
        let (left, right) = out.split_at_mut(2 * quarter);
        for (out, quad) in left.as_chunks_mut::<2>().0.iter_mut().zip(cb.as_chunks::<4>().0) {
            *out = [quad[0], quad[2]];
        }
        for (out, quad) in right.as_chunks_mut::<2>().0.iter_mut().zip(cr.as_chunks::<4>().0) {
            *out = [quad[0], quad[2]];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client's side: both views back into 4:4:4 chroma, reverse filter and threshold
    /// included, as MS-RDPEGFX 3.3.8.3.3 describes it.
    fn combine(w: usize, h: usize, main_cbcr: &[u8], aux_y: &[u8], aux_cbcr: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let (half, quarter) = (w / 2, w / 4);
        let mut planes = [vec![0u8; w * h], vec![0u8; w * h]];
        for (c, plane) in planes.iter_mut().enumerate() {
            for y in 0..h {
                for x in 0..half {
                    plane[y * w + 2 * x + 1] = aux_y[y * w + c * half + x];
                }
            }
            for cy in 0..h / 2 {
                let row = (2 * cy + 1) * w;
                for x in 0..quarter {
                    let pair = 2 * (c * quarter + x);
                    plane[row + 4 * x] = aux_cbcr[cy * w + pair];
                    plane[row + 4 * x + 2] = aux_cbcr[cy * w + pair + 1];
                }
            }
            for cy in 0..h / 2 {
                for cx in 0..half {
                    let average = i32::from(main_cbcr[cy * w + 2 * cx + c]);
                    let (top, bottom) = (2 * cy * w + 2 * cx, (2 * cy + 1) * w + 2 * cx);
                    let others = i32::from(plane[top + 1]) + i32::from(plane[bottom]) + i32::from(plane[bottom + 1]);
                    let reversed = (4 * average - others).clamp(0, 255);
                    plane[top] = if (average - reversed).abs() > 30 { reversed } else { average } as u8;
                }
            }
        }
        let [cb, cr] = planes;
        (cb, cr)
    }

    #[test]
    fn views_rebuild_the_444_chroma() {
        let (w, h) = (32usize, 6usize);
        let mut chroma = Chroma444::new(w, h);
        for y in 0..h {
            for x in 0..w {
                // Smooth ramps with a hard edge in the middle, like coloured text on a page.
                let edge = if (13..=18).contains(&x) { 200 } else { 0 };
                chroma.cb[y * w + x] = ((x * 3 + y * 5 + edge) % 256) as u8;
                chroma.cr[y * w + x] = ((255 - x * 2 + y * 7 + edge / 2) % 256) as u8;
            }
        }
        let (mut main_cbcr, mut aux_y, mut aux_cbcr) = (vec![0u8; w * h / 2], vec![0u8; w * h], vec![0u8; w * h / 2]);
        let aux = Nv12Planes {
            y: aux_y.as_mut_ptr(),
            y_stride: w,
            cbcr: aux_cbcr.as_mut_ptr(),
            cbcr_stride: w,
        };
        unsafe {
            write_main_chroma(&chroma, main_cbcr.as_mut_ptr(), w);
            write_aux(&chroma, &aux);
        }
        let (cb, cr) = combine(w, h, &main_cbcr, &aux_y, &aux_cbcr);
        for (name, original, rebuilt) in [("Cb", &chroma.cb, &cb), ("Cr", &chroma.cr, &cr)] {
            for y in 0..h {
                for x in 0..w {
                    let (want, got) = (original[y * w + x], rebuilt[y * w + x]);
                    if x % 2 == 0 && y % 2 == 0 {
                        // Rounding the average costs the reverse filter up to 2; where it is not
                        // applied the average stands in, within the threshold.
                        assert!(got.abs_diff(want) <= 30, "{name} at {x},{y}: {got} for {want}");
                    } else {
                        assert_eq!(got, want, "{name} at {x},{y}");
                    }
                }
            }
        }
        // At the edge the reverse filter applies and restores the sample closely.
        let at = 2 * w + 14;
        assert!(cb[at].abs_diff(chroma.cb[at]) <= 2, "{} for {}", cb[at], chroma.cb[at]);
    }

    #[test]
    fn widths_must_be_whole_macroblocks() {
        assert!(fits(1920, 1080));
        assert!(fits(3840, 2160));
        assert!(!fits(1366, 768));
        assert!(!fits(1920, 1081));
    }
}
