//! Cursor position and shape as RDP pointer updates.

use ironrdp_pdu::pointer::PointerPositionAttribute;
use ironrdp_server::{DisplayUpdate, RGBAPointer};
use screenio_core::CursorShape;

/// Largest shape the pointer update in this IronRDP release carries. Larger shapes (the
/// fast-path large pointer update, up to 384x384) wait for the next IronRDP release.
const POINTER_MAX: u32 = 96;
/// Pointer cache slots we cycle through; every client offers at least this many.
const CACHE_SLOTS: u16 = 20;

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

    /// The update that installs `shape` as the client's pointer, or `None` if it cannot be sent.
    pub fn update_for(&mut self, shape: &CursorShape) -> Option<DisplayUpdate> {
        let too_large = shape.width > POINTER_MAX || shape.height > POINTER_MAX;
        let inconsistent = shape.rgba.len() != (shape.width * shape.height * 4) as usize;
        if shape.width == 0 || shape.height == 0 || too_large || inconsistent {
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
