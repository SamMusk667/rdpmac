//! BGRA to NV12 in full-range BT.709, the colour space MS-RDPEGFX 3.3.8.3.1 prescribes for AVC420,
//! and to full-range 4:4:4 for AVC444.
//!
//! Given BGRA, VideoToolbox converts to limited range, which a client decoding per the
//! specification shows with grey blacks and whites and pale colours. Converting here with vImage
//! and handing VideoToolbox full-range YUV keeps the samples as the client expects them.

use std::ffi::c_void;

#[repr(C)]
struct VImageBuffer {
    data: *mut c_void,
    height: usize,
    width: usize,
    row_bytes: usize,
}

#[repr(C)]
struct PixelRange {
    yp_bias: i32,
    cbcr_bias: i32,
    yp_range_max: i32,
    cbcr_range_max: i32,
    yp_max: i32,
    yp_min: i32,
    cbcr_max: i32,
    cbcr_min: i32,
}

#[repr(C, align(16))]
struct ConversionInfo {
    opaque: [u8; 128],
}

const ARGB8888: u32 = 0;
const YP8_CBCR8_420: u32 = 4;
const AYP_CB_CR8_444: u32 = 5;
/// Rows converted to 4:4:4 at a time, few enough for a band to stay in cache while it is used.
/// Even, so that bands hold whole pairs of rows.
const BAND: usize = 64;
const NO_FLAGS: u32 = 0;
/// vImage reads ARGB; entry i names the source byte that becomes ARGB channel i of a BGRA pixel.
const BGRA_AS_ARGB: [u8; 4] = [3, 2, 1, 0];

#[link(name = "Accelerate", kind = "framework")]
extern "C" {
    static kvImage_ARGBToYpCbCrMatrix_ITU_R_709_2: *const c_void;
    fn vImageConvert_ARGBToYpCbCr_GenerateConversion(
        matrix: *const c_void,
        range: *const PixelRange,
        info: *mut ConversionInfo,
        argb_type: u32,
        ycbcr_type: u32,
        flags: u32,
    ) -> isize;
    fn vImageConvert_ARGB8888To420Yp8_CbCr8(
        src: *const VImageBuffer,
        dest_yp: *const VImageBuffer,
        dest_cbcr: *const VImageBuffer,
        info: *const ConversionInfo,
        permute: *const u8,
        flags: u32,
    ) -> isize;
    fn vImageConvert_ARGB8888To444AYpCbCr8(
        src: *const VImageBuffer,
        dest: *const VImageBuffer,
        info: *const ConversionInfo,
        permute: *const u8,
        flags: u32,
    ) -> isize;
    fn vImageConvert_ARGB8888toPlanar8(
        src: *const VImageBuffer,
        dest_a: *const VImageBuffer,
        dest_r: *const VImageBuffer,
        dest_g: *const VImageBuffer,
        dest_b: *const VImageBuffer,
        flags: u32,
    ) -> isize;
}

/// vImage's conversion from ARGB to full-range BT.709 in the given YpCbCr layout.
fn conversion(ycbcr_type: u32) -> Option<Box<ConversionInfo>> {
    let full = PixelRange {
        yp_bias: 0,
        cbcr_bias: 128,
        yp_range_max: 255,
        cbcr_range_max: 255,
        yp_max: 255,
        yp_min: 0,
        cbcr_max: 255,
        cbcr_min: 0,
    };
    let mut info = Box::new(ConversionInfo { opaque: [0; 128] });
    let status = unsafe {
        vImageConvert_ARGBToYpCbCr_GenerateConversion(
            kvImage_ARGBToYpCbCrMatrix_ITU_R_709_2,
            &full,
            &mut *info,
            ARGB8888,
            ycbcr_type,
            NO_FLAGS,
        )
    };
    (status == 0).then_some(info)
}

/// One pixel in full-range BT.709, for the edge rows and columns vImage leaves out.
pub fn ycbcr(b: u8, g: u8, r: u8) -> (u8, u8, u8) {
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556 + 128.0;
    let cr = (r - y) / 1.5748 + 128.0;
    let clamp = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    (clamp(y), clamp(cb), clamp(cr))
}

/// Where the converted picture goes: the planes of a locked bi-planar 420 pixel buffer.
pub struct Nv12Planes {
    pub y: *mut u8,
    pub y_stride: usize,
    pub cbcr: *mut u8,
    pub cbcr_stride: usize,
}

pub struct Converter {
    info: Box<ConversionInfo>,
}

// The conversion info is plain data that vImage only reads.
unsafe impl Send for Converter {}

impl Converter {
    pub fn new() -> Option<Self> {
        Some(Self {
            info: conversion(YP8_CBCR8_420)?,
        })
    }

