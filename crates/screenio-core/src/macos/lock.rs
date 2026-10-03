//! The state of the screen lock, from the session dictionary WindowServer keeps for each login
//! session. Its keys are not documented, but they are what `loginwindow` sets and they have
//! been stable for years:
//!
//! - `CGSSessionScreenIsLocked`, present and true while the screen is locked;
//! - `CGSSessionScreenLockedTime`, which changes with every lock (its value runs ahead of the
//!   clock on macOS 27, so it serves only to tell one lock from the next);
//! - `kCGSSessionSecureInputPID`, the process that turned secure keyboard entry on: while the
//!   lock screen's password field has the keyboard, that is `loginwindow`.

use std::ffi::{c_int, c_void};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSNumber, NSString};

use crate::{Error, Result, ScreenLock};

const LOGINWINDOW: &str = "/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow";
/// PROC_PIDPATHINFO_MAXSIZE.
const PATH_MAX: usize = 4096;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// A CFDictionary the caller owns, or null outside a login session.
    fn CGSessionCopyCurrentDictionary() -> *mut c_void;
}

extern "C" {
    // libproc, part of libSystem.
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, size: u32) -> c_int;
}

pub fn screen_lock() -> Result<ScreenLock> {
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    // SAFETY: a CFDictionary is toll-free bridged to NSDictionary, and the copy is ours to release.
    let session: Retained<NSDictionary<NSString, AnyObject>> =
        unsafe { Retained::from_raw(raw.cast()) }.ok_or(Error::Os)?;
    let number = |key: &str| {
        session
            .objectForKey(&NSString::from_str(key))
            .and_then(|value| value.downcast::<NSNumber>().ok())
    };
    let locked = number("CGSSessionScreenIsLocked").is_some_and(|n| n.boolValue());
    let lock_id = number("CGSSessionScreenLockedTime")
        .filter(|_| locked)
        .map(|n| n.longLongValue());
    let password_field = locked
        && number("kCGSSessionSecureInputPID")
            .map(|n| n.intValue())
            .is_some_and(|pid| pid > 0 && executable(pid).as_deref() == Some(LOGINWINDOW));
    Ok(ScreenLock {
        locked,
        lock_id,
        password_field,
    })
}

fn executable(pid: c_int) -> Option<String> {
    let mut buffer = vec![0u8; PATH_MAX];
    let len = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), PATH_MAX as u32) };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    String::from_utf8(buffer).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_own_process_is_found_by_path() {
        let own = executable(std::process::id() as c_int).expect("a path");
        let exe = std::env::current_exe().and_then(std::fs::canonicalize).expect("exe");
        assert_eq!(std::fs::canonicalize(own).expect("exists"), exe);
        assert_eq!(executable(-1), None);
    }

    /// Prints the state of the lock; needs a login session.
    #[test]
    #[ignore]
    fn reads_the_lock() {
        println!("{:?}", screen_lock().expect("in a login session"));
    }
}
