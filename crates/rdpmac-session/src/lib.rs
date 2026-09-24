//! The pieces IronRDP asks a server to provide, implemented on libscreenio.
//!
//! [`display::DisplayHandler`] turns captured frames and cursor state into `DisplayUpdate`s,
//! [`input::InputHandler`] turns RDP keyboard and mouse events into injected input, and
//! [`monitor::MonitorPolicy`] is the seam where multi-monitor layouts plug in later.

pub mod cursor;
pub mod display;
pub mod input;
pub mod monitor;
pub mod pattern;

use std::sync::{Arc, Mutex};

use screenio_core::DisplayInfo;

/// Where the served display sits and how its pixels map to macOS coordinates.
///
/// RDP works in the display's pixels; macOS input and cursor APIs work in points scaled by
/// `scale` and offset by the display's origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

impl Geometry {
    pub fn from_display(d: &DisplayInfo) -> Self {
        Self {
            id: d.id,
            x: d.x,
            y: d.y,
            width: d.width,
            height: d.height,
            scale: if d.scale > 0.0 { d.scale as f64 } else { 1.0 },
        }
    }

    /// RDP pixel coordinates to macOS points.
    pub fn to_points(&self, x: u16, y: u16) -> (i32, i32) {
        (
            self.x + (f64::from(x) / self.scale).round() as i32,
            self.y + (f64::from(y) / self.scale).round() as i32,
        )
    }

    /// macOS points to RDP pixel coordinates, clamped to the display.
    pub fn to_pixels(&self, x: i32, y: i32) -> (u16, u16) {
        let px = ((x - self.x) as f64 * self.scale).round();
        let py = ((y - self.y) as f64 * self.scale).round();
        let max_x = self.width.saturating_sub(1) as f64;
        let max_y = self.height.saturating_sub(1) as f64;
        (px.clamp(0.0, max_x) as u16, py.clamp(0.0, max_y) as u16)
    }
}

pub type SharedGeometry = Arc<Mutex<Geometry>>;

pub fn shared(geometry: Geometry) -> SharedGeometry {
    Arc::new(Mutex::new(geometry))
}

pub(crate) fn current(geometry: &SharedGeometry) -> Geometry {
    *geometry.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::Geometry;

    #[test]
    fn retina_round_trip() {
        let g = Geometry {
            id: 1,
            x: 0,
            y: 0,
            width: 7680,
            height: 4320,
            scale: 2.0,
        };
        assert_eq!(g.to_points(200, 100), (100, 50));
        assert_eq!(g.to_pixels(100, 50), (200, 100));
        assert_eq!(g.to_pixels(-5, 99999), (0, 4319));
    }
}
