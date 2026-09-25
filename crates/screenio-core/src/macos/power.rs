//! Telling macOS that a user is at work. The system does not take injected input for user
//! activity in every respect: a sleeping display stays asleep, a released virtual display lingers,
//! and a locked screen shows its password field but turns every password down unchecked, because
//! its unlock flow starts only when a user becomes active.

use std::ffi::c_void;
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_foundation::NSString;

use super::display;
use crate::{Error, Result};

type AssertionId = u32;
const NO_ASSERTION: AssertionId = 0;
/// kIOPMUserActiveLocal. The remote kind does nothing unless the Mac is in dark wake.
const USER_ACTIVE_LOCAL: u32 = 0;
const POLL: Duration = Duration::from_millis(50);

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        name: *const c_void,
        user_type: u32,
        id: *mut AssertionId,
    ) -> i32;
    fn IOPMAssertionRelease(id: AssertionId) -> i32;
}

pub fn declare_user_activity() -> Result<()> {
    let name = NSString::from_str("screenio remote user");
    let mut id = NO_ASSERTION;
    // SAFETY: NSString is toll-free bridged to the CFString the call takes.
    let status = unsafe {
        IOPMAssertionDeclareUserActivity(Retained::as_ptr(&name).cast(), USER_ACTIVE_LOCAL, &mut id)
    };
    // The declaration wakes the display and restarts the idle timers; the assertion it leaves
    // would only hold the display on.
    if id != NO_ASSERTION {
        unsafe { IOPMAssertionRelease(id) };
    }
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Os)
    }
}

pub fn wake_displays(timeout: Duration) -> Result<bool> {
    declare_user_activity()?;
    let deadline = Instant::now() + timeout;
    loop {
        if display::any_active()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL);
    }
}
