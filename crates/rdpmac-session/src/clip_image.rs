//! Pictures on the clipboard: Windows' device-independent bitmaps (CF_DIB, CF_DIBV5), read and
//! written here, and on macOS PNG and TIFF through ImageIO.

/// A picture as premultiplied BGRA, rows top to bottom, in sRGB.
#[derive(Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bitmap({}x{})", self.width, self.height)
    }
}

/// Larger pictures are neither sent nor taken: about 8K by 4K, as a 24-bit DIB 100 MB.
pub const MAX_PIXELS: usize = 8192 * 4320;

const BITMAPINFOHEADER: usize = 40;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;
/// 72 dots per inch.
const PIXELS_PER_METRE: u32 = 2835;

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// The picture as a CF_DIB: a BITMAPINFOHEADER and 24-bit rows bottom to top, over white.
/// Windows programs disagree on what the fourth byte of a 32-bit DIB means, so transparency goes
/// only with the PNG.
pub fn to_dib(bitmap: &Bitmap) -> Vec<u8> {
    let (width, height) = (bitmap.width, bitmap.height);
    let stride = (width * 3).div_ceil(4) * 4;
    let mut dib = Vec::with_capacity(BITMAPINFOHEADER + stride * height);
    for value in [BITMAPINFOHEADER as u32, width as u32, height as u32] {
        dib.extend_from_slice(&value.to_le_bytes());
    }
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&24u16.to_le_bytes());
    for value in [BI_RGB, (stride * height) as u32, PIXELS_PER_METRE, PIXELS_PER_METRE, 0, 0] {
        dib.extend_from_slice(&value.to_le_bytes());
    }
    for row in bitmap.bgra.chunks_exact(width * 4).rev() {
        let start = dib.len();
        for pixel in row.as_chunks::<4>().0 {
            // Premultiplied colour over white.
            let under = 255 - pixel[3];
            dib.extend_from_slice(&[pixel[0].saturating_add(under), pixel[1].saturating_add(under), pixel[2].saturating_add(under)]);
        }
        dib.resize(start + stride, 0);
    }
    dib
}

/// One channel of a pixel by its bit mask, scaled to 8 bits.
#[derive(Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    max: u32,
}

impl Channel {
    fn new(mask: u32) -> Self {
        let shift = if mask == 0 { 0 } else { mask.trailing_zeros() };
        Self {
            mask,
            shift,
            max: mask.checked_shr(shift).unwrap_or(0),
        }
    }

    fn get(self, value: u32) -> Option<u8> {
        if self.max == 0 {
            return None;
        }
        let v = (value & self.mask) >> self.shift;
        Some(((u64::from(v) * 255 + u64::from(self.max) / 2) / u64::from(self.max)) as u8)
    }
}

