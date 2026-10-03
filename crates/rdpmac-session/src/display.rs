//! `RdpServerDisplay` on top of libscreenio.
//!
//! The session size either follows what the client asks for or is the display's own pixel size.
//! `updates()` starts two OS threads that feed a bounded channel: one produces frames at the
//! session size, one polls the cursor. IronRDP drains the channel from its event loop. Frames are
//! dropped when the channel is full, so a slow client never builds a backlog.
//!
//! A size change asked for mid-session is answered with `DisplayUpdate::Resize`. IronRDP then
//! re-runs the capability exchange at the new size, asks `size()` again and restarts the update
//! stream, which starts both threads afresh.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};
use ironrdp_server::{
    DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates, ServerError, ServerErrorExt as _, ServerResult,
};
use screenio_core::{
    cursor_position, cursor_shape, cursor_shape_id, list_displays, Capturer, DisplayInfo, Error as CaptureError,
};
use tokio::sync::mpsc::{self, error::TrySendError, Receiver, Sender};
use tracing::{debug, error, info, warn};

use crate::cursor::{position_update, ClientPointer, PointerCache};
use crate::monitor::MonitorPolicy;
use crate::pattern::TestPattern;
use crate::unlock::Unlocker;
use crate::virtual_screen::{StreamGuard, VirtualScreen};
use crate::{current, store, Geometry, Rect, SharedGeometry};

const CHANNEL_DEPTH: usize = 4;
const REOPEN_DELAY: Duration = Duration::from_secs(5);
const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// How often the capture loop checks whether the display changed or was replaced.
const DISPLAY_POLL: Duration = Duration::from_secs(1);
/// How long a session start waits for a display to wake.
const WAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a client's request for no picture must stand before the picture stops: mstsc makes
/// one while it connects, and some clients flap it under load.
const SUPPRESS_AFTER: Duration = Duration::from_secs(1);
/// How often a stopped picture checks whether the client wants it again.
const SUPPRESSED_POLL: Duration = Duration::from_millis(100);

/// How the session size is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMode {
    /// The size the client asks for, at connect time and whenever its window changes. The
    /// display is scaled to it when the two differ.
    FollowClient,
    /// The display's own pixel size, whatever the client asks for.
    Native,
}

/// Where frames come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    /// The display chosen by the monitor policy, through libscreenio.
    Screen,
    /// A synthetic moving picture that needs no permission and exercises the encoder. The size
    /// is used when the session does not follow the client.
    TestPattern { width: u32, height: u32 },
}

/// State shared between the handler IronRDP calls and the threads producing updates.
#[derive(Default)]
struct Session {
    /// The size the client asked for; `None` until it asks.
    requested: Mutex<Option<(u32, u32)>>,
    /// Set when the client asks for a new size mid-session; the frame thread answers with a
    /// `Resize`.
    resize_pending: AtomicBool,
    /// Set when the client asks for the whole picture again; the frame thread sends it.
    refresh_requested: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn desktop_size((width, height): (u32, u32)) -> DesktopSize {
    DesktopSize {
        width: width.min(u32::from(u16::MAX)) as u16,
        height: height.min(u32::from(u16::MAX)) as u16,
    }
}

fn select_display(policy: &dyn MonitorPolicy) -> Option<DisplayInfo> {
    match list_displays() {
        Ok(displays) => policy.select(&displays),
        Err(e) => {
            debug!(%e, "listing displays failed");
            None
        }
    }
}

/// True when the policy now picks another display, or the same one at another size or place.
fn display_moved(policy: &dyn MonitorPolicy, display: &DisplayInfo) -> bool {
    match select_display(policy) {
        Some(d) => {
            d.id != display.id
                || d.width != display.width
                || d.height != display.height
                || d.x != display.x
                || d.y != display.y
                || (d.scale - display.scale).abs() > f32::EPSILON
        }
        None => true,
    }
}

/// Declares the user active and waits for a display to wake. Until a user is active, a Mac asleep
/// or locked keeps its display dark, keeps listing a virtual display released earlier, and has
/// its lock screen turn every password down unchecked.
fn wake() {
    match screenio_core::wake_displays(WAKE_TIMEOUT) {
        Ok(true) => {}
        Ok(false) => warn!(seconds = WAKE_TIMEOUT.as_secs(), "no display woke up; the picture stays dark until one does"),
        Err(e) => warn!(%e, "declaring the user active failed"),
    }
}

fn frame_interval(rate: u32) -> Duration {
    Duration::from_millis(1000 / u64::from(rate.max(1)))
}

pub struct DisplayHandler {
    policy: Arc<dyn MonitorPolicy>,
    geometry: SharedGeometry,
    source: FrameSource,
    mode: ResolutionMode,
    session: Arc<Session>,
    fps: u32,
    cursor_hz: u32,
    #[cfg(target_os = "macos")]
    gfx: Option<Arc<crate::gfx::GfxLink>>,
    virtual_screen: Option<Arc<VirtualScreen>>,
    /// IronRDP's flag for a client that asked for no picture.
    suppressed: Option<Arc<AtomicBool>>,
    /// Where the client's input put the cursor, which the client needs no report of.
    pointer: Arc<ClientPointer>,
    /// Types the password of the user who logged on into the lock screen once the picture starts.
    unlocker: Option<Arc<Unlocker>>,
}

impl DisplayHandler {
    pub fn new(
        policy: Arc<dyn MonitorPolicy>,
        geometry: SharedGeometry,
        source: FrameSource,
        mode: ResolutionMode,
        fps: u32,
        cursor_hz: u32,
    ) -> Self {
        Self {
            policy,
            geometry,
            source,
            mode,
            session: Arc::new(Session::default()),
            fps: fps.clamp(1, 120),
            cursor_hz: cursor_hz.clamp(1, 120),
            #[cfg(target_os = "macos")]
            gfx: None,
            virtual_screen: None,
            suppressed: None,
            pointer: Arc::new(ClientPointer::default()),
            unlocker: None,
        }
    }

