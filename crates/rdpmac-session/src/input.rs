//! `RdpServerInputHandler` on top of libscreenio.
//!
//! IronRDP calls the handler from its event loop; the events are forwarded to one injection
//! thread that owns the libscreenio `Input`, converts RDP pixel coordinates to macOS points and
//! releases everything it still holds when the session ends. Button events in this IronRDP
//! release carry no coordinates, so the thread remembers the last pointer position.
//!
//! The thread also declares the remote user active, which injected events alone do not do: a
//! locked screen starts the flow that checks its password only for an active user, and turns
//! every password down unchecked without it.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ironrdp_server::{KeyboardEvent, MouseButton as RdpButton, MouseEvent, RdpServerInputHandler};
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use screenio_core::{key_flags, lock_flags, Input, MouseButton};
use tracing::{error, warn};

/// How often remote input is declared as user activity.
const ACTIVITY_EVERY: Duration = Duration::from_secs(2);
/// After this long without input the lock screen's unlock flow may have timed out, which it does
/// about 30 seconds after it started...
const ACTIVITY_LAPSE: Duration = Duration::from_secs(20);
/// ...and the declaration takes this long to start it again (about 50 ms measured) before the key
/// or click it precedes arrives.
const ACTIVITY_SETTLE: Duration = Duration::from_millis(200);

use crate::cursor::ClientPointer;
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
    /// Starts the injection thread, which records in `pointer` where the client's moves put the
    /// cursor. Injection failures are logged, never fatal.
    pub fn spawn(geometry: SharedGeometry, pointer: Arc<ClientPointer>) -> Self {
        let (tx, rx) = mpsc::channel();
        if let Err(e) = thread::Builder::new()
            .name("rdpmac-input".into())
            .spawn(move || inject_loop(rx, geometry, pointer))
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

/// The button of the Mac's mouse that an RDP mouse button presses.
fn screen_button(button: RdpButton) -> Option<MouseButton> {
    Some(match button {
        RdpButton::Left => MouseButton::Left,
        RdpButton::Right => MouseButton::Right,
        RdpButton::Middle => MouseButton::Middle,
        RdpButton::X1 => MouseButton::X1,
        RdpButton::X2 => MouseButton::X2,
        _ => return None,
    })
}

/// Declares remote input as user activity, at most every [`ACTIVITY_EVERY`].
#[derive(Default)]
struct Activity {
    declared: Option<Instant>,
    failure_reported: bool,
}

impl Activity {
    fn input(&mut self) {
        let now = Instant::now();
        let since = self.declared.map(|t| now.duration_since(t));
        if since.is_some_and(|s| s < ACTIVITY_EVERY) {
            return;
        }
        self.declared = Some(now);
        if let Err(e) = screenio_core::declare_user_activity() {
            if !self.failure_reported {
                warn!(%e, "declaring the remote user active failed; a locked screen may refuse passwords");
                self.failure_reported = true;
            }
            return;
        }
        if since.is_none_or(|s| s >= ACTIVITY_LAPSE) {
            thread::sleep(ACTIVITY_SETTLE);
        }
    }
}

fn inject_loop(rx: Receiver<Event>, geometry: SharedGeometry, pointer: Arc<ClientPointer>) {
    let mut input = match Input::open() {
        Ok(i) => i,
        Err(e) => {
            error!(%e, "input injection unavailable");
            return;
        }
    };
    let mut pending_surrogate = None;
    let mut activity = Activity::default();
    for event in rx {
        activity.input();
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
                pointer.moving_to((px, py));
                input.mouse_move(px, py)
            }
            Event::Mouse(MouseEvent::Button { x, y, button, pressed }) => match screen_button(*button) {
                // The press or release happens where the event says, whether or not a move went first.
                Some(button) => {
                    let (px, py) = current(&geometry).to_points(*x, *y);
                    pointer.moving_to((px, py));
                    input.mouse_move(px, py).and_then(|()| input.mouse_button(button, *pressed))
                }
                None => Ok(()),
            },
            Event::Mouse(MouseEvent::ButtonRel { x, y, button, pressed }) => match screen_button(*button) {
                Some(button) => {
                    let density = current(&geometry).pixels_per_point();
                    let (dx, dy) = ((f64::from(*x) / density).round() as i32, (f64::from(*y) / density).round() as i32);
                    let moved = if (dx, dy) == (0, 0) {
                        Ok(())
                    } else {
                        pointer.moving_by();
                        input.mouse_move_rel(dx, dy)
                    };
                    moved.and_then(|()| input.mouse_button(button, *pressed))
                }
                None => Ok(()),
            },
            Event::Mouse(MouseEvent::VerticalScroll { value }) => input.mouse_wheel(0, i32::from(*value)),
            Event::Mouse(MouseEvent::HorizontalScroll { value }) => input.mouse_wheel(i32::from(*value), 0),
            Event::Mouse(MouseEvent::Scroll { x, y }) => input.mouse_wheel(*x, *y),
            Event::Mouse(MouseEvent::RelMove { x, y }) => {
                let density = current(&geometry).pixels_per_point();
                pointer.moving_by();
                input.mouse_move_rel((f64::from(*x) / density).round() as i32, (f64::from(*y) / density).round() as i32)
            }
            // Kinds of mouse event a later IronRDP adds.
            Event::Mouse(_) => Ok(()),
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
