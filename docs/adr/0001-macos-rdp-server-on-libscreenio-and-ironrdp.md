# ADR-0001: macOS RDP server on libscreenio and IronRDP

| Field | Value |
|---|---|
| Status | Accepted (2026-09-23); partly superseded by [ADR-0002](0002-fully-open-source.md) (2026-09-27) |
| Date | 2026-09-23, accepted the same day; revised 2026-09-24, adding D8, see section 13 |
| Scope | rdpmac's technical approach, architecture, repository and licence, milestones |
| Inputs | The libscreenio prototype; the FreeRDP baseline on macOS measured on 2026-09-23; a check of the IronRDP source (2026-09-23) |
| Alternatives | See section 9: a hybrid architecture with FreeRDP; a from-scratch implementation in Swift/ObjC |

## 1. Background and goals

The target product is an RDP server that runs on macOS: mstsc on Windows, Windows App, and clients
such as FreeRDP and IronRDP can connect to it, see the Mac's console session and operate it.

*Superseded by ADR-0002: rdpmac is free and open source, with no Pro or commercial edition.*
Commercially, it is free for individuals first, with a paid enterprise or Pro edition later.

Facts already established:

- The libscreenio prototype already works on macOS: with ScreenCaptureKit it enumerates displays and
  grabs frames, it reads the cursor's position and shape, and it injects keyboard and mouse input
  following RDP's set-1 scancode model. Its dependency graph has 37 crates, with no rustdesk code
  and no objc 0.2 / block 0.1 / CGDisplayStream.
- FreeRDP's macOS shadow subsystem is dead: upstream has not built it on Apple platforms since
  2025-04, it has 6 hard errors against the macOS 26 SDK, and a binary forced through the build hits
  SIGTRAP before it listens. Its protocol core works; FreeRDP can serve only as a reference
  implementation and for interoperability comparisons.
- The IronRDP server (`ironrdp-server` 0.13.0, 2026-07-10; 17 releases since 2024-11) already has:
  two security layers, TLS and CredSSP; the `CredentialValidator` hook for password checks; RemoteFX
  and RDP6 bitmap encoders; a graphics pipeline server that can send AVC420, AVC444, AVC444v2,
  planar, uncompressed, RemoteFX progressive and ClearCodec frames, with frame acknowledgement and
  QoE statistics; clipboard, sound, display control, drive redirection, touch and USB channels;
  network auto-detection; large pointers; desktop size changes. It is licensed MIT or Apache-2.0 and
  maintained commercially by Devolutions, and its toolchain requires Rust 1.94.

Platform constraints, which do not depend on the approach but set the product's boundaries:

- A third party can only mirror the console session; it cannot open a separate session for each user
  as Windows RDS does.
- The Screen Recording and Accessibility permissions are granted to the host binary. MDM cannot
  grant Screen Recording directly; it can only allow users to approve it themselves.
- The development machine has a 7680x4320 Retina display (a BetterDisplay virtual display on a Mac
  without a screen, as D8 explains). CPU encoding cannot sustain 60 fps at native resolution, so
  hardware encoding through VideoToolbox is required.
- A daemon that needs Screen Recording and listens on a port cannot go into the App Store; it can
  only be distributed signed with a Developer ID and notarized.

## 2. Decisions

**D1 Use IronRDP for the protocol stack; do not write our own, and do not use FreeRDP as the core.**
`ironrdp-server` and its family of crates provide the connection sequence, security layers,
encoders, channels and the graphics pipeline; we implement only its four extension points,
`RdpServerDisplay`, `RdpServerInputHandler`, `CredentialValidator` and `GfxServerFactory`, plus the
channel factories. FreeRDP stays as a reference implementation: for negotiation details, encoding
parameters and client compatibility problems, we compare against its source and the behaviour of its
sample server.

