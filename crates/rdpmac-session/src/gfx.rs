//! The graphics pipeline (MS-RDPEGFX) path: H.264 frames for clients that negotiate AVC420.
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
use rdpmac_encode::h264::H264Encoder;
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
}

impl GfxLink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
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
    announced: bool,
    too_large_reported: bool,
    started: Instant,
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
            announced: false,
            too_large_reported: false,
            started: Instant::now(),
        }
    }

    /// Offers one BGRA frame of `width` x `height`, the session size.
    pub fn send(&mut self, bgra: &[u8], width: u32, height: u32, stride: usize) -> GfxOutcome {
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
        {
            let server = lock(&handle);
            if !server.is_ready() || !server.supports_avc420() {
                return GfxOutcome::Unavailable;
            }
            if server.should_backpressure() {
                drop(server);
                self.adapt(true);
                return GfxOutcome::Skipped;
            }
        }

        if self.encoder.as_ref().map(H264Encoder::size) != Some((width, height)) {
            match H264Encoder::new(width, height, self.fps) {
                Ok(encoder) => {
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
        let Some(surface) = self.surface.as_ref().map(|s| s.id) else {
            return GfxOutcome::Unavailable;
        };
        let Some(encoder) = self.encoder.as_mut() else {
            return GfxOutcome::Unavailable;
        };
        let encoded = match encoder.encode(bgra, stride) {
            Ok(Some(frame)) => frame,
            Ok(None) => return GfxOutcome::Skipped,
            Err(e) => {
                warn!(%e, "H.264 encoding failed; recreating the encoder on the next frame");
                self.encoder = None;
                return GfxOutcome::Unavailable;
            }
        };
        if !self.announced {
            info!(width, height, "sending H.264 through the graphics pipeline");
            self.announced = true;
        }
        let region = Avc420Region {
            left: 0,
            top: 0,
            right: w16 - 1,
            bottom: h16 - 1,
            quantization_parameter: REGION_QP,
            quality: REGION_QUALITY,
        };
        let timestamp = self.started.elapsed().as_millis() as u32;
        let mut server = lock(&handle);
        if server.send_avc420_frame(surface, &encoded.data, &[region], timestamp).is_none() {
            // The client never sees this frame, so the next one must not depend on it.
            if let Some(encoder) = self.encoder.as_mut() {
                encoder.request_key_frame();
            }
            return GfxOutcome::Skipped;
        }
        self.flush(&mut server);
        drop(server);
        self.adapt(false);
        GfxOutcome::Sent(encoded.data.len())
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
