//! Sound (MS-RDPEA): what the Mac plays, streamed to the client as 16-bit PCM.
//!
//! IronRDP asks [`SoundFactory`] for a handler on every connection. Once the client has picked a
//! format, the handler starts a thread that captures the Mac's sound at that format's rate and
//! posts it as waves on IronRDP's event channel, until the channel closes.
//!
//! The client plays whatever it gets, however late: mstsc queues sound without limit and never
//! catches up, and it confirms a wave when it receives it, not when it plays it (MS-RDPEA
//! 3.2.5.2.1.6), so nothing tells how far behind it plays. The stream therefore never sends more
//! sound than real time allows, drops sound that is already stale, and sends none while the client
//! asks for no output, as mstsc does while minimised. The delays it can measure are logged while
//! sound plays. While the stream runs the Mac's own output can be muted, so that the sound plays
//! on the client only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError};
use ironrdp_server::{RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

/// Rates offered, 44.1 kHz first: fed 48 kHz PCM, mstsc plays slower than real time and falls
/// behind for good (docs/audio.md); libscreenio resamples its 48 kHz capture to 44.1 kHz.
const RATES: [u32; 2] = [44_100, 48_000];
const CHANNELS: u16 = 2;
/// How long a read waits before checking whether to stop.
const POLL: Duration = Duration::from_millis(100);
/// Before capture is tried again after it failed to open.
const RETRY: Duration = Duration::from_secs(2);
/// The test tone: a wave of this length at this pitch.
const TONE_CHUNK: Duration = Duration::from_millis(20);
const TONE_HZ: f64 = 440.0;
/// How far ahead of real time sound may be sent: more would sit in the client's queue for good.
const LEAD: Duration = Duration::from_millis(200);
/// Sound that played on the Mac longer ago than this is not sent: it would queue behind
/// everything the client already has and play late for good.
const STALE: Duration = LEAD;
/// When real time gets this far ahead of the sound sent, with nothing playing or the thread
/// stalled, the ledger starts over: what comes next must not be sent as if it filled the gap.
const GAP: Duration = Duration::from_millis(300);
/// The client's wish for no output counts once it has lasted this long: mstsc also asks so for
/// moments under load, and while it connects.
const SUPPRESS_AFTER: Duration = Duration::from_secs(1);
/// How often the delays are logged while sound plays.
const DELAY_REPORT: Duration = Duration::from_secs(10);
/// How often the muted output is checked for having been replaced, by headphones for example.
const OUTPUT_CHECK: Duration = Duration::from_secs(2);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the sound comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundSource {
    /// What the Mac plays.
    Mac,
    /// A steady tone, for tests without the screen recording permission.
    Tone,
}

pub struct SoundFactory {
    source: SoundSource,
    /// Whether the Mac's own output is muted while a client plays its sound.
    mute_mac: bool,
    /// The rate offered first; the other of [`RATES`] follows.
    rate: u32,
    /// Set while the client asks for no output: IronRDP's flag for that.
    suppressed: Option<Arc<AtomicBool>>,
    sender: Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>,
}

impl SoundFactory {
    pub fn new(source: SoundSource, mute_mac: bool, rate: u32) -> Self {
        Self {
            source,
            mute_mac,
            rate,
            suppressed: None,
            sender: Arc::default(),
        }
    }

    /// Sends no sound while the client asks for no output, going by the flag IronRDP keeps for
    /// that (`with_display_suppressed_handle`).
    pub fn with_suppression(mut self, flag: Arc<AtomicBool>) -> Self {
        self.suppressed = Some(flag);
        self
    }
}

/// [`RATES`] with `preferred` first.
fn rates(preferred: u32) -> impl Iterator<Item = u32> {
    let first = RATES.iter().copied().find(|&rate| rate == preferred);
    first.into_iter().chain(RATES.into_iter().filter(move |&rate| Some(rate) != first))
}

impl ServerEventSender for SoundFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *lock(&self.sender) = Some(sender);
    }
}

impl SoundServerFactory for SoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(Handler {
            source: self.source,
            mute_mac: self.mute_mac,
            sender: self.sender.clone(),
            suppressed: self.suppressed.clone(),
            formats: rates(self.rate).map(|rate| pcm(rate, CHANNELS)).collect(),
            streaming: None,
            confirms: Arc::default(),
        })
    }
}

/// 16-bit PCM at `rate` with `channels`.
fn pcm(rate: u32, channels: u16) -> AudioFormat {
    let block_align = channels * 2;
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: channels,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(block_align),
        n_block_align: block_align,
        bits_per_sample: 16,
        data: None,
    }
}