/// A CF_DIB or CF_DIBV5: any BITMAPINFOHEADER up to BITMAPV5HEADER, 1 to 32 bits a pixel,
/// uncompressed or with bit fields, rows either way up.
pub fn from_dib(data: &[u8]) -> Option<Bitmap> {
    let header = u32_at(data, 0)? as usize;
    if header < BITMAPINFOHEADER {
        return None;
    }
    let width = i32::from_le_bytes(data.get(4..8)?.try_into().ok()?);
    let raw_height = i32::from_le_bytes(data.get(8..12)?.try_into().ok()?);
    let bits = u16_at(data, 14)? as usize;
    let compression = u32_at(data, 16)?;
    let used = u32_at(data, 32)? as usize;
    let width = usize::try_from(width).ok().filter(|&w| w > 0)?;
    let height = raw_height.unsigned_abs() as usize;
    if height == 0 || width.checked_mul(height)? > MAX_PIXELS {
        return None;
    }
    let bitfields = matches!(compression, BI_BITFIELDS | BI_ALPHABITFIELDS);
    if !(compression == BI_RGB || bitfields && matches!(bits, 16 | 32)) {
        return None;
    }
    // Masks sit inside V2 and later headers, else right after the header.
    let (masks_at, trailing_masks) = match (bitfields, header) {
        (true, BITMAPINFOHEADER) => (BITMAPINFOHEADER, if compression == BI_ALPHABITFIELDS { 16 } else { 12 }),
        _ => (BITMAPINFOHEADER, 0),
    };
    let alpha_mask = if header >= 56 || trailing_masks == 16 { u32_at(data, masks_at + 12)? } else { 0 };
    let masks = match (bitfields, bits) {
        (true, _) => [u32_at(data, masks_at)?, u32_at(data, masks_at + 4)?, u32_at(data, masks_at + 8)?, alpha_mask],
        (false, 16) => [0x7C00, 0x03E0, 0x001F, 0],
        (false, 32) => [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000],
        _ => [0; 4],
    };
    let palette_len = if bits <= 8 { if used == 0 { 1 << bits } else { used.min(1 << bits) } } else { used };
    let palette_at = header + trailing_masks;
    let pixels_at = palette_at + palette_len * 4;
    let palette = data.get(palette_at..pixels_at)?;
    let stride = (width * bits).div_ceil(32) * 4;
    let pixels = data.get(pixels_at..pixels_at.checked_add(stride.checked_mul(height)?)?)?;

    let [red, green, blue, alpha] = masks.map(Channel::new);
    let mut bgra = vec![0u8; width * height * 4];
    let mut any_alpha = false;
    for y in 0..height {
        // Positive heights store the bottom row first.
        let source = if raw_height > 0 { height - 1 - y } else { y };
        let row = &pixels[source * stride..][..stride];
        let out = &mut bgra[y * width * 4..][..width * 4];
        for x in 0..width {
            let pixel = &mut out[x * 4..x * 4 + 4];
            match bits {
                1 | 2 | 4 | 8 => {
                    let index = (row[x * bits / 8] >> (8 - bits - x * bits % 8)) & ((1 << bits) - 1) as u8;
                    let entry = palette.get(usize::from(index) * 4..usize::from(index) * 4 + 3)?;
                    pixel.copy_from_slice(&[entry[0], entry[1], entry[2], 255]);
                }
                24 => {
                    let p = &row[x * 3..x * 3 + 3];
                    pixel.copy_from_slice(&[p[0], p[1], p[2], 255]);
                }
                16 | 32 => {
                    let value = if bits == 16 {
                        u32::from(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]))
                    } else {
                        u32::from_le_bytes(row[x * 4..x * 4 + 4].try_into().ok()?)
                    };
                    let a = alpha.get(value).unwrap_or(255);
                    any_alpha |= a != 0;
                    pixel.copy_from_slice(&[blue.get(value)?, green.get(value)?, red.get(value)?, a]);
                }
                _ => return None,
            }
        }
    }
    if bits == 32 && !any_alpha {
        // A fourth byte that is zero everywhere is padding, not transparency.
        bgra.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
    }
    premultiply(&mut bgra);
    Some(Bitmap { width, height, bgra })
}

fn premultiply(bgra: &mut [u8]) {
    for pixel in bgra.as_chunks_mut::<4>().0 {
        let a = u16::from(pixel[3]);
        if a != 255 {
            for c in &mut pixel[..3] {
                *c = ((u16::from(*c) * a + 127) / 255) as u8;
            }
        }
    }
}

/// PNG and TIFF through ImageIO, colours converted to sRGB.
#[cfg(target_os = "macos")]
pub mod codec {
    use std::ffi::{c_char, c_void};
    use std::ptr;

    use super::{Bitmap, MAX_PIXELS};

    type CFRef = *const c_void;

