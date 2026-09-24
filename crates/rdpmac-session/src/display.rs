//! `RdpServerDisplay` on top of libscreenio.
//!
//! `updates()` starts two OS threads that feed a bounded channel: one pulls frames from the
//! capturer, one polls the cursor. IronRDP drains the channel from its event loop. Frames are
//! dropped when the channel is full so a slow client never builds a backlog.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use anyhow::Context;
use ironrdp_server::{DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates};
use screenio_core::{cursor_position, cursor_shape, cursor_shape_id, list_displays, Capturer, Error as CaptureError};
use tokio::sync::mpsc::{self, error::TrySendError, Receiver, Sender};
use tracing::{debug, error, info, warn};

use crate::cursor::{position_update, PointerCache};
use crate::monitor::MonitorPolicy;
use crate::pattern::TestPattern;
use crate::{current, Geometry, SharedGeometry};

const CHANNEL_DEPTH: usize = 4;
const REOPEN_DELAY: Duration = Duration::from_secs(5);
const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// How often the capture loop checks whether the display changed size or disappeared.
const DISPLAY_POLL: Duration = Duration::from_secs(1);

/// Where frames come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    /// The selected display through libscreenio.
    Screen,
    /// Synthetic frames of the given size; needs no permission, exercises the encoder.
    TestPattern { width: u32, height: u32 },
}

pub struct DisplayHandler {
    policy: Arc<dyn MonitorPolicy>,
    geometry: SharedGeometry,
    source: FrameSource,
    fps: u32,
    cursor_hz: u32,
}

impl DisplayHandler {
    pub fn new(policy: Arc<dyn MonitorPolicy>, geometry: SharedGeometry, fps: u32, cursor_hz: u32) -> Self {
        Self::with_source(policy, geometry, FrameSource::Screen, fps, cursor_hz)
    }

    pub fn with_source(
        policy: Arc<dyn MonitorPolicy>,
        geometry: SharedGeometry,
        source: FrameSource,
        fps: u32,
        cursor_hz: u32,
    ) -> Self {
        Self {
            policy,
            geometry,
            source,
            fps: fps.clamp(1, 120),
            cursor_hz: cursor_hz.clamp(1, 120),
        }
    }

    /// Re-reads the display list so a changed resolution is served at its new size.
    fn refresh_geometry(&self) -> Geometry {
        match list_displays() {
            Ok(displays) => match self.policy.select(&displays) {
                Some(d) => {
                    let g = Geometry::from_display(&d);
                    *self.geometry.lock().unwrap_or_else(|e| e.into_inner()) = g;
                    g
                }
                None => {
                    warn!("selected display is gone, keeping the last geometry");
                    current(&self.geometry)
                }
            },
            Err(e) => {
                warn!(%e, "listing displays failed, keeping the last geometry");
                current(&self.geometry)
            }
        }
    }
}

#[async_trait]
impl RdpServerDisplay for DisplayHandler {
    async fn size(&mut self) -> DesktopSize {
        let g = match self.source {
            FrameSource::Screen => self.refresh_geometry(),
            FrameSource::TestPattern { width, height } => {
                let mut g = current(&self.geometry);
                g.width = width;
                g.height = height;
                g.scale = 1.0;
                *self.geometry.lock().unwrap_or_else(|e| e.into_inner()) = g;
                g
            }
        };
        DesktopSize {
            width: g.width.min(u32::from(u16::MAX)) as u16,
            height: g.height.min(u32::from(u16::MAX)) as u16,
        }
    }

    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
        let stop = Arc::new(AtomicBool::new(false));
        let capture = Threads {
            geometry: self.geometry.clone(),
            stop: stop.clone(),
            tx: tx.clone(),
        };
        let fps = self.fps;
        let source = self.source;
        thread::Builder::new()
            .name("rdpmac-capture".into())
            .spawn(move || match source {
                FrameSource::Screen => capture.capture_loop(fps),
                FrameSource::TestPattern { width, height } => capture.pattern_loop(width, height, fps),
            })
            .context("spawning the capture thread")?;
        let cursor = Threads {
            geometry: self.geometry.clone(),
            stop: stop.clone(),
            tx,
        };
        let hz = self.cursor_hz;
        thread::Builder::new()
            .name("rdpmac-cursor".into())
            .spawn(move || cursor.cursor_loop(hz))
            .context("spawning the cursor thread")?;
        Ok(Box::new(Updates { rx, stop }))
    }
}