struct Streaming {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// What the client's confirmations say since the last report, gathered for the sound thread: how
/// long after being sent each wave was confirmed, and how long the client says it held it. mstsc
/// confirms on receipt, so these measure the network, not the playing.
#[derive(Default)]
struct Confirms {
    since_sent: Vec<Duration>,
    held: Vec<Duration>,
}

struct Handler {
    source: SoundSource,
    mute_mac: bool,
    sender: Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>,
    suppressed: Option<Arc<AtomicBool>>,
    formats: Vec<AudioFormat>,
    streaming: Option<Streaming>,
    confirms: Arc<Mutex<Confirms>>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler")
            .field("source", &self.source)
            .field("streaming", &self.streaming.is_some())
            .finish()
    }
}

impl RdpsndServerHandler for Handler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(&mut self, common: &'a [NegotiatedFormat]) -> Option<&'a NegotiatedFormat> {
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        self.stop();
        let Some(sender) = lock(&self.sender).clone() else {
            return Err(Box::new(std::io::Error::other("no event channel for sound")));
        };
        let (rate, channels) = (format.format().n_samples_per_sec, u32::from(format.format().n_channels));
        info!(rate, channels, source = ?self.source, "streaming sound");
        *lock(&self.confirms) = Confirms::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, source, mute_mac) = (stop.clone(), self.source, self.mute_mac);
            let pacing = Pacing::new(self.confirms.clone(), rate, channels, Instant::now());
            let suppression = Suppression::new(self.suppressed.clone());
            thread::Builder::new()
                .name("sound".into())
                .spawn(move || match source {
                    SoundSource::Mac => stream_mac(rate, channels, &sender, &stop, pacing, suppression, mute_mac),
                    SoundSource::Tone => stream_tone(rate, channels, &sender, &stop, pacing, suppression),
                })
                .map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?
        };
        self.streaming = Some(Streaming { stop, thread });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(streaming) = self.streaming.take() {
            streaming.stop.store(true, Ordering::Relaxed);
            if streaming.thread.join().is_err() {
                warn!("the sound thread panicked");
            }
            debug!("sound stopped");
        }
    }

    fn on_wave_confirm(&mut self, since_sent: Duration, held: Duration) {
        let mut confirms = lock(&self.confirms);
        confirms.since_sent.push(since_sent);
        confirms.held.push(held);
    }
}

/// Decides which chunks to send so that the client never gets more sound than real time allows,
/// and logs the delays now and then.
struct Pacing {
    confirms: Arc<Mutex<Confirms>>,
    /// Samples, all channels together, per second of sound.
    samples_per_second: f64,
    /// When the ledger started: the sound sent since then is compared with the time passed.
    started: Instant,
    /// Sound sent since `started`.
    sent: Duration,
    /// Since the last report: sound skipped, and how long after playing on the Mac each chunk
    /// was read.
    skipped: Duration,
    ages: Vec<Duration>,
    reported_at: Instant,
}

impl Pacing {
    fn new(confirms: Arc<Mutex<Confirms>>, rate: u32, channels: u32, now: Instant) -> Self {
        Self {
            confirms,
            samples_per_second: f64::from(rate * channels.max(1)),
            started: now,
            sent: Duration::ZERO,
            skipped: Duration::ZERO,
            ages: Vec::new(),
            reported_at: now,
        }
    }

    /// Whether to send a chunk of `samples` that played on the Mac `age` ago.
    fn send(&mut self, samples: usize, age: Duration, now: Instant) -> bool {
        let length = Duration::from_secs_f64(samples as f64 / self.samples_per_second);
        self.ages.push(age);
        let elapsed = now.duration_since(self.started);
        if elapsed > self.sent + GAP {
            self.sent = elapsed;
        }
        let send = age <= STALE && self.sent + length <= elapsed + LEAD;
        if send {
            self.sent += length;
        } else {
            self.skipped += length;
        }
        if now.duration_since(self.reported_at) >= DELAY_REPORT {
            self.report(elapsed);
            self.reported_at = now;
        }
        send
    }

