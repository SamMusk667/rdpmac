//! The pieces IronRDP asks a server to provide, implemented on libscreenio.
//!
//! [`display::DisplayHandler`] turns captured frames and cursor state into `DisplayUpdate`s,
//! [`input::InputHandler`] turns RDP keyboard and mouse events into injected input, and
//! [`monitor::MonitorPolicy`] is the seam where multi-monitor layouts plug in later.

pub mod clip_files;
pub mod clip_image;
pub mod clipboard;
pub mod cursor;
pub mod display;
#[cfg(target_os = "macos")]
pub mod drives;
#[cfg(target_os = "macos")]
pub mod dump;
#[cfg(target_os = "macos")]
pub mod gfx;
pub mod input;
pub mod monitor;
pub mod pattern;
pub mod sound;
pub mod unlock;
pub mod virtual_screen;

use std::sync::{Arc, Mutex};

use screenio_core::DisplayInfo;

/// A rectangle in served-frame pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// How the frames the client sees map to macOS coordinates.
///
/// The client gets frames of `width` x `height` pixels. Inside them the display's picture fills
/// `content`: the whole frame when the display is served at its own size, a centred and
/// letterboxed rectangle when it is scaled to another aspect ratio. macOS input and cursor APIs
/// work in global points; the display starts at `origin_x`, `origin_y` and is `points_width` x
/// `points_height` points large.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub id: u32,
    pub origin_x: f64,
    pub origin_y: f64,
    pub points_width: f64,
    pub points_height: f64,
    pub width: u32,
    pub height: u32,
    pub content: Rect,
}

impl Geometry {
    /// The display at its own pixel size.
    pub fn native(d: &DisplayInfo) -> Self {
        Self::fitted(d, d.width, d.height)
    }

    /// The display scaled into `width` x `height` with its aspect ratio kept.
    pub fn fitted(d: &DisplayInfo, width: u32, height: u32) -> Self {
        let scale = if d.scale > 0.0 { f64::from(d.scale) } else { 1.0 };
        let (dw, dh) = (f64::from(d.width.max(1)), f64::from(d.height.max(1)));
        let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
        let s = (w / dw).min(h / dh);
        let (cw, ch) = (dw * s, dh * s);
        Self {
            id: d.id,
            origin_x: f64::from(d.x),
            origin_y: f64::from(d.y),
            points_width: dw / scale,
            points_height: dh / scale,
            width,
            height,
            content: Rect {
                x: (w - cw) / 2.0,
                y: (h - ch) / 2.0,
                width: cw,
                height: ch,
            },
        }
    }

    /// A synthetic picture in which one pixel is one point, anchored at the global origin.
    pub fn synthetic(width: u32, height: u32) -> Self {
        let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
        Self {
            id: 0,
            origin_x: 0.0,
            origin_y: 0.0,
            points_width: w,
            points_height: h,
            width,
            height,
            content: Rect {
                x: 0.0,
                y: 0.0,
                width: w,
                height: h,
            },
        }
    }

    /// Served pixels per display point.
    pub fn pixels_per_point(&self) -> f64 {
        self.content.width / self.points_width
    }

    /// Served-frame pixel coordinates to global macOS points. Positions on a letterbox bar land
    /// on the nearest edge of the display.
    pub fn to_points(&self, x: u16, y: u16) -> (i32, i32) {
        let px = (f64::from(x) - self.content.x) * self.points_width / self.content.width;
        let py = (f64::from(y) - self.content.y) * self.points_height / self.content.height;
        let px = px.clamp(0.0, (self.points_width - 1.0).max(0.0));
        let py = py.clamp(0.0, (self.points_height - 1.0).max(0.0));
        ((self.origin_x + px).round() as i32, (self.origin_y + py).round() as i32)
    }

    /// Global macOS points to served-frame pixel coordinates, clamped to the picture.
    pub fn to_pixels(&self, x: i32, y: i32) -> (u16, u16) {
        let px = self.content.x + (f64::from(x) - self.origin_x) * self.content.width / self.points_width;
        let py = self.content.y + (f64::from(y) - self.origin_y) * self.content.height / self.points_height;
        let max_x = (self.content.x + self.content.width - 1.0).max(self.content.x);
        let max_y = (self.content.y + self.content.height - 1.0).max(self.content.y);
        (
            px.clamp(self.content.x, max_x).round() as u16,
            py.clamp(self.content.y, max_y).round() as u16,
        )
    }
}

pub type SharedGeometry = Arc<Mutex<Geometry>>;

pub fn shared(geometry: Geometry) -> SharedGeometry {
    Arc::new(Mutex::new(geometry))
}

pub(crate) fn current(geometry: &SharedGeometry) -> Geometry {
    *geometry.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn store(geometry: &SharedGeometry, value: Geometry) {
    *geometry.lock().unwrap_or_else(|e| e.into_inner()) = value;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retina_8k() -> DisplayInfo {
        DisplayInfo {
            id: 5,
            x: 0,
            y: 0,
            width: 7680,
            height: 4320,
            scale: 2.0,
            primary: true,
            name: String::new(),
            placeholder: false,
        }
    }

    #[test]
    fn native_retina_round_trip() {
        let g = Geometry::native(&retina_8k());
        assert_eq!(g.to_points(200, 100), (100, 50));
        assert_eq!(g.to_pixels(100, 50), (200, 100));
        assert_eq!(g.to_pixels(-5, 99999), (0, 4319));
        assert!((g.pixels_per_point() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn scaled_to_same_aspect() {
        let g = Geometry::fitted(&retina_8k(), 1920, 1080);
        assert_eq!(g.content, Rect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 });
        assert_eq!(g.to_points(960, 540), (1920, 1080));
        assert_eq!(g.to_pixels(1920, 1080), (960, 540));
        assert!((g.pixels_per_point() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn letterboxed_when_aspect_differs() {
        // 16:9 display served at 4:3: bars of 96 pixels above and below.
        let g = Geometry::fitted(&retina_8k(), 1024, 768);
        assert_eq!(g.content.y, 96.0);
        assert_eq!(g.content.height, 576.0);
        assert_eq!(g.to_points(512, 96), (1920, 0));
        // A click on the top bar lands on the display's top edge.
        assert_eq!(g.to_points(512, 10), (1920, 0));
        // The cursor never leaves the picture.
        assert_eq!(g.to_pixels(1920, -50).1, 96);
    }

    #[test]
    fn display_origin_is_respected() {
        let mut d = retina_8k();
        d.x = -3840;
        let g = Geometry::fitted(&d, 1920, 1080);
        assert_eq!(g.to_points(0, 0), (-3840, 0));
        assert_eq!(g.to_pixels(-3840, 0), (0, 0));
    }
}