    /// Converts `width` x `height` BGRA pixels. The planes must hold that many luma samples and
    /// the rounded-up half in each direction of chroma pairs.
    ///
    /// # Safety
    /// The plane pointers and strides must describe writable memory of that size.
    pub unsafe fn convert(&self, bgra: &[u8], stride: usize, width: usize, height: usize, planes: &Nv12Planes) -> bool {
        if width == 0 || height == 0 || stride < width * 4 || bgra.len() < stride * (height - 1) + width * 4 {
            return false;
        }
        // vImage works on 2x2 blocks; an odd last row or column is filled below.
        let (even_w, even_h) = (width & !1, height & !1);
        if even_w > 0 && even_h > 0 {
            let src = VImageBuffer {
                data: bgra.as_ptr() as *mut c_void,
                height: even_h,
                width: even_w,
                row_bytes: stride,
            };
            let luma = VImageBuffer {
                data: planes.y.cast(),
                height: even_h,
                width: even_w,
                row_bytes: planes.y_stride,
            };
            let chroma = VImageBuffer {
                data: planes.cbcr.cast(),
                height: even_h / 2,
                width: even_w / 2,
                row_bytes: planes.cbcr_stride,
            };
            let status = vImageConvert_ARGB8888To420Yp8_CbCr8(
                &src,
                &luma,
                &chroma,
                &*self.info,
                BGRA_AS_ARGB.as_ptr(),
                NO_FLAGS,
            );
            if status != 0 {
                return false;
            }
        }
        let pixel = |x: usize, y: usize| {
            let p = &bgra[y * stride + x * 4..][..3];
            ycbcr(p[0], p[1], p[2])
        };
        // Odd edges: each chroma pair takes its colour from the top-left pixel of its block.
        if width % 2 == 1 {
            let x = width - 1;
            for y in 0..height {
                let (luma, cb, cr) = pixel(x, y);
                *planes.y.add(y * planes.y_stride + x) = luma;
                if y % 2 == 0 {
                    let pair = planes.cbcr.add((y / 2) * planes.cbcr_stride + (x / 2) * 2);
                    *pair = cb;
                    *pair.add(1) = cr;
                }
            }
        }
        if height % 2 == 1 {
            let y = height - 1;
            for x in 0..even_w {
                let (luma, cb, cr) = pixel(x, y);
                *planes.y.add(y * planes.y_stride + x) = luma;
                if x % 2 == 0 {
                    let pair = planes.cbcr.add((y / 2) * planes.cbcr_stride + x);
                    *pair = cb;
                    *pair.add(1) = cr;
                }
            }
        }
        true
    }
}

/// The chroma planes of a full-range BT.709 4:4:4 picture, or of a band of its rows, each
/// `width` samples a row.
pub struct Chroma444 {
    pub width: usize,
    pub height: usize,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
}

impl Chroma444 {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cb: vec![128; width * height],
            cr: vec![128; width * height],
        }
    }
}

/// BGRA to full-range BT.709 4:4:4: luma into a plane of the caller's, chroma a band of rows at a
/// time into a [`Chroma444`] the caller reads while it is in cache.
pub struct Converter444 {
    info: Box<ConversionInfo>,
    packed: Vec<u8>,
    alpha: Vec<u8>,
    band: Chroma444,
}

// As for `Converter`; the scratch buffers belong to whoever holds the converter.
unsafe impl Send for Converter444 {}

impl Converter444 {
    pub fn new() -> Option<Self> {
        Some(Self {
            info: conversion(AYP_CB_CR8_444)?,
            packed: Vec::new(),
            alpha: Vec::new(),
            band: Chroma444::new(0, 0),
        })
    }

