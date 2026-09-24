use crate::SessionInfo;

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

pub fn request_permissions() -> SessionInfo {
    unsafe {
        CGRequestScreenCaptureAccess();
    }
    session_info()
}
