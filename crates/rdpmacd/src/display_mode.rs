//! Switching the virtual display's mode from a helper process. macOS keeps 1920x1080 for a
//! 3840x2160 request until a mode switch has taught it that size for the display, and the process
//! that switches keeps hold of the display until it exits: the display would ignore later resizes
//! and outlive the session. So rdpmacd runs itself with `--switch-display-mode` for the switch.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tracing::warn;

/// A switch normally takes well under a second.
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);

/// libscreenio's `ModeSwitch`: whether `rdpmacd --switch-display-mode` switched the display.
pub fn switch_in_helper(display_id: u32, width: u32, height: u32) -> bool {
    let spawned = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .arg("--switch-display-mode")
            .arg(format!("{display_id}:{width}x{height}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    });
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            warn!(%e, "the display mode helper did not start");
            return false;
        }
    };
    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return true,
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    if let Err(e) = pipe.read_to_string(&mut stderr) {
                        stderr = format!("unreadable: {e}");
                    }
                }
                warn!(%status, display_id, width, height, stderr = stderr.trim(), "the display mode helper failed");
                return false;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            Ok(None) => {
                warn!(display_id, width, height, "the display mode helper did not finish; stopping it");
                if let Err(e) = child.kill().and_then(|()| child.wait()) {
                    warn!(%e, "stopping the display mode helper failed");
                }
                return false;
            }
            Err(e) => {
                warn!(%e, "waiting for the display mode helper failed");
                return false;
            }
        }
    }
}
