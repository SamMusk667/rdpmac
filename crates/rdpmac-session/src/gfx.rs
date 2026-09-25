//! The graphics pipeline (MS-RDPEGFX) path: H.264 frames for clients that negotiate AVC420, in
//! full colour as AVC444v2 where the client and the session size allow it.
//!
//! IronRDP asks [`GfxFactory`] for a graphics pipeline server on every connection; the factory
//! keeps a handle to it in a [`GfxLink`] shared with the frame thread. The frame thread's
//! [`GfxStream`] creates the surface, encodes with VideoToolbox and submits frames through that
//! handle, then posts the resulting channel messages on IronRDP's event channel.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent, ServerEventSender};
use ironrdp_svc::ChannelFlags;
use rdpmac_encode::avc444;
use rdpmac_encode::h264::{EncodeError, EncodedFrame, H264Encoder};
use rdpmac_encode::rate::RateControl;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

/// Largest frame sent as H.264. Windows' Media Foundation H.264 decoder, which mstsc uses, stops
/// at 4096x2304; larger sessions stay on RemoteFX, which handles them well.
const MAX_H264_SIDE: u32 = 4096;
const MAX_H264_PIXELS: u32 = 4096 * 2304;

/// Quantisation and quality hints carried with every region; informational for decoders.
const REGION_QP: u8 = 22;
const REGION_QUALITY: u8 = 100;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the factory and the frame thread share.
#[derive(Default)]
pub struct GfxLink {
    handle: Mutex<Option<GfxServerHandle>>,
    sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// Whether AVC444 may be used; AVC420 only otherwise.
    avc444: bool,
}

impl GfxLink {
    pub fn new(avc444: bool) -> Arc<Self> {
        Arc::new(Self {
            avc444,
            ..Self::default()
        })
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
        Box::new(Handler)
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let handle: GfxServerHandle = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(Handler))));
        *lock(&self.link.handle) = Some(handle.clone());
        Some((GfxDvcBridge::new(handle.clone()), handle))
    }
}

struct Handler;

impl GraphicsPipelineHandler for Handler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        debug!(?pdu, "client graphics capabilities");
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        info!(?negotiated, "graphics pipeline ready");
    }

    fn on_close(&mut self) {
        info!("graphics pipeline closed");
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

/// One frame thread's use of the pipeline. Created per update stream, so a session resize or a
/// new connection starts with a fresh surface and a key frame.
pub struct GfxStream {
    link: Arc<GfxLink>,
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
}

impl GfxStream {
    pub fn new(link: Arc<GfxLink>, fps: u32) -> Self {
        Self {
            link,
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
            self.link.avc444 && !self.avc444_unavailable && server.supports_avc444() && avc444::fits(width, height)
        };

        if self.encoder.as_ref().map(|e| (e.size(), e.avc444())) != Some(((width, height), avc444)) {
            match self.open_encoder(width, height, avc444) {
                Ok(encoder) => {
                    let quantiser = encoder.controls_quantiser();
                    let avc444 = encoder.avc444();
                    info!(width, height, quantiser, avc444, "H.264 encoder ready");
                    self.rate = Some(RateControl::new(encoder.target_bitrate(), Instant::now()));
                    self.encoder = Some(encoder);
                }
                Err(e) => {
                    warn!(%e, width, height, "H.264 encoder unavailable, staying on bitmap updates");
                    self.disabled = true;
                    return GfxOutcome::Unavailable;
                }
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
        let server_id = Arc::as_ptr(&handle) as usize;
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

    /// An AVC444 encoder when asked for and possible, an AVC420 one otherwise.
    fn open_encoder(&mut self, width: u32, height: u32, avc444: bool) -> Result<H264Encoder, EncodeError> {
        if avc444 {
            match H264Encoder::new_avc444(width, height, self.fps) {
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
        let avc444 = self.encoder.as_ref().is_some_and(H264Encoder::avc444);
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
            let auxiliary = encoded.auxiliary.as_deref().map(|view| (view, &regions[..]));
            server.send_avc444v2_frame(id, Some((&encoded.data, &regions)), auxiliary, timestamp)
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
