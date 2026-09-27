# Sound (M4 item 3)

The sound the Mac plays goes to the client (MS-RDPEA, static channel RDPSND). Implemented on
2026-09-25 and verified with mstsc: the sound could be heard. The client then turned out to fall
ever further behind (1–2 s, 5–6 s, 12 s); on 2026-09-26 the cause was found, and the sample rate,
the pace of sending and the handling of a minimised client were changed. See "Delay".

## Capture

libscreenio gained `AudioCapture` (C interface `sio_audio_*`, C ABI 1.2), which captures through
ScreenCaptureKit. It needs macOS 13 and the Screen Recording permission, which rdpmacd already
has.

- It is a capture stream of its own that takes only sound, separate from the screen capture: the
  screen capture reopens when the size or the display changes, and the sound does not break off
  with it. A ScreenCaptureKit capture stream must carry a picture, so this one asks for a 2x2
  picture once a second and drops it.
- What it captures is the sound of every app on the Mac, except rdpmacd's own.
- ScreenCaptureKit delivers 32-bit float samples, usually one buffer per channel; the library
  interleaves them as 16-bit samples and queues them for the caller to read. Sound left unread
  beyond about a second is dropped, oldest first.
- ScreenCaptureKit supports only 8000, 16000, 24000 and 48000 Hz (Apple's documentation: asked for
  any other rate, it silently captures at 48000). So libscreenio makes 44100 Hz by resampling a
  48000 Hz capture with AudioToolbox's AudioConverter: 960 frames in and about 882 frames out per
  block. The converter keeps a few frames back for its filter and makes them up in the next
  block.
- While nothing plays, the capture stream produces no data, reads time out, and the client
  receives nothing.
- The library records the actual sample rate of the sound (`source_rate`; for resampled sound,
  scaled by the resampling ratio). When it differs from the sample rate the client plays at, the
  log warns `the Mac's sound comes at another rate than the client plays it at`: the client would
  then play too fast or too slow. But this value comes from ScreenCaptureKit's format
  description; the reliable check is to count frames. A log line every 10 seconds with
  `waves=500`, at 882 frames a block, means 44100 frames a second.

## Transport

IronRDP 0.13 comes with an RDPSND server: on every connection it asks `SoundFactory` for a
handler, the handler lists its formats, and once the client has answered with the formats it
supports, the handler picks one and starts sending.

- rdpmac offers 16-bit stereo PCM at 44100 Hz and 48000 Hz and picks the first, in that order,
  that the client also supports (mstsc supports both, so it gets 44100). For why 44100 comes
  first, see "Delay".
- Once a format is picked, a thread captures at its sample rate and sends each block, about
  20 ms, as a Wave2. When the connection closes the thread stops, and the capture stream closes
  with it.
- Without the Screen Recording permission, the log warns once and the connection has no sound;
  when the capture stream fails to open for any other reason, it is tried again every two
  seconds. The first time sound is captured, the log says `the Mac's sound is captured and sent`,
  with the frames per block.
- IronRDP keeps at most 4 blocks of sound per round of event dispatch, dropping the older ones
  when there is a backlog. No delay of seconds can build up on the server side.
- PCM at 44.1 kHz stereo takes about 1.4 Mbit/s. Compressed formats such as AAC are left for
  later.

## Delay: why the client fell ever further behind, and how that is prevented (2026-09-26)

mstsc played 1–2 seconds behind the Mac, later once 5–6 seconds, and another time 12 seconds,
going by YouTube's timestamps; the delay grew over time. The log showed the server sending
strictly in real time (500 blocks every 10 seconds, 960 frames each, i.e. 48000 frames a second)
and each block confirmed about 1 ms after it was sent; the queues in the server and in
libscreenio are both bounded and cannot add up to 12 seconds. The backlog was inside mstsc.

The cause has three layers; the account below follows the findings of
[macrdp](https://github.com/clintcan/macrdp) (another macOS RDP server, which ran into the same
problem):

1. **mstsc plays 48 kHz PCM slower than real time.** macrdp found that a 48 kHz feed over-fed
   mstsc by about 20% and built up backlogs of several seconds, and that announcing 44.1 kHz and
   resampling on the server itself fixed it; it has kept that fix since. Its documentation puts the
   cause down to pacing or rate accounting without settling the mechanism.
   The three observations above (1–2, 5–6, 12 seconds) fit a backlog that grows by 8.8% of each
   second played (48000/44100 − 1). So 44100 Hz now comes first by default.
2. **mstsc's playback queue has no limit, and mstsc never catches up** (an unbounded waveOut
   queue, played strictly in order; someone has patched mstsc.exe for this). So any sound sent
   ahead of real time, or sent to make up for a stall, becomes a permanent delay. Moreover, the
   specification (MS-RDPEA 3.2.5.2.1.6) has the client send a Wave Confirm once it has
   "consumed" a block, and consuming covers processing, cancelling and dropping it: by design,
   confirmations do not reflect how far playback has got. Measured on 2026-09-26, too, each block
   was confirmed about 1 ms after it was sent, with `held_ms` at 0. The earlier logic, which
   caught up by the confirmation delay, could therefore never trigger, and has been replaced.
3. **mstsc does not consume sound while minimised**; if the server keeps sending, everything
   that piles up plays late once the window is restored. Switching focus sends no PDU at all;
   only minimising sends Suppress Output.

The current approach (`sound.rs`):

- **Ledger.** Each stream records when it started and how much sound it has sent, and checks
  before sending each block: when sent + this block > time passed + 200 ms, the block is
  skipped, so the client gets at most 200 ms ahead of real time. When the time passed exceeds the
  sound sent by more than 300 ms (nothing playing, or the thread stalled), the ledger starts over
  from now, so that sound arriving afterwards is not sent as if it filled the gap.
