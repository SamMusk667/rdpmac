//! The graphics pipeline (MS-RDPEGFX) path: H.264 frames for clients that negotiate AVC420, in
//! full colour as AVC444v2 where the client and the session size allow it.
//!
//! IronRDP asks [`GfxFactory`] for a graphics pipeline server on every connection; the factory
//! keeps a handle to it in a [`GfxLink`] shared with the frame thread. The frame thread's
//! [`GfxStream`] creates the surface, encodes with VideoToolbox and submits frames through that
//! handle, then posts the resulting channel messages on IronRDP's event channel.
//!
//! AVC444 has the client decode two pictures for every frame. Under sustained motion mstsc has
//! shown the auxiliary view in place of the main one, through key frames, until given a new
//! surface; so while the picture keeps changing only the main view goes out, as AVC420, and once
//! it is still the full colour comes back as a key frame on a new surface.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet, Encoding};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer, QoeMetrics};
use ironrdp_server::{EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent, ServerEventSender};
use ironrdp_svc::ChannelFlags;
use rdpmac_encode::avc444;
use rdpmac_encode::h264::{conversion_threads, EncodeError, EncodedFrame, H264Encoder};
use rdpmac_encode::rate::RateControl;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::dump::Dump;

/// Largest frame sent as H.264. Windows' Media Foundation H.264 decoder, which mstsc uses, stops
/// at 4096x2304; larger sessions stay on RemoteFX, which handles them well.
const MAX_H264_SIDE: u32 = 4096;
const MAX_H264_PIXELS: u32 = 4096 * 2304;

/// Quantisation and quality hints carried with every region; informational for decoders.
const REGION_QP: u8 = 22;
const REGION_QUALITY: u8 = 100;

/// An AVC444 picture that changed this often within `MOTION_WINDOW` (15 frames a second) goes
/// out as AVC420 until it has been still for `STILL_AFTER`.
const MOTION_WINDOW: Duration = Duration::from_millis(1500);
const MOTION_FRAMES: usize = 23;
const STILL_AFTER: Duration = Duration::from_secs(1);
/// How often the client's decoding times are logged.
const QOE_REPORT: Duration = Duration::from_secs(10);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the graphics pipeline may use. An update stream takes the options when it starts, so a
/// change reaches the next connection and leaves the current one as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GfxOptions {
    /// Whether AVC444 may be used; AVC420 only otherwise.
    pub avc444: bool,
    /// Whether AVC444's colour conversion may use several cores.
    pub parallel_conversion: bool,
}

/// What the factory and the frame thread share.
pub struct GfxLink {
    handle: Mutex<Option<GfxServerHandle>>,
    sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    options: Mutex<GfxOptions>,
    /// The log directory, when every stream is recorded under it.
    dump_to: Option<PathBuf>,
    /// The current stream's recording.
    dump: Mutex<Option<Dump>>,
}

impl GfxLink {
    /// `dump_to` is the log directory when streams are to be recorded (`h264-dump`).
    pub fn new(options: GfxOptions, dump_to: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            handle: Mutex::new(None),
            sender: Mutex::new(None),
            options: Mutex::new(options),
            dump_to,
            dump: Mutex::new(None),
        })
    }

    pub fn set_options(&self, options: GfxOptions) {
        *lock(&self.options) = options;
    }

    /// Adds to the current stream's recording, if one is being made; a failure ends it.
    fn record(&self, write: impl FnOnce(&mut Dump) -> std::io::Result<()>) {
        let mut dump = lock(&self.dump);
        if let Some(recording) = dump.as_mut() {
            if let Err(e) = write(recording) {
                warn!(%e, dir = %recording.dir().display(), "recording the H.264 stream failed; it ends here");
                *dump = None;
            }
        }
    }
}

pub struct GfxFactory {
    link: Arc<GfxLink>,
}

impl GfxFactory {
    pub fn new(link: Arc<GfxLink>) -> Self {
        Self { link }
    }
}

impl ServerEventSender for GfxFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *lock(&self.link.sender) = Some(sender);
    }
}

impl GfxServerFactory for GfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(Handler::new(self.link.clone()))
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let handler = Box::new(Handler::new(self.link.clone()));
        let handle: GfxServerHandle = Arc::new(Mutex::new(GraphicsPipelineServer::new(handler)));
        *lock(&self.link.handle) = Some(handle.clone());
        Some((GfxDvcBridge::new(handle.clone()), handle))
    }
}

struct Handler {
    link: Arc<GfxLink>,
    /// The client's reported decode and render times since the last report, in milliseconds.
    decode_times: Vec<u16>,
    reported_at: Instant,
}

