# Milestones

The status tables record the state when each was written; the dated notes further down record what
was verified or changed later.

## M1 First frame

| Item | Status | Notes |
|---|---|---|
| Server repository skeleton | Done | Four crates. Licensed AGPL-3.0 with a commercial dual licence at the time; MIT OR Apache-2.0 since 2026-09-27 (ADR-0002) |
| IronRDP wired to libscreenio for display and input | Done | Capture and cursor threads feed a bounded channel; an input injection thread; conversion between pixels and points |
| Self-signed TLS | Done | Generated with rcgen, ECDSA P-256, stored as PEM in the data directory |
| Password check | Done; real accounts to be verified | PAM service `checkpw`, straight through OpenPAM. Fixed on 2026-09-24: `checkpw` passes `use_first_pass`, so the password must be stored in PAM before authentication; until then every account failed. A regression test confirms that the password reaches OpenDirectory |
| RemoteFX | Done | Encoded by IronRDP; the server diffs consecutive full frames and encodes only the tiles that changed |
| Cursor shape and position | Done | Polled by shape id; 2x bitmaps, at most 96 pixels |
| Lock-key sync | Done | libscreenio `sync_locks`; Caps Lock is read and set through IOKit |
| `release_all` | Done | Every key and button is released when a session ends |
| Logging and statistics | Done | One line every 5 seconds: frame rate, dropped frames, raw byte rate |
| sdl-freerdp connection | Done | TLS negotiation, credential validation, session loop, clean disconnect |
| mstsc / Windows App connection | To do | Needs a Windows client machine |
| Verification of the real screen picture, keyboard and mouse on real hardware | To do | Needs the host process to hold the Screen Recording and Accessibility permissions |
| 1080p RemoteFX 30 fps | Met with the synthetic picture | See the table below; real capture awaits the permissions |

Measured with the synthetic picture (moving colour bars), release build, sdl-freerdp connected over
loopback for 15 seconds, 2026-09-24:

| Size | Frame rate offered to the encoder | rdpmacd CPU | RSS |
|---|---|---|---|
| 1920x1080 | 27 fps | about 48% of one core | 61 MB |
| 3840x2160 | 24.5 fps | about 50% of one core | 166 MB |
| 7680x4320 | 30 fps | about 125% | 541 MB |

The CPU figures include generating the synthetic picture itself. At all three sizes the client
received RemoteFX surface bits, with no dropped frames or very few.

## M2 Retina and experience

| Item | Status |
|---|---|
| Resolution follows the client (ADR-0001 D8 step 1) | Done, verified with the synthetic picture; scaled capture of the real screen awaits the permissions |
| Choosing the display again when it is replaced | Done: every reopen chooses again through the monitor policy; to be verified on real hardware |
| Rebuilding capture when the display configuration changes | Implemented: the size is checked every second, and a change reopens capture and sends a Resize; to be verified on real hardware |
| libscreenio dirty rectangles and `Reset` | `Reset` exists and is returned when the system stops the capture stream; dirty rectangles are deferred, since H.264 encodes whole frames anyway and IronRDP diffs RemoteFX frames on the server |
| VideoToolbox H.264 → AVC420 | Done, verified with the synthetic picture; sessions up to 4096x2304 use H.264, larger ones RemoteFX |
| Frame acknowledgement backpressure and bitrate adaptation | Done: frames are skipped when the client's backlog exceeds 3 frames, the H.264 bitrate is adjusted every second by the share of skipped frames and rises again after 3 steady seconds; round-trip time probing is not wired in |
| Clipboard text | Done: plain text both ways, with unit tests for the sync logic; to be tested with a client on another machine |
| Relative mouse | Wired up; to be verified on real hardware |
| Permission ownership and stable signing (for development) | Done: `scripts/sign-dev.sh` signs with a self-signed certificate, and `scripts/agent.sh` installs `rdpmacd` as a LaunchAgent; the TCC log confirms that the responsible process is `rdpmacd` itself, and release and debug builds have the same designated requirement |

## Resolution work in later milestones