**D2 Use libscreenio for capture and injection.** `screenio-core` is the only place that touches
ScreenCaptureKit, CoreGraphics events and the AppKit cursor; other platforms keep stub
implementations.

*Superseded by ADR-0002: libscreenio now lives in this repository, as `crates/screenio-core` and
`crates/screenio`, and keeps its C ABI.* It stays in a repository of its own, with a C ABI, so that
other servers or languages can use it later.

**D3 Encoding in three steps, without OpenH264 or FFmpeg.** Step 1 is RemoteFX plus RDP6 bitmaps,
all done by IronRDP's existing encoders; step 2 is H.264 encoded in hardware by VideoToolbox and
sent as AVC420 through the graphics pipeline; step 3 is AVC444's split into two streams, and
RemoteFX progressive. H.264 uses only the encoder that comes with the system, to avoid patents and
GPL dependencies. H.264 is used only for sessions up to 4096x2304: the Media Foundation H.264
decoder in Windows, which is the decoder mstsc uses, is limited to 4096x2304, and at 8K the hardware
encoder manages only about 7 fps; larger sessions always use RemoteFX. Desktop pictures ask
different things of H.264 than video does, so encoding is configured as follows. Colour is
full-range BT.709, as MS-RDPEGFX 3.3.8.3.1 specifies, converted by vImage before it goes to
VideoToolbox. The SPS's VUI states full-range BT.709 and a bitstream restriction, so that the
decoder buffers only as many pictures as there are reference frames. We do the rate control:
VideoToolbox runs in low-latency mode and receives a base quantiser with every frame, a frame that
changes takes its quantiser from the fill level of a leaky bucket, and once the picture has been
still for 0.2 seconds the current picture is encoded again with finer quantisers, down to QP 16.
Where the hardware does not support per-frame quantisers, VideoToolbox's own rate control is the
fallback, and a still picture is encoded again at a raised bitrate. This is based on measurements:
VideoToolbox's own rate control does not lower the quantiser for several seconds after a change over
a large area, and changing its quantiser limit midway has no effect.

**D4 Authentication and transport security.** TLS 1.2 and 1.3 come from rustls. On first run a
self-signed certificate is created and kept in the configuration directory; an imported certificate
can replace it. The first stage enables only the TLS security layer: credentials come from
ClientInfo, and `CredentialValidator` checks local accounts through OpenDirectory or PAM, with rate
limiting and lockout on failures. NLA comes once the TLS path is stable: IronRDP's CredSSP server
has to know the credentials in advance for the NTLM challenge, so both modes are planned, a per-user
RDP credential store for standalone Macs, and Kerberos with a keytab for Macs joined to a domain.

**D5 Process model.** The Rust daemon `rdpmacd` runs as a LaunchAgent in the user's login session
and handles listening, sessions, capture, encoding and injection; the Swift menu-bar app handles
permission guidance, certificates and settings, status display and updates; the two communicate over
a local Unix socket. TCC checks permissions against the "responsible process": the terminal app when
the daemon is started from a terminal, sshd when it is started over SSH, and `rdpmacd` itself only
when launchd starts it as a LaunchAgent, so the permissions are granted only to `rdpmacd` running as
a LaunchAgent. Every build of the daemon is signed with the same certificate (a self-signed one
during development, Developer ID for releases), and its designated requirement is the identifier
`com.rdpmac.rdpmacd` plus the certificate, so TCC grants survive rebuilds; the linker's default
ad-hoc signature only recognises the binary's hash, and a single rebuild invalidates the grants.

`rdpmacd` ships with the app, in `rdpmac.app/Contents/MacOS/`. An app with a Team ID registers the
service with SMAppService from a plist inside the bundle; macOS does not launch helpers registered
by an app without a Team ID, so development builds and self-built copies use a classic LaunchAgent
in `~/Library/LaunchAgents` instead, which runs the same `rdpmacd` by its full path. Either way the
responsible process is `rdpmacd` itself. Settings are kept in `config.toml` in the data directory,
and the command line takes precedence. Through a Unix socket in the data directory the app reads
status, changes settings, imports certificates, requests permissions and restarts the service, one
JSON request per line; the socket accepts connections only from the same user.

