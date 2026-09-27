//! Grabs one frame of the primary display into `screenshot.ppm` and prints cursor state.
//! Pass `--request-permissions` to trigger the macOS screen-recording prompt first.

use screenio_core as sio;
use std::{error::Error, fs::File, io::Write, time::Duration};

fn capture(display_id: u32) -> Result<(), Box<dyn Error>> {
    let mut capturer = sio::Capturer::open(display_id)?;
    let frame = capturer.frame(Duration::from_secs(3))?;
    let (w, h, stride) = (frame.width as usize, frame.height as usize, frame.stride as usize);
    let mut out = File::create("screenshot.ppm")?;
    write!(out, "P6\n{} {}\n255\n", w, h)?;
    let mut row = Vec::with_capacity(w * 3);
    for y in 0..h {
        row.clear();
        for x in 0..w {
            let p = &frame.data[y * stride + x * 4..][..4];
            row.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        out.write_all(&row)?;
    }
    println!("wrote screenshot.ppm ({}x{}, stride {})", w, h, stride);
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args().any(|a| a == "--request-permissions") {
        println!("after prompt: {:?}", sio::request_permissions());
    }
    println!("session: {:?}", sio::session_info());
    let displays = sio::list_displays()?;
    for d in &displays {
        println!(
            "display {} at ({}, {}) {}x{} scale {} primary {}",
            d.id, d.x, d.y, d.width, d.height, d.scale, d.primary
        );
    }
    let target = displays
        .iter()
        .find(|d| d.primary)
        .or(displays.first())
        .ok_or("no display")?;
    if let Err(e) = capture(target.id) {
        println!("capture failed: {e}");
    }

    match sio::cursor_position() {
        Ok(p) => println!("cursor: {p:?}"),
        Err(e) => println!("cursor position unavailable: {e}"),
    }
    match sio::cursor_shape() {
        Ok(s) => println!(
            "cursor shape id {} {}x{} hotspot ({}, {}) {} bytes",
            s.id,
            s.width,
            s.height,
            s.hot_x,
            s.hot_y,
            s.rgba.len()
        ),
        Err(e) => println!("cursor shape unavailable: {e}"),
    }
    Ok(())
}