| Milestone | Item |
|---|---|
| M3 | rdpmac's own virtual display, created at the pixel size the client asks for (ADR-0001 D8 step 2); done, see below |
| M4 | Switching the real display mode on a Mac with a screen attached, off by default (ADR-0001 D8 step 3) |
| M5 | Several virtual displays for mstsc's multi-monitor sessions (planned for a Pro edition until ADR-0002) |

Resolution following the client, verified on 2026-09-24 with the synthetic picture and sdl-freerdp
over loopback:

| Scenario | Client asks for | Server provides |
|---|---|---|
| Default mode, fixed size | `/size:1280x720` | 1280x720 |
| `--resolution native` | `/size:1280x720` | 1920x1080, the picture's own size |
| Dynamic resolution | `/size:2400x1300 /dynamic-resolution`, window height limited by the system | 2400x1300 at first, 2400x961 after the layout message; the client completes the reactivation and keeps decoding |

H.264 against RemoteFX, 2026-09-24, release build, synthetic picture with a 30 fps target,
sdl-freerdp over loopback, no decoding errors on the client:

| Size | Codec | Actual frame rate | rdpmacd CPU | Bitrate |
|---|---|---|---|---|
| 1920x1080 | RemoteFX | 27 fps | about 48% of one core | n/a |
| 1920x1080 | H.264 | 28 fps | about 16% of one core | about 0.25 Mbit/s |
| 3840x2160 | RemoteFX | 24.5 fps | about 50% of one core | n/a |
| 3840x2160 | H.264 | 25 fps | about 21% of one core | about 0.4 Mbit/s |
| 7680x4320 | H.264, before the size limit | 7 fps | about 17% of one core | about 1.8 Mbit/s |
| 7680x4320 | RemoteFX, with the size limit | 30 fps | about 125% | n/a |

The synthetic picture is mostly still; the bitrate for a real desktop will be markedly higher. At
8K, H.264 is limited by the hardware encoder's throughput, and the decoder mstsc uses is limited to
4096x2304, so larger sessions always use RemoteFX.

The clipboard should not be tested over loopback on one machine: client and server share one system
pasteboard, and the delayed data the client announces overwrites the local clipboard contents. When
testing on one machine, give sdl-freerdp `-clipboard`, and verify the clipboard with mstsc on
another machine.

Still to verify for M2:

| Item | What it needs |
|---|---|
| Real screen picture, scaled capture, cursor scaling | Install `rdpmacd` as a LaunchAgent as the README describes, and grant Screen Recording |
| Keyboard and mouse injection, relative mouse, lock keys | As above, and grant Accessibility; TCC also preflighted Input Monitoring at startup, so if the lock keys do not sync, check that permission first |
| mstsc and Windows App connections, H.264 decoding, dynamic resolution | A Windows client machine |
| Clipboard text both ways | A client on another machine |

## M3 Product shell

| Item | Status |
|---|---|
| rdpmac's own virtual display (D8 step 2) | Done: libscreenio `VirtualDisplay`, built on CGVirtualDisplay and detected at run time; with no screen attached, a session gets a display at the client's size that replaces the placeholder display, at 1x and unscaled; it is removed 30 seconds after the session ends; with a screen attached, or when the size is refused, the session falls back to scaling |
| Swift menu-bar app | Done: status, current connection, permissions, settings, certificate import, thumbprint, restart, logs, diagnostics bundle, welcome window; the interface is still to be tested hands-on |
| Installing and uninstalling the service | Done: SMAppService when there is a Team ID, a classic LaunchAgent when there is none; `--enable-server` and the other command-line options are verified |
| Permission guidance | Done: the daemon requests the permissions itself, then opens the matching pane of System Settings; the TCC log confirms that the responsible process is the `rdpmacd` inside the app |
| Certificate import | Done: X.509 v3 PEM; checked, then swapped in with the old pair kept; after a restart the handshake uses the new certificate |
| Settings UI and settings file | Done: `config.toml`, with the command line taking precedence; control socket with `status`, `get_config`, `set_config`, `import_certificate`, `restart`, `request_permissions` |
| Signing and notarization | Signing done (development identity, inside out, stable designated requirement); Developer ID signing and `notarize.sh` are written but not verified: they need an Apple Developer account |
| pkg installer | Done: installs into the Applications folder, is not relocatable, and after installing restarts a server that is already running and opens the app; not actually installed on the development Mac (needs an administrator password) |
| Crash and log collection | Done: the daemon writes a log file per day and keeps 14 days, and panics go to the log; the diagnostics bundle holds the status, logs, crash reports, settings and system version |
| libscreenio | Done: `open_privacy_settings`; cbindgen generates the header; the C ABI is frozen at 1.0 |