    #[repr(C)]
    struct Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    const UTF8: u32 = 0x0800_0100;
    /// kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little: premultiplied BGRA in memory.
    const BGRA_PREMULTIPLIED: u32 = 2 | (2 << 12);

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: CFRef);
        fn CFDataCreate(allocator: CFRef, bytes: *const u8, length: isize) -> CFRef;
        fn CFDataCreateMutable(allocator: CFRef, capacity: isize) -> CFRef;
        fn CFDataGetLength(data: CFRef) -> isize;
        fn CFDataGetBytePtr(data: CFRef) -> *const u8;
        fn CFStringCreateWithCString(allocator: CFRef, text: *const c_char, encoding: u32) -> CFRef;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        static kCGColorSpaceSRGB: CFRef;
        fn CGColorSpaceCreateWithName(name: CFRef) -> CFRef;
        fn CGImageGetWidth(image: CFRef) -> usize;
        fn CGImageGetHeight(image: CFRef) -> usize;
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: CFRef,
            info: u32,
        ) -> CFRef;
        fn CGBitmapContextCreateImage(context: CFRef) -> CFRef;
        fn CGContextDrawImage(context: CFRef, rect: Rect, image: CFRef);
    }

    #[link(name = "ImageIO", kind = "framework")]
    extern "C" {
        fn CGImageSourceCreateWithData(data: CFRef, options: CFRef) -> CFRef;
        fn CGImageSourceGetType(source: CFRef) -> CFRef;
        fn CGImageSourceGetCount(source: CFRef) -> usize;
        fn CGImageSourceCreateImageAtIndex(source: CFRef, index: usize, options: CFRef) -> CFRef;
        fn CGImageDestinationCreateWithData(data: CFRef, kind: CFRef, count: usize, options: CFRef) -> CFRef;
        fn CGImageDestinationAddImage(destination: CFRef, image: CFRef, properties: CFRef);
        fn CGImageDestinationFinalize(destination: CFRef) -> bool;
    }

    /// Releases a Core Foundation object when dropped.
    struct Owned(CFRef);

    impl Owned {
        fn new(cf: CFRef) -> Option<Self> {
            (!cf.is_null()).then_some(Self(cf))
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }

    /// A bitmap context over `bgra`, which must outlive it.
    fn context(bgra: &mut [u8], width: usize, height: usize) -> Option<Owned> {
        let space = Owned::new(unsafe { CGColorSpaceCreateWithName(kCGColorSpaceSRGB) })?;
        Owned::new(unsafe {
            CGBitmapContextCreate(bgra.as_mut_ptr().cast(), width, height, 8, width * 4, space.0, BGRA_PREMULTIPLIED)
        })
    }

    /// The first picture in PNG, TIFF, JPEG or any file ImageIO reads.
    pub fn decode(file: &[u8]) -> Option<Bitmap> {
        let data = Owned::new(unsafe { CFDataCreate(ptr::null(), file.as_ptr(), file.len() as isize) })?;
        let source = Owned::new(unsafe { CGImageSourceCreateWithData(data.0, ptr::null()) })?;
        // Asked for a picture from data it does not recognise, ImageIO on macOS 27 traps rather
        // than returning none.
        if unsafe { CGImageSourceGetType(source.0).is_null() || CGImageSourceGetCount(source.0) == 0 } {
            return None;
        }
        let image = Owned::new(unsafe { CGImageSourceCreateImageAtIndex(source.0, 0, ptr::null()) })?;
        let (width, height) = unsafe { (CGImageGetWidth(image.0), CGImageGetHeight(image.0)) };
        if width == 0 || height == 0 || width.checked_mul(height)? > MAX_PIXELS {
            return None;
        }
        let mut bgra = vec![0u8; width * height * 4];
        let context = context(&mut bgra, width, height)?;
        let rect = Rect { x: 0.0, y: 0.0, width: width as f64, height: height as f64 };
        unsafe { CGContextDrawImage(context.0, rect, image.0) };
        drop(context);
        Some(Bitmap { width, height, bgra })
    }

    fn encode(bitmap: &Bitmap, kind: &[u8]) -> Option<Vec<u8>> {
        let mut bgra = bitmap.bgra.clone();
        let context = context(&mut bgra, bitmap.width, bitmap.height)?;
        let image = Owned::new(unsafe { CGBitmapContextCreateImage(context.0) })?;
        let kind = Owned::new(unsafe { CFStringCreateWithCString(ptr::null(), kind.as_ptr().cast(), UTF8) })?;
        let data = Owned::new(unsafe { CFDataCreateMutable(ptr::null(), 0) })?;
        let destination = Owned::new(unsafe { CGImageDestinationCreateWithData(data.0, kind.0, 1, ptr::null()) })?;
        unsafe { CGImageDestinationAddImage(destination.0, image.0, ptr::null()) };
        if !unsafe { CGImageDestinationFinalize(destination.0) } {
            return None;
        }
        let length = usize::try_from(unsafe { CFDataGetLength(data.0) }).ok()?;
        let bytes = unsafe { CFDataGetBytePtr(data.0) };
        (!bytes.is_null()).then(|| unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec())
    }

    pub fn png(bitmap: &Bitmap) -> Option<Vec<u8>> {
        encode(bitmap, b"public.png\0")
    }

    pub fn tiff(bitmap: &Bitmap) -> Option<Vec<u8>> {
        encode(bitmap, b"public.tiff\0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two by two: opaque red, half-transparent green, opaque blue, fully transparent.
    fn sample() -> Bitmap {
        Bitmap {
            width: 2,
            height: 2,
            bgra: vec![0, 0, 255, 255, 0, 64, 0, 128, 255, 0, 0, 255, 0, 0, 0, 0],
        }
    }

    #[test]
    fn a_dib_is_24_bits_bottom_up_over_white() {
        let dib = to_dib(&sample());
        assert_eq!(u32_at(&dib, 0), Some(40));
        assert_eq!(u16_at(&dib, 14), Some(24));
        // Rows of 6 bytes padded to 8; the bottom row (blue, transparent) comes first.
        assert_eq!(dib.len(), 40 + 2 * 8);
        assert_eq!(&dib[40..46], &[255, 0, 0, 255, 255, 255]);
        assert_eq!(&dib[48..54], &[0, 0, 255, 127, 191, 127]);
        let back = from_dib(&dib).expect("reads back");
        assert_eq!(&back.bgra[..4], &[0, 0, 255, 255]);
        assert_eq!(&back.bgra[12..], &[255, 255, 255, 255]);
    }

    fn dib(header: usize, bits: u16, compression: u32, height: i32, extra: &[u8], rows: &[u8]) -> Vec<u8> {
        let mut d = vec![0u8; header];
        d[..4].copy_from_slice(&(header as u32).to_le_bytes());
        d[4..8].copy_from_slice(&2i32.to_le_bytes());
        d[8..12].copy_from_slice(&height.to_le_bytes());
        d[12..14].copy_from_slice(&1u16.to_le_bytes());
        d[14..16].copy_from_slice(&bits.to_le_bytes());
        d[16..20].copy_from_slice(&compression.to_le_bytes());
        d.extend_from_slice(extra);
        d.extend_from_slice(rows);
        d
    }

    #[test]
    fn a_v5_dib_with_alpha_is_premultiplied() {
        let mut header = [0u8; 124];
        for (i, mask) in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000].iter().enumerate() {
            header[40 + i * 4..44 + i * 4].copy_from_slice(&mask.to_le_bytes());
        }
        // Top-down: first row red opaque and green half-transparent.
        let rows = [0, 0, 255, 255, 0, 255, 0, 128, 255, 0, 0, 255, 0, 0, 0, 0];
        let mut d = dib(124, 32, BI_BITFIELDS, -2, &[], &rows);
        d[40..124].copy_from_slice(&header[40..124]);
        let b = from_dib(&d).expect("V5 DIB");
        assert_eq!(&b.bgra[..8], &[0, 0, 255, 255, 0, 128, 0, 128]);
        assert_eq!(&b.bgra[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_32_bit_dib_with_zero_fourth_bytes_is_opaque() {
        let rows = [1, 2, 3, 0, 4, 5, 6, 0];
        let b = from_dib(&dib(40, 32, BI_RGB, 1, &[], &rows)).expect("DIB");
        assert_eq!(b.bgra, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn masks_after_the_header_and_palettes_are_read() {
        // 16 bits, 5-6-5 masks after a BITMAPINFOHEADER: white then pure red.
        let masks: Vec<u8> = [0xF800u32, 0x07E0, 0x001F].iter().flat_map(|m| m.to_le_bytes()).collect();
        let b = from_dib(&dib(40, 16, BI_BITFIELDS, 1, &masks, &[0xFF, 0xFF, 0x00, 0xF8])).expect("565");
        assert_eq!(b.bgra, [255, 255, 255, 255, 0, 0, 255, 255]);
        // 1 bit with a two-entry palette: black, white.
        let palette = [0, 0, 0, 0, 255, 255, 255, 0];
        let b = from_dib(&dib(40, 1, BI_RGB, 1, &palette, &[0b0100_0000, 0, 0, 0])).expect("1 bit");
        assert_eq!(b.bgra, [0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn truncated_or_strange_dibs_are_refused() {
        let good = dib(40, 24, BI_RGB, 1, &[], &[0; 8]);
        assert!(from_dib(&good).is_some());
        assert!(from_dib(&good[..45]).is_none());
        assert!(from_dib(&dib(40, 24, 1, 1, &[], &[0; 8])).is_none(), "RLE");
        let mut core = good.clone();
        core[..4].copy_from_slice(&12u32.to_le_bytes());
        assert!(from_dib(&core).is_none(), "OS/2 header");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn png_and_tiff_round_trip_through_imageio() {
        let original = sample();
        for file in [codec::png(&original).expect("PNG"), codec::tiff(&original).expect("TIFF")] {
            let back = codec::decode(&file).expect("decodes");
            assert_eq!((back.width, back.height), (2, 2));
            for (a, b) in back.bgra.iter().zip(&original.bgra) {
                assert!(a.abs_diff(*b) <= 1, "{:?} vs {:?}", back.bgra, original.bgra);
            }
        }
        assert!(codec::decode(b"not a picture").is_none());
    }
}
