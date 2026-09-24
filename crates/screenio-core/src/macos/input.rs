//! Keyboard and mouse injection through CoreGraphics events, posted at the HID level.
//!
//! macOS does not track modifier state for synthesized events, so this keeps its own: every
//! event carries the flags of the modifiers this handle currently holds down, the way a real
//! keyboard would.

use super::keymap;
use crate::{key_flags, lock_flags, Error, MouseButton, Result};
use core_graphics::{
    event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton,
        EventField, KeyCode, ScrollEventUnit,
    },
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use objc2_app_kit::NSEvent;
use std::time::{Duration, Instant};

const WHEEL_DELTA: i32 = 120;
const TAP: CGEventTapLocation = CGEventTapLocation::HID;

pub struct Input {
    source: CGEventSource,
    flags: CGEventFlags,
    keys_down: Vec<CGKeyCode>,
    buttons_down: Vec<MouseButton>,
    double_click: Duration,
    last_click: Option<(Instant, MouseButton)>,
    click_count: i64,
}

fn modifier_flag(key: CGKeyCode) -> Option<CGEventFlags> {
    Some(match key {
        KeyCode::SHIFT | KeyCode::RIGHT_SHIFT => CGEventFlags::CGEventFlagShift,
        KeyCode::CONTROL | KeyCode::RIGHT_CONTROL => CGEventFlags::CGEventFlagControl,
        KeyCode::OPTION | KeyCode::RIGHT_OPTION => CGEventFlags::CGEventFlagAlternate,
        KeyCode::COMMAND | KeyCode::RIGHT_COMMAND => CGEventFlags::CGEventFlagCommand,
        KeyCode::FUNCTION => CGEventFlags::CGEventFlagSecondaryFn,
        _ => return None,
    })
}

/// Flags a physical keyboard sets for keys outside the main block.
fn key_flags(key: CGKeyCode) -> CGEventFlags {
    match key {
        KeyCode::LEFT_ARROW
        | KeyCode::RIGHT_ARROW
        | KeyCode::UP_ARROW
        | KeyCode::DOWN_ARROW
        | KeyCode::HOME
        | KeyCode::END
        | KeyCode::PAGE_UP
        | KeyCode::PAGE_DOWN
        | KeyCode::FORWARD_DELETE
        | KeyCode::HELP => CGEventFlags::CGEventFlagSecondaryFn | CGEventFlags::CGEventFlagNumericPad,
        0x41 | 0x43 | 0x45 | 0x47 | 0x4B | 0x4C | 0x4E | 0x51..=0x5C => {
            CGEventFlags::CGEventFlagNumericPad
        }
        KeyCode::F1
        | KeyCode::F2
        | KeyCode::F3
        | KeyCode::F4
        | KeyCode::F5
        | KeyCode::F6
        | KeyCode::F7
        | KeyCode::F8
        | KeyCode::F9
        | KeyCode::F10
        | KeyCode::F11
        | KeyCode::F12 => CGEventFlags::CGEventFlagSecondaryFn,
        _ => CGEventFlags::CGEventFlagNull,
    }
}

fn button_events(button: MouseButton) -> (CGEventType, CGEventType, CGEventType, CGMouseButton, Option<i64>) {
    use CGEventType::*;
    match button {
        MouseButton::Left => (LeftMouseDown, LeftMouseUp, LeftMouseDragged, CGMouseButton::Left, None),
        MouseButton::Right => (RightMouseDown, RightMouseUp, RightMouseDragged, CGMouseButton::Right, None),
        MouseButton::Middle => (OtherMouseDown, OtherMouseUp, OtherMouseDragged, CGMouseButton::Center, None),
        MouseButton::X1 => (OtherMouseDown, OtherMouseUp, OtherMouseDragged, CGMouseButton::Center, Some(3)),
        MouseButton::X2 => (OtherMouseDown, OtherMouseUp, OtherMouseDragged, CGMouseButton::Center, Some(4)),
    }
}