Virtual display measurements, 2026-09-24, macOS 26.6.2, an Apple M4 Mac with no screen attached:

| Item | Result |
|---|---|
| Creating it, replacing the placeholder display | About 0.3 seconds |
| Resizing, with the display number unchanged | 30 to 290 milliseconds |
| Available sizes | Tested up to 3600x2250 and 5120x2880; the first time, the system set 3840x2160 to 1920x1080, and after one switch by the helper process it is available directly (see "After M3" below) |
| 1600x900 session captured through the virtual display | Unscaled; H.264 at about 30 to 36 fps, 2 to 4.6 Mbit/s, no dropped frames |
| 30 seconds after the session ends | The virtual display is removed; the placeholder display comes back with a new number |

System behaviour confirmed in these tests, which shaped the implementation:

- Once the process that holds a virtual display explicitly switches its mode, the window server
  ignores later settings for it, and it stays online after release until that process exits; a
  switch made by another process has none of these problems. So only one 1x mode is used and the
  system switches to it by itself; when a switch is unavoidable, it is left to a helper process.
- Once a process has read the mode of any display, it cannot read the modes of displays that appear
  later, so readiness is judged by the display bounds.
- SMAppService does not launch helpers registered by an app without a Team ID, so such builds use a
  classic LaunchAgent instead.
- With client and server on the same Mac, the virtual display replacing the placeholder display
  makes the SDL client quit; connecting from another machine does not have this problem.

Still to do for M3:

| Item | What it needs |
|---|---|
| Going through the welcome window, settings and certificate import in the menu-bar app | First `sh scripts/agent.sh uninstall`, then install `build/rdpmac-VERSION.pkg` and grant the permissions to the `rdpmacd` inside the app |
| A new Mac from installation to first connection without a terminal | A clean Mac, and a notarized installer |
| Developer ID signing and notarization | An Apple Developer account: Developer ID Application and Installer certificates, `notarytool` credentials |
| HiDPI modes for the virtual display | Deferred: needs a way to switch that does not trigger the first problem above |


## After M3: fixes from hands-on testing (2026-09-24)

