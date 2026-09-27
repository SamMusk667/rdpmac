//! Interface-only implementation for platforms that have no backend yet.

use crate::{
    AudioChunk, CursorPosition, CursorShape, DisplayInfo, Error, Frame, ModeSwitch, MouseButton,
    PrivacyPane, Result, SessionInfo,
};
use std::time::Duration;

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    Err(Error::Unsupported)
}

pub struct VirtualDisplay;

impl VirtualDisplay {
    pub fn is_supported() -> bool {
        false
    }

    pub fn create(_name: &str, _width: u32, _height: u32, _switch: Option<ModeSwitch>) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn id(&self) -> u32 {
        0
    }

    pub fn resize(&mut self, _width: u32, _height: u32) -> Result<()> {
        Err(Error::Unsupported)
    }
}

pub fn switch_display_mode(_display_id: u32, _width: u32, _height: u32) -> Result<()> {
    Err(Error::Unsupported)
}

pub fn declare_user_activity() -> Result<()> {
    Err(Error::Unsupported)
}

pub fn wake_displays(_timeout: Duration) -> Result<bool> {
    Err(Error::Unsupported)
}

pub struct AudioCapture;

impl AudioCapture {
    pub fn open(_sample_rate: u32, _channels: u32) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn sample_rate(&self) -> u32 {
        0
    }

    pub fn channels(&self) -> u32 {
        0
    }

    pub fn source_rate(&self) -> Option<f64> {
        None
    }

    pub fn read(&mut self, _timeout: Duration) -> Result<AudioChunk<'_>> {
        Err(Error::Unsupported)
    }
}

pub struct OutputMute;

impl OutputMute {
    pub fn engage() -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn follow(&mut self) -> Result<()> {
        Err(Error::Unsupported)
    }
}

pub struct Capturer;

impl Capturer {
    pub fn open(_display_id: u32) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn open_scaled(_display_id: u32, _width: u32, _height: u32) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn open_with_rate(_display_id: u32, _max_fps: u32) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn open_scaled_with_rate(_display_id: u32, _width: u32, _height: u32, _max_fps: u32) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn width(&self) -> u32 {
        0
    }

    pub fn height(&self) -> u32 {
        0
    }

    pub fn frame(&mut self, _timeout: Duration) -> Result<Frame<'_>> {
        Err(Error::Unsupported)
    }
}

pub fn cursor_position() -> Result<CursorPosition> {
    Err(Error::Unsupported)
}

pub fn cursor_shape() -> Result<CursorShape> {
    Err(Error::Unsupported)
}

pub fn cursor_shape_id() -> Result<u64> {
    Err(Error::Unsupported)
}

pub struct Input;

impl Input {
    pub fn open() -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn mouse_move_rel(&mut self, _dx: i32, _dy: i32) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn mouse_button(&mut self, _button: MouseButton, _down: bool) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn mouse_wheel(&mut self, _dx: i32, _dy: i32) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn key_scancode(&mut self, _code: u16, _flags: u32) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn key_unicode(&mut self, _codepoint: u32, _down: bool) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn sync_locks(&mut self, _flags: u32) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn release_all(&mut self) -> Result<()> {
        Err(Error::Unsupported)
    }
}

pub fn session_info() -> SessionInfo {
    SessionInfo {
        backend: "none",
        can_capture: false,
        can_inject: false,
    }
}

pub fn request_permissions() -> SessionInfo {
    session_info()
}

pub fn open_privacy_settings(_pane: PrivacyPane) -> Result<()> {
    Err(Error::Unsupported)
}
