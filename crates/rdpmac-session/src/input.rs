//! `RdpServerInputHandler` on top of libscreenio.
//!
//! IronRDP calls the handler from its event loop; the events are forwarded to one injection
//! thread that owns the libscreenio `Input`, converts RDP pixel coordinates to macOS points and
//! releases everything it still holds when the session ends. Button events in this IronRDP
//! release carry no coordinates, so the thread remembers the last pointer position.
//!
//! The thread also declares the remote user active, which injected events alone do not do: a
//! locked screen starts the flow that checks its password only for an active user, and turns
//! every password down unchecked without it. And it types the password of the user who logged on
//! into the lock screen (see [`crate::unlock`]), before any key the client sends after it.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ironrdp_server::{KeyboardEvent, MouseButton as RdpButton, MouseEvent, RdpServerInputHandler};
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use screenio_core::{key_flags, lock_flags, Input, MouseButton};
use tracing::{error, info, warn};

/// How often remote input is declared as user activity.
const ACTIVITY_EVERY: Duration = Duration::from_secs(2);
/// After this long without input the lock screen's unlock flow may have timed out, which it does
/// about 30 seconds after it started...
const ACTIVITY_LAPSE: Duration = Duration::from_secs(20);
/// ...and the declaration takes this long to start it again (about 50 ms measured) before the key
/// or click it precedes arrives.
const ACTIVITY_SETTLE: Duration = Duration::from_millis(200);
/// After a password went into the lock screen, the client's keys are dropped until it has typed
/// none for this long...
const SWALLOW_QUIET: Duration = Duration::from_secs(1);
/// ...or for this long at most.
const SWALLOW_AT_MOST: Duration = Duration::from_secs(10);

use crate::cursor::ClientPointer;
use crate::unlock::{self, Budget, MacDesk, Password};
use crate::{current, SharedGeometry};

#[derive(Debug)]
pub(crate) enum Event {
    Key(KeyboardEvent),
    Mouse(MouseEvent),
    Unlock(Password),
}

pub struct InputHandler {
    tx: Sender<Event>,
}

/// Where other parts of the server hand the input thread work of its own.
#[derive(Clone)]
pub struct InputQueue(pub(crate) Sender<Event>);

impl InputQueue {
    /// Types `password` into the lock screen if the screen is locked.
    pub(crate) fn unlock(&self, password: Password) {
        let _ = self.0.send(Event::Unlock(password));
    }
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

    pub fn queue(&self) -> InputQueue {
        InputQueue(self.tx.clone())
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

/// Drops the keys the client types while its password goes into the lock screen, and after
/// that until it pauses: the user, seeing the lock screen, may be typing the password too, and
/// once the screen unlocks the rest of it would land in an app, Return included.
#[derive(Default)]
struct Swallow {
    /// Until when the next key is dropped, and the latest that can be.
    window: Option<(Instant, Instant)>,
    /// Keys whose press was dropped, so their release is too.
    held: Vec<(u8, bool)>,
    reported: bool,
}

impl Swallow {
    fn start(&mut self, now: Instant) {
        self.window = Some((now + SWALLOW_QUIET, now + SWALLOW_AT_MOST));
        self.reported = false;
    }

    fn drops(&mut self, event: &KeyboardEvent, now: Instant) -> bool {
        match *event {
            KeyboardEvent::Pressed { .. } | KeyboardEvent::UnicodePressed(_) => {
                let Some((quiet, at_most)) = self.window else {
                    return false;
                };
                if now > quiet || now > at_most {
                    self.window = None;
                    return false;
                }
                self.window = Some(((now + SWALLOW_QUIET).min(at_most), at_most));
                if let KeyboardEvent::Pressed { code, extended } = *event {
                    if !self.held.contains(&(code, extended)) {
                        self.held.push((code, extended));
                    }
                }
                if !self.reported {
                    info!("dropping the keys typed while the password went into the lock screen, until typing pauses");
                    self.reported = true;
                }
                true
            }
            KeyboardEvent::Released { code, extended } => match self.held.iter().position(|k| *k == (code, extended)) {
                Some(i) => {
                    self.held.swap_remove(i);
                    true
                }
                None => false,
            },
            _ => false,
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
    // Shared by the sessions this server runs: a lock that turned the password down gets no more.
    let mut budget = Budget::default();
    let mut swallow = Swallow::default();
    for event in rx {
        activity.input();
        if let Event::Key(key) = &event {
            if swallow.drops(key, Instant::now()) {
                continue;
            }
        }
        let result = match &event {
            Event::Unlock(password) => {
                let outcome = unlock::attempt(&mut MacDesk(&mut input), password.as_str(), &mut budget);
                outcome.report();
                if outcome.typed() {
                    swallow.start(Instant::now());
                }
                Ok(())
            }
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
    use super::*;

    #[test]
    fn keys_typed_while_the_password_goes_in_are_dropped_until_typing_pauses() {
        let press = |code| KeyboardEvent::Pressed { code, extended: false };
        let release = |code| KeyboardEvent::Released { code, extended: false };
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut swallow = Swallow::default();
        assert!(!swallow.drops(&press(0x1E), start), "nothing to drop before an attempt");
        assert!(!swallow.drops(&release(0x1E), start));

        swallow.start(start);
        assert!(swallow.drops(&press(0x1E), at(0)));
        assert!(swallow.drops(&release(0x1E), at(100)), "the release of a dropped key");
        assert!(swallow.drops(&KeyboardEvent::UnicodePressed(0x61), at(900)));
        assert!(swallow.drops(&press(0x1C), at(1800)), "Return, within a second of the last key");
        assert!(!swallow.drops(&KeyboardEvent::Synchronize(SynchronizeFlags::empty()), at(1900)));
        assert!(!swallow.drops(&press(0x1E), at(2900)), "a pause ends it");
        assert!(swallow.drops(&release(0x1C), at(3000)), "Return's release goes with its press");
        assert!(!swallow.drops(&release(0x1E), at(3000)), "a key let through is released too");

        // Typing that never pauses is dropped for ten seconds at most.
        swallow.start(start);
        let mut ms = 0;
        while swallow.drops(&press(0x1E), at(ms)) {
            ms += 500;
        }
        assert_eq!(ms, 10_500);
    }

    #[test]
    fn surrogate_pairs_are_joined() {
        let mut pending = None;
        assert_eq!(utf16_unit(&mut pending, 0xD83D), None);
        assert_eq!(utf16_unit(&mut pending, 0xDE00), Some(0x1F600));
        assert_eq!(utf16_unit(&mut pending, 0x41), Some(0x41));
    }
}
