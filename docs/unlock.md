# Unlocking the lock screen at logon

A Windows PC whose console is locked unlocks when its user logs on over RDP. rdpmac does the same
on a Mac: when the Mac is locked, the password the user logged on with is typed into the lock
screen as the session starts. Before this, the lock screen came up in the session and the user
typed the password a second time.

State on 2026-10-03: implemented, with unit tests for every rule below. The lock-state checks were
read on a locked Mac (macOS 27). It has not yet been tested end to end with mstsc.

## What happens

1. **Logon.** IronRDP hands the client's credentials to the validator: with TLS, those of the
   Client Info PDU; with NLA, the password the client delegates after CredSSP (docs/nla.md). PAM
   checks the password against the account as it is now. When it passes and the account is the user
   rdpmacd runs as, `Unlocker` keeps the password. It keeps it in a buffer that is wiped when
   dropped, for 60 seconds at most, and drops it when the connection ends.
2. **The session starts.** When IronRDP asks for the session's picture, the display has already
   woken for the session. The password then goes to the input thread, ahead of any key the client
   sends after it, and nothing else holds it.
3. **The input thread** (`unlock::attempt`):
   - reads the lock state; when the screen is not locked, which is most connections, nothing else
     happens;
   - finds, for each character, the key of the Mac's current keyboard layout and the Shift and
     Option that type it; if any character has none, nothing is typed;
   - declares the user active, since the lock screen checks a password only for an active user,
     and waits up to 5 seconds for loginwindow to give the password field the keyboard;
   - releases the keys the client holds and turns Caps Lock off;
   - presses Shift, then Backspace once per character plus four, then the password's keys 25 ms
     apart, then Return;
   - waits up to 3 seconds for the screen to unlock, and presses Return once more if it has not;
   - turns Caps Lock back on if it was on.
4. **Keys typed meanwhile.** The keys the client typed while this went on are dropped. So are the
   keys after it, until typing pauses for a second, and for 10 seconds at most. A user who sees the
   lock screen may be typing the password too, and once the screen unlocks the rest of it, Return
   included, would land in an app.

The log says how each attempt ended: `typed the password of the user who logged on into the lock
screen: unlocked`, or why it typed nothing or stopped.

## Safety

- Only the user's own password goes in, just checked by PAM. A logon by another account in
  `allow-users` keeps its password away from this user's lock screen. Static credentials
  (`--auth static`, for development) are not the Mac's password, so they are never typed.
- Before every key, rdpmac checks three things: the screen is still locked, it is still the same
  lock, and loginwindow still holds secure keyboard entry, which the password field turns on while
  it has the keyboard. When any of them changes, for example because someone at the Mac unlocked
  it, typing stops and no Return follows. A key meant for the lock screen must never land in an app.
- A lock gets the password submitted twice at most. macOS delays password checks from the third
  wrong password on, and a lock that has turned the password down twice gets nothing more from
  rdpmac. The count starts over when the screen is seen unlocked, or when a new lock begins:
  `CGSSessionScreenLockedTime` changes with every lock.
- The password is never logged; it prints as `Password(..)`.
- Whoever logs on with the user's password unlocks the Mac, just as they could by typing the
  password in the session. On a Mac with a screen attached, its screen then shows the desktop, as
  it does in that case too.
- `unlock = false` in config.toml, `--no-unlock`, or the switch in the app's settings turns this
  off.

## Findings it rests on

macrdp (MIT OR Apache-2.0) built the same feature and debugged it live on macOS 26. Its
`docs/known-quirks.md` records these findings, which rdpmac reuses:

| Finding | What rdpmac does |
|---|---|
| Text posted as one string event shows in the password field, but the field then ignores Return, even from the Mac's own keyboard | Each character is typed with its own key, from the layout through `UCKeyTranslate` |
| The lock screen takes the first key after it comes up to focus the field, so the first character went missing | A bare Shift first: it types nothing and submits nothing |
| Return as that first key submitted an empty password | Shift, not Return |
| Equal characters typed back to back come out as one | 25 ms between keys |
| A single Return is sometimes ignored | A second Return when the screen is still locked after 3 seconds; 400 ms was too short to see a real unlock |
| macOS throttles from the third wrong password | Two submissions per lock |

rdpmac's own findings, on macOS 27:

- While the lock screen's password field has the keyboard, `kCGSSessionSecureInputPID` in the
  session dictionary is loginwindow's process; this is the readiness check before every key.
- `CGSSessionScreenLockedTime` runs about an hour ahead of the clock, so it serves only to tell one
  lock from the next.
- Text Input Sources (`TISCopyCurrentKeyboardLayoutInputSource`) aborts the process when two threads
  call it at once, which a Swift program reproduced; one thread at a time, any thread, works.
  libscreenio takes a lock around it.
- The Mac's layout matters: this Mac uses the Canadian layout, not US.
- The lock screen turns every password down unless the user was declared active (README), which
  the attempt does before typing.

## Not covered

- The login window before anyone logs in, and FileVault's: rdpmacd runs in the user's session and
  is not running then.
- A logon by auto-reconnect cookie does not go through the validator, so there is no password to
  type.
- A lock during a session: the user types the password.
- Characters that need a dead key or an input method on the Mac's layout, and accounts that log in
  only with a smart card.

## Testing

```sh
cargo test -p rdpmac-session unlock            # the attempt against a scripted Mac: order of keys, checks, budget, Caps Lock
cargo test -p rdpmac-session input             # dropping the keys typed meanwhile
cargo test -p screenio-core layout             # the current layout types letters, digits and space, and each key types its character
cargo test -p screenio-core -- --ignored reads_the_lock --nocapture   # prints the lock state; lock the Mac first to see it locked
```

End to end: lock the Mac, connect with mstsc, and the desktop appears without typing the password.
The log has `typed the password of the user who logged on into the lock screen: unlocked`.