    /// Reports to the client only the cursor moves its own input, which the input handler records
    /// in `pointer`, does not explain.
    pub fn with_client_pointer(mut self, pointer: Arc<ClientPointer>) -> Self {
        self.pointer = pointer;
        self
    }

    /// Has the input thread type the password of the user who logged on into the lock screen
    /// when the session's picture starts, after the display woke up for it.
    pub fn with_unlocker(mut self, unlocker: Option<Arc<Unlocker>>) -> Self {
        self.unlocker = unlocker;
        self
    }

    /// Stops the picture while the client asks for none, as mstsc does while minimised, with
    /// the flag IronRDP keeps for that (`with_display_suppressed_handle`).
    pub fn with_suppression(mut self, flag: Arc<AtomicBool>) -> Self {
        self.suppressed = Some(flag);
        self
    }

    /// Serves sessions that follow the client from a virtual display at the client's size when
    /// no screen is attached.
    pub fn with_virtual_screen(mut self, screen: Arc<VirtualScreen>) -> Self {
        self.virtual_screen = Some(screen);
        self
    }

    /// Sends frames as H.264 through the graphics pipeline when the client negotiates it.
    #[cfg(target_os = "macos")]
    pub fn with_gfx(mut self, link: Arc<crate::gfx::GfxLink>) -> Self {
        self.gfx = Some(link);
        self
    }

    /// The session size for a picture whose own size is `native`.
    fn session_size(&self, native: (u32, u32)) -> (u32, u32) {
        match (self.mode, *lock(&self.session.requested)) {
            (ResolutionMode::FollowClient, Some(requested)) => requested,
            _ => native,
        }
    }

