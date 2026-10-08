# Reconnecting after a dropped connection

When a client's network drops (Wi-Fi switched, laptop asleep, a VPN reconnecting), mstsc and
FreeRDP reconnect by themselves, without asking for the password again. They come back to the
same session, as they do with a Windows PC.

State on 2026-10-08: tested with FreeRDP on the same Mac; not yet tested with mstsc over a real
drop.

## How it works

- **The cookie.** After every logon rdpmacd hands the client an auto-reconnect cookie (MS-RDPBCGR
  2.2.4.2): 16 random bytes, the first from `/dev/urandom`, and a fresh random from IronRDP's CSPRNG
  for every client after that, plus an update every hour. A reconnecting client proves it holds
  the cookie with an HMAC over it, so the cookie itself never crosses the network again. It gets in
  without the password check. The status and the app's menu show such a connection as reconnected,
  with the user of the connection before it.
- **Heartbeats.** rdpmacd sends a Server Heartbeat PDU every 5 seconds while nothing else goes out.
  A client that asked for heartbeats warns after about 15 seconds of silence and reconnects after
  about 40, instead of waiting for TCP to give up.
- **The dead connection.** rdpmacd serves one connection at a time, and a reconnecting client waits
  until the old one is gone. A client whose network went away never closes its connection, and
  macOS would keep it for minutes, or hours when it is idle. So each connection gets TCP keepalive
  (15 seconds idle, then 3 probes 5 seconds apart) and `TCP_RXT_CONNDROPTIME` of 30 seconds: once
  the client has acknowledged nothing for 30 seconds, the connection is dropped. That comes before
  mstsc's own reconnection at about 40 seconds. The option is a patch in the IronRDP fork,
  `RdpServerBuilder::with_dead_peer_timeout`.

A second client still waits for the session to end, as before. IronRDP could let a new client
evict the old one (`ConnectionPolicy::Preempt`), but under TLS, rdpmac's default, any peer that
can reach the port could then evict the user before proving anything. That is why the dead
connection is dropped instead.

## Security

The cookie stands in for the password, for the session it was issued in. The client keeps it in
memory only, while it tries to reconnect. A forged or altered cookie is turned away: the
connection ends with `cookie rejected`, as the test below shows. A cookie stays valid until two
newer ones have been issued; every logon and every hourly update issues one, so that a cookie lost
on its way to the client does not end reconnection. The client before the current one can
therefore still come back once the current one has left. A reconnection skips PAM and the lockout
count, as on Windows.

A reconnection carries no password, so it types nothing into the lock screen (docs/unlock.md).
If the Mac locked while the client was away, the user types the password in the session.

## Testing

FreeRDP prints the cookie it receives with `+print-reconnect-cookie` (with `WLOG_LEVEL=INFO`) and
presents one with `/reconnect-cookie:<base64>`. Against `rdpmacd --test-pattern` with static
credentials, on 2026-10-08:

| Case | Result |
|---|---|
| A client logs on with the password | The cookie arrives: version 1, logon ID the user ID, 16 random bytes |
| A second client presents that cookie with a wrong password | Let in: `Auto-reconnect cookie validation accepted`, `reconnected: true`, user as before, and a fresh cookie sent |
| A client presents the cookie with one byte changed and a wrong password | Turned away: `cookie rejected`, no session |

`cargo test -p ironrdp-server dead_peer` in the fork reads the socket options back from an
accepted connection. To test a real drop, connect mstsc over Wi-Fi and switch the Wi-Fi off for a
minute, then on: mstsc shows "Reconnecting" and comes back.