**D6 Repository and licence.** *Superseded by ADR-0002: rdpmac, libscreenio included, is free and
open source under MIT OR Apache-2.0; there is no Pro or commercial edition and no contributor
licence agreement, and libscreenio lives in this repository. ADR-0002 keeps the rule that no
rustdesk (AGPL) code may enter the repository.* The product is named rdpmac, the daemon `rdpmacd`,
and the repository is rdpmac (a Cargo workspace). libscreenio stays in its own repository, under
Apache-2.0. rdpmac adopts the same dual licence as RustDesk: the free edition under AGPL-3.0, and a
Pro edition with more features released under a commercial licence, which needs a contributor
licence agreement to keep the right to dual-license; Pro features go in a separate closed-source
repository and plug in as crates. Until libscreenio has a remote repository it comes in as a path
dependency; after that it becomes a git dependency pinned to a tag, with `[patch]` pointing at a
local path for development. No AGPL code from rustdesk may enter any of the repositories.

**D7 Interoperability and test baseline.** The `sdl-freerdp` and `probe_x224.py` already built for
the FreeRDP baseline serve as automated smoke-test tools; mstsc, Windows App, Microsoft Remote
Desktop for Mac, FreeRDP and IronRDP client make up the manual interoperability matrix; every
milestone produces numbers measured the same way as the FreeRDP baseline.

**D8 Resolution follows the client, in three steps.** The resolution that a client such as mstsc
requests decides the session's resolution, no longer depending on external tools such as
BetterDisplay. This is based on a check on the development machine on 2026-09-24: without
BetterDisplay, a Mac mini with no display attached has only one virtual display from the system,
with a single mode, 1920x1080, so there is no resolution to switch to; with BetterDisplay running,
its virtual display defaults to 7680x4320, and the client has to take an 8K picture.

1. **Follow the client's resolution (M2).** Turn on IronRDP's `with_honor_client_desktop_size`, so
   that a connection takes the width and height the client requests in the GCC Client Core Data;
   implement `request_layout`, so that when mstsc has dynamic resolution enabled, dragging its
   window to another size changes the session's resolution in real time. When the Mac's display
   cannot provide that size, ScreenCaptureKit itself outputs the picture scaled to it, letterboxed
   when the aspect ratios differ, and mouse coordinates and the cursor size are converted to match.
   When the display is replaced during a session (for example when turning BetterDisplay on or off
   changes the display IDs), the display policy chooses again, without a reconnect.
2. **rdpmac's own virtual display (M3, free edition).** Create a display through the private
   CGVirtualDisplay interface at the pixel size the client requests, and change its mode for a new
   layout during the session: native resolution, no scaling, and no dependence on BetterDisplay at
   all. The private interface is detected at run time; where it is unavailable, the scaling of
   step 1 is the fallback. A single virtual display goes into the free edition, because a Mac mini
   with no display attached is the most common deployment. Implementation (2026-09-24): the display
   is created only when no display is attached and only the system's placeholder display is left,
   and it replaces the placeholder display as the desktop; when a display is attached, that display
   is left alone and scaling continues. The virtual display is created after the credentials pass,
   changes mode in place when resized, and is removed 30 seconds after the last session ends. Only
   1x modes are used, and the system switches to them by itself. Until it has learnt 3840x2160,
   macOS 26 sets the display to 1920x1080; rdpmacd then switches it explicitly from a helper
   process. The process that starts a switch holds on to the display, which then no longer responds
   to resizing and does not go away when released, so the switch cannot happen in the daemon; once
   macOS has remembered the mode for the display's identity, it is available directly. HiDPI is
   deferred.
