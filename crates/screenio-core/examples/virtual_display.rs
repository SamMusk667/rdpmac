//! Creates a virtual display, resizes it a few times and removes it again, printing the display
//! list at each step. Each step runs on its own thread. When macOS keeps another size than asked
//! for, the example runs itself as the helper process that switches the mode.
//!
//!     cargo run -p screenio-core --example virtual_display

use std::process::Command;
use std::thread;
use std::time::Instant;

use screenio_core::{list_displays, switch_display_mode, VirtualDisplay};

/// The helper side: `virtual_display switch ID WIDTH HEIGHT`.
fn run_helper(args: &[String]) -> Option<i32> {
    let [_, command, id, width, height] = args else {
        return None;
    };
    if command != "switch" {
        return None;
    }
    let parsed = (id.parse(), width.parse(), height.parse());
    let (Ok(id), Ok(width), Ok(height)) = parsed else {
        return Some(2);
    };
    match switch_display_mode(id, width, height) {
        Ok(()) => Some(0),
        Err(e) => {
            eprintln!("switching display {id} to {width}x{height} failed: {e}");
            Some(1)
        }
    }
}

fn switch_in_helper(id: u32, width: u32, height: u32) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    Command::new(exe)
        .args(["switch", &id.to_string(), &width.to_string(), &height.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

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
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = run_helper(&args) {
        std::process::exit(code);
    }
    let _ = log::set_logger(&Stderr).map(|()| log::set_max_level(log::LevelFilter::Debug));
    println!("supported: {}", VirtualDisplay::is_supported());
    show("before");
    let started = Instant::now();
    let display =
        on_thread(|| VirtualDisplay::create_with_switch("screenio example", 1920, 1080, switch_in_helper));
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
