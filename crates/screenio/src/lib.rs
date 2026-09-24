#![allow(non_camel_case_types)]
// Every function has the same contract, the C one: each pointer is null or valid for the call.
#![allow(clippy::missing_safety_doc)]
//! C ABI over `screenio-core`. Every function is synchronous, returns `0` on success or a
//! negative `SIO_E_*` code, and never unwinds across the boundary.
//!
//! `include/screenio.h` is generated from this file by `scripts/header.sh` (cbindgen), so the
//! doc comments here are the header's comments.

use screenio_core as sio;
use std::{
    ffi::c_char,
    panic::{catch_unwind, AssertUnwindSafe},
    time::Duration,
};

pub const SIO_OK: i32 = 0;
/// No new frame within the timeout.
pub const SIO_E_TIMEOUT: i32 = -1;
/// The capture target changed; reopen the capturer.
pub const SIO_E_RESET: i32 = -2;
/// A missing OS permission.
pub const SIO_E_PERMISSION: i32 = -3;
/// Not implemented on this platform.
pub const SIO_E_UNSUPPORTED: i32 = -4;
/// A bad argument.
pub const SIO_E_INVALID: i32 = -5;
/// An OS call failed.
pub const SIO_E_OS: i32 = -6;
/// An internal error.
pub const SIO_E_PANIC: i32 = -7;

pub const SIO_FORMAT_BGRA: u32 = 0;

/// Flags for `sio_input_key_scancode`: an E0-prefixed scancode.
pub const SIO_KEY_EXTENDED: u32 = 1;
/// An E1-prefixed scancode.
pub const SIO_KEY_EXTENDED1: u32 = 2;
/// A key release; without it the event is a press.
pub const SIO_KEY_RELEASE: u32 = 4;

/// Lock key flags for `sio_input_sync_locks`, as in RDP's TS_SYNC_EVENT.
pub const SIO_LOCK_SCROLL: u32 = 1;
pub const SIO_LOCK_NUM: u32 = 2;
pub const SIO_LOCK_CAPS: u32 = 4;
pub const SIO_LOCK_KANA: u32 = 8;

/// Buttons for `sio_input_mouse_button`.
pub const SIO_BUTTON_LEFT: u32 = 0;
pub const SIO_BUTTON_RIGHT: u32 = 1;
pub const SIO_BUTTON_MIDDLE: u32 = 2;
pub const SIO_BUTTON_X1: u32 = 3;
pub const SIO_BUTTON_X2: u32 = 4;

/// Panes for `sio_open_privacy_settings`.
pub const SIO_PANE_SCREEN_RECORDING: u32 = 0;
pub const SIO_PANE_ACCESSIBILITY: u32 = 1;

fn code(e: sio::Error) -> i32 {
    match e {
        sio::Error::Timeout => SIO_E_TIMEOUT,
        sio::Error::Reset => SIO_E_RESET,
        sio::Error::Permission => SIO_E_PERMISSION,
        sio::Error::Unsupported => SIO_E_UNSUPPORTED,
        sio::Error::Invalid => SIO_E_INVALID,
        sio::Error::Os => SIO_E_OS,
    }
}

fn guard(f: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(SIO_E_PANIC)
}

#[repr(C)]
pub struct sio_display_t {
    pub id: u32,
    /// Origin in virtual-desktop coordinates.
    pub x: i32,
    pub y: i32,
    /// Size in captured pixels.
    pub width: u32,
    pub height: u32,
    /// Captured pixels per coordinate unit.
    pub scale: f32,
    pub primary: u8,
    pub name: [c_char; 64],
    /// 1 for the stand-in display macOS keeps when no screen is attached.
    pub placeholder: u8,
}

#[repr(C)]
pub struct sio_frame_t {
    /// Valid until the next `sio_capture_frame` on the same handle, or `sio_capture_close`.
    pub data: *const u8,
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    /// `SIO_FORMAT_*`.
    pub format: u32,
}

#[repr(C)]
pub struct sio_cursor_shape_t {
    /// Changes whenever the shape changes.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    /// `width * height * 4` bytes, owned by the library; release with `sio_cursor_shape_free`.
    pub rgba: *mut u8,
    pub rgba_len: usize,
    /// Bitmap pixels per point.
    pub scale: f32,
}