3. **Switch the physical display's mode (M4).** A Mac with a display attached picks the closest of
   the modes the display supports, and restores the original mode on disconnect; off by default, so
   as not to disturb the local user.

*Superseded by ADR-0002: multi-monitor is an ordinary roadmap item, not a Pro feature.* Creating
several virtual displays for mstsc's multi-monitor mode goes into Pro (M5), and needs upstream to
lift the single-monitor limit of IronRDP's display control channel.

## 3. Architecture

### 3.1 Components

```
Windows / macOS / Linux RDP clients
        │  TCP 3389, TLS
        ▼
┌───────────────────────────── rdpmacd (Rust, LaunchAgent) ──────────────────────────────┐
│ ironrdp-server: listening, X.224/MCS/GCC, TLS, capability negotiation, fast-path,      │
│   channels, egfx                                                                       │
│   ▲ DisplayUpdate            ▲ credential check    │ KeyboardEvent/MouseEvent          │
│   │                          │                     ▼                                   │
│ rdpmac-session: frame scheduling, dirty rectangles, codec choice, cursor cache,        │
│   QoE feedback, session state machine                                                  │
│   ▲ Frame / CursorShape      │ rdpmac-auth         │ key_scancode / mouse_*            │
│   │                          │ OpenDirectory       ▼                                   │
│ screenio-core: ScreenCaptureKit capture │ PAM │ CGEvent injection, cursor, permissions │
│ rdpmac-encode: RemoteFX(IronRDP) │ VideoToolbox H.264 → AVC420/AVC444                  │
└───────────────────────────────┬────────────────────────────────────────────────────────┘
                                │ Unix socket (status, settings, permission requests)
                Swift menu-bar app (permission guidance, certificates, settings, updates)
```

### 3.2 Data flow

- **Picture.** The capture thread pulls frames from libscreenio, with dirty rectangles;
  `rdpmac-session` merges the dirty rectangles and chooses a path by the client's capabilities. A
  client without the graphics pipeline gets `DisplayUpdate::Bitmap`, which IronRDP encodes
  internally as RemoteFX or RDP6 bitmaps and fragments; a client with the graphics pipeline and
  H.264 support goes through VideoToolbox, the hardware-encoded frames are sent with
  `send_avc420_frame` or `send_avc444_frame`, and frame acknowledgements and `should_backpressure`
  set the pace of the next frame. When displays are reconfigured, libscreenio returns `Reset`, and
  the session sends `DisplayUpdate::Resize` and rebuilds the capturer.
- **Cursor.** ScreenCaptureKit does not draw the cursor, so a cursor thread polls its position and
  shape id: a change of position sends `PointerPosition`; a change of shape sends `RGBAPointer` or
  `LargePointer` depending on the size, with a cache index kept per id; a hidden cursor sends
  `HidePointer`.
- **Input.** `KeyboardEvent::Pressed{code, extended}` passes straight through to
  `key_scancode(code, EXTENDED)`, and `Released` adds `RELEASE`; `UnicodePressed(u16)` goes to
  `key_unicode` once surrogate pairs are combined; `Synchronize(flags)` goes to the new
  `sync_locks`. `MouseEvent::Move` and `Button` use absolute coordinates, `RelMove` and `ButtonRel`
  relative ones, and wheel values pass straight through at 120 per notch. `release_all` is called
  when the client disconnects.
- **Authentication.** The ClientInfo credentials of a TLS connection go to `rdpmac-auth`, which
  checks local or directory accounts with OpenDirectory; failures are counted and rate-limited by
  source IP and user name; the session starts only after the check passes; the audit log records the
  source, account, result and duration.
- **Control.** Through the Unix socket, the menu-bar app reads the daemon's status (listening
  address, current session, permission status), writes settings (port, bind address, certificate,
  encoding preferences), and guides the user to System Settings when a permission is missing.

### 3.3 Runtime and threads