impl Input {
    pub fn open() -> Result<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| Error::Os)?;
        let interval = NSEvent::doubleClickInterval();
        Ok(Self {
            source,
            flags: CGEventFlags::CGEventFlagNull,
            keys_down: Vec::new(),
            buttons_down: Vec::new(),
            double_click: Duration::from_secs_f64(if interval > 0.0 { interval } else { 0.5 }),
            last_click: None,
            click_count: 1,
        })
    }

    fn location(&self) -> Result<CGPoint> {
        CGEvent::new(self.source.clone())
            .map(|e| e.location())
            .map_err(|_| Error::Os)
    }

    fn post_mouse(&self, kind: CGEventType, point: CGPoint, button: CGMouseButton, number: Option<i64>) -> Result<CGEvent> {
        let event = CGEvent::new_mouse_event(self.source.clone(), kind, point, button)
            .map_err(|_| Error::Os)?;
        event.set_flags(self.flags);
        if let Some(n) = number {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, n);
        }
        Ok(event)
    }

    fn move_to(&mut self, point: CGPoint, delta: Option<(i32, i32)>) -> Result<()> {
        // A move with a button held is a drag, and the drag type names that button.
        let (kind, button, number) = match self.buttons_down.first() {
            Some(&held) => {
                let (_, _, dragged, cg_button, number) = button_events(held);
                (dragged, cg_button, number)
            }
            None => (CGEventType::MouseMoved, CGMouseButton::Left, None),
        };
        let event = self.post_mouse(kind, point, button, number)?;
        if let Some((dx, dy)) = delta {
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
        }
        event.post(TAP);
        Ok(())
    }

    pub fn mouse_move(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_to(CGPoint::new(x as f64, y as f64), None)
    }

    pub fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        let current = self.location()?;
        let point = CGPoint::new(current.x + dx as f64, current.y + dy as f64);
        self.move_to(point, Some((dx, dy)))
    }

    pub fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        let point = self.location()?;
        let (down_kind, up_kind, _, cg_button, number) = button_events(button);
        if down {
            let now = Instant::now();
            self.click_count = match self.last_click {
                Some((at, last)) if last == button && now.duration_since(at) <= self.double_click => {
                    self.click_count + 1
                }
                _ => 1,
            };
            self.last_click = Some((now, button));
        }
        let event = self.post_mouse(if down { down_kind } else { up_kind }, point, cg_button, number)?;
        event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, self.click_count);
        event.post(TAP);
        if down {
            if !self.buttons_down.contains(&button) {
                self.buttons_down.push(button);
            }
        } else {
            self.buttons_down.retain(|b| *b != button);
        }
        Ok(())
    }

    /// `dy` and `dx` in units of 120 per notch; positive is up and right, as RDP sends them.
    pub fn mouse_wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        let lines_y = dy / WHEEL_DELTA;
        let lines_x = dx / WHEEL_DELTA;
        if lines_x == 0 && lines_y == 0 {
            return Ok(());
        }
        // CoreGraphics: positive wheel1 scrolls up, positive wheel2 scrolls left.
        let event = CGEvent::new_scroll_event(
            self.source.clone(),
            ScrollEventUnit::LINE,
            2,
            lines_y,
            -lines_x,
            0,
        )
        .map_err(|_| Error::Os)?;
        event.set_flags(self.flags);
        event.post(TAP);
        Ok(())
    }

    fn post_key(&mut self, key: CGKeyCode, down: bool) -> Result<()> {
        let event = CGEvent::new_keyboard_event(self.source.clone(), key, down)
            .map_err(|_| Error::Os)?;
        match modifier_flag(key) {
            Some(flag) => {
                self.flags.set(flag, down);
                event.set_type(CGEventType::FlagsChanged);
                event.set_flags(self.flags);
            }
            None => {
                if key == KeyCode::CAPS_LOCK && down {
                    self.flags.toggle(CGEventFlags::CGEventFlagAlphaShift);
                }
                event.set_flags(self.flags | key_flags(key));
                if down && self.keys_down.contains(&key) {
                    event.set_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT, 1);
                }
            }
        }
        event.post(TAP);
        if down {
            if !self.keys_down.contains(&key) {
                self.keys_down.push(key);
            }
        } else {
            self.keys_down.retain(|k| *k != key);
        }
        Ok(())
    }

    pub fn key_scancode(&mut self, code: u16, flags: u32) -> Result<()> {
        let extended = flags & key_flags::EXTENDED != 0;
        let key = keymap::set1_to_vkey(code, extended).ok_or(Error::Invalid)?;
        self.post_key(key, flags & key_flags::RELEASE == 0)
    }

    pub fn key_unicode(&mut self, codepoint: u32, down: bool) -> Result<()> {
        // Text arrives as press-and-release pairs; the release carries nothing extra.
        if !down {
            return Ok(());
        }
        let text = char::from_u32(codepoint).ok_or(Error::Invalid)?.to_string();
        for pressed in [true, false] {
            let event = CGEvent::new_keyboard_event(self.source.clone(), 0, pressed)
                .map_err(|_| Error::Os)?;
            event.set_string(&text);
            event.post(TAP);
        }
        Ok(())
    }

    /// Caps Lock is the only lock key macOS has; its state lives in the HID system, not in the
    /// event stream, so it is read and written through IOKit rather than by pressing the key.
    pub fn sync_locks(&mut self, flags: u32) -> Result<()> {
        let wanted = flags & lock_flags::CAPS != 0;
        let current = hid::caps_lock()?;
        if current != wanted {
            hid::set_caps_lock(wanted)?;
        }
        self.flags.set(CGEventFlags::CGEventFlagAlphaShift, wanted);
        Ok(())
    }

    pub fn release_all(&mut self) -> Result<()> {
        for key in std::mem::take(&mut self.keys_down) {
            self.post_key(key, false)?;
        }
        for button in std::mem::take(&mut self.buttons_down) {
            self.mouse_button(button, false)?;
        }
        self.flags = CGEventFlags::CGEventFlagNull;
        Ok(())
    }
}

