# libscreenio

libscreenio is the capture and injection layer of rdpmac. It lived in its own repository until
2026-09-27 and keeps its own version (0.1.0) and C ABI version.

Screen capture, cursor state, and keyboard and mouse injection, behind a synchronous API kept as
small as possible, with a C ABI (`libscreenio.dylib` / `.a`). It is meant to be the capture and
injection layer of an RDP server. Only macOS has a real implementation so far; other platforms
compile the same interface and return `SIO_E_UNSUPPORTED`.

## Layout

```
crates/screenio-core/     public Rust API; src/macos is the macOS implementation, src/stub.rs the interface stub for other platforms
crates/screenio/          C ABI, include/screenio.h; examples/c is an example caller in plain C
```

## macOS implementation

| Capability | System interfaces used | Rust bindings |
|---|---|---|
| Capture | ScreenCaptureKit: `SCShareableContent`, `SCContentFilter`, `SCStream`, BGRA frames | `objc2-screen-capture-kit`, `objc2-core-media`, `objc2-core-video` |
| Display enumeration | CoreGraphics: `CGGetActiveDisplayList`, `CGDisplayCopyDisplayMode` | `core-graphics` |
| Cursor | Bitmap and hot spot of `NSCursor.currentSystemCursor`; `CGEventGetLocation` | `objc2-app-kit`, plain extern |
| Keyboard and mouse injection | `CGEventCreateKeyboardEvent` / `CGEventCreateMouseEvent` / `CGEventCreateScrollWheelEvent`, posted at the HID level | `core-graphics` |
| Permissions | `CGPreflightScreenCaptureAccess`, `AXIsProcessTrusted` | plain extern |
| User activity | IOKit: `IOPMAssertionDeclareUserActivity` | plain extern |

It does not use CGDisplayStream, which Apple has deprecated, and does not depend on the old
`objc` 0.2 / `block` 0.1; the object model comes from the `objc2` 0.6 ecosystem (`objc2`,
`block2`, `dispatch2` and the framework bindings).

Capture needs the Screen Recording permission, and keyboard and mouse injection needs the
Accessibility permission. Both are granted to the host process (a terminal, an IDE or the eventual
server program); the library can only query them (`sio_session_info`) and trigger the system
prompts (`sio_session_request_permissions`). The cursor position and the cursor image need no
permission.

## Build and run

From the repository root:

```sh
cargo build -p screenio                       # produces target/debug/libscreenio.{dylib,a}
cargo run -p screenio-core --example screenshot [--request-permissions]
sh crates/screenio/examples/c/build.sh && ./crates/screenio/examples/c/screenshot
```

## Conventions

* Coordinates are the OS's virtual-desktop coordinates; on macOS they are logical points, and
  `sio_display_t.scale` gives the number of captured pixels per point.
* `Capturer::open_scaled` (C interface `sio_capture_open_scaled`) has ScreenCaptureKit scale the
  picture to a given size on the GPU, centred with letterboxing when the aspect ratios differ.
* Capture delivers at most 60 frames a second by default. `open_with_rate` and
  `open_scaled_with_rate` in the Rust interface take 1 to 120 frames; when the screen changes
  faster, frames in between are skipped, never the latest one. The C interface does not have these
  two yet.
* Frames are BGRA, rows top-down, with a stride; the `data` pointer stays valid until the next
  `sio_capture_frame` or `sio_capture_close`. `sio_capture_frame` returns a new frame only when the
  picture has changed, and `SIO_E_TIMEOUT` on timeout; when the system stops the capture stream (a
  display disconnected, for example) it returns `SIO_E_RESET`, and the capturer should then be
  closed and opened again.
* Keyboard input uses PC/AT set-1 scan codes with E0/E1/release flags, exactly as RDP messages
  carry them; there are Unicode events as well. The library keeps the modifier state itself and
  attaches it to every event.
* Lock keys: `sync_locks` synchronises them from RDP's TS_SYNC_EVENT bits; macOS has only Caps
  Lock, whose state is read and written through the IOKit HID system.
* The display list includes displays that are connected but asleep, so a server can still address
  a display while its panel is off.
* Virtual displays: `VirtualDisplay` (C interface `sio_virtual_display_*`) is built on the private
  CGVirtualDisplay API, checks at run time whether it is available, and is sized in 1x pixels. On a
  Mac with no display attached it replaces the system's placeholder display (the one whose
  `placeholder` is true) as the desktop; once it is released, the placeholder display comes back
  with a new id. macOS 26 settles it at 1920x1080 until it has learned 3840x2160:
  `create_with_switch` in the Rust interface then switches to that size by calling
  `switch_display_mode` through a helper process the caller provides, and once macOS remembers the
  size for the display's identity, later displays are 4K straight away. The switch has to happen in
  another process, because the process that performs it keeps hold of the display, which then no
  longer responds to resizes. The C interface lacks this step: `resize` returns an error, and the
  display keeps the size the system chose.
* The cursor shape carries `scale`, the ratio of bitmap pixels to points, which callers use when
  they scale the cursor for a session.