    /// Re-reads the display and recomputes the mapping for the current session size.
    fn refresh(&self) -> Geometry {
        let geometry = match self.source {
            FrameSource::TestPattern { width, height } => {
                let (w, h) = self.session_size((width, height));
                Geometry::synthetic(w, h)
            }
            FrameSource::Screen => match select_display(self.policy.as_ref()) {
                Some(d) => {
                    let (w, h) = self.session_size((d.width, d.height));
                    Geometry::fitted(&d, w, h)
                }
                None => {
                    // No display right now (asleep or being replaced): keep the last place at the
                    // session size; the capture thread waits for a display to come back.
                    let last = current(&self.geometry);
                    let (w, h) = self.session_size((last.width, last.height));
                    Geometry {
                        width: w,
                        height: h,
                        content: Rect {
                            x: 0.0,
                            y: 0.0,
                            width: f64::from(w),
                            height: f64::from(h),
                        },
                        ..last
                    }
                }
            },
        };
        store(&self.geometry, geometry);
        geometry
    }
}

#[async_trait]
impl RdpServerDisplay for DisplayHandler {
    async fn size(&mut self) -> DesktopSize {
        let g = self.refresh();
        desktop_size((g.width, g.height))
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        if self.source == FrameSource::Screen {
            if let Err(e) = tokio::task::spawn_blocking(wake).await {
                warn!(%e, "waking the displays failed");
            }
        }
        if self.mode == ResolutionMode::FollowClient {
            let requested = (u32::from(client_size.width), u32::from(client_size.height));
            info!(width = requested.0, height = requested.1, "session size follows the client");
            *lock(&self.session.requested) = Some(requested);
            // Called after the credentials passed, and again at the new size after a resize.
            if let (Some(screen), FrameSource::Screen) = (self.virtual_screen.clone(), self.source) {
                let prepared = tokio::task::spawn_blocking(move || screen.prepare(requested.0, requested.1)).await;
                if let Err(e) = prepared {
                    warn!(%e, "preparing the virtual display failed");
                }
            }
        }
        self.size().await
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let monitors = layout.monitors();
        let Some(monitor) = monitors.iter().find(|m| m.is_primary()).or_else(|| monitors.first()) else {
            return;
        };
        let (w, h) = monitor.dimensions();
        let (width, height) = MonitorLayoutEntry::adjust_display_size(w, h);
        if self.mode != ResolutionMode::FollowClient {
            debug!(width, height, "client asked for a new size; serving the display's own size");
            return;
        }
        let mut requested = lock(&self.session.requested);
        if *requested == Some((width, height)) {
            return;
        }
        info!(width, height, scale = ?monitor.desktop_scale_factor(), "client asked for a new session size");
        *requested = Some((width, height));
        self.session.resize_pending.store(true, Ordering::Release);
    }

    fn request_refresh(&mut self) {
        self.session.refresh_requested.store(true, Ordering::Release);
    }

    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let geometry = self.refresh();
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
        let stop = Arc::new(AtomicBool::new(false));
        let producer = |tx: Sender<DisplayUpdate>| Producer {
            policy: self.policy.clone(),
            geometry: self.geometry.clone(),
            session: self.session.clone(),
            mode: self.mode,
            announced: (geometry.width, geometry.height),
            stop: stop.clone(),
            tx,
            #[cfg(target_os = "macos")]
            gfx: self.gfx.clone(),
            suppressed: self.suppressed.clone(),
            pointer: self.pointer.clone(),
        };
        let frames = producer(tx.clone());
        let cursor = producer(tx);
        let (fps, source, hz) = (self.fps, self.source, self.cursor_hz);
        thread::Builder::new()
            .name("rdpmac-frames".into())
            .spawn(move || match source {
                FrameSource::Screen => frames.screen_loop(fps),
                FrameSource::TestPattern { .. } => frames.pattern_loop(fps),
            })
            .map_err(|e| ServerError::io("spawning the frame thread", e))?;
        thread::Builder::new()
            .name("rdpmac-cursor".into())
            .spawn(move || cursor.cursor_loop(hz))
            .map_err(|e| ServerError::io("spawning the cursor thread", e))?;
        info!(width = geometry.width, height = geometry.height, "session picture");
        if let (Some(unlocker), FrameSource::Screen) = (&self.unlocker, self.source) {
            unlocker.session_started();
        }
        Ok(Box::new(Updates {
            rx,
            stop,
            _virtual_screen: self.virtual_screen.as_ref().map(VirtualScreen::stream),
        }))
    }
}

struct Updates {
    rx: Receiver<DisplayUpdate>,
    stop: Arc<AtomicBool>,
    _virtual_screen: Option<StreamGuard>,
}

#[async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        Ok(self.rx.recv().await)
    }
}

