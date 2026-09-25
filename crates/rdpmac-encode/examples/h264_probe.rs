//! Encodes an image with the session's H.264 encoder the way a session would and writes the
//! Annex B stream, to inspect the bitstream with ffprobe and measure quality against the source.
//!
//!     cargo run --release -p rdpmac-encode --example h264_probe -- \
//!         WIDTH HEIGHT OUT.h264 [IN.bgra|-] [still|scroll|switch] [avc420|avc444]
//!
//! Without an input file, or with `-`, the image is a synthetic pattern. Every scenario ends on
//! the image itself, followed by a second and a half of the refinement a session asks for while
//! the screen does not change, so the last frame of the stream shows the picture a client
//! settles on:
//!
//! - `still` (the default): the image alone, like connecting to an idle desktop.
//! - `scroll`: a second of scrolling at 30 frames per second, stopping on the image.
//! - `switch`: a third of a second of the image alternating with an inverted copy, the worst case
//!   for the rate control, then the image.
//!
//! With `avc444` every frame is two frames of the stream, the main view and then the auxiliary
//! view, in the order one decoder decodes them.

use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const SCROLL_FRAMES: usize = 30;
const SCROLL_ROWS: usize = 20;
const SWITCH_FRAMES: usize = 10;
/// A session asks for refinement every time capture waits a frame interval for a new frame.
const IDLE: Duration = Duration::from_millis(1500);

fn synthetic(width: usize, height: usize) -> Vec<u8> {
    let mut bgra = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let glyph = ((x / 7) * 31 + (y / 14) * 17) % 5 != 0 && (x % 7) < 5 && (y % 14) < 10 && ((x ^ y) & 3) != 0;
            let px = &mut bgra[(y * width + x) * 4..][..4];
            if glyph {
                px.copy_from_slice(&[40, 40, 40, 255]);
            } else {
                px.copy_from_slice(&[200, 225, 240, 255]);
            }
        }
    }
    bgra
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: h264_probe WIDTH HEIGHT OUT.h264 [IN.bgra|-] [still|scroll|switch] [avc420|avc444]");
        std::process::exit(2);
    }
    let width: u32 = args[1].parse().expect("width");
    let height: u32 = args[2].parse().expect("height");
    let image = match args.get(4).map(String::as_str) {
        Some(path) if path != "-" => std::fs::read(path).expect("input image"),
        _ => synthetic(width as usize, height as usize),
    };
    assert_eq!(image.len(), (width * height * 4) as usize, "input must be WIDTH x HEIGHT BGRA");
    let row = width as usize * 4;
    let frames: Vec<Vec<u8>> = match args.get(5).map_or("still", String::as_str) {
        "still" => vec![image.clone()],
        "scroll" => (0..SCROLL_FRAMES)
            .map(|i| {
                let shift = (SCROLL_FRAMES - 1 - i) * SCROLL_ROWS % height as usize * row;
                image[shift..].iter().chain(&image[..shift]).copied().collect()
            })
            .collect(),
        "switch" => {
            let inverted: Vec<u8> =
                image.chunks(4).rev().flat_map(|p| [255 - p[0], 255 - p[1], 255 - p[2], 255]).collect();
            (0..SWITCH_FRAMES).map(|i| if i % 2 == 0 { inverted.clone() } else { image.clone() }).collect()
        }
        other => panic!("unknown scenario {other}"),
    };

    let mut encoder = match args.get(6).map_or("avc420", String::as_str) {
        "avc420" => rdpmac_encode::h264::H264Encoder::new(width, height, 30),
        "avc444" => rdpmac_encode::h264::H264Encoder::new_avc444(width, height, 30),
        other => panic!("unknown codec {other}"),
    }
    .expect("encoder");
    let mut out = std::fs::File::create(&args[3]).expect("output file");
    let started = Instant::now();
    eprintln!(
        "{}",
        if encoder.controls_quantiser() { "quantiser per frame" } else { "VideoToolbox rate control" }
    );
    let mut index = 0;
    let mut report = |what: &str, encoded: Option<rdpmac_encode::h264::EncodedFrame>| {
        if let Some(frame) = encoded {
            out.write_all(&frame.data).expect("write");
            if let Some(auxiliary) = &frame.auxiliary {
                out.write_all(auxiliary).expect("write");
            }
            eprintln!(
                "frame {index:>2} {what:<7} at {:>5} ms: {:>8} bytes{}{}{}",
                started.elapsed().as_millis(),
                frame.data.len(),
                frame.qp.map(|qp| format!(" qp {qp}")).unwrap_or_default(),
                if frame.key_frame { " (key)" } else { "" },
                frame.auxiliary.as_ref().map(|a| format!(" + auxiliary view {} bytes", a.len())).unwrap_or_default()
            );
            index += 1;
        } else if what == "changed" {
            eprintln!("frame {index:>2} changed: dropped");
        }
    };
    for (i, frame) in frames.iter().enumerate() {
        if i > 0 {
            sleep(FRAME_INTERVAL);
        }
        report("changed", encoder.encode(frame, row).expect("encode"));
    }
    let idle = Instant::now();
    while idle.elapsed() < IDLE {
        sleep(FRAME_INTERVAL);
        report("refine", encoder.refine().expect("refine"));
    }
}