- `rdpmacd` runs the IronRDP event loop on tokio's current-thread runtime, as the official example
  does. Capture, encoding and the cursor each have a dedicated OS thread and hand their output to
  the session task through bounded channels, which drop old frames when full, to guarantee "newest
  frame first".
- libscreenio's `Capturer` and `Input` are each held on their own thread and not shared across
  threads; `Input` is called from a single injection thread only.
- Single-session rule: only one interactive client is served at any moment, and others queue or are
  view-only, as expressed by IronRDP's `ConnectionPolicy`.

### 3.4 Interface mapping

| IronRDP extension point | Implementation in libscreenio or this project | Notes |
|---|---|---|
| `RdpServerDisplay::size` | Session resolution: the size the client requested when following the client, otherwise the display's size in pixels | `--resolution follow-client` is the command-line default |
| `RdpServerDisplay::request_initial_size` and `with_honor_client_desktop_size` | Take the width and height the client requested in the handshake | M2; 0.13.0 passes on only the width and height, without the scale factor |
| `RdpServerDisplayUpdates::next_update` | `Capturer::frame` plus dirty rectangles → `BitmapUpdate{x,y,w,h,BGRA,stride}` | Nothing when nothing changed; `Reset` → `Resize` |
| Cursor updates | `cursor_position` / `cursor_shape` → `PointerPosition`, `RGBAPointer`, `LargePointer`, `HidePointer` | The shape id is the cache key |
| `RdpServerInputHandler::keyboard` | `Input::key_scancode`, `key_unicode`, `sync_locks` | Same scancode model, no conversion |
| `RdpServerInputHandler::mouse` | `Input::mouse_move`, `mouse_move_rel`, `mouse_button`, `mouse_wheel` | X1/X2 supported |
| `CredentialValidator::validate` | `rdpmac-auth` → OpenDirectory / PAM | TLS mode |
| `GfxServerFactory` and `send_avc420_frame` etc. | The VideoToolbox pipeline in `rdpmac-encode` | From M2 |
| `CliprdrServerFactory` | NSPasteboard bridge | M2 text, M4 pictures and files |
| `request_layout` | Resolution changes during a session: M2 scaled output, M3 a new mode on the virtual display, M4 a mode switch on the physical display | Single monitor, area limit slightly above 4K, hard-coded in IronRDP |
| Sound factory | ScreenCaptureKit audio or CoreAudio | M4 |

### 3.5 Configuration and data

- The configuration file is `~/Library/Application Support/rdpmac/config.toml`; the certificate and
  private key are in the same directory, with mode 0600.
- Logs go through `tracing`, to both a file and the unified log; the session audit has a file of its
  own.