| Report | Cause | Fix |
|---|---|---|
| The picture is blurry, like an over-compressed JPEG | VideoToolbox turns BGRA into limited-range YUV, which the client, as the specification prescribes, decodes as full range, so contrast was low; frame timestamps advanced at a fixed 30 fps, so the frame after a still spell got only 1/30 of a second's worth of bitrate; VideoToolbox's own rate control does not lower the quantiser for several seconds after large changes, and a new maximum quantiser set mid-stream has no effect | vImage converts to full-range BT.709; timestamps use real time; VideoToolbox runs in low-latency mode with a base quantiser given for every frame, and rdpmac does its own rate control (a leaky bucket); 0.2 seconds after the picture stops, it is refined to QP 16; the bitrate goes from 0.1 to 0.2 bits per pixel; where the hardware does not support a per-frame quantiser, rdpmac falls back to VideoToolbox's rate control and re-encodes a still picture at 4 times the bitrate |
| 1920x1200 disconnects right after connecting, error 0x1108 | The SPS had no bitstream restriction in its VUI, so the decoder had to prepare a 12-frame buffer, the maximum for level 5.0; 1920x1080 is level 4.0 and needs only 4 frames | The SPS VUI is rewritten: full-range BT.709, no reordering, a buffer equal to the number of reference frames; low-latency mode declares 12 reference frames by default, and they are limited to 2 (limited to 1, it produces only key frames) |
| The cursor is upside down | IronRDP passes the pointer data unchanged as the 32-bit XOR mask, which RDP defines as bottom-up BGRA | Before sending, the row order is reversed and R and B are swapped; the hotspot stays relative to the top-left corner |
| In full screen or at 3840x2160 the Mac has only 1920x1080; sometimes 4K is in the list but has to be switched to by hand | For a display it has not learned, macOS turns 3840x2160 into a 1920x1080 mode it adds itself; when the new display was refused, rdpmac removed it and fell back to the placeholder display, so the list held only 1080p; after one switch by hand, macOS remembers the choice by display identity (vendor, product, serial number), and from then on the display gets 4K directly | When the size is refused, rdpmacd switches from a helper process (`rdpmacd --switch-display-mode`) through the public CoreGraphics calls, the same as switching by hand; switching in the daemon would hold on to the display, which then no longer responds to resizes and stays online after release |
| Clicks still have no effect after granting the permissions, and the log gives no clue | macOS does not apply a new grant to a running process, and the Screen Recording check inside the process keeps returning its result from launch; without the Accessibility permission, events are dropped silently | The status gains `restart_needed`, and the app says so under the permission items and offers a restart button; when a connection comes in without the Accessibility permission, the log states the reason and what to do; the warning for a failed injection names the event |
| After the Mac locks, an RDP connection shows only black; the cursor moves, but clicks have no effect | After a lock the display soon sleeps, and macOS does not remove a virtual display released meanwhile until there is user activity; rdpmac did not declare user activity when connecting, so with no active display it took that leftover display, treated it as a real screen and created no virtual display, and then failed to capture it (`invalid argument`) | A session start declares user activity (`IOPMAssertionDeclareUserActivity`, the same as `caffeinate -u`) and waits for a display to wake; rdpmac remembers the virtual displays it released, and waits for macOS to remove them before deciding which display to use |
| A password can be typed on the lock screen but is always rejected; RustDesk can unlock | loginwindow starts the unlock flow (`startUnlock` in the log) only when a user becomes active, and the flow times out after about 30 seconds; injected keyboard and mouse events do not start it, and while it has not started every password is rejected unchecked: the system logged no password check for the failed attempts, and did for the successful ones; RustDesk runs `caffeinate -u` first for every connection | User activity is declared when a session starts and when remote input arrives (at most once every 2 seconds); the first input after a gap of more than 20 seconds waits 200 milliseconds so that the unlock flow starts first |
| The menu-bar icon sometimes disappears | The app crashed (three times in a day, with the same crash report): by default the SwiftUI content of the settings and welcome windows rewrites the window's minimum and maximum size while AppKit updates constraints; when the content changed with the status refresh every two seconds, AppKit judged the constraint update to be stuck in a loop and threw an exception that ended the process; closed windows were not released either, and their content kept refreshing in the background | Windows now resize after their content's preferred size has changed, and are released when closed; the menu bar uses the template icon from the design mock-ups, with the same size for all five states |

Lock screen test (2026-09-24, with the Mac locked): a test instance created a virtual display and
released it 30 seconds after the session ended; after release it was still online and asleep, with 0
active displays, matching the state found when the picture was black. When the next session started,
loginwindow logged an unlock request (reason 9) and `startUnlock`, the leftover display disappeared
within 100 milliseconds, and a new virtual display was created at the client's size. Verified on
0.3.0: when mstsc connected to the locked Mac, loginwindow started the unlock flow 16 milliseconds
after the session started, and the first password typed unlocked it; each of the reconnections after
that created a new virtual display at the client's size.

Picture quality measurements: a screenshot of a desktop full of text at 1920x1200, encoded at the
session's pace, with luma PSNR computed between the picture the client finally settles on and the
original. The left column is the result with only the colour and timestamp changes, rate control
still by VideoToolbox; before the changes the picture looked worse than that (low contrast, too
little bitrate for still frames).

| Scenario | VideoToolbox rate control | Per-frame quantiser and still refinement | Refinement cost |
|---|---|---|---|
| Desktop still when connecting | 39.4 dB | 42.6 dB, 49.1 dB after 0.2 seconds | 1 frame, 182 KB |
| Scrolling for 1 second, then stopping | 47.3 dB | 42.6 dB, 49.1 dB after 0.2 seconds | 1 frame, 200 KB |
| Full-screen switching back and forth for 1/3 second, then stopping | 23.9 dB, does not rise again | 49.2 dB within 0.7 seconds of stopping | 3 frames, 595 KB |