impl Drop for Updates {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Frames offered to the client per period, and what became of them.
#[derive(Default)]
struct Stats {
    frames: u32,
    h264_frames: u32,
    dropped: u32,
    bitmap_bytes: u64,
    h264_bytes: u64,
    since: Option<Instant>,
}

impl Stats {
    fn record(&mut self, delivery: Delivery) {
        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        match delivery {
            Delivery::Bitmap(bytes) => {
                self.frames += 1;
                self.bitmap_bytes += bytes as u64;
            }
            Delivery::H264(bytes) => {
                self.frames += 1;
                self.h264_frames += 1;
                self.h264_bytes += bytes as u64;
            }
            Delivery::Dropped => self.dropped += 1,
        }
        let elapsed = now.duration_since(since);
        if elapsed >= STATS_INTERVAL {
            let secs = elapsed.as_secs_f64();
            info!(
                fps = format_args!("{:.1}", f64::from(self.frames) / secs),
                h264_frames = self.h264_frames,
                dropped = self.dropped,
                h264_mbit_per_s = format_args!("{:.2}", self.h264_bytes as f64 * 8.0 / secs / 1_000_000.0),
                bitmap_mib_per_s = format_args!("{:.1}", self.bitmap_bytes as f64 / secs / 1_048_576.0),
                "frames delivered"
            );
            *self = Self::default();
        }
    }
}

/// One producer thread's view of the session.
struct Producer {
    policy: Arc<dyn MonitorPolicy>,
    geometry: SharedGeometry,
    session: Arc<Session>,
    mode: ResolutionMode,
    /// The session size IronRDP negotiated for this activation.
    announced: (u32, u32),
    stop: Arc<AtomicBool>,
    tx: Sender<DisplayUpdate>,
    #[cfg(target_os = "macos")]
    gfx: Option<Arc<crate::gfx::GfxLink>>,
    suppressed: Option<Arc<AtomicBool>>,
    pointer: Arc<ClientPointer>,
}

/// What the client asked of the picture: none at all (Suppress Output, which mstsc sends while
/// its window is minimised), or all of it again (Refresh Rect, or Suppress Output lifted).
struct ClientRequests {
    /// IronRDP's flag, set while the client asks for no picture.
    suppressed: Option<Arc<AtomicBool>>,
    /// Set once the client wanted the picture after this stream sent some: mstsc also asks for
    /// none while it connects, and honouring that would keep the first picture from it.
    armed: bool,
    suppressed_since: Option<Instant>,
    refreshed_at: Option<Instant>,
}

impl ClientRequests {
    fn new(suppressed: Option<Arc<AtomicBool>>) -> Self {
        Self {
            suppressed,
            armed: false,
            suppressed_since: None,
            refreshed_at: None,
        }
    }

    fn none_wanted(&self) -> bool {
        self.suppressed.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// Whether the picture should stop now. `sent` tells whether this stream sent a picture yet.
    fn stop(&mut self, sent: bool, now: Instant) -> bool {
        if !self.none_wanted() {
            self.suppressed_since = None;
            self.armed |= sent;
            return false;
        }
        let since = *self.suppressed_since.get_or_insert(now);
        self.armed && now.duration_since(since) >= SUPPRESS_AFTER
    }

    /// The picture restarts after a stop, complete, which answers any refresh asked for with it.
    fn resumed(&mut self, now: Instant) {
        self.refreshed_at = Some(now);
    }

    /// Whether to send the whole picture again for a refresh the client `requested`; not when one
    /// went out moments ago, since mstsc asks twice when restored, and clients under load flap.
    fn refresh(&mut self, requested: bool, now: Instant) -> bool {
        if !requested || self.refreshed_at.is_some_and(|at| now.duration_since(at) < SUPPRESS_AFTER) {
            return false;
        }
        self.refreshed_at = Some(now);
        true
    }
}

/// How a frame left the server, for the statistics.
enum Delivery {
    Bitmap(usize),
    H264(usize),
    Dropped,
}

/// Sends frames through the graphics pipeline while the client has it open with H.264.
struct Route {
    #[cfg(target_os = "macos")]
    stream: Option<crate::gfx::GfxStream>,
}

impl Route {
    fn new(producer: &Producer, fps: u32) -> Self {
        #[cfg(not(target_os = "macos"))]
        let _ = (producer, fps);
        Self {
            #[cfg(target_os = "macos")]
            stream: producer.gfx.clone().map(|link| crate::gfx::GfxStream::new(link, fps)),
        }
    }