struct Updates {
    rx: Receiver<DisplayUpdate>,
    stop: Arc<AtomicBool>,
}

#[async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        Ok(self.rx.recv().await)
    }
}

impl Drop for Updates {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Threads {
    geometry: SharedGeometry,
    stop: Arc<AtomicBool>,
    tx: Sender<DisplayUpdate>,
}

/// Frames offered to the client per period, and what became of them.
#[derive(Default)]
struct Stats {
    sent: u32,
    dropped: u32,
    bytes: u64,
    since: Option<Instant>,
}

impl Stats {
    fn record(&mut self, sent: bool, bytes: usize) {
        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        if sent {
            self.sent += 1;
            self.bytes += bytes as u64;
        } else {
            self.dropped += 1;
        }
        let elapsed = now.duration_since(since);
        if elapsed >= STATS_INTERVAL {
            let secs = elapsed.as_secs_f64();
            info!(
                fps = format_args!("{:.1}", f64::from(self.sent) / secs),
                dropped = self.dropped,
                mib_per_s = format_args!("{:.1}", self.bytes as f64 / secs / 1_048_576.0),
                "frames offered to the encoder"
            );
            *self = Self::default();
        }
    }
}

impl Threads {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.tx.is_closed()
    }

    /// Offers one frame without blocking; a full channel means the client is behind.
    fn offer(&self, update: DisplayUpdate, stats: &mut Stats) -> bool {
        let bytes = match &update {
            DisplayUpdate::Bitmap(b) => b.data.len(),
            _ => 0,
        };
        match self.tx.try_send(update) {
            Ok(()) => {
                stats.record(true, bytes);
                true
            }
            Err(TrySendError::Full(_)) => {
                stats.record(false, bytes);
                true
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }

    fn pattern_loop(self, width: u32, height: u32, fps: u32) {
        let interval = Duration::from_millis(1000 / u64::from(fps));
        let mut pattern = TestPattern::new(width, height);
        let mut stats = Stats::default();
        info!(width, height, fps, "test pattern started");
        while !self.stopped() {
            let started = Instant::now();
            if let Some(update) = pattern.next_frame() {
                if !self.offer(DisplayUpdate::Bitmap(update), &mut stats) {
                    return;
                }
            }
            if let Some(rest) = interval.checked_sub(started.elapsed()) {
                thread::sleep(rest);
            }
        }
    }

    fn capture_loop(self, fps: u32) {
        let interval = Duration::from_millis(1000 / u64::from(fps));
        let mut permission_reported = false;
        while !self.stopped() {
            let geometry = current(&self.geometry);
            let mut capturer = match Capturer::open(geometry.id) {
                Ok(c) => c,
                Err(CaptureError::Permission) => {
                    if !permission_reported {
                        error!("screen recording permission missing; clients will see no picture until it is granted");
                        permission_reported = true;
                    }
                    thread::sleep(REOPEN_DELAY);
                    continue;
                }
                Err(e) => {
                    error!(%e, "opening the capturer failed");
                    thread::sleep(REOPEN_DELAY);
                    continue;
                }
            };
            permission_reported = false;
            let mut stats = Stats::default();
            let mut last_poll = Instant::now();
            info!(display = geometry.id, width = capturer.width(), height = capturer.height(), "capture started");
            if capturer.width() != geometry.width || capturer.height() != geometry.height {
                self.send_resize(capturer.width(), capturer.height());
            }
            while !self.stopped() {
                if last_poll.elapsed() >= DISPLAY_POLL {
                    last_poll = Instant::now();
                    if self.display_changed(geometry.id, capturer.width(), capturer.height()) {
                        info!("display size changed, reopening the capturer");
                        break;
                    }
                }
                match capturer.frame(interval) {
                    Ok(frame) => {
                        if let Some(update) = rdpmac_encode::full_frame_update(&frame) {
                            if !self.offer(DisplayUpdate::Bitmap(update), &mut stats) {
                                return;
                            }
                        }
                    }
                    Err(CaptureError::Timeout) => {}
                    Err(CaptureError::Reset) => {
                        info!("display configuration changed, reopening the capturer");
                        break;
                    }
                    Err(e) => {
                        error!(%e, "capture failed, reopening");
                        thread::sleep(Duration::from_secs(1));
                        break;
                    }
                }
            }
        }
    }

    /// True when the display is gone or no longer has the size the capturer was opened with.
    /// ScreenCaptureKit keeps a stream alive across a resolution change and scales the picture,
    /// so the size has to be watched from outside.
    fn display_changed(&self, id: u32, width: u32, height: u32) -> bool {
        match list_displays() {
            Ok(displays) => match displays.iter().find(|d| d.id == id) {
                Some(d) => {
                    if d.width != width || d.height != height {
                        let mut g = self.geometry.lock().unwrap_or_else(|e| e.into_inner());
                        g.x = d.x;
                        g.y = d.y;
                        g.scale = if d.scale > 0.0 { d.scale as f64 } else { 1.0 };
                        true
                    } else {
                        false
                    }
                }
                None => true,
            },
            Err(_) => false,
        }
    }

    fn send_resize(&self, width: u32, height: u32) {
        {
            let mut g = self.geometry.lock().unwrap_or_else(|e| e.into_inner());
            g.width = width;
            g.height = height;
        }
        let size = DesktopSize {
            width: width.min(u32::from(u16::MAX)) as u16,
            height: height.min(u32::from(u16::MAX)) as u16,
        };
        let _ = self.tx.blocking_send(DisplayUpdate::Resize(size));
    }

    fn cursor_loop(self, hz: u32) {
        let interval = Duration::from_millis(1000 / u64::from(hz));
        let mut cache = PointerCache::default();
        let mut last_position: Option<(u16, u16)> = None;
        let mut last_shape: Option<u64> = None;
        let mut visible = true;
        while !self.stopped() {
            thread::sleep(interval);
            let geometry = current(&self.geometry);
            match cursor_position() {
                Ok(pos) if !pos.visible => {
                    if visible {
                        visible = false;
                        if self.tx.blocking_send(DisplayUpdate::HidePointer).is_err() {
                            return;
                        }
                    }
                    continue;
                }
                Ok(pos) => {
                    if !visible {
                        visible = true;
                        last_shape = None;
                    }
                    let (x, y) = geometry.to_pixels(pos.x, pos.y);
                    if last_position != Some((x, y)) {
                        last_position = Some((x, y));
                        if self.tx.blocking_send(position_update(x, y)).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    debug!(%e, "cursor position unavailable");
                    continue;
                }
            }
            // Reading the shape means decoding a bitmap; the id is a cheap probe for change.
            match cursor_shape_id() {
                Ok(id) if last_shape == Some(id) => continue,
                Ok(_) => {}
                Err(e) => {
                    debug!(%e, "cursor shape id unavailable");
                    continue;
                }
            }
            match cursor_shape() {
                Ok(shape) => {
                    last_shape = Some(shape.id);
                    if let Some(update) = cache.update_for(&shape) {
                        if self.tx.blocking_send(update).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => debug!(%e, "cursor shape unavailable"),
            }
        }
    }
}
