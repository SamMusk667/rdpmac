//! screenio: capture the screen, read the cursor, inject keyboard and mouse input.
//!
//! The API is deliberately small and synchronous so that a C binding is a one-to-one mapping.
//! Coordinates are in the OS's virtual-desktop space; on macOS that is logical points, and each
//! [`DisplayInfo::scale`] tells how many captured pixels one point covers.
//!
//! Only macOS has a real implementation today. Every other target compiles the same API against
//! [`stub`] and answers [`Error::Unsupported`], so callers and bindings do not need `cfg`s.

use std::{fmt, time::Duration};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(not(target_os = "macos"))]
mod stub;
#[cfg(not(target_os = "macos"))]
use stub as platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No frame that differs from the previous one arrived within the timeout.
    Timeout,
    /// The capture target went away (display change, session switch); reopen the capturer.
    Reset,
    /// The host process lacks a permission (macOS: screen recording or accessibility).
    Permission,
    /// Not implemented on this platform or in this build.
    Unsupported,
    /// A bad argument, for example an unknown display id or scancode.
    Invalid,
    /// The OS API failed.
    Os,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Timeout => "no new frame within the timeout",
            Error::Reset => "capture target changed, reopen the capturer",
            Error::Permission => "missing OS permission",
            Error::Unsupported => "not supported on this platform",
            Error::Invalid => "invalid argument",
            Error::Os => "OS call failed",
        })
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    /// Size in captured pixels.
    pub width: u32,
    pub height: u32,
    /// Captured pixels per coordinate unit (2.0 on a Retina display).
    pub scale: f32,
    pub primary: bool,
    pub name: String,
    /// The stand-in display macOS keeps when no screen is attached. A [`VirtualDisplay`] replaces
    /// it.
    pub placeholder: bool,
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    platform::list_displays()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit B, G, R, A in memory order, rows top-down.
    Bgra,
}

/// One captured frame. `data` stays valid until the next [`Capturer::frame`] call or drop.
pub struct Frame<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// Bytes per row, which can exceed `width * 4`.
    pub stride: u32,
    pub format: PixelFormat,
}

pub struct Capturer(platform::Capturer);

impl Capturer {
    pub fn open(display_id: u32) -> Result<Self> {
        platform::Capturer::open(display_id).map(Capturer)
    }

    /// Captures the display scaled to `width` x `height`, letterboxed and centred when the
    /// aspect ratios differ. Scaling happens in the capture pipeline, not on the CPU.
    pub fn open_scaled(display_id: u32, width: u32, height: u32) -> Result<Self> {
        platform::Capturer::open_scaled(display_id, width, height).map(Capturer)
    }

    pub fn width(&self) -> u32 {
        self.0.width()
    }

    pub fn height(&self) -> u32 {
        self.0.height()
    }

    /// Waits up to `timeout` for a frame that differs from the last one returned.
    pub fn frame(&mut self, timeout: Duration) -> Result<Frame<'_>> {
        self.0.frame(timeout)
    }
}

/// Runs [`switch_display_mode`] with these arguments in another process and says whether it
/// worked; see [`VirtualDisplay::create_with_switch`].
pub type ModeSwitch = fn(display_id: u32, width: u32, height: u32) -> bool;

/// A display that exists only in software. On macOS it comes from the private CGVirtualDisplay
/// API; on a Mac without a screen attached it replaces the placeholder display and becomes the
/// desktop. It stays online while the value lives.
pub struct VirtualDisplay(platform::VirtualDisplay);

impl VirtualDisplay {
    /// Whether this system can create virtual displays. Checked at run time, because the macOS
    /// API is private.
    pub fn is_supported() -> bool {
        platform::VirtualDisplay::is_supported()
    }

    /// Creates a display of `width` x `height` pixels at 1x density and waits until it shows that
    /// size. macOS may keep a smaller default instead: 3840x2160 ends at 1920x1080 until macOS has
    /// learned that size for the display; see [`VirtualDisplay::create_with_switch`].
    pub fn create(name: &str, width: u32, height: u32) -> Result<Self> {
        platform::VirtualDisplay::create(name, width, height, None).map(VirtualDisplay)
    }

    /// Like [`VirtualDisplay::create`], but when macOS keeps another size than a listed one it was
    /// asked for, now or in a later resize, `switch` switches to it the way System Settings does.
    /// macOS remembers that choice for the display, so later displays and resizes get the size
    /// without a switch. `switch` must run [`switch_display_mode`] in another process.
    pub fn create_with_switch(name: &str, width: u32, height: u32, switch: ModeSwitch) -> Result<Self> {
        platform::VirtualDisplay::create(name, width, height, Some(switch)).map(VirtualDisplay)
    }

    /// The display id used by [`list_displays`] and [`Capturer::open`].
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    /// Switches to another size and waits until the display shows it; the id stays. Sizes above
    /// 3840x2400 need a display created at least that large. When macOS settles on another size
    /// and no switch helps, this returns [`Error::Os`] and the display keeps the size macOS
    /// chose, which [`list_displays`] reports.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.0.resize(width, height)
    }
}