- **Stale sound is not sent.** libscreenio gives each block the time it played on the Mac and
  how long ago that was when it was read (`age`); a block older than 200 ms is not sent, since it
  would only queue behind the sound the client already has and play late for good. Of a second
  of sound that piled up in the queue while the thread stalled, only the newest 200 ms or so is
  sent.
- **Nothing is sent while the client asks for no output.** IronRDP sets a flag when it receives
  Suppress Output (the picture side already uses it to pause capture); once the flag has stayed
  set for 1 second, no more sound is sent (mstsc also sends Suppress Output for moments under
  high load, and while connecting). When output resumes, the ledger starts over after the gap,
  so nothing is sent all at once to make up. Log:
  `the client asked for no output, as mstsc does while minimised; withholding the sound` /
  `the client wants output again; the sound resumes`.
- **Timestamps.** Each block's `wTimeStamp` holds the time its PDU was built (IronRDP used to put
  0 in every one), and `dwAudioTimeStamp` the milliseconds since the Mac started up, as Windows
  servers do. RDP has no audio-video synchronisation (RDPSND and EGFX are independent of each
  other); do not count on the timestamps to fix delay.
- **Report.** While there is sound, a line every 10 seconds, `sound delay lead_ms=… captured_ms=…
  captured_max_ms=… confirmed_ms=… confirmed_max_ms=… held_ms=… skipped_ms=… waves=…`:
  `lead_ms` is how far the sound sent is ahead of real time, `captured_ms` the time from playing
  on the Mac to being read, `confirmed_ms` the time from sending to the client's confirmation
  (mstsc confirms on receipt, so this is the network round trip), and `skipped_ms` the sound
  skipped in these 10 seconds.

`audio-rate = 44100 | 48000` (`--audio-rate`) changes which sample rate comes first; the default
is 44100, and 48000 is kept for comparison. The app has no setting for it; saving the settings
keeps it.

## Muting the Mac while the client plays

`mute-mac = true | false` (`--mute-mac false`), on by default; in the app's settings it is
"Mute the Mac meanwhile".

- When sound starts going to the client, the Mac's current default output device is muted
  (libscreenio `OutputMute`, Core Audio's device mute property), and the log says
  `the Mac's output is muted while the client plays its sound`; when the connection ends, the
  device's original setting is restored. A device that was muted already stays muted.
- Every 2 seconds the default output device is checked for having changed (headphones plugged
  in, for example); if it has, the old one's setting is restored and the new one is muted.
- Some HDMI outputs have no mute control; the log then warns once, and the Mac plays its sound
  as usual.
- Only the output device is muted: apps play and ScreenCaptureKit captures as usual (verified on
  2026-09-26: the Mac is silent, and the client hears the sound as usual).
- If rdpmacd crashes while the output is muted, the Mac stays muted and has to be unmuted by
  hand.

## Settings

- `audio = true | false` (`--no-audio`), on by default; in the app's settings it is
  "Play the Mac's sound on the client".
- `mute-mac`: see the previous section; `audio-rate`: see "Delay".

All of them take effect only after the server restarts, because the sound channel is decided at
start-up.

## Tests

- Unit tests: the formats (44100 first), float to 16-bit conversion and interleaving, the test
  tone continuing across blocks, the byte order and millisecond timestamps of the Wave data,
  sending stopping once the connection closes; the ledger: sound arriving in real time is all
  sent, sound 11% faster than real time is sent only up to a 200 ms lead, of 50 blocks backed up
  by a one-second stall only the newest 10 or so are sent, of 30 blocks arriving at once after a
  5-second gap only about 10 are sent, one report every 10 seconds, which clears the figures; a
  request for no output takes effect only once it has lasted 1 second, and sending resumes as
  soon as it is withdrawn. libscreenio: AudioConverter resamples a 1 kHz sine at 48 kHz to
  44.1 kHz, one second in giving one second out (short by a few dozen frames of filter delay) at
  the same pitch, and blocks of any size come out in proportion; a read-only check of whether
  the default output device can be muted.
- Loopback: with `--test-pattern` the sound comes from a 440 Hz tone instead, which needs no
  permission. FreeRDP's `sfreerdp /sound:sys:fake` should then negotiate 16-bit stereo PCM at
  44100 Hz, 882 frames a block (3528 bytes, 20 ms), 500 blocks every 10 seconds.
- Measuring with mstsc: after watching YouTube for a minute, pause it; how long the client keeps
  playing is the backlog. Measuring the pitch of the `--test-pattern` 440 Hz tone on the Windows
  side shows whether mstsc plays 48k data at 44.1k (it would then sound at about 404 Hz).

## Known limitations and next steps

- PCM only; AAC would cut the bandwidth to about 1/10, and compete less with video for the same
  TCP connection.
- The client's volume changes are not passed on.
- The ledger only keeps the server from sending ahead of real time or making up after a stall; a
  client's own slow playing (cause 1) can only be sidestepped by changing the sample rate.
  If mstsc still falls ever further behind at 44.1 kHz, the next things to try are AAC, or a
  manual resync that rebuilds the capture stream, as macrdp does.
- While mstsc reconnects (after a certificate prompt, for example), the sound threads of the old
  and the new connection may briefly run side by side, both sending on the same event channel;
  macrdp keeps only the newest with a generation counter. A `waves` of 1000 rather than 500 in
  the 10-second log line means this has happened; it has not been seen so far.
