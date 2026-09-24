//! Interface-only implementation for platforms that have no backend yet.

use crate::{
    CursorPosition, CursorShape, DisplayInfo, Error, Frame, MouseButton, Result, SessionInfo,
};
use std::time::Duration;

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    Err(Error::Unsupported)
}

pub struct Capturer;

impl Capturer {
    pub fn open(_display_id: u32) -> Result<Self> {
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
