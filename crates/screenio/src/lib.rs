#![allow(non_camel_case_types)]
//! C ABI over `screenio-core`. Every function is synchronous, returns `0` on success or a
//! negative `SIO_E_*` code, and never unwinds across the boundary.
//!
//! The header `include/screenio.h` mirrors this file by hand for now; `cbindgen.toml` is in
//! place for generating it once the surface settles.

use screenio_core as sio;
use std::{
    ffi::c_char,
    panic::{catch_unwind, AssertUnwindSafe},
    time::Duration,
};

pub const SIO_OK: i32 = 0;
pub const SIO_E_TIMEOUT: i32 = -1;
pub const SIO_E_RESET: i32 = -2;
pub const SIO_E_PERMISSION: i32 = -3;
pub const SIO_E_UNSUPPORTED: i32 = -4;
pub const SIO_E_INVALID: i32 = -5;
pub const SIO_E_OS: i32 = -6;
pub const SIO_E_PANIC: i32 = -7;

pub const SIO_FORMAT_BGRA: u32 = 0;

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
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub primary: u8,
    pub name: [c_char; 64],
}

#[repr(C)]
pub struct sio_frame_t {
    /// Valid until the next `sio_capture_frame` on the same handle, or `sio_capture_close`.
    pub data: *const u8,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

#[repr(C)]
pub struct sio_cursor_shape_t {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    /// Owned by the library; release with `sio_cursor_shape_free`.
    pub rgba: *mut u8,
    pub rgba_len: usize,
}

#[repr(C)]
pub struct sio_session_info_t {
    pub can_capture: u8,
    pub can_inject: u8,
    pub backend: [c_char; 32],
}

pub struct sio_capture_t(sio::Capturer);
pub struct sio_input_t(sio::Input);

fn copy_cstr(dst: &mut [c_char], src: &str) {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (d, s) in dst.iter_mut().zip(&bytes[..n]) {
        *d = *s as c_char;
    }
    dst[n] = 0;
}

#[no_mangle]
pub extern "C" fn sio_version() -> u32 {
    0x00_01_00
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
        }
        SIO_OK
    })
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

/// `button`: 0 left, 1 right, 2 middle, 3 X1, 4 X2.
#[no_mangle]
pub unsafe extern "C" fn sio_input_mouse_button(input: *mut sio_input_t, button: u32, down: u8) -> i32 {
    let button = match button {
        0 => sio::MouseButton::Left,
        1 => sio::MouseButton::Right,
        2 => sio::MouseButton::Middle,
        3 => sio::MouseButton::X1,
        4 => sio::MouseButton::X2,
        _ => return SIO_E_INVALID,
    };
    with_input(input, |i| i.mouse_button(button, down != 0))
}

#[no_mangle]
pub unsafe extern "C" fn sio_input_mouse_wheel(input: *mut sio_input_t, dx: i32, dy: i32) -> i32 {
    with_input(input, |i| i.mouse_wheel(dx, dy))
}

/// `flags`: bit 0 extended (E0), bit 1 extended1 (E1), bit 2 release.
#[no_mangle]
pub unsafe extern "C" fn sio_input_key_scancode(input: *mut sio_input_t, code: u16, flags: u32) -> i32 {
    with_input(input, |i| i.key_scancode(code, flags))
}

#[no_mangle]
pub unsafe extern "C" fn sio_input_key_unicode(input: *mut sio_input_t, codepoint: u32, down: u8) -> i32 {
    with_input(input, |i| i.key_unicode(codepoint, down != 0))
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
