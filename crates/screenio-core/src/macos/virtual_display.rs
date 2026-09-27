//! Virtual displays through CGVirtualDisplay, the private CoreGraphics API that BetterDisplay,
//! DeskPad and Chromium's display tests use. The classes are looked up at run time, so a macOS
//! release without them reports `Unsupported` instead of failing to load.
//!
//! A display offers exactly one 1x mode, and applying new settings is the whole resize: macOS
//! switches to the offered mode on its own. What was observed on macOS 26 shapes the rest:
//!
//! - A process that has read any display's modes never sees the modes of displays that appear
//!   afterwards, so readiness is judged by the bounds, which stay current and equal the pixel
//!   size at 1x.
//! - For very large sizes macOS may pick a smaller default first; applying the settings again
//!   fixes 5K. For 3840x2160 it keeps 1920x1080, one of the standard modes it adds to the list,
//!   until a switch to 3840x2160 has taught it otherwise: macOS remembers the mode chosen for a
//!   display by its vendor, product and serial number, and from then on displays with this
//!   identity take 3840x2160 like any other size.
//! - The process that switches a display's mode holds on to the display until it exits: the
//!   display ignores later settings and stays online after its release. So the switch runs in a
//!   helper process that the owner provides; see [`switch_display_mode`].

use std::ffi::CStr;
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use core_graphics::display::{CGConfigureOption, CGDisplay, CGDisplayMode};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSString};

use crate::{Error, ModeSwitch, Result};

/// "rd" and "ma". A fixed serial number lets macOS remember the display's arrangement.
const VENDOR_ID: u32 = 0x7264;
const PRODUCT_ID: u32 = 0x6d61;
const SERIAL_NUMBER: u32 = 1;
const REFRESH_HZ: f64 = 60.0;
/// The largest mode a display accepts is fixed when it is created; leave room for later resizes
/// up to 4K UHD and its 16:10 sibling.
const ROOM: (u32, u32) = (3840, 2400);
const MIN_SIDE: u32 = 128;
const MAX_SIDE: u32 = 8192;
/// How long one attempt may take to show a new size; settings are applied at most twice. A
/// switch normally shows within 0.35 s.
const SETTLE: Duration = Duration::from_millis(1500);
const POLL: Duration = Duration::from_millis(20);

const CLASSES: [&CStr; 4] = [
    c"CGVirtualDisplay",
    c"CGVirtualDisplayDescriptor",
    c"CGVirtualDisplaySettings",
    c"CGVirtualDisplayMode",
];

fn class(name: &CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).ok_or(Error::Unsupported)
}

fn validate(width: u32, height: u32) -> Result<()> {
    let side = MIN_SIDE..=MAX_SIDE;
    if side.contains(&width) && side.contains(&height) {
        Ok(())
    } else {
        Err(Error::Invalid)
    }
}

fn shows(id: u32, width: u32, height: u32) -> bool {
    let size = CGDisplay::new(id).bounds().size;
    size.width == f64::from(width) && size.height == f64::from(height)
}

fn wait_until_shown(id: u32, width: u32, height: u32) -> bool {
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline {
        if shows(id, width, height) {
            return true;
        }
        thread::sleep(POLL);
    }
    shows(id, width, height)
}

pub struct VirtualDisplay {
    display: Retained<AnyObject>,
    id: u32,
    max: (u32, u32),
    /// Switches the mode from another process when macOS keeps another size.
    switch: Option<ModeSwitch>,
    /// The queue CGVirtualDisplay runs its termination handler on.
    _queue: DispatchRetained<DispatchQueue>,
}

// CGVirtualDisplay talks to the window server and has no thread affinity; creating it, applying
// settings and releasing it on different threads works.
unsafe impl Send for VirtualDisplay {}

impl VirtualDisplay {
    pub fn is_supported() -> bool {
        CLASSES.iter().all(|name| AnyClass::get(name).is_some())
    }