- No client credentials are stored on disk. (Since 2026-09-24 the NLA credential store keeps the
  enrolled account's NT hash, not its password, in the login keychain; see docs/nla.md.)

## 4. Milestones

Estimates assume one experienced engineer working full time, and include testing and documentation.

| Milestone | Scope | Acceptance criteria | Estimate |
|---|---|---|---|
| M0 Done | libscreenio prototype; FreeRDP baseline; check of IronRDP's capabilities | See the input documents | Done |
| M1 First frame | Server repository skeleton; IronRDP connected to libscreenio's display and input; self-signed TLS; OpenDirectory password check; RemoteFX; cursor shape and position; `release_all`; logging | mstsc and Windows App can connect and operate the primary display; RemoteFX holds a steady 30 fps at a 1080p logical resolution on the development machine; the keyboard, modifier keys included, and the mouse, wheel and dragging included, work correctly in Finder, Terminal and a browser | 4 to 6 weeks |
| M2 Retina and experience | Resolution follows the client (D8 step 1); choosing again when the display is replaced; libscreenio dirty rectangles and `Reset`; VideoToolbox H.264 → AVC420; frame acknowledgement backpressure and network detection wired in; rebuilding on display changes; clipboard text; relative mouse | At 5K native resolution and 30 fps the daemon uses less than 60% of one core; end-to-end input latency on a LAN below 50 ms; plugging and unplugging displays does not drop the connection | 3 to 4 weeks |
| M3 Product shell | rdpmac's own virtual display (D8 step 2); Swift menu-bar app; LaunchAgent install and uninstall; permission guidance; certificate import; settings UI; signing and notarization; pkg installer; crash and log collection | On a new Mac, no terminal is needed from installation to the first remote connection; permissions survive rebuilds; notarization passes | 3 to 4 weeks |
| M4 Enterprise features | NLA in both modes (credential store, Kerberos); AVC444 and progressive; switching the physical display's mode (D8 step 3, off by default); sound; clipboard pictures and files; MDM-managed configuration; audit and session recording interfaces; investigation of login-window sessions | Both domain and standalone accounts can use NLA; enterprise security questionnaires can be answered | 8 to 12 weeks |
| *Superseded by ADR-0002, which redefines M5.* M5 Pro and commercialisation | Licences and activation; update channel; optional telemetry; Pro crates plugged in; multi-monitor implementation (interface kept from M1 on), creating several virtual displays for mstsc's multi-monitor mode | Free and Pro editions built from the same code base | Scheduled by business priorities |

M1 to M3 take about 3 to 4 months in total, to a releasable version.

## 5. Changes libscreenio needs

Grouped by milestone; all are incremental:

- M1: `Input::sync_locks`; Unicode surrogate pairs; ISO/JIS keys added to the scancode table;
  modifier keys, dragging and wheel direction verified on a real machine with the Accessibility
  permission; the threading rules of `Capturer` and `Input` documented.
- M2: the dirty rectangles from `SCStreamFrameInfoDirtyRects` exposed; frame timestamps;
  `CGDisplayRegisterReconfigurationCallback` triggering `Reset` and display change notifications;
  `CVPixelBuffer` frame handles for VideoToolbox without a CPU copy, alongside the existing BGRA
  copy path; cursor shapes as 1x or 2x bitmaps, chosen by the client's capabilities.
- M2 addition: `Capturer::open_scaled` has ScreenCaptureKit output at a given size; `CursorShape`
  reports the bitmap's ratio of pixels to points, so that the server can scale the cursor for the
  session.
- M3: `VirtualDisplay`, based on CGVirtualDisplay, with its availability detected at run time;
  permission guidance helpers such as `open_privacy_settings`; the header generated by cbindgen and
  the C ABI frozen at 1.0. Done: after C ABI 1.0, changes are only additive, and the size and layout
  of structs no longer change.
- M4: display mode switching, audio capture. The interface for multi-monitor capture is kept from
  M1 on, and the implementation goes into Pro (*superseded by ADR-0002*).
- Windows and Linux backends keep only interface stubs, at the lowest priority.

## 6. IronRDP dependency policy

- Use the releases on crates.io, locked by `Cargo.lock`; evaluate an upgrade once a month, treat a
  0.x minor release as possibly breaking the API, and make upgrades in separate PRs.
- When changes are needed, submit them upstream first, and meanwhile point `[patch.crates-io]` at a
  fixed commit of a fork; do not maintain a private fork for the long term.
- The toolchain is pinned with `rust-toolchain.toml`; the current minimum is 1.94.
- Things to watch: whether we have to write the graphics pipeline's AVC444 stream-splitting helpers
  ourselves; how complete the progressive encoder is; server support for multi-monitor layouts (Pro,
  *superseded by ADR-0002*).

## 7. Testing and acceptance

- Unit: scancode table, dirty rectangle merging, cursor cache, authentication rate limiting.
- Integration: `probe_x224.py` checks negotiation; `sdl-freerdp` and `ironrdp-client` connect
  automatically and check the frames and cursor they receive; error paths are verified in an
  environment without the permissions.
- Interoperability matrix: mstsc (Windows 10, 11), Windows App, Microsoft Remote Desktop for Mac,
  FreeRDP, IronRDP client, run once per milestone, recording the negotiated security layer and
  codec.
- Performance: benchmark scripts with synthetic picture changes, recording fps, bitrate, daemon CPU
  and end-to-end input latency; the numbers go side by side with the FreeRDP baseline.
- Security: TLS configuration scan; authentication brute-force test; dependency vulnerability audit;
  one external review before release.

## 8. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| IronRDP 0.x API changes | Upgrade cost | Lock versions, upgrade monthly, wrap the extension points in the `rdpmac-session` layer |
| Interoperability problems in the server code paths (mstsc's special behaviour) | Failed connections or garbled pictures | Run the interoperability matrix every milestone; compare with the FreeRDP source; take problems upstream |
| RemoteFX cannot keep up at Retina resolution | Poor experience in M1 | M1 offers a logical-resolution mode; M2 hardware encoding |
| Colour fringes on text with AVC420 | Appearance | M4 AVC444; mixed frames that send text regions as RemoteFX |
| TCC permission flow | Installation fails; permissions lost after a rebuild | Stable signing identity; guidance in the menu-bar app; documentation |
| NLA's credential model on macOS | Acceptance by enterprises | A dedicated decision before M4: credential store or Kerberos |
| One-session limit | Falls short of what users expect from Windows RDS | Make it clear in pricing and promotion; investigate login-window sessions |
| Keyboard layout differences (non-US layouts, IME) | Wrong characters typed | Scancodes passed straight through, with a Unicode fallback; layout test matrix |
| VideoToolbox encoding parameters and latency | Stutter, blurry text | Low-latency configuration, real-time property, frame acknowledgement backpressure; per-frame quantisers and refinement of still pictures (D3) |
| The licence choice drags on | Holds up splitting the repositories | Settle it in D6 before M1 ends |
| CGVirtualDisplay is a private interface | rdpmac's own virtual display stops working after a system update | Detect it at run time and fall back to scaled output when it fails; regression tests on every major macOS version |
| IronRDP 0.13.0 does not give the scale factor at connection time | The first connection cannot choose HiDPI from mstsc's scaling setting | Take the scale factor from layout messages during the session; propose upstream that Client Core Data be exposed |
| Quality of scaled output | Blurry picture or small text when the display and the requested size differ a lot | A known cost of step 1, removed in step 2 by a virtual display at native size |

## 9. Alternatives

- **libscreenio + FreeRDP hybrid.** The most complete features and the most mature interoperability,
  but a C and FFI boundary, a CMake dependency chain, more than 200 public security advisories, no
  clipboard in shadow, NLA that depends on SAM, and a dead Mac subsystem. Kept as a reference
  implementation and for comparison, not as the core.
- **From scratch in Swift or ObjC/C++.** The protocol stack would take at least 1.5 to 2
  person-years to become usable, and the Swift ecosystem has no basic RDP or ASN.1 libraries. Using
  Swift only for the shell is compatible with this decision.
- **Our own Rust protocol stack.** The same amount of work as above; IronRDP already covers the
  capabilities needed, under a permissive licence.

## 10. Consequences

- Benefits: a core in one language, memory safety, a clean licence, the option of keeping it closed
  (*superseded by ADR-0002*), few and clear extension points, and libscreenio can evolve and be
  reused independently.
- Costs: dependence on IronRDP's roadmap; AVC444 stream splitting and some encoder details may have
  to be filled in ourselves; the macOS platform constraints do not change with the choice.
- The project lead has answered the questions in section 11, and the decision was finalised on that
  basis.

## 11. Resolved questions (answered by the project lead on 2026-09-23)

1. The product is named rdpmac, the daemon `rdpmacd`, the repository rdpmac.
2. Licence: the same dual licence as RustDesk, AGPL-3.0 for the free edition and a commercial
   licence for Pro (*superseded by ADR-0002*).
3. NLA: added once the TLS path is stable, with both modes supported; see D4.
4. Login-window sessions: kept as an item to investigate, not part of M1 to M3.
5. Multi-monitor: the interface is kept from M1 on, and the implementation goes into Pro, in M5
   (*superseded by ADR-0002*).
6. Windows and Linux backends: lowest priority; only the interface is kept, with no implementation.
7. Resolution (2026-09-24): the three steps of D8, with step 1 in M2; a single virtual display of
   rdpmac's own goes into the free edition, several go into Pro (*superseded by ADR-0002*).

## 12. Tasks for the next two weeks

1. Create the server repository rdpmac, with the workspace and skeletons of four crates (the
   `rdpmacd` binary, `rdpmac-session`, `rdpmac-auth`, `rdpmac-encode`); `screenio-core` comes in as
   a path dependency for now.
2. Using IronRDP's `examples/server.rs` as a template, connect the display and input traits to
   libscreenio, starting with `with_tls`.
3. Generate and load a self-signed certificate; get `ExactMatchCredentialValidator` working first,
   then switch to checks through OpenDirectory.
4. Connect once each with `sdl-freerdp` and mstsc, and record the negotiation results and RemoteFX's
   fps and CPU at native and half resolution.
5. libscreenio: `sync_locks`, Unicode surrogate pairs, cursor cache ids; and a signing script that
   gives the daemon a stable bundle identifier, so that once it has the Screen Recording and
   Accessibility permissions, keyboard and mouse can be verified on a real machine.

## 13. Revision history

| Date | Revision |
|---|---|
| 2026-09-23 | First version, accepted the same day |
| 2026-09-24 | D3 adds H.264's size limit of 4096x2304, based on the limit of mstsc's decoder and measurements at 8K |
| 2026-09-24 | New D8, resolution follows the client; adjusted the scope of M2 to M5, the interface mapping in section 3.4, the libscreenio changes in section 5, the risks in section 8 and the resolved questions in section 11 |
| 2026-09-24 | D5 states that TCC checks permissions against the responsible process, that the grants go to `rdpmacd` running as a LaunchAgent, and that it must be signed with a fixed certificate; based on the TCC log naming sshd as the responsible process when started over SSH |
| 2026-09-24 | M3: D5 adds how the service is registered (SMAppService with a Team ID, a classic LaunchAgent without one), the settings file and the control socket; D8 step 2 states the scope of the implementation: created only when no display is attached, 1x, 4K falls back to scaling, HiDPI deferred; the M3 items for libscreenio in section 5 are done, and the C ABI is frozen at 1.0 |
| 2026-09-24 | D3 adds the H.264 configuration for desktop pictures: full-range BT.709, the SPS's VUI, rate control with per-frame quantisers, and refinement of still pictures; based on blurry pictures reported in real use, the disconnects at 1920x1200 and measurements of picture quality |
| 2026-09-24 | D8 step 2: 3840x2160 no longer falls back to scaling; a helper process switches to it explicitly once, and after macOS remembers it, it is available directly; based on a test showing that a switch made by hand holds and that later 4K sessions are 4K straight away, and on experiments with which process makes the switch |
| 2026-09-24 | D4's credential store mode lands, see `docs/nla.md`: NT hashes kept in the login keychain, checked through PAM at enrollment, and the delegated password checked through PAM again; following section 6, the IronRDP patches are kept in the IronRDP fork (branch `rdpmac/nla`), checked out locally and referenced with `[patch.crates-io]`, and all ironrdp crates point at the checkout together so their types stay consistent |
| 2026-09-27 | D6, and the Pro and commercial parts of sections 1, 2 (D2, D8), 4, 5, 6, 10 and 11, superseded by ADR-0002: MIT OR Apache-2.0, no Pro edition, libscreenio moved into this repository |
