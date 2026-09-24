//! Which display a session serves.
//!
//! The free version serves one display. A multi-monitor layout is a different policy behind the
//! same trait and is planned for the Pro edition, so nothing else in the session should assume
//! a single display beyond calling [`MonitorPolicy::select`].

use screenio_core::DisplayInfo;

pub trait MonitorPolicy: Send + Sync {
    fn select(&self, displays: &[DisplayInfo]) -> Option<DisplayInfo>;
}

/// The primary display, falling back to the first one listed.
pub struct PrimaryMonitor;

impl MonitorPolicy for PrimaryMonitor {
    fn select(&self, displays: &[DisplayInfo]) -> Option<DisplayInfo> {
        displays.iter().find(|d| d.primary).or(displays.first()).cloned()
    }
}

/// A display chosen by id, for example from the command line.
pub struct FixedMonitor(pub u32);

impl MonitorPolicy for FixedMonitor {
    fn select(&self, displays: &[DisplayInfo]) -> Option<DisplayInfo> {
        displays.iter().find(|d| d.id == self.0).cloned()
    }
}