    /// Logs the delays since the last report and starts the next one.
    fn report(&mut self, elapsed: Duration) {
        let mut confirms = lock(&self.confirms);
        let ms = |d: Duration| d.as_millis() as u64;
        let median = |values: &mut Vec<Duration>| {
            values.sort_unstable();
            values.get(values.len() / 2).copied().unwrap_or_default()
        };
        let confirmed_max = confirms.since_sent.iter().max().copied().unwrap_or_default();
        let captured_max = self.ages.iter().max().copied().unwrap_or_default();
        // lead: how far ahead of real time the sound sent is; captured: from playing on the Mac to
        // being read; confirmed: from sending to the client's confirmation, which mstsc gives on
        // receipt; held: how long the client says it kept each wave before confirming.
        info!(
            lead_ms = ms(self.sent.saturating_sub(elapsed)),
            captured_ms = ms(median(&mut self.ages)),
            captured_max_ms = ms(captured_max),
            confirmed_ms = ms(median(&mut confirms.since_sent)),
            confirmed_max_ms = ms(confirmed_max),
            held_ms = ms(median(&mut confirms.held)),
            skipped_ms = ms(self.skipped),
            waves = confirms.since_sent.len(),
            "sound delay"
        );
        confirms.since_sent.clear();
        confirms.held.clear();
        self.ages.clear();
        self.skipped = Duration::ZERO;
    }
}

/// The client's wish for no output, which mstsc sends while minimised: sound sent meanwhile
/// queues up on the client and plays late once it is restored.
struct Suppression {
    flag: Option<Arc<AtomicBool>>,
    since: Option<Instant>,
    withheld: bool,
}

impl Suppression {
    fn new(flag: Option<Arc<AtomicBool>>) -> Self {
        Self {
            flag,
            since: None,
            withheld: false,
        }
    }

    /// Whether to withhold the sound now.
    fn withhold(&mut self, now: Instant) -> bool {
        let none_wanted = self.flag.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed));
        if !none_wanted {
            self.since = None;
            if self.withheld {
                self.withheld = false;
                info!("the client wants output again; the sound resumes");
            }
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if !self.withheld && now.duration_since(since) >= SUPPRESS_AFTER {
            self.withheld = true;
            info!("the client asked for no output, as mstsc does while minimised; withholding the sound");
        }
        self.withheld
    }
}

/// Sends one wave; false once the connection's event channel is gone.
fn send_wave(sender: &UnboundedSender<ServerEvent>, samples: &[i16], timestamp: Duration) -> bool {
    let data = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    // Waves carry milliseconds in 32 bits, which wrap after 49 days.
    let millis = timestamp.as_millis() as u32;
    sender
        .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, millis)))
        .is_ok()
}

/// Mutes the Mac's output for as long as it lives, following the default output device.
struct Muted {
    mute: Option<screenio_core::OutputMute>,
    checked_at: Instant,
}

impl Muted {
    fn engage(wanted: bool) -> Self {
        let mute = wanted
            .then(|| match screenio_core::OutputMute::engage() {
                Ok(mute) => {
                    info!("the Mac's output is muted while the client plays its sound");
                    Some(mute)
                }
                Err(e) => {
                    warn!(%e, "muting the Mac's output failed; it keeps playing too");
                    None
                }
            })
            .flatten();
        Self {
            mute,
            checked_at: Instant::now(),
        }
    }

    fn follow(&mut self, now: Instant) {
        let Some(mute) = self.mute.as_mut() else {
            return;
        };
        if now.duration_since(self.checked_at) < OUTPUT_CHECK {
            return;
        }
        self.checked_at = now;
        if let Err(e) = mute.follow() {
            debug!(%e, "muting the new default output failed");
        }
    }
}

impl Drop for Muted {
    fn drop(&mut self) {
        if self.mute.take().is_some() {
            info!("the Mac's output has its own mute setting back");
        }
    }
}

fn stream_mac(
    rate: u32,
    channels: u32,
    sender: &UnboundedSender<ServerEvent>,
    stop: &AtomicBool,
    mut pacing: Pacing,
    mut suppression: Suppression,
    mute_mac: bool,
) {
    let (mut failures, mut heard) = (0u32, false);
    let mut muted = Muted::engage(mute_mac);
    while !stop.load(Ordering::Relaxed) {
        let mut capture = match screenio_core::AudioCapture::open(rate, channels) {
            Ok(capture) => capture,
            Err(screenio_core::Error::Permission) => {
                warn!("the Mac's sound needs the screen recording permission; the client hears nothing");
                return;
            }
            Err(e) => {
                if failures == 0 {
                    warn!(%e, "capturing the Mac's sound failed; trying again every few seconds");
                }
                failures += 1;
                sleep_unless_stopped(RETRY, stop);
                continue;
            }
        };
        let mut rate_checked = false;
        while !stop.load(Ordering::Relaxed) {
            muted.follow(Instant::now());
            if let (false, Some(source)) = (rate_checked, capture.source_rate()) {
                rate_checked = true;
                if (source - f64::from(rate)).abs() > 1.0 {
                    warn!(source, rate, "the Mac's sound comes at another rate than the client plays it at");
                }
            }
            match capture.read(POLL) {
                Ok(chunk) => {
                    let now = Instant::now();
                    if !heard {
                        heard = true;
                        let frames = chunk.samples.len() / channels.max(1) as usize;
                        info!(frames, "the Mac's sound is captured and sent");
                    }
                    if suppression.withhold(now) {
                        continue;
                    }
                    // dwAudioTimeStamp counts from start-up (MS-RDPEA 2.2.3.10), as Windows sends it.
                    if pacing.send(chunk.samples.len(), chunk.age, now) && !send_wave(sender, chunk.samples, chunk.played) {
                        return;
                    }
                }
                Err(screenio_core::Error::Timeout) => {}
                Err(e) => {
                    warn!(%e, "sound capture stopped; reopening it");
                    break;
                }
            }
        }
    }
}