* User activity: to macOS, injected keyboard and mouse events do not fully count as user activity.
  They do not wake a sleeping display, and a released virtual display stays in the list. The lock
  screen shows its password field, but its unlock flow starts only when the user becomes active;
  until it has started, no password is checked and every one is judged wrong.
  `declare_user_activity` (C interface `sio_declare_user_activity`) has the same effect as
  `caffeinate -u`; a remote desktop server calls it when a session starts and when remote input
  arrives. `wake_displays` (`sio_wake_displays`) declares activity and returns once a display has
  woken. Both were added in 1.1.
* Screen lock, in the Rust interface only: `screen_lock` reads whether the session's screen is
  locked, an ID that changes with every lock, and whether the lock screen's password field has the
  keyboard (loginwindow turned secure keyboard entry on), from WindowServer's session dictionary.
  An error means the state is unknown, never unlocked. `keystrokes` gives, for each character of a
  text, the key of the current keyboard layout and the Shift and Option that type it, through
  `UCKeyTranslate`; a character that needs a dead key or an input method is an error.
  `Input::keystroke` types one, and `Input::locks` reads Caps Lock. Text Input Sources aborts the
  process when two threads call it at once, so `keystrokes` takes a lock around it.
* Sound: `AudioCapture` (C interface `sio_audio_*`) captures, through ScreenCaptureKit, the sound
  the Mac is playing (except this process's), in one or two channels, as interleaved 16-bit
  samples. 8000, 16000, 24000 and 48000 Hz are captured directly; ScreenCaptureKit supports only
  these sample rates and silently captures at 48000 when asked for another, so the library gets
  44100 Hz by resampling from 48000 with AudioToolbox's AudioConverter. It needs the Screen
  Recording permission and macOS 13. While nothing plays it produces no data and `read` times out;
  sound left unread beyond about a second is dropped, oldest first. The capture stream has to
  carry a picture, so the library asks for 2x2 at one frame a second and discards it. Added in
  1.2. `source_rate` in the Rust interface gives the actual sample rate of the sound (scaled by
  the resampling ratio when resampled), for checking.
* Output mute: `OutputMute` in the Rust interface mutes the default output device while it lives
  (the mute property of the Core Audio device) and restores the device's original setting when
  released; `follow` follows a change of the default output device. Only the output is muted:
  apps keep playing. A device without a mute control (some HDMI outputs) returns `Unsupported`.
  The C interface does not have it yet.
* Permission guidance: `open_privacy_settings` (C interface `sio_open_privacy_settings`) opens the
  Screen Recording or Accessibility page of Privacy & Security; `request_permissions` puts the
  process into those two lists.
* Cursor shapes are cached by id: `cursor_shape_id` only reads a counter and suits polling at frame
  rate; when the id changes, call `cursor_shape` for the bitmap.
* All functions return synchronously: `0` on success, a negative `SIO_E_*` otherwise.

## C ABI

`crates/screenio/include/screenio.h` is generated by cbindgen from `crates/screenio/src/lib.rs`;
the comments in the header are the doc comments there. After changing the C interface, run
`sh scripts/screenio-header.sh`; `sh scripts/screenio-header.sh --verify` only checks that the
header is current. Both need `cargo install cbindgen` first.

The C ABI has been frozen since 1.0 (`sio_version()` returns `0x010000`); 1.x only adds to it:

* Existing functions, constants, type names, parameters and meanings do not change.
* Struct sizes and field layouts do not change: callers allocate arrays by `sizeof` (for
  `sio_display_list`, for example), and added fields would break them. New data comes through new
  functions.
* New capabilities come as new functions and new constants, and the minor version goes up with
  them.
* New negative error codes may appear; callers should treat a negative value they do not recognise
  as a failure.

## Relation to rustdesk

rustdesk (AGPL-3.0) served as read-only reference material while libscreenio was written: its
macOS cursor reading, its mapping from scan codes to virtual key codes, and its DXGI / X11 /
PipeWire capture back ends for other platforms. On macOS rustdesk uses CGDisplayStream plus enigo
(objc 0.2). None of its code was ever compiled into libscreenio, and the rule of ADR-0001 D6, which
ADR-0002 keeps, allows no rustdesk code into this repository.

## Not done yet

* Dirty rects for frames (ScreenCaptureKit provides them through `SCStreamFrameInfoDirtyRects`;
  they are not exposed yet).
* The direction and modifier behaviour of keyboard and mouse injection need to be verified on a
  real Mac once the Accessibility permission is granted.
* Ctrl+Alt+Del.
* HiDPI modes for virtual displays: switching the mode explicitly makes the system ignore later
  settings, so another way is needed.
* Windows / Linux back ends.

## Design decisions

The architecture decisions and the milestone plan of the RDP server project as a whole (rdpmac) are
in `docs/adr/0001-macos-rdp-server-on-libscreenio-and-ironrdp.md`, with parts superseded by
`docs/adr/0002-fully-open-source.md`.
