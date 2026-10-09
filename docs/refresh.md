# Pausing and refreshing the picture, recording the stream

Reported on 2026-09-25: switching to another window while the screen saver ran could leave mstsc
showing a corrupted picture (in the screenshot the whole desktop was tinted purple and green, the
terminal's black background had turned grey-green, and text was reduced to faint outlines). The
assessment concluded that the client decoder's reference frames had fallen out of step with the
encoder: the luma was wrong too, and later frames only laid their differences over a wrong base
picture. Offline, the same encoder was run through a simulated "animation switching to the
desktop", with random backlog, failed sends and key frame requests added, and ffmpeg decoded every
frame correctly. So the problem is on mstsc's side or appears only in the field; the existing logs
cannot confirm it. The damage stayed because rdpmac ignored the client's requests to refresh the
picture, and a still picture got no key frame either.

Based on this, two things were done first: answering the client's requests to pause and to refresh
the picture (A), and optional stream recording (C), so that the next time the problem appears it
can be settled whether the bitstream or mstsc is at fault.

## Pause and resume (Suppress Output)

When minimised, mstsc sends a Suppress Output request for no picture; when restored, it sends
another that allows the picture again, usually followed by a Refresh Rect. IronRDP only flips a
flag (`display_suppressed`) when it receives one, and rdpmac used to ignore that flag entirely.
Now:

- Once the request has stood for 1 second, the picture pauses: screen capture is closed, and
  nothing is encoded or sent. The wait of 1 second is there because some clients flip the flag
  back and forth under load.
- mstsc also asks for no picture once while it connects. So the picture pauses only after this
  stream has sent a picture and the client has since said it wants the picture; the first frame is
  always sent. If the client never withdraws the request, the picture keeps going out as before.
- When the client wants the picture again, capture is reopened (the first frame of the new capture
  stream is the screen as it is now), and that frame is a key frame. The bitrate starts again from
  its target: the client acknowledges no frames while minimised, so the slowdown caused by that
  backlog has nothing to do with the network. Before this, the bitrate was pushed down to
  500 kbit/s while mstsc was in the background, and after it came back the picture took about two
  minutes to become sharp again.
- Log: `the client asked for no picture, as mstsc does while minimised; pausing the picture` and
  `the client wants the picture again; sending all of it`, plus `H.264 bitrate reset` when the
  bitrate changes.

H.264 and RemoteFX both pause this way. So does test pattern mode, except that it has no capture
to close.

## Refresh (Refresh Rect)

An IronRDP patch adds `request_refresh` to `RdpServerDisplay`, called when a Refresh Rect or a
Suppress Output that allows the picture arrives. rdpmac then sends the current picture again as a
key frame: as the next frame if the screen is changing, or at the next idle moment if it is still.
Several requests within one second produce one resend, and the request that comes with a resume is
superseded by the resume's key frame. Log: `the client asked for the whole picture again`.
RemoteFX bitmaps do not depend on the previous frame and need no resending.

A refresh or a resume also resets the client's graphics state: the server sends ResetGraphics,
creates a new surface (log `graphics surface created`) and sends the key frame to the new surface.
This way mstsc's decoder starts over as well; see the next section.

## The two views out of step (2026-09-26)

With recording on, the corrupted picture appeared three more times: on waking from the lock screen,
during a screen saver, and while watching YouTube in ordinary use. In the recorded bitstream every
frame has the main view first and the auxiliary view after it; ffmpeg decodes it with zero errors,
and the combined picture is correct. mstsc also acknowledged every frame, and its count of decoded
frames was not a single frame off. The screenshots matched exactly a simulation that "swaps the two
views and then combines them as usual": the whole desktop squeezed into two half-width pictures
side by side (the arrangement of the auxiliary view's luma plane). So mstsc paired the main and
auxiliary views one frame apart. It decodes both views with one decoder, one after the other; once
it produces a frame late at some moment, it stays out of step from then on, and key frames do not
repair it. AVC420 has only one view, so a frame late does not show. That time it recovered on its
own after the screen went still (the mouse was moved, a video preview stopped). mstsc sends no
pause or refresh request when the focus changes.

The two changes made in response:

- A refresh or a resume resets the graphics state (previous section). Minimising mstsc and
  restoring it when the picture is corrupted should make it rebuild its decoder and recover.
- Each view's bitstream now starts with an access unit delimiter (AUD, NAL type 9, 6 bytes). It is
  optional in H.264, and VideoToolbox does not write one; some decoders rely on it to find where a
  frame starts. This is an attempt: whether it keeps the views from falling out of step remains to
  be verified in real use.

Another finding: while the picture was corrupted that time, the server sent about 50 frames a
second. libscreenio captures at up to 60 fps, and the `fps` setting is only used as the timeout
for waiting for a frame and as the encoder's expected frame rate; it limits nothing. Video content
goes out as fast as the encoder can run, and the client has to decode 100 frames of 2560x1440
H.264 a second. Changed on 2026-09-26: capture now keeps to `fps` (30 by default).

## Stream recording (`h264-dump`)

Off by default. To turn it on, switch on "Record the picture stream" under Debugging in the app's
settings, add the line `h264-dump = true` to `~/Library/Application Support/rdpmac/config.toml`, or
add `--h264-dump` to the command line. Saving the settings needs no restart: recording starts with
the next connection, and turning it off ends the current recording at once. Recordings already made
stay on disk; the settings window shows how much they take and opens their folder in Finder. Turning
the switch off removes the key from the file.

Until 0.5.0 the settings window had no switch for it, so a `h264-dump = true` added by hand stayed
on, through every save, until it was removed from the file by hand.

- One directory per H.264 stream:
  `~/Library/Logs/rdpmac/h264/<UTC start time>-<width>x<height>-<avc444|avc420>/`. The log has
  `recording the H.264 stream dir=…`.
- The bitstream is split into segments at key frames: `0001.h264`, `0002.h264` and so on. Each
  segment starts with a key frame and decodes on its own (`ffmpeg -i 0001.h264 …`). For AVC444
  every frame is the main view followed by the auxiliary view, in the same order as they are sent
  to the client. Once a segment reaches 64 MB, a new one starts at the next key frame.
- Beside each segment is a `.txt` file of the same name, one event per line, with times in UTC
  that match the log:

  ```text
  2026-09-25T22:27:39.005Z frame 0 main 3163 aux 3959 key 1 qp 24      frame sent: frame number, bytes of the two views, key frame or not, quantiser
  2026-09-25T22:27:39.005Z ack 0 queue 0 decoded 1                     client acknowledgement: frame number, queue depth, frames decoded
  2026-09-25T22:27:46.001Z resumed                                     client resumed the picture
  2026-09-25T22:27:48.845Z refresh                                     client asked for a refresh
  ```

  When the client sends quality reports (QoE), there are also `qoe` lines. Pauses are in the log.
- Each stream's directory keeps about 1 GB at most, deleting segments from the oldest beyond that;
  only the newest 3 directories are kept under `h264/`, so recording takes about 3 GB at most. A
  full-screen animation such as a screen saver runs at about 15 Mbit/s, so 1 GB is roughly the last
  9 minutes; ordinary desktop use keeps several hours.

After reproducing a corrupted picture, note the time, disconnect as soon as possible (within a few
minutes), and attach the matching directory to the bug report, saying whether windows were
switched on Windows or on the Mac in the session. The stream is then decoded with ffmpeg and
combined as AVC444: if the recorded stream is itself corrupted, the problem is in the encoder; if
it decodes correctly while mstsc shows a corrupted picture, the problem is in the client, and the
acknowledgement and refresh records are then used to work out what it lost.

## Tests (2026-09-25)

- Unit tests: the picture pauses only after "a picture has been sent, the client has wanted the
  picture, and the request has stood for 1 second", and a brief flip does not count; a refresh
  goes out only once within a second, and a refresh after a resume is superseded; the encoder's
  `resend` sends a still picture again as a key frame with both views; recording splits segments
  at key frames, deletes old segments beyond the limit and keeps only 3 directories; the UTC time
  format.
- Loopback: a throwaway test client built on IronRDP negotiated AVC444, then sent "no picture" at
  3 seconds, "picture allowed" with a Refresh Rect at 7 seconds, and a Refresh Rect alone at 10
  seconds. With AVC444 on the test pattern, sending stopped after about 1 second, the first frame
  after the resume was a key frame, and a key frame followed the lone refresh at once; RemoteFX
  paused and resumed the same way. The recording directory held 274 frames and 273
  acknowledgements, with key frames right after `resumed` and `refresh`, and ffmpeg decoded 548
  frames (two views per frame) without errors.
- Pausing and reopening capture of a real screen needs the Screen Recording permission, which the
  local test process does not have, so it is left to a check with mstsc: minimise mstsc for a
  while, then restore it; the two log lines above should appear, and the picture should be current
  at once.

## Still unknown (2026-09-25)

Both were answered on 2026-09-26; see "The two views out of step".

- Whether mstsc sends these requests when "switching to another window" (as opposed to
  minimising). Reproducing it once with recording on will answer this in the log and the `.txt`
  file.
- The root cause of the corrupted picture. A only lets the picture recover when the client asks
  for a refresh; if mstsc does not ask for one when its picture is corrupted, the damage still
  stays until the next key frame. If so, sending a key frame once the picture goes still (B in the
  assessment) will be considered.
