use std::ffi::c_void;

use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};

use crate::{Error, PrivacyPane, Result, SessionInfo};

#[link(name = "CoreGraphics", kind = "framework")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    // macOS 11 and later.
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn AXIsProcessTrusted() -> bool;
}

pub fn can_capture() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn session_info() -> SessionInfo {
    SessionInfo {
        backend: "macos-screencapturekit",
        can_capture: can_capture(),
        can_inject: unsafe { AXIsProcessTrusted() },
    }
}

pub fn open_privacy_settings(pane: PrivacyPane) -> Result<()> {
    let anchor = match pane {
        PrivacyPane::ScreenRecording => "Privacy_ScreenCapture",
        PrivacyPane::Accessibility => "Privacy_Accessibility",
    };
    let link = NSString::from_str(&format!("x-apple.systempreferences:com.apple.preference.security?{anchor}"));
    let url = NSURL::URLWithString(&link).ok_or(Error::Os)?;
    if NSWorkspace::sharedWorkspace().openURL(&url) {
        Ok(())
    } else {
        Err(Error::Os)
    }
}

pub fn request_permissions() -> SessionInfo {
    unsafe {
        CGRequestScreenCaptureAccess();
        prompt_accessibility();
    }
    session_info()
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: *const c_void;
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// Shows the Accessibility prompt if this process is not trusted yet.
unsafe fn prompt_accessibility() {
    let keys = [kAXTrustedCheckOptionPrompt];
    let values = [kCFBooleanTrue];
    let options = CFDictionaryCreate(
        std::ptr::null(),
        keys.as_ptr(),
        values.as_ptr(),
        1,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks,
    );
    if options.is_null() {
        return;
    }
    AXIsProcessTrustedWithOptions(options);
    CFRelease(options);
}
