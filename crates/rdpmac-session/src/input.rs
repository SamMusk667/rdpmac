//! `RdpServerInputHandler` on top of libscreenio.
//!
//! IronRDP calls the handler from its event loop; the events are forwarded to one injection
//! thread that owns the libscreenio `Input`, converts RDP pixel coordinates to macOS points and
//! releases everything it still holds when the session ends. Button events in this IronRDP
//! release carry no coordinates, so the thread remembers the last pointer position.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use screenio_core::{key_flags, lock_flags, Input, MouseButton};
use tracing::{error, warn};

use crate::{current, SharedGeometry};

#[derive(Debug)]
enum Event {
    Key(KeyboardEvent),
    Mouse(MouseEvent),
}

pub struct InputHandler {
    tx: Sender<Event>,
}

impl InputHandler {
    /// Starts the injection thread. Injection failures are logged, never fatal.
    pub fn spawn(geometry: SharedGeometry) -> Self {
        let (tx, rx) = mpsc::channel();
        if let Err(e) = thread::Builder::new()
            .name("rdpmac-input".into())
            .spawn(move || inject_loop(rx, geometry))
        {
            error!(%e, "input thread could not be started, input is disabled");
        }
        Self { tx }
    }
}

impl RdpServerInputHandler for InputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let _ = self.tx.send(Event::Key(event));
    }

    fn mouse(&mut self, event: MouseEvent) {
        let _ = self.tx.send(Event::Mouse(event));
    }
}

/// Joins UTF-16 surrogate halves; returns the code point once it is complete.
fn utf16_unit(pending: &mut Option<u16>, unit: u16) -> Option<u32> {
    match (pending.take(), unit) {
        (None, 0xD800..=0xDBFF) => {
            *pending = Some(unit);
            None
        }
        (Some(high), 0xDC00..=0xDFFF) => {
            Some(0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00))
        }
        (_, 0xD800..=0xDFFF) => None,
        (_, _) => Some(u32::from(unit)),
    }
}

fn lock_bits(flags: SynchronizeFlags) -> u32 {
    let mut bits = 0;
    if flags.contains(SynchronizeFlags::CAPS_LOCK) {
        bits |= lock_flags::CAPS;
    }
    if flags.contains(SynchronizeFlags::NUM_LOCK) {
        bits |= lock_flags::NUM;
    }
    if flags.contains(SynchronizeFlags::SCROLL_LOCK) {
        bits |= lock_flags::SCROLL;
    }
    if flags.contains(SynchronizeFlags::KANA_LOCK) {
        bits |= lock_flags::KANA;
    }
    bits
}

fn button_event(event: &MouseEvent) -> Option<(MouseButton, bool)> {
    Some(match event {
        MouseEvent::LeftPressed => (MouseButton::Left, true),
        MouseEvent::LeftReleased => (MouseButton::Left, false),
        MouseEvent::RightPressed => (MouseButton::Right, true),
        MouseEvent::RightReleased => (MouseButton::Right, false),
        MouseEvent::MiddlePressed => (MouseButton::Middle, true),
        MouseEvent::MiddleReleased => (MouseButton::Middle, false),
        MouseEvent::Button4Pressed => (MouseButton::X1, true),
        MouseEvent::Button4Released => (MouseButton::X1, false),
        MouseEvent::Button5Pressed => (MouseButton::X2, true),
        MouseEvent::Button5Released => (MouseButton::X2, false),
        _ => return None,
    })
}

fn inject_loop(rx: Receiver<Event>, geometry: SharedGeometry) {
    let mut input = match Input::open() {
        Ok(i) => i,
        Err(e) => {
            error!(%e, "input injection unavailable");
            return;
        }
    };
    let mut pending_surrogate = None;
    for event in rx {
        let result = match &event {
            Event::Key(KeyboardEvent::Pressed { code, extended }) => {
                input.key_scancode(u16::from(*code), if *extended { key_flags::EXTENDED } else { 0 })
            }
            Event::Key(KeyboardEvent::Released { code, extended }) => input.key_scancode(
                u16::from(*code),
                key_flags::RELEASE | if *extended { key_flags::EXTENDED } else { 0 },
            ),
            Event::Key(KeyboardEvent::UnicodePressed(unit)) => match utf16_unit(&mut pending_surrogate, *unit) {
                Some(cp) => input.key_unicode(cp, true),
                None => Ok(()),
            },
            Event::Key(KeyboardEvent::UnicodeReleased(_)) => Ok(()),
            Event::Key(KeyboardEvent::Synchronize(flags)) => input.sync_locks(lock_bits(*flags)),
            #[allow(unreachable_patterns)]
            Event::Key(_) => Ok(()),
            Event::Mouse(MouseEvent::Move { x, y }) => {
                let (px, py) = current(&geometry).to_points(*x, *y);
                input.mouse_move(px, py)
            }
            Event::Mouse(MouseEvent::VerticalScroll { value }) => input.mouse_wheel(0, i32::from(*value)),
            Event::Mouse(MouseEvent::Scroll { x, y }) => input.mouse_wheel(*x, *y),
            Event::Mouse(MouseEvent::RelMove { x, y }) => {
                let density = current(&geometry).pixels_per_point();
                input.mouse_move_rel((f64::from(*x) / density).round() as i32, (f64::from(*y) / density).round() as i32)
            }
            Event::Mouse(other) => match button_event(other) {
                Some((button, pressed)) => input.mouse_button(button, pressed),
                None => Ok(()),
            },
        };
        if let Err(e) = result {
            warn!(%e, ?event, "input injection failed");
        }
    }
    if let Err(e) = input.release_all() {
        warn!(%e, "releasing keys at session end failed");
    }
}

#[cfg(test)]
mod tests {
    use super::utf16_unit;

    #[test]
    fn surrogate_pairs_are_joined() {
        let mut pending = None;
        assert_eq!(utf16_unit(&mut pending, 0xD83D), None);
        assert_eq!(utf16_unit(&mut pending, 0xDE00), Some(0x1F600));
        assert_eq!(utf16_unit(&mut pending, 0x41), Some(0x41));
    }
}