    pub fn create(name: &str, width: u32, height: u32, switch: Option<ModeSwitch>) -> Result<Self> {
        validate(width, height)?;
        let display_class = class(c"CGVirtualDisplay")?;
        let descriptor_class = class(c"CGVirtualDisplayDescriptor")?;
        let max = (width.max(ROOM.0), height.max(ROOM.1));
        let queue = DispatchQueue::new("screenio.virtual-display", DispatchQueueAttr::SERIAL);
        let terminated = RcBlock::new(|_: *mut AnyObject, _: *mut AnyObject| {
            log::warn!("the window server removed a virtual display");
        });
        // Physical size at 96 dpi; it only feeds the dpi macOS reports.
        let millimetres = |pixels: u32| f64::from(pixels) * 25.4 / 96.0;
        let size = CGSize {
            width: millimetres(max.0),
            height: millimetres(max.1),
        };
        let descriptor: Option<Retained<AnyObject>> = unsafe { msg_send![descriptor_class, new] };
        let descriptor = descriptor.ok_or(Error::Os)?;
        unsafe {
            let _: () = msg_send![&*descriptor, setName: &*NSString::from_str(name)];
            let _: () = msg_send![&*descriptor, setMaxPixelsWide: max.0];
            let _: () = msg_send![&*descriptor, setMaxPixelsHigh: max.1];
            let _: () = msg_send![&*descriptor, setSizeInMillimeters: size];
            let _: () = msg_send![&*descriptor, setVendorID: VENDOR_ID];
            let _: () = msg_send![&*descriptor, setProductID: PRODUCT_ID];
            let _: () = msg_send![&*descriptor, setSerialNum: SERIAL_NUMBER];
            let _: () = msg_send![&*descriptor, setQueue: &*queue];
            let _: () = msg_send![&*descriptor, setTerminationHandler: &*terminated];
        }
        let allocated: Allocated<AnyObject> = unsafe { msg_send![display_class, alloc] };
        let display: Option<Retained<AnyObject>> = unsafe { msg_send![allocated, initWithDescriptor: &*descriptor] };
        let display = display.ok_or(Error::Os)?;
        let id: u32 = unsafe { msg_send![&*display, displayID] };
        if id == 0 {
            return Err(Error::Os);
        }
        let mut created = Self {
            display,
            id,
            max,
            switch,
            _queue: queue,
        };
        created.resize(width, height)?;
        Ok(created)
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        validate(width, height)?;
        if width > self.max.0 || height > self.max.1 {
            return Err(Error::Invalid);
        }
        // Applying the same settings again still reconfigures the display.
        if shows(self.id, width, height) {
            return Ok(());
        }
        // macOS may answer a large size with a smaller default mode; asking again gets the size
        // in most cases.
        for _ in 0..2 {
            self.apply(width, height)?;
            if wait_until_shown(self.id, width, height) {
                return Ok(());
            }
        }
        if let Some(switch) = self.switch {
            if switch(self.id, width, height) && wait_until_shown(self.id, width, height) {
                log::info!(
                    "switched virtual display {} to {width}x{height}; macOS keeps that size for it from now on",
                    self.id
                );
                return Ok(());
            }
        }
        log::warn!("macOS did not switch virtual display {} to {width}x{height}", self.id);
        Err(Error::Os)
    }

    fn apply(&self, width: u32, height: u32) -> Result<()> {
        let settings_class = class(c"CGVirtualDisplaySettings")?;
        let mode_class = class(c"CGVirtualDisplayMode")?;
        let settings: Option<Retained<AnyObject>> = unsafe { msg_send![settings_class, new] };
        let settings = settings.ok_or(Error::Os)?;
        let allocated: Allocated<AnyObject> = unsafe { msg_send![mode_class, alloc] };
        let mode: Option<Retained<AnyObject>> =
            unsafe { msg_send![allocated, initWithWidth: width, height: height, refreshRate: REFRESH_HZ] };
        let modes = NSArray::from_retained_slice(&[mode.ok_or(Error::Os)?]);
        let applied: Bool = unsafe {
            let _: () = msg_send![&*settings, setHiDPI: 0u32];
            let _: () = msg_send![&*settings, setModes: &*modes];
            msg_send![&*self.display, applySettings: &*settings]
        };
        if applied.as_bool() {
            Ok(())
        } else {
            Err(Error::Os)
        }
    }
}

/// Switches through the public CoreGraphics calls, which see every display in a process that
/// started after it appeared, as a helper process does.
pub fn switch_display_mode(id: u32, width: u32, height: u32) -> Result<()> {
    let (width, height) = (u64::from(width), u64::from(height));
    let modes = CGDisplayMode::all_display_modes(id, ptr::null()).ok_or(Error::Invalid)?;
    let mode = modes
        .iter()
        .find(|m| m.width() == width && m.height() == height && m.pixel_width() == width)
        .ok_or(Error::Invalid)?;
    let display = CGDisplay::new(id);
    let config = display.begin_configuration().map_err(|_| Error::Os)?;
    if display.configure_display_with_display_mode(&config, mode).is_err() {
        if let Err(e) = display.cancel_configuration(&config) {
            log::debug!("discarding the display configuration failed: {e}");
        }
        return Err(Error::Os);
    }
    display
        .complete_configuration(&config, CGConfigureOption::ConfigurePermanently)
        .map_err(|_| Error::Os)
}
