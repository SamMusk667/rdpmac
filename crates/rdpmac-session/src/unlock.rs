//! Typing the password of the user who logs on into the Mac's lock screen.
//!
//! Windows unlocks a locked console when its user logs on over RDP. rdpmac does the same: the
//! password the client sent, which PAM has just accepted for the user rdpmacd runs as, is held
//! until the session's picture starts and then typed into the lock screen, key by key, by the
//! input thread. docs/unlock.md gives the reason for each step.

use std::fmt;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use screenio_core::{key_flags, lock_flags, Input, Keystroke, ScreenLock};
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use crate::input::InputQueue;

/// How long a password waits for its session to start before it is dropped.
const KEEP_FOR: Duration = Duration::from_secs(60);
/// How many times one lock gets the password submitted. macOS delays the password checks from
/// the third wrong password on, so a lock never sees that many from rdpmac.
pub(crate) const SUBMISSIONS_PER_LOCK: u32 = 2;
/// How long the lock screen gets to show its password field after the display wakes.
const FIELD_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);
/// After declaring the user active: the lock screen starts its unlock flow within about 50 ms.
const SETTLE: Duration = Duration::from_millis(200);
/// After the Shift that focuses the field.
const AFTER_WAKE: Duration = Duration::from_millis(250);
const BETWEEN_BACKSPACES: Duration = Duration::from_millis(8);
const BEFORE_TYPING: Duration = Duration::from_millis(150);
/// Two equal characters in a row come out as one with less.
const BETWEEN_KEYS: Duration = Duration::from_millis(25);
/// The field takes a moment to take the last character before it submits.
const BEFORE_RETURN: Duration = Duration::from_millis(150);
/// How long a Return gets to unlock the screen before it counts as ignored.
const SUBMIT_WAIT: Duration = Duration::from_secs(3);
/// Set-1 scancodes of the keys an attempt presses besides the password's own.
const SHIFT: u16 = 0x2A;
const BACKSPACE: u16 = 0x0E;
const RETURN: u16 = 0x1C;

/// A password held for the lock screen: never printed, and wiped from memory when dropped.
pub struct Password(Zeroizing<String>);