fn stream_tone(
    rate: u32,
    channels: u32,
    sender: &UnboundedSender<ServerEvent>,
    stop: &AtomicBool,
    mut pacing: Pacing,
    mut suppression: Suppression,
) {
    let started = Instant::now();
    let frames = (f64::from(rate) * TONE_CHUNK.as_secs_f64()) as usize;
    let mut position = 0usize;
    let mut samples = Vec::with_capacity(frames * channels as usize);
    while !stop.load(Ordering::Relaxed) {
        let timestamp = started.elapsed();
        samples.clear();
        tone(rate, channels, position, frames, &mut samples);
        position += frames;
        let now = Instant::now();
        if !suppression.withhold(now)
            && pacing.send(samples.len(), Duration::ZERO, now)
            && !send_wave(sender, &samples, timestamp)
        {
            return;
        }
        // Real time, so the client's buffer neither drains nor overflows.
        let due = TONE_CHUNK.mul_f64(position as f64 / frames as f64);
        sleep_unless_stopped(due.saturating_sub(started.elapsed()), stop);
    }
}

/// `frames` frames of a quiet sine from frame `position` on, the same in every channel.
fn tone(rate: u32, channels: u32, position: usize, frames: usize, out: &mut Vec<i16>) {
    for frame in position..position + frames {
        let phase = frame as f64 * TONE_HZ / f64::from(rate) * std::f64::consts::TAU;
        let sample = (phase.sin() * f64::from(i16::MAX) * 0.25) as i16;
        out.extend(std::iter::repeat_n(sample, channels as usize));
    }
}

fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(POLL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 ms of two channels at 48 kHz.
    const CHUNK_SAMPLES: usize = 1920;
    const CHUNK: Duration = Duration::from_millis(20);

    fn pacing(now: Instant) -> Pacing {
        Pacing::new(Arc::default(), 48_000, 2, now)
    }

    #[test]
    fn the_preferred_rate_comes_first() {
        assert_eq!(rates(48_000).collect::<Vec<_>>(), [48_000, 44_100]);
        assert_eq!(rates(44_100).collect::<Vec<_>>(), [44_100, 48_000]);
        assert_eq!(rates(22_050).collect::<Vec<_>>(), [44_100, 48_000]);
    }

    #[test]
    fn formats_are_16_bit_stereo_pcm_44_1_khz_first() {
        let formats: Vec<_> = RATES.iter().map(|&rate| pcm(rate, CHANNELS)).collect();
        assert_eq!(formats[0].n_samples_per_sec, 44_100);
        assert_eq!(formats[0].n_avg_bytes_per_sec, 176_400);
        assert_eq!(formats[0].n_block_align, 4);
        assert!(formats.iter().all(|f| f.format == WaveFormat::PCM && f.bits_per_sample == 16));
    }

    #[test]
    fn the_tone_repeats_each_sample_per_channel_and_continues_across_chunks() {
        let mut whole = Vec::new();
        tone(48_000, 2, 0, 20, &mut whole);
        let mut parts = Vec::new();
        tone(48_000, 2, 0, 12, &mut parts);
        tone(48_000, 2, 12, 8, &mut parts);
        assert_eq!(whole.len(), 40);
        assert_eq!(parts, whole);
        assert!(whole.chunks(2).all(|frame| frame[0] == frame[1]));
        assert!(whole.iter().any(|&s| s != 0));
    }

    #[test]
    fn waves_are_little_endian_samples_with_millisecond_timestamps() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(send_wave(&tx, &[1, -2], Duration::from_micros(2_500)));
        match rx.try_recv() {
            Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) => {
                assert_eq!(data, [1, 0, 0xFE, 0xFF]);
                assert_eq!(timestamp, 2);
            }
            other => panic!("expected a wave, got {other:?}"),
        }
        drop(rx);
        assert!(!send_wave(&tx, &[0], Duration::ZERO), "a closed channel stops the stream");
    }

    #[test]
    fn sound_at_real_time_is_all_sent() {
        let t0 = Instant::now();
        let mut pacing = pacing(t0);
        // Read a little after it played, as from the capture.
        assert!((0..500).all(|i| pacing.send(CHUNK_SAMPLES, Duration::from_millis(30), t0 + CHUNK * i)));
        assert_eq!(pacing.skipped, Duration::ZERO);
    }

    #[test]
    fn sound_faster_than_real_time_is_capped_at_the_lead() {
        let t0 = Instant::now();
        let mut pacing = pacing(t0);
        // 20 ms chunks every 18 ms: sound said to be at 48 kHz that really comes at 53 kHz.
        let sent = (0..1000)
            .filter(|&i| pacing.send(CHUNK_SAMPLES, Duration::ZERO, t0 + Duration::from_millis(18 * i)))
            .count();
        let elapsed = Duration::from_millis(18 * 999);
        assert!(CHUNK * sent as u32 <= elapsed + LEAD, "{sent} chunks sent in {elapsed:?}");
        assert!((900..=910).contains(&sent), "{sent} chunks: only the excess is skipped");
    }

    #[test]
    fn a_backlog_keeps_only_its_fresh_end() {
        let t0 = Instant::now();
        let mut pacing = pacing(t0);
        assert!(pacing.send(CHUNK_SAMPLES, Duration::ZERO, t0));
        // The thread stalled for a second; the queue held 50 chunks, read all at once, oldest first.
        let now = t0 + Duration::from_secs(1);
        let sent: Vec<bool> = (0..50)
            .map(|i| pacing.send(CHUNK_SAMPLES, Duration::from_millis(1000 - 20 * i), now))
            .collect();
        let fresh = sent.iter().filter(|&&s| s).count();
        assert!(sent.iter().rev().take(fresh).all(|&s| s), "the freshest chunks are the ones sent");
        assert!((9..=11).contains(&fresh), "{fresh} chunks, about the lead allowed");
    }

    #[test]
    fn a_gap_earns_no_credit() {
        let t0 = Instant::now();
        let mut pacing = pacing(t0);
        assert!((0..50).all(|i| pacing.send(CHUNK_SAMPLES, Duration::ZERO, t0 + CHUNK * i)));
        // Silence for five seconds, then 30 fresh chunks at once.
        let now = t0 + Duration::from_secs(6);
        let sent = (0..30).filter(|_| pacing.send(CHUNK_SAMPLES, Duration::ZERO, now)).count();
        assert!((9..=11).contains(&sent), "{sent} chunks: the lead, not the whole gap");
    }

    #[test]
    fn the_delays_are_reported_and_the_window_starts_again() {
        let confirms = Arc::new(Mutex::new(Confirms::default()));
        let t0 = Instant::now();
        let mut pacing = Pacing::new(confirms.clone(), 48_000, 2, t0);
        {
            let mut c = lock(&confirms);
            c.since_sent = vec![Duration::from_millis(2), Duration::from_millis(1), Duration::from_millis(3)];
            c.held = vec![Duration::ZERO; 3];
        }
        pacing.send(CHUNK_SAMPLES, Duration::ZERO, t0 + Duration::from_secs(1));
        assert_eq!(lock(&confirms).since_sent.len(), 3, "not yet time to report");
        pacing.send(CHUNK_SAMPLES, Duration::ZERO, t0 + DELAY_REPORT);
        assert!(lock(&confirms).since_sent.is_empty() && lock(&confirms).held.is_empty());
        assert!(pacing.ages.is_empty() && pacing.skipped.is_zero());
    }

    #[test]
    fn no_output_is_withheld_once_the_wish_has_lasted_a_second() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut suppression = Suppression::new(Some(flag.clone()));
        let t0 = Instant::now();
        assert!(!suppression.withhold(t0));
        flag.store(true, Ordering::Relaxed);
        assert!(!suppression.withhold(t0 + Duration::from_millis(100)), "a moment's wish, as under load");
        assert!(suppression.withhold(t0 + Duration::from_millis(1100)));
        assert!(suppression.withhold(t0 + Duration::from_secs(60)));
        flag.store(false, Ordering::Relaxed);
        assert!(!suppression.withhold(t0 + Duration::from_secs(61)));
        // Asking again starts the second over.
        flag.store(true, Ordering::Relaxed);
        assert!(!suppression.withhold(t0 + Duration::from_secs(62)));
        assert!(!Suppression::new(None).withhold(t0), "no flag, never withheld");
    }
}