/// Switches display `display_id` to its `width` x `height` mode at 1x, as System Settings does;
/// macOS remembers the choice for that display. The process that switches holds on to the
/// display until it exits: the display ignores its owner's resizes and outlives its
/// [`VirtualDisplay`]. For a virtual display, call this from a short-lived helper process, never
/// from the process that owns the display. [`Error::Invalid`] when the display lists no such mode.
pub fn switch_display_mode(display_id: u32, width: u32, height: u32) -> Result<()> {
    platform::switch_display_mode(display_id, width, height)
}

/// Tells the OS a user is at work, as a key press would: the display wakes, idle timers restart,
/// and a locked macOS screen starts the flow that checks its password. Input injected through
/// [`Input`] does not do all of this, so a remote-desktop server calls it when a session starts
/// and while remote input arrives.
pub fn declare_user_activity() -> Result<()> {
    platform::declare_user_activity()
}

/// Declares user activity and waits up to `timeout` for a display to be awake. False when none
/// woke in time.
pub fn wake_displays(timeout: Duration) -> Result<bool> {
    platform::wake_displays(timeout)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

pub fn cursor_position() -> Result<CursorPosition> {
    platform::cursor_position()
}

/// The current cursor image. `id` changes whenever the shape changes, so a caller can poll
/// cheaply and only upload a new pointer when it does.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorShape {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    /// Bitmap pixels per point (2.0 for a Retina rendition), so a caller serving a scaled
    /// picture can resize the cursor to match.
    pub scale: f32,
    /// 8-bit R, G, B, A, rows top-down, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

pub fn cursor_shape() -> Result<CursorShape> {
    platform::cursor_shape()
}

/// The id [`cursor_shape`] would report right now, without building the bitmap. Cheap enough
/// to poll at frame rate; fetch the shape only when it changes.
pub fn cursor_shape_id() -> Result<u64> {
    platform::cursor_shape_id()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// Flags for [`Input::key_scancode`], matching RDP's `TS_KEYBOARD_EVENT` flags.
pub mod key_flags {
    /// The scancode is E0-prefixed (right Ctrl, arrows, navigation block, Win keys).
    pub const EXTENDED: u32 = 1;
    /// The scancode is E1-prefixed (Pause).
    pub const EXTENDED1: u32 = 2;
    /// Key release; absent means key press.
    pub const RELEASE: u32 = 4;
}

/// Flags for [`Input::sync_locks`], matching RDP's `TS_SYNC_EVENT` toggle flags.
pub mod lock_flags {
    pub const SCROLL: u32 = 1;
    pub const NUM: u32 = 2;
    pub const CAPS: u32 = 4;
    pub const KANA: u32 = 8;
}

pub struct Input(platform::Input);

impl Input {
    pub fn open() -> Result<Self> {
        platform::Input::open().map(Input)
    }

    /// Absolute move in virtual-desktop coordinates.
    pub fn mouse_move(&mut self, x: i32, y: i32) -> Result<()> {
        self.0.mouse_move(x, y)
    }

    pub fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.0.mouse_move_rel(dx, dy)
    }

    pub fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        self.0.mouse_button(button, down)
    }

    /// Wheel motion in Windows/RDP units: 120 per notch, positive is up and right.
    pub fn mouse_wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.0.mouse_wheel(dx, dy)
    }

    /// PC/AT set-1 scancode with [`key_flags`], exactly as RDP carries them.
    pub fn key_scancode(&mut self, code: u16, flags: u32) -> Result<()> {
        self.0.key_scancode(code, flags)
    }

    /// Types one Unicode code point, independent of keyboard layout.
    pub fn key_unicode(&mut self, codepoint: u32, down: bool) -> Result<()> {
        self.0.key_unicode(codepoint, down)
    }

    /// Makes the local lock keys match the client's state, given as [`lock_flags`].
    /// Platforms without a lock key (macOS has no Num Lock or Scroll Lock) ignore those bits.
    pub fn sync_locks(&mut self, flags: u32) -> Result<()> {
        self.0.sync_locks(flags)
    }

    /// Releases every key and button this handle still holds down; call on disconnect.
    pub fn release_all(&mut self) -> Result<()> {
        self.0.release_all()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub backend: &'static str,
    pub can_capture: bool,
    pub can_inject: bool,
}

pub fn session_info() -> SessionInfo {
    platform::session_info()
}

/// A pane of the system's privacy settings where the user grants what this library needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyPane {
    /// Screen recording, which [`Capturer`] needs.
    ScreenRecording,
    /// Accessibility, which [`Input`] needs.
    Accessibility,
}

/// Opens the pane where the user switches a permission on. [`request_permissions`] puts the
/// process into both lists; this shows the user where they are.
pub fn open_privacy_settings(pane: PrivacyPane) -> Result<()> {
    platform::open_privacy_settings(pane)
}

/// Triggers the OS permission prompts where the platform has them, then reports the state.
pub fn request_permissions() -> SessionInfo {
    platform::request_permissions()
}