impl Handler {
    fn new(link: Arc<GfxLink>) -> Self {
        Self {
            link,
            decode_times: Vec::new(),
            reported_at: Instant::now(),
        }
    }
}

impl GraphicsPipelineHandler for Handler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        debug!(?pdu, "client graphics capabilities");
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        info!(?negotiated, "graphics pipeline ready");
    }

    fn on_frame_ack(&mut self, frame_id: u32, queue_depth: u32, total_frames_decoded: u32) {
        self.link.record(|dump| {
            dump.event(format_args!("ack {frame_id} queue {queue_depth} decoded {total_frames_decoded}"))
        });
    }

    fn on_qoe_metrics(&mut self, metrics: QoeMetrics) {
        self.decode_times.push(metrics.time_diff_dr);
        let now = Instant::now();
        if now.duration_since(self.reported_at) >= QOE_REPORT {
            self.decode_times.sort_unstable();
            let median = self.decode_times[self.decode_times.len() / 2];
            let max = self.decode_times.last().copied().unwrap_or_default();
            // timeDiffEDR: from receiving a frame's End Frame PDU to having decoded and rendered it,
            // in milliseconds by MS-RDPEGFX 2.2.2.21 (IronRDP's comment says microseconds).
            info!(median_ms = median, max_ms = max, frames = self.decode_times.len(), "client decoding time");
            self.decode_times.clear();
            self.reported_at = now;
        }
        self.link.record(|dump| {
            dump.event(format_args!(
                "qoe {} timestamp {} se {} dr {}",
                metrics.frame_id, metrics.timestamp, metrics.time_diff_se, metrics.time_diff_dr
            ))
        });
    }

    fn on_close(&mut self) {
        info!("graphics pipeline closed");
        *lock(&self.link.dump) = None;
    }
}

/// What became of a frame offered to the graphics pipeline.
pub enum GfxOutcome {
    /// Encoded and queued; the payload is this many bytes.
    Sent(usize),
    /// Not sent because the client is behind; the caller should not fall back.
    Skipped,
    /// The pipeline cannot take frames now; send a bitmap update instead.
    Unavailable,
}

struct SurfaceState {
    id: u16,
    width: u32,
    height: u32,
    /// Which pipeline server the surface belongs to; each connection gets a new one.
    server: usize,
}

/// When the picture changed lately, to tell sustained motion from typing and stillness.
#[derive(Default)]
struct Motion {
    changes: VecDeque<Instant>,
}

impl Motion {
    /// Notes a change; whether the picture has kept changing for a while.
    fn changed(&mut self, now: Instant) -> bool {
        while self.changes.front().is_some_and(|&at| now.duration_since(at) > MOTION_WINDOW) {
            self.changes.pop_front();
        }
        self.changes.push_back(now);
        self.changes.len() >= MOTION_FRAMES
    }

    /// How long the picture has not changed.
    fn still_for(&self, now: Instant) -> Duration {
        self.changes.back().map_or(Duration::MAX, |&at| now.duration_since(at))
    }
}

/// One frame thread's use of the pipeline. Created per update stream, so a session resize or a
/// new connection starts with a fresh surface and a key frame.
pub struct GfxStream {
    link: Arc<GfxLink>,
    options: GfxOptions,
    fps: u32,
    encoder: Option<H264Encoder>,
    rate: Option<RateControl>,
    surface: Option<SurfaceState>,
    disabled: bool,
    /// Set once an AVC444 encoder could not be made, which leaves AVC420.
    avc444_unavailable: bool,
    announced: bool,
    too_large_reported: bool,
    started: Instant,
    /// Whether the encoder holds the newest picture, sent or waiting to be, so that refining it
    /// cannot put back something older than what the client shows.
    refinable: bool,
    motion: Motion,
}

impl GfxStream {
    pub fn new(link: Arc<GfxLink>, fps: u32) -> Self {
        let options = *lock(&link.options);
        Self {
            link,
            options,
            fps,
            encoder: None,
            rate: None,
            surface: None,
            disabled: false,
            avc444_unavailable: false,
            announced: false,
            too_large_reported: false,
            started: Instant::now(),
            refinable: false,
            motion: Motion::default(),
        }
    }