Compared at 4x magnification, refinements at QP 14 to 20 are hard to tell from the original by eye;
only the colour fringes on text inherent to 4:2:0 remain. 49 dB corresponds to QP 16. When sustained
large changes exceed the bitrate, the quantiser goes up to 44; beyond that, new frames are held
back, and once the picture stops, the latest picture is sent first and then refined. Over FreeRDP
loopback, 1366x768, 1920x1080, 1920x1200 and 2560x1600 all decoded correctly, and refinement frames
went out as expected while the picture was still.

To be verified: whether mstsc connecting at 1920x1200 still reports 0x1108, and whether text is
sharp while the picture is still.

4K measurements (2026-09-24): when a new display identity asks for 3840x2160, both attempts to set
it end at 1920x1080 (about 3.3 seconds); the switch by the helper process then succeeds, about
3.6 seconds in total. After that a display with the same identity gets 4K directly (about
0.3 seconds), and resizing to 2560x1600, to 1920x1080 and back to 4K all work. rdpmacd over
loopback: when a client connects at 4K with an identity not learned yet, the log records the switch
by the helper process and then the display created at 4K; when it connects again at 2560x1600, the
display is resized in place.

## M4 Enterprise features (plan, 2026-09-24)

The scope comes from section 4 of ADR-0001, and acceptance means that domain accounts and local
accounts can both use NLA and that enterprise security questionnaires can be answered. The items are
ordered by risk and dependencies, starting with NLA, which changes IronRDP:

| Order | Item | IronRDP 0.13 today | What to do |
|---|---|---|---|
| 1 | NLA: credential store (standalone Macs) and Kerberos (Macs joined to a domain) | Has `RdpServerSecurity::Hybrid` and a CredSSP server; NTLM accepts only one account and password given in advance, and Kerberos can take a `KerberosServerConfig`; after authentication the client delegates its password | Add a pluggable credentials lookup to the acceptor, pointing `[patch.crates-io]` at a local IronRDP checkout as section 6 of ADR-0001 says, and submit it upstream later; the credential store checks the password through PAM at enrollment and keeps each user's NT hash in the keychain; Kerberos uses the service key from a keytab; the delegated password is checked through PAM again |
| 2 | AVC444 | The graphics pipeline has the AVC444 capability bit | Split the picture into two 4:2:0 views, luma and chroma, encode both with VideoToolbox, and remove the colour fringes on coloured text |
| 3 | Sound | Has `with_sound_factory` and an RDPSND server | libscreenio gains system sound capture (ScreenCaptureKit audio); negotiate PCM or AAC |
| 4 | Clipboard pictures and files | cliprdr has FileContents requests and responses | Pictures both ways, files both ways (done on 2026-09-26, see docs/clipboard.md) |
| 5 | Mode switching on physical displays (D8 step 3, off by default) | Not involved | Switch through a helper process with libscreenio's `switch_display_mode`, and restore the original mode when the session ends |
| 6 | MDM managed configuration | Not involved | Read the managed preferences that configuration profiles deliver, taking precedence over config.toml; the documentation lists every key |
| 7 | Audit and session recording interfaces | Not involved | Structured audit records (source, account, method, result, duration); for recording, only the interface is defined |
| 8 | Login window sessions | Not involved | A research report, no implementation |
| 9 | RemoteFX progressive | graphics has progressive-related code; whether the encoder is complete is still to be checked | Lowest priority; H.264 already covers modern clients |

Progress:

- 2026-09-24: NLA with the credential store is done; the design, implementation and tests are in
  `docs/nla.md`. The IronRDP patches are in the IronRDP fork (branch `rdpmac/nla`) and not yet
  submitted upstream; the NT hashes are kept in the login keychain, and accounts are enrolled in the
  app; the setting `security = "nla"` turns NLA on. Loopback on the same Mac (FreeRDP sfreerdp) has
  been tested with the right password, a wrong password, an account not enrolled, a client that
  supports only TLS, and the lockout; verified with mstsc on 2026-09-25. Kerberos waits until there
  is an AD domain.
- 2026-09-25: NLA broke after an update. Under a signature without a Team ID, every build counts as
  another program and cannot read the hash the previous build wrote. The status, the startup log and
  the app now ask for enrollment again; enrolling again overwrites the old item, and removing an
  enrollment whose item cannot be deleted overwrites it with a marker. The long-term fix is
  Developer ID signing. See `docs/nla.md`.
