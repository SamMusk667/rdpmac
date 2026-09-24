use crate::{DisplayInfo, Error, Result};
use core_graphics::display::CGDisplay;

/// Pixel size of a display from its current mode, falling back to its point size.
pub fn pixel_size(display: &CGDisplay) -> (u32, u32) {
    let bounds = display.bounds();
    match display.display_mode() {
        Some(mode) => (mode.pixel_width() as u32, mode.pixel_height() as u32),
        None => (bounds.size.width as u32, bounds.size.height as u32),
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
}

/// Displays that are connected but asleep are not "active"; list them anyway so a server can
/// keep addressing a display whose panel has been powered down.
fn online_displays() -> Result<Vec<u32>> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    let status = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status != 0 {
        return Err(Error::Os);
    }
    Ok(ids[..count as usize].to_vec())
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    let mut ids = CGDisplay::active_displays().map_err(|_| Error::Os)?;
    if ids.is_empty() {
        ids = online_displays()?;
    }
    Ok(ids
        .into_iter()
        .map(|id| {
            let display = CGDisplay::new(id);
            let bounds = display.bounds();
            let (width, height) = pixel_size(&display);
            let scale = if bounds.size.width > 0.0 {
                width as f32 / bounds.size.width as f32
            } else {
                1.0
            };
            DisplayInfo {
                id,
                x: bounds.origin.x as i32,
                y: bounds.origin.y as i32,
                width,
                height,
                scale,
                primary: display.is_main(),
                name: format!("Display {id}"),
            }
        })
        .collect())
}
