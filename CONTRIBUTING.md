# Contributing to rdpmac

Bug reports, measurements from other Macs and clients, and patches are welcome.

## Reporting a problem

Say which rdpmac version (the welcome window and `rdpmacd --version` show it, such as
0.4.0-dev55), macOS version and Mac, and which client (mstsc, Windows App on which
platform, FreeRDP, IronRDP) and client version you used. Attach the daemon log for the time of the
problem (`~/Library/Logs/rdpmac/rdpmacd.YYYY-MM-DD.log`, or the diagnostics bundle from the
menu-bar app). For a wrong or corrupted picture, a recording made with `h264-dump` shows whether the
stream or the client is at fault (docs/refresh.md).

Security problems go through SECURITY.md, not the issue tracker.

## Building and testing

Build as the README describes: the workspace needs the IronRDP fork, branch `rdpmac/nla`, checked
out at `../IronRDP`. Before sending a change, run

```sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd app && swift build)
sh scripts/screenio-header.sh --verify   # when the C ABI in crates/screenio changes; needs cbindgen
```

Tests that change the Mac, such as creating a real virtual display or writing to the login
keychain, are ignored by default. Run them with `cargo test -p <crate> -- --ignored` on a Mac where
that is acceptable; each says what it needs.

Capture and input need the Screen Recording and Accessibility permissions, which macOS grants to the
process launchd starts, not to a terminal. `scripts/sign-dev.sh` and `scripts/agent.sh` sign the
daemon with a stable identity and run it as a LaunchAgent, so the permissions survive rebuilds
(README, "Development LaunchAgent"). `--test-pattern` serves a synthetic picture and needs no
permission; FreeRDP against it covers most protocol work on one Mac.

## Changes

- Match the code around the change: its naming, its comments (they say why, in British spelling)
  and its hand formatting to about 120 columns. rustfmt is not enforced.
- A change of behaviour updates the matching document in docs/, with measurements and the date they
  were taken.
- Commit messages: a short summary line in the style of `git log` ("Copy files through the
  clipboard"), then what changed and why.
- The C ABI in `crates/screenio` only grows within 1.x: see "C ABI" in docs/libscreenio.md.
- No code from rustdesk (AGPL-3.0) or under any licence incompatible with MIT OR Apache-2.0
  (ADR-0002).

## License

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
rdpmac by you, as defined in the Apache-2.0 license, shall be dual licensed under MIT OR Apache-2.0,
without any additional terms or conditions.