#[repr(C)]
pub struct sio_session_info_t {
    pub can_capture: u8,
    pub can_inject: u8,
    pub backend: [c_char; 32],
}

pub struct sio_capture_t(sio::Capturer);
pub struct sio_virtual_display_t(sio::VirtualDisplay);
pub struct sio_input_t(sio::Input);

fn copy_cstr(dst: &mut [c_char], src: &str) {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (d, s) in dst.iter_mut().zip(&bytes[..n]) {
        *d = *s as c_char;
    }
    dst[n] = 0;
}

/// The library version as 0xMMmmpp: 0x010000 is 1.0.0.
#[no_mangle]
pub extern "C" fn sio_version() -> u32 {
    0x01_00_00
}

#[no_mangle]
pub extern "C" fn sio_strerror(err: i32) -> *const c_char {
    let s: &'static [u8] = match err {
        SIO_OK => b"ok\0",
        SIO_E_TIMEOUT => b"no new frame within the timeout\0",
        SIO_E_RESET => b"capture target changed, reopen the capturer\0",
        SIO_E_PERMISSION => b"missing OS permission\0",
        SIO_E_UNSUPPORTED => b"not supported on this platform\0",
        SIO_E_INVALID => b"invalid argument\0",
        SIO_E_OS => b"OS call failed\0",
        SIO_E_PANIC => b"internal error\0",
        _ => b"unknown error\0",
    };
    s.as_ptr() as *const c_char
}

/// Fills up to `cap` entries and always stores the total number of displays in `count`.
#[no_mangle]
pub unsafe extern "C" fn sio_display_list(
    out: *mut sio_display_t,
    cap: u32,
    count: *mut u32,
) -> i32 {
    if count.is_null() || (out.is_null() && cap > 0) {
        return SIO_E_INVALID;
    }
    guard(|| {
        let displays = match sio::list_displays() {
            Ok(d) => d,
            Err(e) => return code(e),
        };
        *count = displays.len() as u32;
        for (i, d) in displays.iter().take(cap as usize).enumerate() {
            let slot = &mut *out.add(i);
            slot.id = d.id;
            slot.x = d.x;
            slot.y = d.y;
            slot.width = d.width;
            slot.height = d.height;
            slot.scale = d.scale;
            slot.primary = d.primary as u8;
            copy_cstr(&mut slot.name, &d.name);
            slot.placeholder = d.placeholder as u8;
        }
        SIO_OK
    })
}

/// 1 when this system can create virtual displays, 0 otherwise.
#[no_mangle]
pub extern "C" fn sio_virtual_display_supported() -> i32 {
    guard(|| sio::VirtualDisplay::is_supported() as i32)
}

/// Creates a display that exists only in software (macOS: the private CGVirtualDisplay API,
/// checked at run time), `width` x `height` pixels at 1x, named `name` (UTF-8). On a Mac without
/// a screen it replaces the placeholder and becomes the desktop.
#[no_mangle]
pub unsafe extern "C" fn sio_virtual_display_create(
    name: *const c_char,
    width: u32,
    height: u32,
    out: *mut *mut sio_virtual_display_t,
) -> i32 {
    if name.is_null() || out.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| {
        let Ok(name) = std::ffi::CStr::from_ptr(name).to_str() else {
            return SIO_E_INVALID;
        };
        match sio::VirtualDisplay::create(name, width, height) {
            Ok(d) => {
                *out = Box::into_raw(Box::new(sio_virtual_display_t(d)));
                SIO_OK
            }
            Err(e) => code(e),
        }
    })
}

/// The display id for `sio_display_list` and `sio_capture_open`; 0 for a null handle.
#[no_mangle]
pub unsafe extern "C" fn sio_virtual_display_id(display: *const sio_virtual_display_t) -> u32 {
    if display.is_null() {
        return 0;
    }
    (*display).0.id()
}

/// Waits until the display shows the new size. Returns `SIO_E_OS` when macOS settles on another
/// size instead (3840x2160 ends at 1920x1080); the display then keeps that size.
#[no_mangle]
pub unsafe extern "C" fn sio_virtual_display_resize(
    display: *mut sio_virtual_display_t,
    width: u32,
    height: u32,
) -> i32 {
    if display.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| match (*display).0.resize(width, height) {
        Ok(()) => SIO_OK,
        Err(e) => code(e),
    })
}