mod hid {
    use std::ffi::{c_char, c_int, c_void};

    use crate::{Error, Result};

    type IoObject = u32;
    type KernReturn = c_int;
    const KIOHID_PARAM_CONNECT_TYPE: u32 = 1;
    const KIOHID_CAPS_LOCK_STATE: u32 = 1;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        static mach_task_self_: u32;
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> IoObject;
        fn IOServiceOpen(service: IoObject, owning_task: u32, connect_type: u32, connect: *mut IoObject) -> KernReturn;
        fn IOServiceClose(connect: IoObject) -> KernReturn;
        fn IOObjectRelease(object: IoObject) -> KernReturn;
        fn IOHIDGetModifierLockState(handle: IoObject, selector: u32, state: *mut bool) -> KernReturn;
        fn IOHIDSetModifierLockState(handle: IoObject, selector: u32, state: bool) -> KernReturn;
    }

    struct Connection(IoObject);

    impl Connection {
        fn open() -> Result<Self> {
            unsafe {
                let service = IOServiceGetMatchingService(0, IOServiceMatching(c"IOHIDSystem".as_ptr()));
                if service == 0 {
                    return Err(Error::Os);
                }
                let mut connect = 0;
                let status = IOServiceOpen(service, mach_task_self_, KIOHID_PARAM_CONNECT_TYPE, &mut connect);
                IOObjectRelease(service);
                if status != 0 || connect == 0 {
                    return Err(Error::Os);
                }
                Ok(Self(connect))
            }
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            unsafe { IOServiceClose(self.0) };
        }
    }

    pub fn caps_lock() -> Result<bool> {
        let conn = Connection::open()?;
        let mut state = false;
        let status = unsafe { IOHIDGetModifierLockState(conn.0, KIOHID_CAPS_LOCK_STATE, &mut state) };
        if status != 0 {
            return Err(Error::Os);
        }
        Ok(state)
    }

    pub fn set_caps_lock(on: bool) -> Result<()> {
        let conn = Connection::open()?;
        let status = unsafe { IOHIDSetModifierLockState(conn.0, KIOHID_CAPS_LOCK_STATE, on) };
        if status != 0 {
            return Err(Error::Os);
        }
        Ok(())
    }
}
