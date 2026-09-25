//! The session's own display (ADR D8 step 2). On a Mac without a screen attached, a virtual
//! display at the client's size replaces macOS's placeholder, so the session runs at the client's
//! resolution without scaling. With a screen attached nothing changes and the session scales.
//!
//! The display outlives a single update stream, because IronRDP restarts the stream on every
//! resize; it is removed once no session has used it for a while.

use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};

use screenio_core::{list_displays, DisplayInfo, ModeSwitch, VirtualDisplay};
use tracing::{debug, info, warn};

const NAME: &str = "rdpmac";
/// How long the display stays after the last session ended, so a reconnect finds it in place.
const GRACE: Duration = Duration::from_secs(30);
const REAP_POLL: Duration = Duration::from_secs(1);

/// True when a display other than the placeholder and our own is online.
fn screen_attached(displays: &[DisplayInfo], ours: Option<u32>) -> bool {
    displays.iter().any(|d| !d.placeholder && Some(d.id) != ours)
}

struct State {
    display: Option<VirtualDisplay>,
    streams: usize,
    last_used: Instant,
}

pub struct VirtualScreen {
    state: Mutex<State>,
    switch: ModeSwitch,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl VirtualScreen {
    /// `None` when this Mac cannot create virtual displays. `switch` runs
    /// `screenio_core::switch_display_mode` in another process, which macOS needs once before it
    /// gives the display 3840x2160.
    pub fn new(switch: ModeSwitch) -> Option<Arc<Self>> {
        if !VirtualDisplay::is_supported() {
            return None;
        }
        let screen = Arc::new(Self {
            state: Mutex::new(State {
                display: None,
                streams: 0,
                last_used: Instant::now(),
            }),
            switch,
        });
        let weak = Arc::downgrade(&screen);
        if let Err(e) = thread::Builder::new()
            .name("rdpmac-virtual-display".into())
            .spawn(move || reap(weak))
        {
            warn!(%e, "no thread to remove an unused virtual display");
        }
        Some(screen)
    }

    /// Makes the virtual display show `width` x `height`, creating it when no screen is attached.
    /// Blocks while macOS reconfigures, normally well under a second. Returns the display id when
    /// the display shows the size; otherwise the session scales whatever display it serves.
    pub fn prepare(&self, width: u32, height: u32) -> Option<u32> {
        let mut state = lock(&self.state);
        state.last_used = Instant::now();
        let ours = state.display.as_ref().map(VirtualDisplay::id);
        let displays = match list_displays() {
            Ok(d) => d,
            Err(e) => {
                warn!(%e, "listing displays failed; not using a virtual display");
                return None;
            }
        };
        if screen_attached(&displays, ours) {
            if state.streams == 0 && state.display.take().is_some() {
                info!("a screen is attached; removed the virtual display");
            }
            debug!("a screen is attached; the session scales it");
            return None;
        }
        match state.display.as_mut() {
            Some(existing) => {
                if let Err(e) = existing.resize(width, height) {
                    warn!(%e, width, height, "macOS kept another size for the virtual display; scaling it");
                    return None;
                }
                info!(display = existing.id(), width, height, "virtual display resized");
            }
            None => match VirtualDisplay::create_with_switch(NAME, width, height, self.switch) {
                Ok(created) => {
                    info!(display = created.id(), width, height, "virtual display created");
                    state.display = Some(created);
                }
                Err(e) => {
                    warn!(%e, width, height, "no virtual display at this size; scaling the screen");
                    return None;
                }
            },
        }
        state.display.as_ref().map(VirtualDisplay::id)
    }

    /// The id of the virtual display while it exists.
    pub fn current_id(&self) -> Option<u32> {
        lock(&self.state).display.as_ref().map(VirtualDisplay::id)
    }

    /// Counts an update stream as using the display until the returned guard drops.
    pub fn stream(self: &Arc<Self>) -> StreamGuard {
        lock(&self.state).streams += 1;
        StreamGuard(self.clone())
    }
}

pub struct StreamGuard(Arc<VirtualScreen>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.streams = state.streams.saturating_sub(1);
        state.last_used = Instant::now();
    }
}

fn reap(screen: Weak<VirtualScreen>) {
    loop {
        thread::sleep(REAP_POLL);
        let Some(screen) = screen.upgrade() else {
            return;
        };
        let mut state = lock(&screen.state);
        if state.streams == 0 && state.last_used.elapsed() >= GRACE && state.display.take().is_some() {
            info!(idle_secs = GRACE.as_secs(), "no session used the virtual display; removed it");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, placeholder: bool) -> DisplayInfo {
        DisplayInfo {
            id,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
            name: format!("Display {id}"),
            placeholder,
        }
    }

    #[test]
    fn headless_mac_has_no_screen_attached() {
        assert!(!screen_attached(&[display(1, true)], None));
        assert!(!screen_attached(&[], None));
    }

    #[test]
    fn our_own_display_is_not_a_screen() {
        assert!(!screen_attached(&[display(7, false)], Some(7)));
        assert!(!screen_attached(&[display(1, true), display(7, false)], Some(7)));
    }

    /// Changes the Mac's displays for a few seconds, so it only runs on request:
    /// `cargo test -p rdpmac-session -- --ignored`, on a Mac without a screen attached.
    #[test]
    #[ignore = "creates a real display"]
    fn prepares_resizes_and_scales_on_a_real_mac() {
        let screen = VirtualScreen::new(|_, _, _| false).expect("virtual displays are supported");
        let id = screen.prepare(1600, 900).expect("created at the client's size");
        let shown = |id: u32| {
            let displays = list_displays().expect("listing displays");
            displays.iter().find(|d| d.id == id).map(|d| (d.width, d.height, d.primary))
        };
        assert_eq!(shown(id), Some((1600, 900, true)));
        assert_eq!(screen.prepare(2400, 1300), Some(id), "resized in place");
        assert_eq!(shown(id), Some((2400, 1300, true)));
        // Until macOS has learned 3840x2160 for this display it keeps 1920x1080, and without a
        // switch helper the session then scales that display.
        match screen.prepare(3840, 2160) {
            Some(same) => assert_eq!((same, shown(id)), (id, Some((3840, 2160, true)))),
            None => assert_eq!(shown(id), Some((1920, 1080, true))),
        }
        drop(screen);
    }

    #[test]
    fn any_other_display_counts_as_a_screen() {
        assert!(screen_attached(&[display(2, false)], None));
        assert!(screen_attached(&[display(2, false), display(7, false)], Some(7)));
    }
}