/// Removes the display; on a Mac without a screen the placeholder comes back.
#[no_mangle]
pub unsafe extern "C" fn sio_virtual_display_destroy(display: *mut sio_virtual_display_t) {
    if !display.is_null() {
        drop(Box::from_raw(display));
    }
}

#[no_mangle]
pub unsafe extern "C" fn sio_capture_open(display_id: u32, out: *mut *mut sio_capture_t) -> i32 {
    if out.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| match sio::Capturer::open(display_id) {
        Ok(c) => {
            *out = Box::into_raw(Box::new(sio_capture_t(c)));
            SIO_OK
        }
        Err(e) => code(e),
    })
}

/// Like `sio_capture_open`, but the frames are scaled to `width` x `height`.
#[no_mangle]
pub unsafe extern "C" fn sio_capture_open_scaled(
    display_id: u32,
    width: u32,
    height: u32,
    out: *mut *mut sio_capture_t,
) -> i32 {
    if out.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| match sio::Capturer::open_scaled(display_id, width, height) {
        Ok(c) => {
            *out = Box::into_raw(Box::new(sio_capture_t(c)));
            SIO_OK
        }
        Err(e) => code(e),
    })
}

/// Waits up to `timeout_ms` for a frame that differs from the last one returned. `SIO_E_RESET`
/// means the capture stream stopped (display change and the like): close and reopen.
#[no_mangle]
pub unsafe extern "C" fn sio_capture_frame(
    cap: *mut sio_capture_t,
    timeout_ms: u32,
    out: *mut sio_frame_t,
) -> i32 {
    if cap.is_null() || out.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| {
        let cap = &mut (*cap).0;
        match cap.frame(Duration::from_millis(timeout_ms as u64)) {
            Ok(f) => {
                *out = sio_frame_t {
                    data: f.data.as_ptr(),
                    width: f.width,
                    height: f.height,
                    stride: f.stride,
                    format: SIO_FORMAT_BGRA,
                };
                SIO_OK
            }
            Err(e) => code(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sio_capture_close(cap: *mut sio_capture_t) {
    if !cap.is_null() {
        drop(Box::from_raw(cap));
    }
}

#[no_mangle]
pub unsafe extern "C" fn sio_cursor_position(x: *mut i32, y: *mut i32, visible: *mut u8) -> i32 {
    if x.is_null() || y.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| match sio::cursor_position() {
        Ok(p) => {
            *x = p.x;
            *y = p.y;
            if !visible.is_null() {
                *visible = p.visible as u8;
            }
            SIO_OK
        }
        Err(e) => code(e),
    })
}

#[no_mangle]
pub unsafe extern "C" fn sio_cursor_shape(out: *mut sio_cursor_shape_t) -> i32 {
    if out.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| match sio::cursor_shape() {
        Ok(s) => {
            let mut rgba = s.rgba.into_boxed_slice();
            let rgba_len = rgba.len();
            let ptr = rgba.as_mut_ptr();
            std::mem::forget(rgba);
            *out = sio_cursor_shape_t {
                id: s.id,
                width: s.width,
                height: s.height,
                hot_x: s.hot_x,
                hot_y: s.hot_y,
                rgba: ptr,
                rgba_len,
                scale: s.scale,
            };
            SIO_OK
        }
        Err(e) => code(e),
    })
}

#[no_mangle]
pub unsafe extern "C" fn sio_cursor_shape_free(shape: *mut sio_cursor_shape_t) {
    if shape.is_null() || (*shape).rgba.is_null() {
        return;
    }
    let s = &mut *shape;
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(s.rgba, s.rgba_len)));
    s.rgba = std::ptr::null_mut();
    s.rgba_len = 0;
}

#[no_mangle]
pub unsafe extern "C" fn sio_input_open(out: *mut *mut sio_input_t) -> i32 {
    if out.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| match sio::Input::open() {
        Ok(i) => {
            *out = Box::into_raw(Box::new(sio_input_t(i)));
            SIO_OK
        }
        Err(e) => code(e),
    })
}

unsafe fn with_input(input: *mut sio_input_t, f: impl FnOnce(&mut sio::Input) -> sio::Result<()>) -> i32 {
    if input.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| match f(&mut (*input).0) {
        Ok(()) => SIO_OK,
        Err(e) => code(e),
    })
}