    /// Converts `width` x `height` BGRA pixels, writing luma rows `y_stride` apart from `y` and
    /// handing each band's chroma to `band` along with the band's first row.
    ///
    /// # Safety
    /// `y` and `y_stride` must describe a writable plane of that many rows and samples.
    pub unsafe fn convert(
        &mut self,
        bgra: &[u8],
        stride: usize,
        (width, height): (usize, usize),
        (y, y_stride): (*mut u8, usize),
        mut band: impl FnMut(usize, &Chroma444),
    ) -> bool {
        if width == 0 || height == 0 || stride < width * 4 || bgra.len() < stride * (height - 1) + width * 4 {
            return false;
        }
        if y_stride < width {
            return false;
        }
        self.packed.resize(width * 4 * BAND, 0);
        self.alpha.resize(width * BAND, 0);
        if self.band.width != width {
            self.band = Chroma444::new(width, BAND);
        }
        let mut top = 0;
        while top < height {
            let rows = BAND.min(height - top);
            let src = VImageBuffer {
                data: bgra.as_ptr().add(top * stride) as *mut c_void,
                height: rows,
                width,
                row_bytes: stride,
            };
            let packed = VImageBuffer {
                data: self.packed.as_mut_ptr().cast(),
                height: rows,
                width,
                row_bytes: width * 4,
            };
            let status =
                vImageConvert_ARGB8888To444AYpCbCr8(&src, &packed, &*self.info, BGRA_AS_ARGB.as_ptr(), NO_FLAGS);
            if status != 0 {
                return false;
            }
            let plane = |data: *mut u8, row_bytes: usize| VImageBuffer {
                data: data.cast(),
                height: rows,
                width,
                row_bytes,
            };
            // The packed pixels are A, Y', Cb, Cr: split them as if they were A, R, G, B.
            let status = vImageConvert_ARGB8888toPlanar8(
                &packed,
                &plane(self.alpha.as_mut_ptr(), width),
                &plane(y.add(top * y_stride), y_stride),
                &plane(self.band.cb.as_mut_ptr(), width),
                &plane(self.band.cr.as_mut_ptr(), width),
                NO_FLAGS,
            );
            if status != 0 {
                return false;
            }
            self.band.height = rows;
            band(top, &self.band);
            top += rows;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(width: usize, height: usize, bgra: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let converter = Converter::new().expect("vImage conversion");
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let mut y = vec![0u8; width * height];
        let mut cbcr = vec![0u8; cw * 2 * ch];
        let planes = Nv12Planes {
            y: y.as_mut_ptr(),
            y_stride: width,
            cbcr: cbcr.as_mut_ptr(),
            cbcr_stride: cw * 2,
        };
        assert!(unsafe { converter.convert(bgra, width * 4, width, height, &planes) });
        (y, cbcr)
    }

    fn solid(width: usize, height: usize, bgr: [u8; 3]) -> Vec<u8> {
        (0..width * height).flat_map(|_| [bgr[0], bgr[1], bgr[2], 255]).collect()
    }

    #[test]
    fn primaries_land_on_full_range_bt709() {
        for (bgr, expected) in [
            ([0, 0, 0], (0, 128, 128)),
            ([255, 255, 255], (255, 128, 128)),
            ([0, 0, 255], ycbcr(0, 0, 255)),
            ([0, 255, 0], ycbcr(0, 255, 0)),
            ([255, 0, 0], ycbcr(255, 0, 0)),
        ] {
            let (y, cbcr) = run(4, 4, &solid(4, 4, bgr));
            let got = (y[0], cbcr[0], cbcr[1]);
            let close = |a: u8, b: u8| a.abs_diff(b) <= 1;
            assert!(
                close(got.0, expected.0) && close(got.1, expected.1) && close(got.2, expected.2),
                "{bgr:?}: got {got:?}, expected {expected:?}"
            );
        }
        assert_eq!(ycbcr(0, 0, 255).0, 54, "red's luma under BT.709");
    }

    #[test]
    fn odd_sizes_fill_the_last_row_and_column() {
        let (y, cbcr) = run(5, 3, &solid(5, 3, [255, 255, 255]));
        assert!(y.iter().all(|&v| v == 255), "every luma sample written: {y:?}");
        assert!(cbcr.iter().all(|&v| v.abs_diff(128) <= 1), "every chroma pair written: {cbcr:?}");
    }

    #[test]
    fn converts_to_444_across_bands() {
        // Taller than one band, with a stride wider than the rows.
        let (width, height, stride) = (18usize, BAND + 9, 18 * 4 + 12);
        let bgra: Vec<u8> = (0..stride * height).map(|i| ((i * 37 + i / stride * 11) % 256) as u8).collect();
        let mut converter = Converter444::new().expect("vImage conversion");
        let mut chroma = Chroma444::new(width, height);
        let y_stride = width + 3;
        let mut y = vec![0u8; y_stride * height];
        let mut bands = Vec::new();
        let converted = unsafe {
            converter.convert(&bgra, stride, (width, height), (y.as_mut_ptr(), y_stride), |top, band| {
                bands.push((top, band.height));
                let rows = top * width..(top + band.height) * width;
                chroma.cb[rows.clone()].copy_from_slice(&band.cb[..band.height * width]);
                chroma.cr[rows].copy_from_slice(&band.cr[..band.height * width]);
            })
        };
        assert!(converted);
        assert_eq!(bands, [(0, BAND), (BAND, 9)]);
        for row in 0..height {
            for x in 0..width {
                let p = &bgra[row * stride + x * 4..][..3];
                let (luma, cb, cr) = ycbcr(p[0], p[1], p[2]);
                let at = row * width + x;
                assert!(y[row * y_stride + x].abs_diff(luma) <= 1, "Y at {x},{row}");
                assert!(chroma.cb[at].abs_diff(cb) <= 1, "Cb at {x},{row}");
                assert!(chroma.cr[at].abs_diff(cr) <= 1, "Cr at {x},{row}");
            }
        }
    }
}
