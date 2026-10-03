//! The keys that type each character on the keyboard layout in use.
//!
//! A password field takes typed keys only: text posted as one string event shows in the field
//! but leaves it unable to submit (macrdp found this on macOS 26). So each character is typed
//! with its own key, and the keys come from the layout itself through `UCKeyTranslate`, so that
//! the characters come out right on any layout, not only US.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use crate::{Error, Keystroke, Result};

/// kUCKeyActionDown.
const KEY_DOWN: u16 = 0;
/// shiftKey and optionKey of Carbon's event modifiers, shifted right by 8 as `UCKeyTranslate`
/// takes them.
const SHIFT: u32 = 0x02;
const OPTION: u32 = 0x08;
/// The main block of a Mac keyboard, ANSI, ISO and JIS: the keys that type characters.
/// Return (0x24) and Tab (0x30) type control characters and are left out.
const CHARACTER_KEYS: [std::ops::RangeInclusive<u16>; 3] = [0x00..=0x23, 0x25..=0x2F, 0x31..=0x32];
/// The two JIS keys outside that block: ¥ and _.
const JIS_KEYS: [u16; 2] = [0x5D, 0x5E];

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    static kTISPropertyUnicodeKeyLayoutData: *const c_void;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *mut c_void;
    fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *const c_void;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const c_void,
        key: u16,
        action: u16,
        modifiers: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        max_len: usize,
        actual_len: *mut usize,
        chars: *mut u16,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFRelease(cf: *const c_void);
}

/// Text Input Sources aborts the process when two threads call it at once (seen on macOS 27);
/// one thread at a time, any thread, is fine.
static TEXT_INPUT_SOURCES: Mutex<()> = Mutex::new(());

pub fn keystrokes(text: &str) -> Result<Vec<Keystroke>> {
    let map = {
        let _one_at_a_time = TEXT_INPUT_SOURCES.lock().unwrap_or_else(|e| e.into_inner());
        Layout::current()?.keys()
    };
    text.chars().map(|c| map.get(&c).copied().ok_or(Error::Invalid)).collect()
}

/// The keyboard layout in use. With an input method on (Pinyin, Kotoeri), that is the layout
/// the method types through, which is also what password fields take.
struct Layout {
    /// The input source, which owns `data`.
    source: *mut c_void,
    data: *const u8,
    keyboard_type: u32,
}

impl Layout {
    fn current() -> Result<Self> {
        let copies: [unsafe extern "C" fn() -> *mut c_void; 2] = [
            TISCopyCurrentKeyboardLayoutInputSource,
            TISCopyCurrentASCIICapableKeyboardLayoutInputSource,
        ];
        for copy in copies {
            // SAFETY: the copy is ours to release; the property belongs to the source.
            unsafe {
                let source = copy();
                if source.is_null() {
                    continue;
                }
                let property = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
                let data = if property.is_null() {
                    std::ptr::null()
                } else {
                    CFDataGetBytePtr(property)
                };
                if data.is_null() {
                    CFRelease(source);
                    continue;
                }
                return Ok(Self {
                    source,
                    data,
                    keyboard_type: u32::from(LMGetKbdType()),
                });
            }
        }
        Err(Error::Os)
    }

    /// The one character a key types with these modifiers; `None` for a dead key, which types
    /// nothing until the next key, and for keys that type more than one character or a control
    /// character.
    fn translate(&self, key: u16, shift: bool, option: bool) -> Option<char> {
        let modifiers = (if shift { SHIFT } else { 0 }) | (if option { OPTION } else { 0 });
        let mut dead_key_state = 0;
        let mut chars = [0u16; 4];
        let mut len = 0;
        // SAFETY: `data` lives as long as `source`, which `self` holds.
        let status = unsafe {
            UCKeyTranslate(
                self.data.cast(),
                key,
                KEY_DOWN,
                modifiers,
                self.keyboard_type,
                0,
                &mut dead_key_state,
                chars.len(),
                &mut len,
                chars.as_mut_ptr(),
            )
        };
        if status != 0 || dead_key_state != 0 {
            return None;
        }
        let mut decoded = char::decode_utf16(chars[..len].iter().copied());
        match (decoded.next(), decoded.next()) {
            (Some(Ok(c)), None) if !c.is_control() => Some(c),
            _ => None,
        }
    }

    /// For each character the layout types with one key, the simplest keystroke that types it:
    /// no modifier before Shift before Option.
    fn keys(&self) -> HashMap<char, Keystroke> {
        let keys: Vec<u16> = CHARACTER_KEYS.into_iter().flatten().chain(JIS_KEYS).collect();
        let mut map = HashMap::new();
        for (shift, option) in [(false, false), (true, false), (false, true), (true, true)] {
            for &key in &keys {
                if let Some(c) = self.translate(key, shift, option) {
                    map.entry(c).or_insert(Keystroke { key, shift, option });
                }
            }
        }
        map
    }
}

impl Drop for Layout {
    fn drop(&mut self) {
        unsafe { CFRelease(self.source) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a Latin layout (US, ABC, Canadian, German, French...) as the current one.
    #[test]
    fn letters_digits_and_space_have_keys_that_type_them() {
        let _one_at_a_time = TEXT_INPUT_SOURCES.lock().unwrap_or_else(|e| e.into_inner());
        let layout = Layout::current().expect("a keyboard layout");
        let map = layout.keys();
        for c in ('a'..='z').chain('A'..='Z').chain('0'..='9').chain([' ']) {
            let k = map.get(&c).unwrap_or_else(|| panic!("no key types {c:?}"));
            assert_eq!(layout.translate(k.key, k.shift, k.option), Some(c));
        }
        assert!(map[&'A'].shift && !map[&'a'].shift);
        for (c, k) in &map {
            assert_eq!(layout.translate(k.key, k.shift, k.option), Some(*c), "{k:?}");
        }
    }

    #[test]
    fn control_characters_have_no_keystroke() {
        assert_eq!(keystrokes("a\n"), Err(Error::Invalid));
        assert_eq!(keystrokes("a\t"), Err(Error::Invalid));
        assert_eq!(keystrokes("").map(|k| k.len()), Ok(0));
    }
}