#[no_mangle]
pub unsafe extern "C" fn sio_input_mouse_move(input: *mut sio_input_t, x: i32, y: i32) -> i32 {
    with_input(input, |i| i.mouse_move(x, y))
}

#[no_mangle]
pub unsafe extern "C" fn sio_input_mouse_move_rel(input: *mut sio_input_t, dx: i32, dy: i32) -> i32 {
    with_input(input, |i| i.mouse_move_rel(dx, dy))
}

/// `button` is one of `SIO_BUTTON_*`.
#[no_mangle]
pub unsafe extern "C" fn sio_input_mouse_button(input: *mut sio_input_t, button: u32, down: u8) -> i32 {
    let button = match button {
        SIO_BUTTON_LEFT => sio::MouseButton::Left,
        SIO_BUTTON_RIGHT => sio::MouseButton::Right,
        SIO_BUTTON_MIDDLE => sio::MouseButton::Middle,
        SIO_BUTTON_X1 => sio::MouseButton::X1,
        SIO_BUTTON_X2 => sio::MouseButton::X2,
        _ => return SIO_E_INVALID,
    };
    with_input(input, |i| i.mouse_button(button, down != 0))
}

/// 120 per notch; positive is up or right.
#[no_mangle]
pub unsafe extern "C" fn sio_input_mouse_wheel(input: *mut sio_input_t, dx: i32, dy: i32) -> i32 {
    with_input(input, |i| i.mouse_wheel(dx, dy))
}

/// A set-1 scancode; `flags` combines `SIO_KEY_*`.
#[no_mangle]
pub unsafe extern "C" fn sio_input_key_scancode(input: *mut sio_input_t, set1_code: u16, flags: u32) -> i32 {
    with_input(input, |i| i.key_scancode(set1_code, flags))
}

#[no_mangle]
pub unsafe extern "C" fn sio_input_key_unicode(input: *mut sio_input_t, codepoint: u32, down: u8) -> i32 {
    with_input(input, |i| i.key_unicode(codepoint, down != 0))
}

/// `lock_flags` combines `SIO_LOCK_*` for the lock keys that should be on.
#[no_mangle]
pub unsafe extern "C" fn sio_input_sync_locks(input: *mut sio_input_t, lock_flags: u32) -> i32 {
    with_input(input, |i| i.sync_locks(lock_flags))
}

#[no_mangle]
pub unsafe extern "C" fn sio_input_release_all(input: *mut sio_input_t) -> i32 {
    with_input(input, |i| i.release_all())
}

#[no_mangle]
pub unsafe extern "C" fn sio_input_close(input: *mut sio_input_t) {
    if !input.is_null() {
        drop(Box::from_raw(input));
    }
}

#[no_mangle]
pub unsafe extern "C" fn sio_session_info(out: *mut sio_session_info_t) -> i32 {
    if out.is_null() {
        return SIO_E_INVALID;
    }
    guard(|| {
        let s = sio::session_info();
        let slot = &mut *out;
        slot.can_capture = s.can_capture as u8;
        slot.can_inject = s.can_inject as u8;
        copy_cstr(&mut slot.backend, s.backend);
        SIO_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn sio_session_request_permissions(out: *mut sio_session_info_t) -> i32 {
    guard(|| {
        let s = sio::request_permissions();
        if !out.is_null() {
            let slot = &mut *out;
            slot.can_capture = s.can_capture as u8;
            slot.can_inject = s.can_inject as u8;
            copy_cstr(&mut slot.backend, s.backend);
        }
        SIO_OK
    })
}

/// Opens the System Settings pane where the user switches a permission on.
#[no_mangle]
pub extern "C" fn sio_open_privacy_settings(pane: u32) -> i32 {
    let pane = match pane {
        SIO_PANE_SCREEN_RECORDING => sio::PrivacyPane::ScreenRecording,
        SIO_PANE_ACCESSIBILITY => sio::PrivacyPane::Accessibility,
        _ => return SIO_E_INVALID,
    };
    guard(|| match sio::open_privacy_settings(pane) {
        Ok(()) => SIO_OK,
        Err(e) => code(e),
    })
}
