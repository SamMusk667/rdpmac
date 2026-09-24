//! Frame quantisers for the low-latency encoder, which takes one with every frame instead of
//! running its own rate control.
//!
//! A frame with new content gets a quantiser that keeps the stream near the bitrate: a leaky
//! bucket drains at the bitrate, every frame fills it by its size, and the quantiser rises with
//! the bucket's level. Once the picture stops changing it is encoded again with finer
//! quantisers, a step at a time whenever the bucket has room, until text is as sharp as the
//! codec makes it. VideoToolbox's own rate control cannot do this: after a burst of motion it
//! keeps the quantiser high for seconds, and it ignores quantiser limits changed mid-stream.

use std::time::Instant;

/// Quantiser for new content while the bucket is empty and the bitrate is the size's target.
const BASE_QP: i32 = 24;
const MAX_QP: i32 = 44;
/// How far a full bucket raises the quantiser; six steps halve a frame's size.
const QP_PER_FULL_BUCKET: f64 = 16.0;
/// Seconds of bitrate that fill the bucket.
const BUCKET_SECONDS: f64 = 0.5;
/// Past this level even the coarsest quantiser sends more than the bitrate allows, so new
/// frames wait for the bucket to drain instead.
const SKIP_LEVEL: f64 = 1.5;
/// Where refinement stops: about 49 dB luma PSNR on desktop text, indistinguishable from the
/// source at four times magnification apart from 4:2:0 colour.
const REFINE_QP: i32 = 16;
/// The largest step one refinement takes, which bounds its size on a slow link.
const REFINE_STEP: i32 = 10;
/// Refinement waits until the bucket is at most this full.
const REFINE_ROOM: f64 = 0.25;

pub struct QpControl {
    /// The bitrate chosen for the session size, where the base quantiser applies.
    target: f64,
    bitrate: f64,
    /// Bits sent and not yet drained at the bitrate.
    bucket: f64,
    drained_at: Instant,
    /// The quantiser of the last frame, the level the picture on screen has been refined to.
    picture: i32,
}

impl QpControl {
    pub fn new(target: u32, now: Instant) -> Self {
        let target = f64::from(target.max(1));
        Self {
            target,
            bitrate: target,
            bucket: 0.0,
            drained_at: now,
            picture: MAX_QP,
        }
    }

    pub fn set_bitrate(&mut self, bits_per_second: u32) {
        self.bitrate = f64::from(bits_per_second.max(1));
    }

    /// The quantiser for a frame with new content, or `None` when the frame should wait because
    /// the stream is too far over the bitrate.
    pub fn changed(&mut self, now: Instant) -> Option<i32> {
        let level = self.drain(now);
        if level > SKIP_LEVEL {
            return None;
        }
        // Below the target bitrate every frame has to be smaller: six steps per halving.
        let base = f64::from(BASE_QP) + 6.0 * (self.target / self.bitrate).max(1.0).log2();
        Some(((base + QP_PER_FULL_BUCKET * level).round() as i32).clamp(BASE_QP, MAX_QP))
    }

    /// The quantiser for encoding the unchanged picture again, or `None` when it is as sharp as
    /// refinement makes it or the bucket has no room yet.
    pub fn refinement(&mut self, now: Instant) -> Option<i32> {
        if self.picture <= REFINE_QP || self.drain(now) > REFINE_ROOM {
            return None;
        }
        Some((self.picture - REFINE_STEP).max(REFINE_QP))
    }

    /// Accounts for a frame encoded with `qp` that came out `bytes` long.
    pub fn record(&mut self, qp: i32, bytes: usize, now: Instant) {
        self.drain(now);
        self.bucket += bytes as f64 * 8.0;
        self.picture = qp;
    }

    /// Drains the bucket up to `now` and returns its level, 1.0 being full.
    fn drain(&mut self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.drained_at).as_secs_f64();
        self.drained_at = now;
        self.bucket = (self.bucket - elapsed * self.bitrate).max(0.0);
        self.bucket / (self.bitrate * BUCKET_SECONDS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TARGET: u32 = 8_000_000;

    fn ms(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn small_changes_keep_the_base_quantiser() {
        let t0 = Instant::now();
        let mut qp = QpControl::new(TARGET, t0);
        for i in 0..20 {
            let now = ms(t0, i * 150);
            assert_eq!(qp.changed(now), Some(BASE_QP));
            qp.record(BASE_QP, 4_000, now);
        }
    }

    #[test]
    fn a_burst_raises_the_quantiser_until_the_bucket_drains() {
        let t0 = Instant::now();
        let mut qp = QpControl::new(TARGET, t0);
        let mut last = BASE_QP;
        for i in 0..4 {
            let now = ms(t0, i * 33);
            last = qp.changed(now).expect("room for the burst's start");
            qp.record(last, 250_000, now);
        }
        assert!(last > BASE_QP + 10, "four 2 Mbit frames in a tenth of a second: {last}");
        assert_eq!(qp.changed(ms(t0, 3_000)), Some(BASE_QP), "drained after a few seconds");
    }

    #[test]
    fn an_overfull_bucket_holds_frames_back() {
        let t0 = Instant::now();
        let mut qp = QpControl::new(TARGET, t0);
        // A second's worth of bits at once: twice a full bucket.
        qp.record(MAX_QP, TARGET as usize / 8, t0);
        assert_eq!(qp.changed(ms(t0, 33)), None);
        assert_eq!(qp.changed(ms(t0, 350)), Some(MAX_QP), "sent coarsely once below the limit");
    }

    #[test]
    fn refinement_steps_down_to_the_floor() {
        let t0 = Instant::now();
        let mut qp = QpControl::new(TARGET, t0);
        qp.record(34, 10_000, t0);
        let now = ms(t0, 200);
        assert_eq!(qp.refinement(now), Some(24));
        qp.record(24, 10_000, now);
        assert_eq!(qp.refinement(now), Some(REFINE_QP));
        qp.record(REFINE_QP, 10_000, now);
        assert_eq!(qp.refinement(ms(t0, 400)), None, "nothing left to sharpen");
    }

    #[test]
    fn refinement_waits_for_room() {
        let t0 = Instant::now();
        let mut qp = QpControl::new(TARGET, t0);
        // A full bucket's worth: half a second of the bitrate.
        qp.record(30, TARGET as usize / 16, t0);
        assert_eq!(qp.refinement(ms(t0, 100)), None);
        assert_eq!(qp.refinement(ms(t0, 400)), Some(20));
    }

    #[test]
    fn a_lower_bitrate_raises_the_base() {
        let t0 = Instant::now();
        let mut qp = QpControl::new(TARGET, t0);
        qp.set_bitrate(TARGET / 4);
        assert_eq!(qp.changed(t0), Some(BASE_QP + 12));
        qp.set_bitrate(TARGET);
        assert_eq!(qp.changed(t0), Some(BASE_QP));
    }
}