- 2026-09-25: AVC444 is done; the design, measurements and tests are in `docs/avc444.md`. It uses
  AVC444v2, sending both views together in every frame. VideoToolbox predicts only from the previous
  frame; long-term references let each view predict from the previous view of its own kind, so
  typing costs only a few KB per view. RGB PSNR of coloured text rises from 28.4 dB to 37.0 dB. The
  IronRDP patches add `send_avc444v2_frame` and correct the length field of frames that carry only
  chroma. The setting `codec = "avc420"` turns it off; at 4K, sustained full-screen change runs at
  about 18 fps (30 fps with AVC420). Loopback on the same Mac (FreeRDP) passes, and a test with
  mstsc showed no problems.
- 2026-09-25: AVC444 colour conversion can use several cores (at most 6 threads by default, 10% to
  15% faster per frame); changing the codec or multi-core conversion in the app needs no restart and
  applies from the next connection, so the options are easy to compare in practice.
- 2026-09-25: Sound. libscreenio captures the sound the Mac plays through ScreenCaptureKit
  (C ABI 1.2), and rdpmac sends it to the client with IronRDP's RDPSND as 16-bit stereo PCM at
  48/44.1 kHz; the `audio` setting turns it off. Loopback (FreeRDP, test tone) passes; real capture
  is still to be tested with mstsc. The design and tests are in `docs/audio.md`.
- 2026-09-25: Corrupted picture after switching windows while the screen saver was on. While the
  client is minimised the picture pauses; when it is restored or asks for a redraw, a key frame is
  sent and the bitrate is reset (the IronRDP patches add `request_refresh`). `h264-dump` records the
  bitstream exactly as sent, for telling which end is at fault when it happens again. See
  `docs/refresh.md`.
