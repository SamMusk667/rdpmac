//! Muting the Mac's sound output through the Core Audio object properties.

use crate::{Error, Result};
use std::ffi::c_void;

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

const SYSTEM_OBJECT: u32 = 1;
const DEFAULT_OUTPUT_DEVICE: u32 = u32::from_be_bytes(*b"dOut");
const MUTE: u32 = u32::from_be_bytes(*b"mute");
const SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
const SCOPE_OUTPUT: u32 = u32::from_be_bytes(*b"outp");
const ELEMENT_MAIN: u32 = 0;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectHasProperty(object: u32, address: *const PropertyAddress) -> u8;
    fn AudioObjectIsPropertySettable(object: u32, address: *const PropertyAddress, settable: *mut u8) -> i32;
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectSetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> i32;
}

/// Reads a 32-bit property of `object`.
fn get_u32(object: u32, address: &PropertyAddress) -> Result<u32> {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(object, address, 0, std::ptr::null(), &mut size, (&mut value as *mut u32).cast())
    };
    if status != 0 {
        return Err(Error::Os);
    }
    Ok(value)
}

fn default_output() -> Result<u32> {
    let address = PropertyAddress {
        selector: DEFAULT_OUTPUT_DEVICE,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    };
    match get_u32(SYSTEM_OBJECT, &address)? {
        0 => Err(Error::Invalid),
        device => Ok(device),
    }
}

const fn mute_address() -> PropertyAddress {
    PropertyAddress {
        selector: MUTE,
        scope: SCOPE_OUTPUT,
        element: ELEMENT_MAIN,
    }
}

/// Whether `device` is muted; [`Error::Unsupported`] when it has no mute this can change.
fn is_muted(device: u32) -> Result<bool> {
    let address = mute_address();
    let mut settable = 0u8;
    let has = unsafe { AudioObjectHasProperty(device, &address) } != 0;
    if !has || unsafe { AudioObjectIsPropertySettable(device, &address, &mut settable) } != 0 || settable == 0 {
        return Err(Error::Unsupported);
    }
    Ok(get_u32(device, &address)? != 0)
}

fn set_muted(device: u32, muted: bool) -> Result<()> {
    let value = u32::from(muted);
    let status = unsafe {
        AudioObjectSetPropertyData(
            device,
            &mute_address(),
            0,
            std::ptr::null(),
            std::mem::size_of::<u32>() as u32,
            (&value as *const u32).cast(),
        )
    };
    if status != 0 {
        return Err(Error::Os);
    }
    Ok(())
}

pub struct OutputMute {
    device: u32,
    /// Whether this muted `device`, which then gets unmuted again; false when it was muted.
    muted_here: bool,
}

impl OutputMute {
    pub fn engage() -> Result<Self> {
        let device = default_output()?;
        let muted_here = mute(device)?;
        Ok(Self { device, muted_here })
    }

    pub fn follow(&mut self) -> Result<()> {
        let device = default_output()?;
        if device == self.device {
            return Ok(());
        }
        self.release();
        self.device = device;
        self.muted_here = mute(device)?;
        Ok(())
    }

    fn release(&mut self) {
        if std::mem::take(&mut self.muted_here) {
            // A device that went away meanwhile needs no unmuting.
            let _ = set_muted(self.device, false);
        }
    }
}

impl Drop for OutputMute {
    fn drop(&mut self) {
        self.release();
    }
}

/// Mutes `device`: true when this muted it, false when it was muted already.
fn mute(device: u32) -> Result<bool> {
    if is_muted(device)? {
        return Ok(false);
    }
    set_muted(device, true)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads, never changes: which output is the default and whether it can be muted.
    #[test]
    fn the_default_output_reads() {
        let device = default_output().expect("a default output device");
        match is_muted(device) {
            Ok(muted) => println!("output {device} muted: {muted}"),
            Err(e) => println!("output {device} cannot be muted: {e:?}"),
        }
    }
}
