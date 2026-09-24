//! Bitrate control driven by client backpressure.
//!
//! The graphics pipeline reports when a client has too many unacknowledged frames, which is the
//! signal that the network or the client's decoder cannot keep up. Every second the controller
//! looks at how many frames had to be skipped: above a tenth it cuts the bitrate by a quarter,
//! and after a few calm seconds it raises it by a tenth, never above the target set for the
//! session size.

use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(1);
const CALM_BEFORE_RAISE: Duration = Duration::from_secs(3);
const SKIP_RATIO_LIMIT: f64 = 0.1;
const CUT: f64 = 0.75;
const RAISE: f64 = 1.1;
const FLOOR: u32 = 500_000;

pub struct RateControl {
    target: u32,
    current: u32,
    frames: u32,
    skipped: u32,
    window_start: Instant,
    last_change: Instant,
}

impl RateControl {
    pub fn new(target: u32, now: Instant) -> Self {
        Self {
            target,
            current: target,
            frames: 0,
            skipped: 0,
            window_start: now,
            last_change: now,
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// Records one frame that was sent, or skipped because the client was behind. Returns the
    /// new bitrate when it should change.
    pub fn record(&mut self, skipped: bool, now: Instant) -> Option<u32> {
        self.frames += 1;
        if skipped {
            self.skipped += 1;
        }
        if now.duration_since(self.window_start) < WINDOW {
            return None;
        }
        let ratio = f64::from(self.skipped) / f64::from(self.frames.max(1));
        let calm = self.skipped == 0;
        self.frames = 0;
        self.skipped = 0;
        self.window_start = now;
        let next = if ratio > SKIP_RATIO_LIMIT {
            ((f64::from(self.current) * CUT) as u32).max(FLOOR.min(self.target))
        } else if calm && self.current < self.target && now.duration_since(self.last_change) >= CALM_BEFORE_RAISE {
            ((f64::from(self.current) * RAISE) as u32).min(self.target)
        } else {
            return None;
        };
        if next == self.current {
            return None;
        }
        self.current = next;
        self.last_change = now;
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn second(rc: &mut RateControl, start: Instant, at: u64, frames: u32, skipped: u32) -> Option<u32> {
        let mut out = None;
        for i in 0..frames {
            let t = start + Duration::from_millis(at * 1000 + u64::from(i) * 1000 / u64::from(frames));
            out = rc.record(i < skipped, t).or(out);
        }
        // Close the window.
        rc.record(false, start + Duration::from_secs(at + 1)).or(out)
    }

    #[test]
    fn cuts_under_backpressure_and_recovers_when_calm() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(8_000_000, t0);
        assert_eq!(second(&mut rc, t0, 0, 30, 10), Some(6_000_000));
        assert_eq!(second(&mut rc, t0, 1, 30, 10), Some(4_500_000));
        // Calm, but not calm for long enough yet.
        assert_eq!(second(&mut rc, t0, 2, 30, 0), None);
        assert_eq!(second(&mut rc, t0, 3, 30, 0), None);
        let raised = second(&mut rc, t0, 4, 30, 0).expect("raised after a calm spell");
        assert!(raised > 4_500_000 && raised <= 8_000_000);
    }

    #[test]
    fn never_exceeds_target_or_drops_below_floor() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(2_000_000, t0);
        for s in 0..20 {
            second(&mut rc, t0, s, 30, 30);
        }
        assert_eq!(rc.current(), FLOOR);
        for s in 20..80 {
            second(&mut rc, t0, s, 30, 0);
        }
        assert_eq!(rc.current(), 2_000_000);
    }
}
