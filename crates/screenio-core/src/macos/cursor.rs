use crate::{CursorPosition, CursorShape, Error, Result};
use core_graphics::geometry::CGPoint;
use objc2::rc::autoreleasepool;
use core_graphics::display::CGDisplay;
use objc2::{rc::Retained, Message};
use objc2_app_kit::{NSBitmapImageRep, NSCursor, NSImage};
use std::{ffi::c_void, ptr};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const c_void) -> *const c_void;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    fn CFRelease(cf: *const c_void);
    fn CGCursorIsVisible() -> u32;
    // Private, but stable for over a decade; rustdesk relies on it for the same purpose.
    fn CGSCurrentCursorSeed() -> i32;
}

pub fn cursor_position() -> Result<CursorPosition> {
    unsafe {
        let event = CGEventCreate(ptr::null());
        if event.is_null() {
            return Err(Error::Os);
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        Ok(CursorPosition {
            x: point.x as i32,
            y: point.y as i32,
            visible: CGCursorIsVisible() != 0,
        })
    }
}

// `currentSystemCursor` is deprecated in favour of letting ScreenCaptureKit draw the cursor into
// the frames; a remote desktop needs the shape separately, and no other public API returns it.
pub fn cursor_shape_id() -> Result<u64> {
    Ok(unsafe { CGSCurrentCursorSeed() } as u32 as u64)
}

#[allow(deprecated)]
pub fn cursor_shape() -> Result<CursorShape> {
    autoreleasepool(|_| {
        let cursor = NSCursor::currentSystemCursor().ok_or(Error::Os)?;
        let hot = cursor.hotSpot();
        let image = cursor.image();
        let size = image.size();
        if size.width <= 0.0 || size.height <= 0.0 {
            return Err(Error::Os);
        }
        let rep = pick_rep(&image, size.width * primary_scale())?;
        let (w, h) = (rep.pixelsWide(), rep.pixelsHigh());
        if w <= 0 || h <= 0 {
            return Err(Error::Os);
        }
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                match rep.colorAtX_y(x, y) {
                    Some(color) => rgba.extend_from_slice(&[
                        (color.redComponent() * 255.0) as u8,
                        (color.greenComponent() * 255.0) as u8,
                        (color.blueComponent() * 255.0) as u8,
                        (color.alphaComponent() * 255.0) as u8,
                    ]),
                    None => rgba.extend_from_slice(&[0, 0, 0, 0]),
                }
            }
        }
        // The hotspot is in points; the bitmap may be a 2x rendition.
        let sx = w as f64 / size.width;
        let sy = h as f64 / size.height;
        Ok(CursorShape {
            id: unsafe { CGSCurrentCursorSeed() } as u32 as u64,
            width: w as u32,
            height: h as u32,
            hot_x: (hot.x * sx) as i32,
            hot_y: (hot.y * sy) as i32,
            rgba,
        })
    })
}

/// Captured pixels per point on the main display, so the cursor matches the frames.
fn primary_scale() -> f64 {
    let display = CGDisplay::main();
    let points = display.bounds().size.width;
    let (pixels, _) = super::display::pixel_size(&display);
    if points > 0.0 {
        pixels as f64 / points
    } else {
        1.0
    }
}

/// System cursors carry one bitmap per scale; take the one closest to `target_width` pixels,
/// falling back to a TIFF round trip when no representation is a bitmap.
fn pick_rep(image: &NSImage, target_width: f64) -> Result<Retained<NSBitmapImageRep>> {
    let mut best: Option<(f64, Retained<NSBitmapImageRep>)> = None;
    for rep in image.representations().iter() {
        let Some(bitmap) = rep.downcast_ref::<NSBitmapImageRep>() else {
            continue;
        };
        let distance = (bitmap.pixelsWide() as f64 - target_width).abs();
        if best.as_ref().map_or(true, |(d, _)| distance < *d) {
            best = Some((distance, bitmap.retain()));
        }
    }
    if let Some((_, rep)) = best {
        return Ok(rep);
    }
    let data = image.TIFFRepresentation().ok_or(Error::Os)?;
    NSBitmapImageRep::imageRepWithData(&data).ok_or(Error::Os)
}