impl Password {
    pub fn new(password: &str) -> Self {
        Self(Zeroizing::new(password.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(..)")
    }
}

/// Holds the password of the logon that just passed until its session starts, then hands it to
/// the input thread, which types it once and drops it.
pub struct Unlocker {
    pending: Mutex<Option<(Password, Instant)>>,
    queue: InputQueue,
}

impl Unlocker {
    pub fn new(queue: InputQueue) -> Self {
        Self {
            pending: Mutex::new(None),
            queue,
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Option<(Password, Instant)>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The credentials of the user rdpmacd runs as passed: keep the password for the lock screen.
    pub fn remember(&self, password: &str) {
        *self.pending() = Some((Password::new(password), Instant::now()));
    }

    /// The session's picture starts: if the screen is locked, the input thread types the
    /// password into the lock screen before any key the client sends from here on.
    pub fn session_started(&self) {
        let taken = self.pending().take();
        match taken {
            Some((password, at)) if at.elapsed() < KEEP_FOR => self.queue.unlock(password),
            Some(_) => debug!("the password waited too long for its session; not typed"),
            None => {}
        }
    }

    /// The connection ended: drop a password its session never used.
    pub fn forget(&self) {
        self.pending().take();
    }
}

/// What an attempt does to the Mac, apart so that the tests can play the Mac.
pub(crate) trait Desk {
    fn screen(&mut self) -> screenio_core::Result<ScreenLock>;
    fn keystrokes(&mut self, password: &str) -> screenio_core::Result<Vec<Keystroke>>;
    fn declare_active(&mut self);
    fn release_held(&mut self);
    fn caps_lock(&mut self) -> screenio_core::Result<bool>;
    fn set_caps_lock(&mut self, on: bool) -> screenio_core::Result<()>;
    fn press(&mut self, key: Key) -> screenio_core::Result<()>;
    fn pause(&mut self, duration: Duration);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Shift,
    Backspace,
    Return,
    Char(Keystroke),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    NotLocked,
    /// Whether the screen is locked could not be read.
    Unknown,
    /// Earlier attempts spent what this lock allows.
    GaveUpEarlier,
    /// The keyboard layout types a character of the password with no single key, or could not
    /// be read.
    Untypeable,
    /// Locked, but the password field never took the keyboard.
    NoPasswordField,
    /// Caps Lock is on and could not be turned off.
    CapsLock,
    /// The lock screen went away or lost the keyboard while the password went in; nothing was
    /// submitted from then on.
    Interrupted,
    /// Pressing a key failed; nothing was submitted from then on.
    Failed(screenio_core::Error),
    Unlocked,
    /// The password was submitted as often as a lock allows and the screen stayed locked.
    StillLocked,
}

impl Outcome {
    /// Whether keys were pressed.
    pub(crate) fn typed(self) -> bool {
        matches!(
            self,
            Outcome::Interrupted | Outcome::Failed(_) | Outcome::Unlocked | Outcome::StillLocked
        )
    }

    pub(crate) fn report(self) {
        match self {
            Outcome::NotLocked => debug!("the screen is not locked"),
            Outcome::Unknown => warn!("whether the screen is locked could not be read; the password is not typed into it"),
            Outcome::GaveUpEarlier => info!(
                "the lock screen did not take the password earlier during this lock; type the password in the session"
            ),
            Outcome::Untypeable => warn!(
                "the Mac's keyboard layout has no single key for a character of the password; type it in the session"
            ),
            Outcome::NoPasswordField => warn!(
                "the lock screen did not show its password field; type the password in the session"
            ),
            Outcome::CapsLock => warn!("Caps Lock could not be turned off; the password is not typed into the lock screen"),
            Outcome::Interrupted => info!("the lock screen went away while the password went in; nothing was submitted"),
            Outcome::Failed(e) => warn!(%e, "typing the password into the lock screen failed; nothing was submitted"),
            Outcome::Unlocked => info!("typed the password of the user who logged on into the lock screen: unlocked"),
            Outcome::StillLocked => warn!(
                "the lock screen did not take the password; it is not typed into this lock again, type it in the session"
            ),
        }
    }
}

/// What the password has been submitted to: one lock at a time.
#[derive(Debug, Default)]
pub(crate) struct Budget {
    lock: Option<Option<i64>>,
    spent: u32,
}

impl Budget {
    /// The screen was seen unlocked, so the next lock is a new one.
    fn unlocked(&mut self) {
        *self = Self::default();
    }

    fn left(&self, lock: Option<i64>) -> bool {
        self.lock != Some(lock) || self.spent < SUBMISSIONS_PER_LOCK
    }

    /// Counts one submission to `lock`, false when it has had all it gets.
    fn spend(&mut self, lock: Option<i64>) -> bool {
        if self.lock != Some(lock) {
            self.lock = Some(lock);
            self.spent = 0;
        }
        if self.spent >= SUBMISSIONS_PER_LOCK {
            return false;
        }
        self.spent += 1;
        true
    }
}

/// Types `password` into the lock screen and submits it, if the screen is locked and its password
/// field has the keyboard. Before every key it checks that this is still so: a key meant for the
/// lock screen must never land in an app.
pub(crate) fn attempt(desk: &mut impl Desk, password: &str, budget: &mut Budget) -> Outcome {
    let lock = match desk.screen() {
        Err(_) => return Outcome::Unknown,
        Ok(screen) if !screen.locked => {
            budget.unlocked();
            return Outcome::NotLocked;
        }
        Ok(screen) => screen.lock_id,
    };
    if !budget.left(lock) {
        return Outcome::GaveUpEarlier;
    }
    let keys = match desk.keystrokes(password) {
        Ok(keys) if !keys.is_empty() => keys,
        _ => return Outcome::Untypeable,
    };
    // The lock screen checks a password only for an active user, and takes a moment to show its
    // field after the display wakes.
    desk.declare_active();
    desk.pause(SETTLE);
    let mut waited = Duration::ZERO;
    loop {
        match desk.screen() {
            Ok(screen) if !screen.locked => {
                budget.unlocked();
                return Outcome::NotLocked;
            }
            Ok(screen) if screen.password_field && screen.lock_id == lock => break,
            _ if waited >= FIELD_WAIT => return Outcome::NoPasswordField,
            _ => {
                desk.pause(POLL);
                waited += POLL;
            }
        }
    }
    desk.release_held();
    // The keystrokes are those of the layout without Caps Lock.
    let caps_was_on = desk.caps_lock().ok();
    if caps_was_on != Some(false) && desk.set_caps_lock(false).is_err() {
        return Outcome::CapsLock;
    }
    let outcome = type_and_submit(desk, &keys, lock, budget);
    if caps_was_on == Some(true) {
        let _ = desk.set_caps_lock(true);
    }
    outcome
}

fn ready(desk: &mut impl Desk, lock: Option<i64>) -> bool {
    matches!(desk.screen(), Ok(screen) if screen.locked && screen.password_field && screen.lock_id == lock)
}

fn type_and_submit(desk: &mut impl Desk, keys: &[Keystroke], lock: Option<i64>, budget: &mut Budget) -> Outcome {
    // The lock screen takes the first key after it comes up to focus the field. Shift types
    // nothing and submits nothing.
    if !ready(desk, lock) {
        return Outcome::Interrupted;
    }
    if let Err(e) = desk.press(Key::Shift) {
        return Outcome::Failed(e);
    }
    desk.pause(AFTER_WAKE);
    // Clear what the field holds: keys the client typed, or the password of an earlier attempt.
    for _ in 0..(keys.len() + 4).min(64) {
        if !ready(desk, lock) {
            return Outcome::Interrupted;
        }
        if let Err(e) = desk.press(Key::Backspace) {
            return Outcome::Failed(e);
        }
        desk.pause(BETWEEN_BACKSPACES);
    }
    desk.pause(BEFORE_TYPING);
    for &key in keys {
        if !ready(desk, lock) {
            return Outcome::Interrupted;
        }
        if let Err(e) = desk.press(Key::Char(key)) {
            return Outcome::Failed(e);
        }
        desk.pause(BETWEEN_KEYS);
    }
    desk.pause(BEFORE_RETURN);
    // The field sometimes ignores the first Return; it holds the password still, so a second
    // Return submits it.
    let mut submitted = false;
    loop {
        if !ready(desk, lock) {
            return match desk.screen() {
                Ok(screen) if !screen.locked => {
                    budget.unlocked();
                    // A Return of ours may have unlocked it after its wait ran out.
                    if submitted {
                        Outcome::Unlocked
                    } else {
                        Outcome::Interrupted
                    }
                }
                _ => Outcome::Interrupted,
            };
        }
        if !budget.spend(lock) {
            return Outcome::StillLocked;
        }
        if let Err(e) = desk.press(Key::Return) {
            return Outcome::Failed(e);
        }
        submitted = true;
        let mut waited = Duration::ZERO;
        while waited < SUBMIT_WAIT {
            desk.pause(POLL);
            waited += POLL;
            if matches!(desk.screen(), Ok(screen) if !screen.locked) {
                budget.unlocked();
                return Outcome::Unlocked;
            }
        }
    }
}

/// The Mac, through libscreenio.
pub(crate) struct MacDesk<'a>(pub &'a mut Input);

impl MacDesk<'_> {
    fn tap(&mut self, scancode: u16) -> screenio_core::Result<()> {
        self.0.key_scancode(scancode, 0)?;
        self.0.key_scancode(scancode, key_flags::RELEASE)
    }
}

impl Desk for MacDesk<'_> {
    fn screen(&mut self) -> screenio_core::Result<ScreenLock> {
        screenio_core::screen_lock()
    }

    fn keystrokes(&mut self, password: &str) -> screenio_core::Result<Vec<Keystroke>> {
        screenio_core::keystrokes(password)
    }

    fn declare_active(&mut self) {
        if let Err(e) = screenio_core::declare_user_activity() {
            warn!(%e, "declaring the user active failed; the lock screen may turn the password down");
        }
    }

    fn release_held(&mut self) {
        if let Err(e) = self.0.release_all() {
            warn!(%e, "releasing the keys the client held failed");
        }
    }

    fn caps_lock(&mut self) -> screenio_core::Result<bool> {
        self.0.locks().map(|locks| locks & lock_flags::CAPS != 0)
    }

    fn set_caps_lock(&mut self, on: bool) -> screenio_core::Result<()> {
        self.0.sync_locks(if on { lock_flags::CAPS } else { 0 })
    }

    fn press(&mut self, key: Key) -> screenio_core::Result<()> {
        match key {
            Key::Shift => self.tap(SHIFT),
            Key::Backspace => self.tap(BACKSPACE),
            Key::Return => self.tap(RETURN),
            Key::Char(keystroke) => self.0.keystroke(keystroke),
        }
    }

    fn pause(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::mpsc;

    use super::*;
    use crate::input::Event;

    const LOCK: Option<i64> = Some(1_791_055_304);

    /// A Mac whose lock screen takes keys as the tests script it.
    struct FakeMac {
        locked: bool,
        lock_id: Option<i64>,
        /// Polls before the password field has the keyboard.
        field_after: u32,
        polls: u32,
        unreadable: bool,
        keystrokes: Option<Vec<Keystroke>>,
        caps: bool,
        /// For each Return, whether it unlocks the screen.
        returns: VecDeque<bool>,
        /// Someone at the Mac unlocks it after this many keys.
        unlocked_after: Option<usize>,
        pressed: Vec<Key>,
        /// The Caps Lock state at each key.
        caps_at_key: Vec<bool>,
        elapsed: Duration,
    }

    impl FakeMac {
        fn locked() -> Self {
            Self {
                locked: true,
                lock_id: LOCK,
                field_after: 0,
                polls: 0,
                unreadable: false,
                keystrokes: Some(keys("pw")),
                caps: false,
                returns: VecDeque::from([true]),
                unlocked_after: None,
                pressed: Vec::new(),
                caps_at_key: Vec::new(),
                elapsed: Duration::ZERO,
            }
        }

        fn count(&self, key: Key) -> usize {
            self.pressed.iter().filter(|k| **k == key).count()
        }
    }

    fn keys(text: &str) -> Vec<Keystroke> {
        text.chars()
            .map(|c| Keystroke {
                key: c as u16,
                shift: c.is_uppercase(),
                option: false,
            })
            .collect()
    }

    impl Desk for FakeMac {
        fn screen(&mut self) -> screenio_core::Result<ScreenLock> {
            if self.unreadable {
                return Err(screenio_core::Error::Os);
            }
            self.polls += 1;
            Ok(ScreenLock {
                locked: self.locked,
                lock_id: self.lock_id.filter(|_| self.locked),
                password_field: self.locked && self.polls > self.field_after,
            })
        }

        fn keystrokes(&mut self, _password: &str) -> screenio_core::Result<Vec<Keystroke>> {
            self.keystrokes.clone().ok_or(screenio_core::Error::Invalid)
        }

        fn declare_active(&mut self) {}

        fn release_held(&mut self) {}

        fn caps_lock(&mut self) -> screenio_core::Result<bool> {
            Ok(self.caps)
        }

        fn set_caps_lock(&mut self, on: bool) -> screenio_core::Result<()> {
            self.caps = on;
            Ok(())
        }

        fn press(&mut self, key: Key) -> screenio_core::Result<()> {
            assert!(self.locked, "{key:?} pressed on an unlocked Mac");
            self.pressed.push(key);
            self.caps_at_key.push(self.caps);
            if key == Key::Return && self.returns.pop_front().unwrap_or(false) {
                self.locked = false;
            }
            if self.unlocked_after == Some(self.pressed.len()) {
                self.locked = false;
            }
            Ok(())
        }

        fn pause(&mut self, duration: Duration) {
            self.elapsed += duration;
        }
    }

    #[test]
    fn an_unlocked_screen_gets_no_keys() {
        let mut mac = FakeMac::locked();
        mac.locked = false;
        let mut budget = Budget::default();
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::NotLocked);
        assert!(mac.pressed.is_empty());
    }

    #[test]
    fn the_password_goes_in_after_shift_and_backspaces_then_one_return() {
        let mut mac = FakeMac::locked();
        let mut budget = Budget::default();
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::Unlocked);
        let mut expected = vec![Key::Shift];
        expected.extend([Key::Backspace; 6]);
        expected.extend(keys("pw").into_iter().map(Key::Char));
        expected.push(Key::Return);
        assert_eq!(mac.pressed, expected);
    }