- 2026-09-26: A recording proved that the corrupted picture came from mstsc pairing AVC444's two
  views one frame apart (the bitstream itself was correct). Redraws and restores now reset the
  graphics state, and an access unit delimiter precedes each view; capture really keeps to `fps` (it
  used to run at up to 60 fps). Sound: `wTimeStamp` is filled in as the specification says; a ledger
  keeps what is sent at most 200 ms ahead of real time, blocks older than 200 ms are not sent, and
  nothing is sent while the client asks for no output; `mute-mac` (on by default) mutes the Mac while
  the client plays the sound. (Corrected on 2026-09-27: this note first described catching up on
  the client's confirmations, which cannot work, since mstsc confirms a block as soon as it arrives.)
  See `docs/refresh.md` and `docs/audio.md`.
- 2026-09-26: Pictures and files on the clipboard, both ways: pictures as PNG, CF_DIB and CF_DIBV5,
  files and folders through File Contents requests. See `docs/clipboard.md`.

## M5 Open-source release (from 2026-09-27)

ADR-0002 made rdpmac fully open source under MIT OR Apache-2.0 and put this milestone in place of
"Pro and commercialisation".

| Order | Item | State |
|---|---|---|
| 1 | MIT OR Apache-2.0; libscreenio in this repository with its history; documentation in English; CONTRIBUTING.md, SECURITY.md and CI | Done (2026-09-27) |
| 2 | Publish the repository and the IronRDP fork with branch `rdpmac/nla`; CI passing on GitHub's macOS runners | Done (2026-09-27): github.com/SamMusk667/rdpmac, whose first CI run passed |
| 3 | Developer ID signing and notarization; the .pkg on GitHub Releases; automatic updates | To do; needs an Apple Developer account |
| 4 | Propose the five IronRDP patches upstream | To do |
| 5 | Who may connect: TLS logons limited to the user rdpmacd runs as, or to a list, as NLA already is | Done (2026-09-27): `allow-users`; other accounts are turned away before PAM |

After M5, for people who use rdpmac every day, in this order:

1. Keyboard: check which macOS system shortcuts injected events fail to trigger (Cmd+Tab,
   Spotlight, screenshots) and handle them; layouts other than US; an optional mapping of Windows'
   Ctrl shortcuts to Cmd.
2. Input methods: Chinese and Japanese input through the Mac's own input sources, verified with
   mstsc.
3. Clients: Windows App on macOS, iOS and Android; recent FreeRDP; Intel Macs.
4. Weak networks: bitrate from the round-trip time as well as from frame acknowledgements;
   reconnecting after a dropped connection.
5. A Mac with a display attached: blanking its own screen while a session runs.
6. Files copied on the client fetched only when Finder pastes them.
7. Several virtual displays for mstsc's multi-monitor sessions.

M4's other items stay open and come after these: Kerberos, physical display modes, MDM managed
preferences, audit records and a session recording interface, login-window research.

## Drive redirection (from 2026-09-29)

ADR-0003 decided how the drives a client shares reach the Mac: each is mounted at
`~/RDP Drives/<drive> on <client>` through an NFS server inside rdpmacd. The work was brought
forward ahead of the list above, in the ADR's four steps:

| Step | Item | State |
|---|---|---|
| 1 | Protocol: the server side of RDPDR in the IronRDP fork, with a unit test for every PDU | Done (2026-09-29): branch `rdpmac/rdpdr` of the fork; 16 PDU tests and 9 tests of the channel (handshake, drives accepted and other devices declined, removal, stat, listing, failure status, timeout, end of connection) |
| 2 | Read-only drives: the hardened NFS server, the mounts and their life cycle, answers for macOS's own names | Done with FreeRDP (2026-09-29): 14 tests in rdpmac-session, one of them mounting through macOS's NFS client; end to end with sfreerdp on the same Mac, a 5 MiB read with equal SHA-256 in 0.05 s and 300 files listed in 0.06 s (docs/drives.md). mstsc still to test |
| 3 | Writing, with macOS's own files kept on the Mac | Done with FreeRDP (2026-09-29): 18 tests in rdpmac-session and 10 of the channel; end to end with sfreerdp, a 5 MiB copy onto the drive with equal SHA-256 in 0.28 s, SQLite, renaming over a file, setting times, and no `._` or `.DS_Store` file on the client (docs/drives.md). mstsc still to test |
| 4 | Finishing: the setting in the app, docs/drives.md, mstsc and Windows App, timeouts tuned on a slow link | Under way: the setting and docs/drives.md are done; with mstsc, opening and copying files work (2026-09-29, after the share-mode fix); Windows App and a slow link still to test |

Later on 2026-09-29 the fork's `rdpmac/nla` was merged with upstream IronRDP's master, which has a
server side of RDPDR of its own, and branch `rdpmac/rdpdr` was retired. rdpmac uses upstream's, with
six small commits in the fork (ADR-0003, decision 5 as amended). The requests to the client's files
moved into rdpmac-session, with 10 tests against IronRDP's channel, and the end-to-end results with
sfreerdp did not change.

## Unlocking the lock screen at logon (2026-10-03)

When the Mac is locked, the password the user logged on with, just checked by PAM, is typed into
the lock screen as the session starts, as Windows unlocks its console. It is on by default;
`unlock = false` turns it off. The design, its safety checks and what it rests on are in
docs/unlock.md. Unit tests cover the attempt against a scripted Mac. The lock-state checks were
read on a locked Mac; a test with mstsc is still to come.

## UDP (2026-10-05)

The fork's branch `rdpmac/udp` merges upstream IronRDP master 38b074e4, which adds reliable UDP
for servers (#1954). With `udp = true`, rdpmacd offers it on the listening port, and the picture
moves onto the tunnel for clients that take it up, such as mstsc. It is off by default until a
comparison with TCP shows a gain; macrdp measured none for the picture. Tested with FreeRDP, which
declines, so the session stays on TCP; mstsc is still to test. docs/udp.md has the details and
limits.

## Reconnecting (2026-10-08)

The first of the weak-network items. rdpmacd hands clients an auto-reconnect cookie, sends
heartbeats so that a client notices a silent link, and drops a connection whose client has
acknowledged nothing for 30 seconds (`with_dead_peer_timeout`, a patch in the IronRDP fork), so
that the reconnecting client is served. Tested with FreeRDP: the right cookie gets in without the
password, an altered one is turned away. A real drop with mstsc is still to test.
docs/reconnect.md has the details.
