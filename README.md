# rdpmac

An RDP server for macOS. Remote Desktop Connection (mstsc) and Windows App on Windows, and clients
such as FreeRDP and IronRDP, connect to the Mac's console session and work in it. The protocol comes
from [IronRDP](https://github.com/Devolutions/IronRDP); screen capture, the cursor, keyboard and
mouse injection and virtual displays come from libscreenio, in this repository. rdpmac is free and
open source under MIT OR Apache-2.0.

rdpmac is young (0.4.x). It has been used with mstsc and FreeRDP on an Apple M4 Mac running macOS 26
and 27; other clients, Intel Macs and older macOS versions are untested. There are no signed
releases yet: build it as described below.

## What it does

- **Picture.** H.264 through the graphics pipeline, encoded by VideoToolbox in hardware: AVC444v2 in
  full colour when the client supports it, so coloured text has no colour fringes; AVC420 otherwise;
  RemoteFX for clients without H.264. A picture that stops changing is encoded again with a finer
  quantiser, so text is close to lossless (docs/avc444.md).
- **Resolution.** The session takes the client's size, from mstsc's /w and /h, full screen, or an
  .rdp file, and follows the window when the client resizes it.
- **Macs without a screen.** Each session gets a virtual display of the client's pixel size, which
  becomes the desktop in place of macOS's placeholder display: native resolution, no scaling, Dock
  and menu bar included.
- **Lock screen.** A locked Mac wakes when a session starts and unlocks with the password the
  user logged on with, as Windows does (docs/unlock.md).
- **Sign-in.** TLS with PAM, or Network Level Authentication against an NT hash kept in the login
  keychain; lockout after repeated failures (docs/nla.md).
- **Clipboard.** Text, pictures, and files and folders, both ways (docs/clipboard.md).
- **Sound.** What the Mac plays reaches the client, paced to real time, while the Mac itself is
  muted (docs/audio.md).
- **Drives.** The drives a client shares appear in `~/RDP Drives`, named like "C on DESKTOP-01",
  to read and write (docs/drives.md).
- **UDP.** Optional: clients that support it, such as mstsc, get the picture over UDP on the same
  port (docs/udp.md).
- **Reconnecting.** When the network drops, the client reconnects to the same session by itself,
  without the password (docs/reconnect.md).
- **Menu-bar app and installer.** A welcome window walks through the permissions; the menu shows the
  state and connections and has settings, certificate import, logs and a diagnostics bundle.

```
crates/rdpmacd          The daemon: arguments and settings file, TLS certificate, control socket, IronRDP server assembly
crates/rdpmac-session   IronRDP's extension points: display updates, virtual displays, cursor, input mapping, clipboard, sound, H.264 pipeline
crates/rdpmac-encode    Frames to RDP updates: VideoToolbox H.264 and AVC444, rate control
crates/rdpmac-auth      Credential checks: PAM (local and directory accounts), static credentials, lockout, NLA credential store
crates/screenio-core    libscreenio: capture, cursor, input injection, virtual displays, sound (docs/libscreenio.md)
crates/screenio         libscreenio's C ABI
app/                    The Swift menu-bar app, which contains rdpmacd once packaged; app/Icons holds the icon designs
scripts/                Signing, the development LaunchAgent, packaging the app and installer, notarization
docs/                   Architecture decisions, milestones and design notes
```

## Building

You need macOS 13 or later, Xcode or its command line tools (Swift 5.10, for the menu-bar app), and
Rust 1.94 or newer.

rdpmac builds IronRDP 0.13, as on upstream's master of 2026-09-29, with patches that are not upstream
yet: NLA against a credentials lookup, refreshing on Refresh Rect and Suppress Output, how late the
client plays each sound wave, and what drive redirection needs of IronRDP's RDPDR server. They are on
branch `rdpmac/nla` of an IronRDP fork, which `[patch.crates-io]` in Cargo.toml takes from
`../IronRDP`:

```sh
git clone https://github.com/SamMusk667/rdpmac
git clone --branch rdpmac/nla https://github.com/SamMusk667/IronRDP
cd rdpmac
cargo build --release
```

## Installing the app

```sh
sh scripts/sign-dev.sh setup        # once per Mac: a self-signed "rdpmac Development" code signing identity
sh scripts/build-app.sh --pkg       # gives build/rdpmac.app and build/rdpmac-VERSION.pkg
```

Between releases the version reads like 0.4.0-dev55: the version in Cargo.toml, then the number of
commits the build is made from. A build of the commit tagged v0.4.0 is plain 0.4.0. The welcome
window, `rdpmacd --version`, the status and the log all show it.

The installer puts rdpmac.app in Applications and opens it. The welcome window goes through three
steps: turn the service on, allow Screen & System Audio Recording, allow Accessibility. Then it shows
the address clients connect to and the certificate fingerprint. The menu-bar icon's menu has the
state, the current connections, settings, certificate import, restarting the service, logs and a
diagnostics bundle. The mark at the icon's lower right shows the state: a hollow circle waits for
connections, a filled circle has a client connected, two bars mean the service is off, an
exclamation mark needs attention (a missing permission, a login item waiting for approval, NLA on
with nobody enrolled), and no mark means the service is starting.

The icons come from `app/Icons` (design V2, described in its README). When packaging,
`scripts/icons.swift` makes AppIcon.icns from `app/Icons/app/app-light.png`, with the rounded square
and drop shadow of a macOS icon, and turns `menu/*.svg` into vector PDFs for the menu-bar template
images; new designs only need replacing that directory. The dark master `app-dark.png` is not used
yet; it is meant for an Icon Composer .icon.

- launchd runs the service in the login session and restarts it after a crash. An app signed with a
  Developer ID, which carries a Team ID, registers the service through SMAppService. Builds without
  a Team ID (development builds and self-built copies) use a classic LaunchAgent in
  `~/Library/LaunchAgents` instead, because macOS does not start helpers that such apps register
  through SMAppService.
- The two permissions are recorded for the `rdpmacd` inside the app. After moving the app, grant
  them again.
- The development agent from `scripts/agent.sh` has the same name and port as the app's service, so
  the two cannot coexist: run `sh scripts/agent.sh uninstall` first.
- `rdpmac.app/Contents/MacOS/rdpmac --enable-server | --disable-server | --server-status |
  --collect-diagnostics` does the same without the menu, for scripts and support.

For distribution, set `RDPMAC_SIGN_IDENTITY` (Developer ID Application) and
`RDPMAC_INSTALLER_IDENTITY` (Developer ID Installer) and run `build-app.sh --pkg`; the signatures
then carry the hardened runtime and a secure timestamp. Then run `sh scripts/notarize.sh` with
`RDPMAC_NOTARY_PROFILE` naming credentials saved by `xcrun notarytool store-credentials`, to
notarize and staple the package.

## Running the daemon directly

```sh
cargo build --release
# Static credentials, for trying things out on one Mac:
RDPMAC_LOG=info target/release/rdpmacd --listen 0.0.0.0:33389 --auth static --user test --password test
# Sign in with a Mac account (PAM service checkpw):
RDPMAC_LOG=info target/release/rdpmacd --listen 0.0.0.0:3389
# A synthetic picture that needs no permission, to test clients and encoding cost:
target/release/rdpmacd --test-pattern 1920x1080 --auth static --user test --password test
# Trigger the Screen Recording and Accessibility prompts, then exit:
target/release/rdpmacd --request-permissions
```

The picture goes as H.264 when the client supports it (`--codec auto`: VideoToolbox hardware
encoding, sent through the graphics pipeline, up to 4096x2304), and as RemoteFX otherwise. When the
client supports it, H.264 uses AVC444, with full chroma, so coloured text has no colour fringes;
sizes whose width is not a multiple of 16 use AVC420. `--codec avc420` uses AVC420 only, at about
half the encoding cost; `--codec remotefx` always uses RemoteFX (docs/avc444.md). AVC444's colour
conversion runs on several cores by default; `--parallel-conversion false` uses one. Switching
between AVC444 and AVC420, or the colour conversion, in the app's settings needs no restart and
applies from the next connection.

H.264 uses full-range BT.709, as the specification says. About 0.2 seconds after the picture stops,
the server encodes it again with a finer quantiser, so text is close to lossless; a changing picture
gets the quantiser its bitrate allows. Capture is limited by `--fps` (default 30): when the picture
changes faster, frames in between are skipped, never the last one. While mstsc is minimised the
picture pauses, with no capture and nothing sent; on restore, or when the client asks for a refresh,
the whole picture goes out. "Record the picture stream" under Debugging in the settings (or
`--h264-dump`) records the H.264 stream exactly as sent, to find out why a client showed a corrupted
picture; it is off by default (docs/refresh.md).

Text, pictures and files on the clipboard are shared both ways by default (docs/clipboard.md);
`--no-clipboard` turns that off. What the Mac plays goes to the client by default (16-bit stereo PCM
at 44.1 kHz); `--no-audio` turns it off. While the client plays it the Mac itself is muted;
`--mute-mac false` keeps it audible. The sound is sent no faster than real time, and not while the
client is minimised, so the client does not fall ever further behind (docs/audio.md).

By default the session follows the client's resolution (`--resolution follow-client`): mstsc's `/w`
and `/h`, full screen, or `desktopwidth` and `desktopheight` in an .rdp file set it at connect time,
and with dynamic resolution on, resizing the window changes it live.

- On a Mac without a screen attached, the session gets a virtual display of its own at exactly the
  pixel size the client asked for, which replaces the system's 1920x1080 placeholder display as the
  desktop: native resolution, no scaling. 30 seconds after the last session ends, the virtual
  display goes and the placeholder display comes back. `--virtual-display off` turns this off.
- The first time, macOS sets 3840x2160 to 1920x1080. rdpmacd then switches the display to 4K from a
  helper process, the same as switching it by hand in System Settings; once macOS remembers the
  display's identity, later 4K sessions are 4K straight away.
- With a screen attached, or when macOS refuses the requested size, ScreenCaptureKit scales the
  picture to the requested size, with black bars when the aspect ratios differ.
- A locked Mac can be reached and unlocked. When a session starts, rdpmacd declares user activity
  (as `caffeinate -u` does): the screen lights up, the lock screen appears, and rdpmacd types the
  password the user logged on with into it (docs/unlock.md). With `unlock = false`, or when that
  does not work, the password typed in the session unlocks it. rdpmacd also declares activity on
  remote input, at most every 2 seconds, because macOS does not count injected events as the user
  activity that starts the unlock flow: without it the lock screen still shows the password field
  but turns every password down unchecked.
- `--resolution native` goes back to the display's own pixel size.

On first run rdpmacd makes a self-signed TLS certificate in `~/Library/Application Support/rdpmac/`.
Capture needs the Screen Recording permission and injection needs Accessibility. Without them the
service still accepts connections, but the client sees no picture and input has no effect; the
startup log says which process macOS checks, and the log of each connection says when Accessibility
is missing. Permissions granted while the service runs take effect only after it restarts: the
menu-bar app says so and offers a restart button.

### Settings file and control socket

`~/Library/Application Support/rdpmac/config.toml` holds the settings, under the names of the
command-line flags (`listen`, `auth`, `security`, `pam-service`, `allow-users`, `codec`,
`parallel-conversion`, `clipboard`, `audio`, `drives`, `unlock`, `udp`, `mute-mac`, `audio-rate`,
`resolution`, `virtual-display`, `fps`, `cursor-hz`, `cert`, `key` and `h264-dump`). Values given
on the command line win, and `--config` names another file. An unknown key or a value out of range stops the
start, with the reason.

`control.sock` in the same directory is the control socket of the menu-bar app. Only the same user
can connect, and each line is one JSON request: `status`, `request_permissions`, `get_config`,
`set_config`, `import_certificate`, `nla_enroll`, `nla_remove`, `restart`. For example:

```sh
printf '%s\n' '{"cmd":"status"}' | nc -U ~/Library/Application\ Support/rdpmac/control.sock
```

Importing a certificate takes an X.509 v3 certificate and its private key (PEM). The old pair is kept
as `*.previous.pem`, and the new one takes effect when the service restarts. The SHA-1 and SHA-256
fingerprints in the status are the ones mstsc shows when it asks whether to trust the server.

### Network Level Authentication (NLA)

`--security nla` (or `security = "nla"` in the settings file, "Require Network Level Authentication"
in the app's settings) makes the client prove it knows the password before a session exists, and the
server prove it knows the account, before the client hands the password over. A server posing as
this one gets no password. Only clients that support NLA are accepted; mstsc and Windows App support
it by default. The default is still `tls`.

NTLM needs the server to hold the account's NT hash beforehand, so NLA needs enrolling first: enter
the Mac password in the app's settings, rdpmacd checks it with PAM and stores the hash in the login
keychain, where only rdpmacd can read the item. Enroll again after changing the Mac password. Once
the client is authenticated, the password it delegates is still checked with PAM, and failures in
the NTLM stage count towards the lockout too. With `--auth static`, NLA uses the static password and
needs no enrollment. Details are in docs/nla.md.

Which accounts can log on: the session is always the console session of the user rdpmacd runs as,
and by default only that user can log on. `allow-users = ["name", ...]` in the settings file, or
`--allow-user NAME` once per account, lets other accounts of the Mac log on with `tls` and take
that session over; NLA admits only enrolled accounts, and only the user rdpmacd runs as can enroll.
Any other account is turned away before its password is checked (SECURITY.md).

### Logs

Run as a launchd service, the daemon writes to the directory in `RDPMAC_LOG_DIR` (a leading `~/`
means the home directory): one `rdpmacd.YYYY-MM-DD.log` a day, kept for 14 days, crashes included.
`launchd.log` only holds the output from before logging starts. Run in a terminal, the daemon logs
to the terminal.

## Development LaunchAgent

macOS checks the two permissions against the responsible process: the terminal app when rdpmacd
starts from a terminal, sshd when it starts over SSH, and `rdpmacd` itself only when launchd starts
it. During development `target/release/rdpmacd` can run as a LaunchAgent in the login session
without packaging the app:

```sh
sh scripts/sign-dev.sh setup        # once per Mac
cargo build --release
sh scripts/agent.sh install -- --listen 0.0.0.0:3389   # sign, install and start; arguments after -- go to rdpmacd as they are
sh scripts/agent.sh permissions     # make macOS ask for Screen Recording and Accessibility for rdpmacd
# Turn rdpmacd on under System Settings > Privacy & Security, in Screen & System Audio Recording and in Accessibility, then:
sh scripts/agent.sh restart
sh scripts/agent.sh status          # state, arguments, signature, recent log; logs -f follows the log
```

- The binary goes to `~/Library/Application Support/rdpmac/bin/rdpmacd` and the logs to
  `~/Library/Logs/rdpmac/`. The launchd job is `~/Library/LaunchAgents/com.rdpmac.rdpmacd.plist`:
  the process restarts after a crash and starts at login.
- Every install signs with the same certificate, and the designated requirement is the identifier
  `com.rdpmac.rdpmacd` plus that certificate. After a rebuild, run `install` again to update; without
  arguments it keeps the last ones, and the permissions stay.
- The agent runs only once a user has logged in at the Mac's screen. On the first install macOS may
  say a background item was added; keep it allowed under System Settings > General > Login Items &
  Extensions.
- `stop` stops it until the next login; `uninstall` removes the agent and keeps the logs and the TLS
  certificate.
- The signing identity lives in a keychain of its own, whose password is in a file next to it that
  only your account can read, so signing works over SSH too; the keychain is on the search list only
  while codesign runs. Set `RDPMAC_SIGN_IDENTITY` to sign with another certificate, such as an Apple
  Development one.

## Status

Progress by milestone, measured numbers and what is still to verify are in docs/milestones.md. The
work of M1 to M3 is done: capture, input and H.264, the virtual display, the menu-bar app, installing
the service, permission guidance, certificate import, settings, logs and diagnostics, signing and the
installer. From M4, NLA with the credential store, AVC444, sound, and pictures and files on the
clipboard are done; Kerberos waits for an Active Directory domain to test with. M5 is the open-source
release (docs/adr/0002-fully-open-source.md). Drive redirection works with FreeRDP and is still to be tested
with mstsc and Windows App (docs/adr/0003-drive-redirection-through-a-local-nfs-mount.md).

Known limitations of the released IronRDP 0.13.0: cursor shapes larger than 96 pixels are not sent
(large pointer updates exist only on IronRDP's master branch), horizontal wheel events have no
variant, and mouse button events carry no position, so the last move counts.

## Documentation

| Document | What it covers |
|---|---|
| [ADR-0001](docs/adr/0001-macos-rdp-server-on-libscreenio-and-ironrdp.md) | The architecture: IronRDP and libscreenio, encoding, authentication, processes, milestones |
| [ADR-0002](docs/adr/0002-fully-open-source.md) | Fully open source under MIT OR Apache-2.0 |
| [ADR-0003](docs/adr/0003-drive-redirection-through-a-local-nfs-mount.md) | Drive redirection through a local NFS mount |
| [Milestones](docs/milestones.md) | Progress, measurements, what is left |
| [AVC444](docs/avc444.md) | Full-colour H.264 with VideoToolbox |
| [Refresh](docs/refresh.md) | Pausing, refreshing and recording the picture |
| [Sound](docs/audio.md) | Capturing, pacing and muting |
| [Clipboard](docs/clipboard.md) | Text, pictures and files |
| [Drives](docs/drives.md) | The client's drives in `~/RDP Drives` |
| [NLA](docs/nla.md) | Network Level Authentication and the credential store |
| [libscreenio](docs/libscreenio.md) | The capture and injection library and its C ABI |

## Contributing

See CONTRIBUTING.md. Report security problems as SECURITY.md describes, not in public issues.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.

## Acknowledgements

- [IronRDP](https://github.com/Devolutions/IronRDP), by Devolutions, provides the protocol.
- [macrdp](https://github.com/clintcan/macrdp) documented client behaviour that rdpmac relies on,
  such as mstsc playing 48 kHz sound slower than real time and Explorer's folder copies. Drive
  redirection is adapted from its server side of RDPDR and its NFS bridge (MIT OR Apache-2.0).
- [nfsserve](https://github.com/huggingface/nfsserve) (BSD-3-Clause) serves the redirected drives.
- [FreeRDP](https://github.com/FreeRDP/FreeRDP) served as reference implementation and test client.