    /// Offers one BGRA frame of `width` x `height`, the session size.
    pub fn send(&mut self, bgra: &[u8], width: u32, height: u32, stride: usize) -> GfxOutcome {
        self.refinable = false;
        if self.disabled {
            return GfxOutcome::Unavailable;
        }
        let Some(handle) = lock(&self.link.handle).clone() else {
            return GfxOutcome::Unavailable;
        };
        let (Ok(w16), Ok(h16)) = (u16::try_from(width), u16::try_from(height)) else {
            return GfxOutcome::Unavailable;
        };
        if width > MAX_H264_SIDE || height > MAX_H264_SIDE || width * height > MAX_H264_PIXELS {
            if !self.too_large_reported {
                info!(width, height, "session larger than H.264 decoders accept, using RemoteFX");
                self.too_large_reported = true;
            }
            return GfxOutcome::Unavailable;
        }
        let avc444 = {
            let server = lock(&handle);
            if !server.is_ready() || !server.supports_avc420() {
                return GfxOutcome::Unavailable;
            }
            if server.should_backpressure() {
                drop(server);
                self.adapt(true);
                self.stage(bgra, width, height, stride);
                return GfxOutcome::Skipped;
            }
            self.options.avc444 && !self.avc444_unavailable && server.supports_avc444() && avc444::fits(width, height)
        };

        if self.encoder.as_ref().map(|e| (e.size(), e.avc444())) != Some(((width, height), avc444)) {
            match self.open_encoder(width, height, avc444) {
                Ok(encoder) => {
                    let quantiser = encoder.controls_quantiser();
                    let avc444 = encoder.avc444();
                    let parallel_conversion = avc444 && self.options.parallel_conversion;
                    info!(width, height, quantiser, avc444, parallel_conversion, "H.264 encoder ready");
                    self.rate = Some(RateControl::new(encoder.target_bitrate(), Instant::now()));
                    self.encoder = Some(encoder);
                    self.start_recording(width, height, avc444);
                }
                Err(e) => {
                    warn!(%e, width, height, "H.264 encoder unavailable, staying on bitmap updates");
                    self.disabled = true;
                    return GfxOutcome::Unavailable;
                }
            }
        }

        if let Some(encoder) = self.encoder.as_mut().filter(|e| e.avc444()) {
            if self.motion.changed(Instant::now()) && !encoder.main_only() {
                encoder.set_main_only(true);
                self.surface = None;
                info!("the picture keeps changing; sending the main view only, as AVC420");
                self.link.record(|dump| dump.event(format_args!("main view only")));
            }
        }

        let server_id = Arc::as_ptr(&handle) as usize;
        let stale = match &self.surface {
            Some(s) => s.server != server_id || s.width != width || s.height != height,
            None => true,
        };
        if stale {
            let mut server = lock(&handle);
            // A pipeline that already has surfaces (from before a resize) or another output size
            // has to be reset; a fresh one only needs to know the output size.
            if server.surface_ids().next().is_some() || !matches!(server.output_dimensions(), (0, 0)) {
                server.resize(w16, h16);
            } else {
                server.set_output_dimensions(w16, h16);
            }
            let Some(id) = server.create_surface(w16, h16) else {
                return GfxOutcome::Unavailable;
            };
            server.map_surface_to_output(id, 0, 0);
            self.flush(&mut server);
            self.surface = Some(SurfaceState {
                id,
                width,
                height,
                server: server_id,
            });
            if let Some(encoder) = self.encoder.as_mut() {
                encoder.request_key_frame();
            }
            info!(surface = id, width, height, "graphics surface created");
        }
        let Some(encoder) = self.encoder.as_mut() else {
            return GfxOutcome::Unavailable;
        };
        let encoded = match encoder.encode(bgra, stride) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                // Dropped or held back by the encoder, which keeps it for `refine`.
                self.refinable = true;
                return GfxOutcome::Skipped;
            }
            Err(e) => {
                warn!(%e, "H.264 encoding failed; recreating the encoder on the next frame");
                self.encoder = None;
                return GfxOutcome::Unavailable;
            }
        };
        self.refinable = true;
        self.deliver(&handle, encoded)
    }

    /// Called when capture waited a frame interval without a new frame: sends the picture the
    /// client is missing, or a sharper encoding of the one it has. `Some(bytes)` when a frame
    /// went out.
    pub fn refine(&mut self) -> Option<usize> {
        if !self.refinable {
            return None;
        }
        let handle = lock(&self.link.handle).clone()?;
        if let Some(encoder) = self.encoder.as_mut().filter(|e| e.main_only()) {
            if self.motion.still_for(Instant::now()) >= STILL_AFTER {
                encoder.set_main_only(false);
                encoder.resend();
                self.surface = None;
                info!("the picture is still; sending it in full colour again");
                self.link.record(|dump| dump.event(format_args!("both views")));
            }
        }
        let server_id = Arc::as_ptr(&handle) as usize;
        if self.surface.is_none() && !self.new_surface(&handle) {
            return None;
        }
        if self.surface.as_ref().is_none_or(|s| s.server != server_id) {
            return None;
        }
        {
            let server = lock(&handle);
            if !server.is_ready() || server.should_backpressure() {
                return None;
            }
        }
        let encoded = match self.encoder.as_mut()?.refine() {
            Ok(Some(frame)) => frame,
            Ok(None) => return None,
            Err(e) => {
                warn!(%e, "H.264 refinement failed; recreating the encoder on the next frame");
                self.encoder = None;
                self.refinable = false;
                return None;
            }
        };
        let qp = encoded.qp;
        match self.deliver(&handle, encoded) {
            GfxOutcome::Sent(bytes) => {
                debug!(bytes, ?qp, "sent a frame while the screen was still");
                Some(bytes)
            }
            GfxOutcome::Skipped | GfxOutcome::Unavailable => None,
        }
    }

    /// The client asked for the whole picture again: the newest picture goes out as a key frame,
    /// at the next still moment unless a new frame comes first. The client's graphics are reset
    /// and the picture goes to a new surface, which also starts its decoder afresh: mstsc has shown
    /// AVC444's two views swapped for minutes, through key frames, until something reset it.
    pub fn refresh(&mut self) {
        self.link.record(|dump| dump.event(format_args!("refresh")));
        self.surface = None;
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.resend();
        }
    }

    /// The client wants the picture again after asking for none, during which nothing was
    /// captured: the next frame is a key frame on a new surface, as for a refresh, and the bitrate
    /// starts over from the target, since frames a minimised client left unacknowledged say nothing
    /// about the link.
    pub fn resume(&mut self) {
        self.link.record(|dump| dump.event(format_args!("resumed")));
        self.refinable = false;
        self.surface = None;
        self.motion = Motion::default();
        let Some(encoder) = self.encoder.as_mut() else {
            return;
        };
        encoder.set_main_only(false);
        encoder.request_key_frame();
        let target = encoder.target_bitrate();
        if self.rate.as_ref().is_some_and(|rate| rate.current() != target) {
            match encoder.set_bitrate(target) {
                Ok(()) => info!(kbit_per_s = target / 1000, "H.264 bitrate reset"),
                Err(e) => warn!(%e, "resetting the H.264 bitrate failed"),
            }
        }
        self.rate = Some(RateControl::new(target, Instant::now()));
    }

    /// Resets the client's graphics and gives it a new surface at the encoder's size, for a picture
    /// resent while the screen is still; `send` does the same before a new frame.
    fn new_surface(&mut self, handle: &GfxServerHandle) -> bool {
        let Some((width, height)) = self.encoder.as_ref().map(H264Encoder::size) else {
            return false;
        };
        let (Ok(w16), Ok(h16)) = (u16::try_from(width), u16::try_from(height)) else {
            return false;
        };
        let mut server = lock(handle);
        if !server.is_ready() {
            return false;
        }
        server.resize(w16, h16);
        let Some(id) = server.create_surface(w16, h16) else {
            return false;
        };
        server.map_surface_to_output(id, 0, 0);
        self.flush(&mut server);
        self.surface = Some(SurfaceState {
            id,
            width,
            height,
            server: Arc::as_ptr(handle) as usize,
        });
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.request_key_frame();
        }
        info!(surface = id, width, height, "graphics surface created");
        true
    }

    /// Records the stream from a new encoder on, in a directory of its own, when streams are
    /// recorded.
    fn start_recording(&self, width: u32, height: u32, avc444: bool) {
        let Some(logs) = &self.link.dump_to else {
            return;
        };
        let started = Dump::start(logs, width, height, if avc444 { "avc444" } else { "avc420" });
        let mut dump = lock(&self.link.dump);
        match started {
            Ok(recording) => {
                info!(dir = %recording.dir().display(), "recording the H.264 stream");
                *dump = Some(recording);
            }
            Err(e) => {
                warn!(%e, "recording the H.264 stream failed");
                *dump = None;
            }
        }
    }

    /// An AVC444 encoder when asked for and possible, an AVC420 one otherwise.
    fn open_encoder(&mut self, width: u32, height: u32, avc444: bool) -> Result<H264Encoder, EncodeError> {
        if avc444 {
            let threads = if self.options.parallel_conversion { conversion_threads() } else { 1 };
            match H264Encoder::new_avc444(width, height, self.fps, threads) {
                Ok(encoder) => return Ok(encoder),
                Err(e) => {
                    warn!(%e, "AVC444 unavailable, sending H.264 in 4:2:0");
                    self.avc444_unavailable = true;
                }
            }
        }
        H264Encoder::new(width, height, self.fps)
    }

    /// Keeps a frame the client is too far behind to take, for `refine` to send later.
    fn stage(&mut self, bgra: &[u8], width: u32, height: u32, stride: usize) {
        let Some(encoder) = self.encoder.as_mut().filter(|e| e.size() == (width, height)) else {
            return;
        };
        match encoder.stage(bgra, stride) {
            Ok(()) => self.refinable = true,
            Err(e) => debug!(%e, "keeping a skipped frame failed"),
        }
    }

    /// Submits an encoded frame on the current surface.
    fn deliver(&mut self, handle: &GfxServerHandle, encoded: EncodedFrame) -> GfxOutcome {
        let Some(surface) = self.surface.as_ref() else {
            return GfxOutcome::Unavailable;
        };
        let (id, width, height) = (surface.id, surface.width, surface.height);
        let avc444 = self.encoder.as_ref().is_some_and(|e| e.avc444() && !e.main_only());
        if !self.announced {
            let codec = if avc444 { "AVC444v2" } else { "AVC420" };
            info!(width, height, codec, "sending H.264 through the graphics pipeline");
            self.announced = true;
        }
        let region = Avc420Region {
            left: 0,
            top: 0,
            right: (width - 1) as u16,
            bottom: (height - 1) as u16,
            quantization_parameter: REGION_QP,
            quality: REGION_QUALITY,
        };
        let regions = [region];
        let timestamp = self.started.elapsed().as_millis() as u32;
        let mut server = lock(handle);
        let queued = if avc444 {
            // Luma in the first stream; chroma, when the encoder made it, in the second.
            let (encoding, chroma_regions) = if encoded.auxiliary.is_some() {
                (Encoding::LUMA_AND_CHROMA, Some(&regions[..]))
            } else {
                (Encoding::LUMA, None)
            };
            server.send_avc444v2_frame(
                id,
                encoding,
                &encoded.data,
                &regions,
                encoded.auxiliary.as_deref(),
                chroma_regions,
                timestamp,
            )
        } else {
            server.send_avc420_frame(id, &encoded.data, &regions, timestamp)
        };
        if queued.is_none() {
            // The client never sees this frame, so the next one must not depend on it.
            if let Some(encoder) = self.encoder.as_mut() {
                encoder.request_key_frame();
            }
            return GfxOutcome::Skipped;
        }
        self.flush(&mut server);
        drop(server);
        if let Some(frame_id) = queued {
            self.link.record(|dump| dump.frame(frame_id, &encoded));
        }
        self.adapt(false);
        GfxOutcome::Sent(encoded.bytes())
    }

    /// Feeds the rate controller and applies a bitrate change it asks for.
    fn adapt(&mut self, skipped: bool) {
        let (Some(rate), Some(encoder)) = (self.rate.as_mut(), self.encoder.as_mut()) else {
            return;
        };
        if let Some(bitrate) = rate.record(skipped, Instant::now()) {
            match encoder.set_bitrate(bitrate) {
                Ok(()) => info!(kbit_per_s = bitrate / 1000, "H.264 bitrate adjusted"),
                Err(e) => warn!(%e, "changing the H.264 bitrate failed"),
            }
        }
    }

    /// Hands everything the pipeline queued to IronRDP's event loop for sending.
    fn flush(&self, server: &mut GraphicsPipelineServer) {
        let Some(channel) = server.channel_id() else {
            return;
        };
        let messages = server.drain_output();
        if messages.is_empty() {
            return;
        }
        match encode_dvc_messages(channel, messages, ChannelFlags::SHOW_PROTOCOL) {
            Ok(messages) => {
                if let Some(tx) = lock(&self.link.sender).as_ref() {
                    let _ = tx.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }));
                }
            }
            Err(e) => warn!(%e, "encoding graphics pipeline messages failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_changes_count_as_motion_typing_does_not() {
        let t0 = Instant::now();
        let mut typing = Motion::default();
        // Eight keystrokes a second for five seconds.
        assert!(!(0..40).any(|i| typing.changed(t0 + Duration::from_millis(125 * i))));

        let mut video = Motion::default();
        let moving: Vec<bool> = (0..60).map(|i| video.changed(t0 + Duration::from_millis(33 * i))).collect();
        assert!(!moving[..22].iter().any(|&m| m), "not before three quarters of a second");
        assert!(moving[22..].iter().all(|&m| m), "then for as long as it lasts");

        let last = t0 + Duration::from_millis(33 * 59);
        assert!(video.still_for(last + Duration::from_millis(500)) < STILL_AFTER);
        assert!(video.still_for(last + STILL_AFTER) >= STILL_AFTER);
        assert_eq!(Motion::default().still_for(t0), Duration::MAX);
    }
}