    #[test]
    fn the_field_gets_time_to_come_up_but_no_key_goes_in_before() {
        let mut mac = FakeMac::locked();
        mac.field_after = 20;
        assert_eq!(attempt(&mut mac, "pw", &mut Budget::default()), Outcome::Unlocked);

        let mut mac = FakeMac::locked();
        mac.field_after = u32::MAX;
        assert_eq!(attempt(&mut mac, "pw", &mut Budget::default()), Outcome::NoPasswordField);
        assert!(mac.pressed.is_empty());
        assert!(mac.elapsed <= FIELD_WAIT + SETTLE + POLL);
    }

    #[test]
    fn an_ignored_return_is_pressed_once_more_and_a_lock_gets_two_at_most() {
        let mut mac = FakeMac::locked();
        mac.returns = VecDeque::from([false, true]);
        assert_eq!(attempt(&mut mac, "pw", &mut Budget::default()), Outcome::Unlocked);
        assert_eq!(mac.count(Key::Return), 2);

        let mut mac = FakeMac::locked();
        mac.returns = VecDeque::from([false, false, true]);
        let mut budget = Budget::default();
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::StillLocked);
        assert_eq!(mac.count(Key::Return), 2);

        // A later session finds the same lock: nothing more is typed into it.
        mac.pressed.clear();
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::GaveUpEarlier);
        assert!(mac.pressed.is_empty());

        // The next lock is tried again.
        mac.lock_id = Some(1_791_060_000);
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::Unlocked);
    }

    #[test]
    fn the_budget_starts_over_once_the_screen_is_seen_unlocked() {
        let mut mac = FakeMac::locked();
        mac.lock_id = None;
        mac.returns = VecDeque::from([false, false]);
        let mut budget = Budget::default();
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::StillLocked);
        // Without a lock ID, the next lock looks like the same one...
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::GaveUpEarlier);
        // ...until the screen is seen unlocked in between.
        mac.locked = false;
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::NotLocked);
        mac.locked = true;
        mac.returns = VecDeque::from([true]);
        assert_eq!(attempt(&mut mac, "pw", &mut budget), Outcome::Unlocked);
    }

    #[test]
    fn someone_unlocking_at_the_mac_stops_the_keys_before_return() {
        for after in [1, 4, 8, 9] {
            let mut mac = FakeMac::locked();
            mac.unlocked_after = Some(after);
            assert_eq!(attempt(&mut mac, "pw", &mut Budget::default()), Outcome::Interrupted, "after {after}");
            assert_eq!(mac.pressed.len(), after, "no key after the unlock");
            assert_eq!(mac.count(Key::Return), 0);
        }
    }

    #[test]
    fn a_lock_screen_that_loses_the_keyboard_gets_no_return() {
        struct Losing(FakeMac);
        impl Desk for Losing {
            fn screen(&mut self) -> screenio_core::Result<ScreenLock> {
                let mut screen = self.0.screen()?;
                // The field has the keyboard until the password is typed.
                screen.password_field &= self.0.pressed.len() < 1 + 6 + 2;
                Ok(screen)
            }
            fn keystrokes(&mut self, password: &str) -> screenio_core::Result<Vec<Keystroke>> {
                self.0.keystrokes(password)
            }
            fn declare_active(&mut self) {}
            fn release_held(&mut self) {}
            fn caps_lock(&mut self) -> screenio_core::Result<bool> {
                self.0.caps_lock()
            }
            fn set_caps_lock(&mut self, on: bool) -> screenio_core::Result<()> {
                self.0.set_caps_lock(on)
            }
            fn press(&mut self, key: Key) -> screenio_core::Result<()> {
                self.0.press(key)
            }
            fn pause(&mut self, duration: Duration) {
                self.0.pause(duration)
            }
        }
        let mut mac = Losing(FakeMac::locked());
        assert_eq!(attempt(&mut mac, "pw", &mut Budget::default()), Outcome::Interrupted);
        assert_eq!(mac.0.count(Key::Return), 0);
    }

    #[test]
    fn caps_lock_is_off_while_the_password_goes_in() {
        let mut mac = FakeMac::locked();
        mac.caps = true;
        assert_eq!(attempt(&mut mac, "pw", &mut Budget::default()), Outcome::Unlocked);
        assert!(mac.caps_at_key.iter().all(|on| !on));
        assert!(mac.caps, "and back on afterwards");
    }

    #[test]
    fn nothing_is_typed_when_the_lock_or_the_layout_cannot_be_read() {
        let mut mac = FakeMac::locked();
        mac.unreadable = true;
        assert_eq!(attempt(&mut mac, "pw", &mut Budget::default()), Outcome::Unknown);
        let mut mac = FakeMac::locked();
        mac.keystrokes = None;
        assert_eq!(attempt(&mut mac, "pw", &mut Budget::default()), Outcome::Untypeable);
        let mut mac = FakeMac::locked();
        mac.keystrokes = Some(Vec::new());
        assert_eq!(attempt(&mut mac, "", &mut Budget::default()), Outcome::Untypeable);
        assert!(mac.pressed.is_empty());
    }

    #[test]
    fn a_password_waits_for_its_session_once() {
        let (tx, rx) = mpsc::channel();
        let unlocker = Unlocker::new(InputQueue(tx));
        unlocker.session_started();
        assert!(rx.try_recv().is_err(), "no logon, nothing to type");

        unlocker.remember("first");
        unlocker.remember("second");
        unlocker.session_started();
        match rx.try_recv() {
            Ok(Event::Unlock(password)) => assert_eq!(password.as_str(), "second"),
            other => panic!("{other:?}"),
        }
        unlocker.session_started();
        assert!(rx.try_recv().is_err(), "typed once");

        unlocker.remember("third");
        unlocker.forget();
        unlocker.session_started();
        assert!(rx.try_recv().is_err(), "the connection ended first");

        unlocker.remember("stale");
        if let Some((_, at)) = unlocker.pending().as_mut() {
            *at = at.checked_sub(KEEP_FOR).expect("the clock runs longer than that");
        }
        unlocker.session_started();
        assert!(rx.try_recv().is_err(), "too old");
    }

    #[test]
    fn passwords_are_not_printed() {
        assert_eq!(format!("{:?}", Password::new("secret")), "Password(..)");
    }
}
