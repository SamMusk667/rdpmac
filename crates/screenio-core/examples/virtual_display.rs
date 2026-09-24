//! Creates a virtual display, resizes it a few times and removes it again, printing the display
//! list at each step. Each step runs on its own thread.
//!
//!     cargo run -p screenio-core --example virtual_display

use std::thread;
use std::time::Instant;

use screenio_core::{list_displays, VirtualDisplay};

fn show(step: &str) {
    println!("{step}:");
    match list_displays() {
        Ok(displays) => {
            for d in displays {
                println!(
                    "  id {:>3}  at ({}, {})  {}x{} px  scale {}  primary {}  placeholder {}",
                    d.id, d.x, d.y, d.width, d.height, d.scale, d.primary, d.placeholder
                );
            }
        }
        Err(e) => println!("  listing failed: {e}"),
    }
}

fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    thread::spawn(f).join().expect("step thread panicked")
}

struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("[{}] {}", record.level(), record.args());
    }
    fn flush(&self) {}
}

fn main() {
    let _ = log::set_logger(&Stderr).map(|()| log::set_max_level(log::LevelFilter::Debug));
    println!("supported: {}", VirtualDisplay::is_supported());
    show("before");
    let started = Instant::now();
    let display = on_thread(|| VirtualDisplay::create("screenio example", 1920, 1080));
    let mut display = match display {
        Ok(d) => d,
        Err(e) => {
            println!("create failed: {e}");
            return;
        }
    };
    println!("created display {} in {:?}", display.id(), started.elapsed());
    show("1920x1080");
    for (width, height) in [(1280, 720), (1602, 947), (2560, 1440), (1920, 1080), (3840, 2160)] {
        let started = Instant::now();
        let (moved, result) = on_thread(move || {
            let result = display.resize(width, height);
            (display, result)
        });
        display = moved;
        println!("resize {width}x{height}: {result:?} in {:?}", started.elapsed());
        show("after resize");
    }
    let too_big = display.resize(5120, 2880);
    println!("resize beyond the size fixed at creation: {too_big:?}");
    on_thread(move || drop(display));
    thread::sleep(std::time::Duration::from_secs(2));
    show("after drop");
}