    /// `Some` when the pipeline took care of the frame, sent or deliberately skipped; `None`
    /// means the caller sends a bitmap update instead.
    fn try_h264(&mut self, bgra: &[u8], width: u32, height: u32, stride: usize) -> Option<Delivery> {
        #[cfg(target_os = "macos")]
        if let Some(stream) = self.stream.as_mut() {
            return match stream.send(bgra, width, height, stride) {
                crate::gfx::GfxOutcome::Sent(bytes) => Some(Delivery::H264(bytes)),
                crate::gfx::GfxOutcome::Skipped => Some(Delivery::Dropped),
                crate::gfx::GfxOutcome::Unavailable => None,
            };
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (bgra, width, height, stride);
        None
    }

    /// The screen did not change for a frame interval: lets the pipeline catch up or sharpen.
    fn idle(&mut self, stats: &mut Stats) {
        #[cfg(target_os = "macos")]
        if let Some(bytes) = self.stream.as_mut().and_then(crate::gfx::GfxStream::refine) {
            stats.record(Delivery::H264(bytes));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = stats;
    }

    /// The client asked for the whole picture again.
    fn refresh(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(stream) = self.stream.as_mut() {
            stream.refresh();
        }
    }

    /// The client wants the picture again after asking for none.
    fn resume(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(stream) = self.stream.as_mut() {
            stream.resume();
        }
    }
}

impl Producer {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.tx.is_closed()
    }

    /// Waits while the client wants no picture; false when the stream ended meanwhile.
    fn wait_while_suppressed(&self, requests: &ClientRequests) -> bool {
        info!("the client asked for no picture, as mstsc does while minimised; pausing the picture");
        while requests.none_wanted() {
            if self.stopped() {
                return false;
            }
            thread::sleep(SUPPRESSED_POLL);
        }
        info!("the client wants the picture again; sending all of it");
        true
    }

    /// Sends the whole picture again if the client asked for it.
    fn answer_refresh(&self, requests: &mut ClientRequests, route: &mut Route) {
        let requested = self.session.refresh_requested.swap(false, Ordering::AcqRel);
        if requests.refresh(requested, Instant::now()) {
            info!("the client asked for the whole picture again");
            route.refresh();
        }
    }

    /// Answers a pending mid-session size request by announcing the new size. IronRDP restarts
    /// the update stream at that size, so the caller ends when this returns true.
    fn answer_resize(&self) -> bool {
        if !self.session.resize_pending.swap(false, Ordering::AcqRel) {
            return false;
        }
        let Some(target) = *lock(&self.session.requested) else {
            return false;
        };
        if target == self.announced {
            return false;
        }
        info!(width = target.0, height = target.1, "resizing the session");
        let _ = self.tx.blocking_send(DisplayUpdate::Resize(desktop_size(target)));
        true
    }

    /// Offers one frame without blocking; a full channel means the client is behind.
    fn offer(&self, update: DisplayUpdate, stats: &mut Stats) -> bool {
        let bytes = match &update {
            DisplayUpdate::Bitmap(b) => b.data.len(),
            _ => 0,
        };
        match self.tx.try_send(update) {
            Ok(()) => {
                stats.record(Delivery::Bitmap(bytes));
                true
            }
            Err(TrySendError::Full(_)) => {
                stats.record(Delivery::Dropped);
                true
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }

    fn screen_loop(self, fps: u32) {
        let interval = frame_interval(fps);
        let mut stats = Stats::default();
        let mut route = Route::new(&self, fps);
        let mut permission_reported = false;
        let mut missing_reported = false;
        let mut requests = ClientRequests::new(self.suppressed.clone());
        let mut sent = false;
        let mut paused = false;
        while !self.stopped() {
            if self.answer_resize() {
                return;
            }
            if paused || requests.stop(sent, Instant::now()) {
                // Nothing is captured meanwhile; reopening captures the screen as it is by then.
                if !self.wait_while_suppressed(&requests) {
                    return;
                }
                requests.resumed(Instant::now());
                route.resume();
                paused = false;
                continue;
            }
            // Chosen afresh on every (re)open: display numbers change when displays are replaced,
            // for example when BetterDisplay starts or stops.
            let Some(chosen) = select_display(self.policy.as_ref()) else {
                if !missing_reported {
                    warn!("no display to capture; waiting for one");
                    missing_reported = true;
                }
                thread::sleep(DISPLAY_POLL);
                continue;
            };
            missing_reported = false;
            let native = (chosen.width, chosen.height);
            let size = match self.mode {
                ResolutionMode::FollowClient => self.announced,
                ResolutionMode::Native => native,
            };
            if size != self.announced {
                // Serving the display's own size and that size changed: the client must follow.
                info!(width = size.0, height = size.1, "display size changed, resizing the session");
                let _ = self.tx.blocking_send(DisplayUpdate::Resize(desktop_size(size)));
                return;
            }
            store(&self.geometry, Geometry::fitted(&chosen, size.0, size.1));
            let scaled = size != native;
            let opened = if scaled {
                Capturer::open_scaled_with_rate(chosen.id, size.0, size.1, fps)
            } else {
                Capturer::open_with_rate(chosen.id, fps)
            };
            let mut capturer = match opened {
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
                    error!(%e, display = chosen.id, "opening the capturer failed");
                    thread::sleep(REOPEN_DELAY);
                    continue;
                }
            };
            permission_reported = false;
            info!(display = chosen.id, width = size.0, height = size.1, scaled, "capture started");
            let mut last_poll = Instant::now();
            while !self.stopped() {
                if self.session.resize_pending.load(Ordering::Acquire) {
                    break;
                }
                if requests.stop(sent, Instant::now()) {
                    paused = true;
                    break;
                }
                self.answer_refresh(&mut requests, &mut route);
                if last_poll.elapsed() >= DISPLAY_POLL {
                    last_poll = Instant::now();
                    if display_moved(self.policy.as_ref(), &chosen) {
                        info!(display = chosen.id, "display changed or was replaced, reopening the capturer");
                        break;
                    }
                }
                match capturer.frame(interval) {
                    Ok(frame) => {
                        let exact = (frame.width, frame.height) == size;
                        match exact.then(|| route.try_h264(frame.data, size.0, size.1, frame.stride as usize)).flatten() {
                            Some(delivery) => {
                                sent |= matches!(delivery, Delivery::H264(_));
                                stats.record(delivery);
                            }
                            None => {
                                if let Some(update) = rdpmac_encode::frame_update(&frame, size.0, size.1) {
                                    if !self.offer(DisplayUpdate::Bitmap(update), &mut stats) {
                                        return;
                                    }
                                    sent = true;
                                }
                            }
                        }
                    }
                    Err(CaptureError::Timeout) => route.idle(&mut stats),
                    Err(CaptureError::Reset) => {
                        info!("capture stream stopped, reopening");
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

    fn pattern_loop(self, fps: u32) {
        let interval = frame_interval(fps);
        let (width, height) = self.announced;
        store(&self.geometry, Geometry::synthetic(width, height));
        let mut pattern = TestPattern::new(width, height);
        let mut stats = Stats::default();
        let mut route = Route::new(&self, fps);
        let mut requests = ClientRequests::new(self.suppressed.clone());
        let mut sent = false;
        info!(width, height, fps, "test pattern started");
        while !self.stopped() {
            if self.answer_resize() {
                return;
            }
            if requests.stop(sent, Instant::now()) {
                if !self.wait_while_suppressed(&requests) {
                    return;
                }
                requests.resumed(Instant::now());
                route.resume();
                continue;
            }
            self.answer_refresh(&mut requests, &mut route);
            let started = Instant::now();
            if let Some(update) = pattern.next_frame() {
                match route.try_h264(&update.data, width, height, update.stride.get()) {
                    Some(delivery) => {
                        sent |= matches!(delivery, Delivery::H264(_));
                        stats.record(delivery);
                    }
                    None => {
                        if !self.offer(DisplayUpdate::Bitmap(update), &mut stats) {
                            return;
                        }
                        sent = true;
                    }
                }
            }
            if let Some(rest) = interval.checked_sub(started.elapsed()) {
                thread::sleep(rest);
            }
        }
    }

    fn cursor_loop(self, hz: u32) {
        let interval = frame_interval(hz);
        let mut cache = PointerCache::default();
        let mut last_position: Option<(u16, u16)> = None;
        let mut last_shape: Option<u64> = None;
        let mut last_density = 0.0f64;
        let mut visible = true;
        while !self.stopped() {
            thread::sleep(interval);
            let geometry = current(&self.geometry);
            let density = geometry.pixels_per_point();
            if (density - last_density).abs() > 1e-6 {
                // The picture was rescaled: resend the pointer at its new size.
                last_density = density;
                last_shape = None;
            }
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
                        // The client shows its pointer where its own input put it; told so again, a
                        // moment late, it would pull the pointer back while the user moves it.
                        let clients_own = self.pointer.explains((pos.x, pos.y));
                        if !clients_own && self.tx.blocking_send(position_update(x, y)).is_err() {
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
                    if let Some(update) = cache.update_for(&shape, density) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::PrimaryMonitor;

    fn handler(mode: ResolutionMode) -> DisplayHandler {
        DisplayHandler::new(
            Arc::new(PrimaryMonitor),
            crate::shared(Geometry::synthetic(1920, 1080)),
            FrameSource::TestPattern { width: 1920, height: 1080 },
            mode,
            30,
            30,
        )
    }

    fn layout(width: u32, height: u32) -> DisplayControlMonitorLayout {
        let entry = MonitorLayoutEntry::new_primary(width, height).expect("valid monitor");
        DisplayControlMonitorLayout::new(&[entry]).expect("valid layout")
    }

    fn size(width: u16, height: u16) -> DesktopSize {
        DesktopSize { width, height }
    }

    #[tokio::test]
    async fn follows_the_client_at_connect_and_on_resize() {
        let mut h = handler(ResolutionMode::FollowClient);
        assert_eq!(h.size().await, size(1920, 1080));
        assert_eq!(h.request_initial_size(size(1280, 720)).await, size(1280, 720));
        assert_eq!(h.size().await, size(1280, 720));

        h.request_layout(layout(1600, 900));
        assert!(h.session.resize_pending.load(Ordering::Acquire));
        assert_eq!(h.size().await, size(1600, 900));

        // Asking again for the size already requested is not another resize.
        h.session.resize_pending.store(false, Ordering::Release);
        h.request_layout(layout(1600, 900));
        assert!(!h.session.resize_pending.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn odd_widths_are_made_even() {
        let mut h = handler(ResolutionMode::FollowClient);
        h.request_layout(layout(1601, 900));
        assert_eq!(h.size().await.width % 2, 0);
    }

    #[tokio::test]
    async fn native_mode_ignores_client_sizes() {
        let mut h = handler(ResolutionMode::Native);
        assert_eq!(h.request_initial_size(size(1280, 720)).await, size(1920, 1080));
        h.request_layout(layout(1600, 900));
        assert!(!h.session.resize_pending.load(Ordering::Acquire));
        assert_eq!(h.size().await, size(1920, 1080));
    }

    #[test]
    fn the_picture_stops_only_once_a_request_for_none_has_stood_after_one_was_wanted() {
        let flag = Arc::new(AtomicBool::new(true));
        let mut requests = ClientRequests::new(Some(flag.clone()));
        let t0 = Instant::now();
        let later = |ms| t0 + Duration::from_millis(ms);
        // mstsc asks for no picture while it connects: the first picture goes out regardless.
        assert!(!requests.stop(false, t0));
        assert!(!requests.stop(true, later(5_000)), "not before the client wanted a picture");
        flag.store(false, Ordering::Relaxed);
        assert!(!requests.stop(true, later(5_100)));
        // Minimised: the picture stops once the request has stood for a moment.
        flag.store(true, Ordering::Relaxed);
        assert!(!requests.stop(true, later(6_000)));
        assert!(!requests.stop(true, later(6_500)), "a flap is not a stop");
        assert!(requests.stop(true, later(7_000)));
        // Restored, then a short flap.
        flag.store(false, Ordering::Relaxed);
        assert!(!requests.stop(true, later(9_000)));
        flag.store(true, Ordering::Relaxed);
        assert!(!requests.stop(true, later(9_100)));
        flag.store(false, Ordering::Relaxed);
        assert!(!requests.stop(true, later(9_200)));
        flag.store(true, Ordering::Relaxed);
        assert!(!requests.stop(true, later(9_300)), "the flap before does not count towards this one");
        assert!(!ClientRequests::new(None).stop(true, later(20_000)), "no flag, never stopped");
    }

    #[test]
    fn a_refresh_goes_out_once_for_requests_close_together() {
        let mut requests = ClientRequests::new(None);
        let t0 = Instant::now();
        let later = |ms| t0 + Duration::from_millis(ms);
        assert!(!requests.refresh(false, t0));
        assert!(requests.refresh(true, t0));
        assert!(!requests.refresh(true, later(300)), "restoring mstsc asks twice");
        assert!(requests.refresh(true, later(2_000)));
        requests.resumed(later(5_000));
        assert!(!requests.refresh(true, later(5_200)), "the picture sent on resuming answers it");
        assert!(requests.refresh(true, later(7_000)));
    }
}
