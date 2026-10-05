# UDP

With `udp = true` in config.toml, `--udp`, or "Offer UDP" in the app's settings, rdpmacd offers
clients RDP-UDP on its listening port. A client that takes it up opens a UDP tunnel next to the
TCP connection, and the picture moves onto it. Everything else stays on TCP: input, sound,
clipboard and drives. The setting is off by default until measurements show that UDP helps.

State on 2026-10-05: wired up and tested with FreeRDP, which declines UDP; the session continues
over TCP. Not yet tested with mstsc.

## Where it comes from

Upstream IronRDP added reliable UDP for servers in #1951, #1953 and #1954, merged between
2026-09-28 and 2026-09-30. These provide:

- RDP-UDP (MS-RDPEUDP and MS-RDPEUDP2) with its own congestion control and round-trip time;
- TLS over the reliable stream, with the same certificate as TCP;
- the tunnel of MS-RDPEMT, bound by a 16-byte cookie sent over TCP;
- Soft-Sync (MS-RDPEDYC), which moves the graphics pipeline's channel onto the tunnel.

The fork takes them in on branch `rdpmac/udp`, merged from upstream master 38b074e4. rdpmac only
calls `RdpServerBuilder::with_udp_transport` with the listening address.

rdpmac did not build its own. macrdp, which did, needed about 6,000 lines and BoringSSL for DTLS,
kept fixing it for weeks afterwards, and measured no gain for the picture over reliable UDP
compared with TCP. Its one measured gain was sound sent twice over lossy UDP, which needs DTLS and
which upstream does not have. Upstream's RDP-UDP has congestion control and round-trip times,
which macrdp's lacked, so rdpmac needs measurements of its own.

## Clients

| Client | UDP |
|---|---|
| mstsc on Windows | Takes it up (upstream's tests; macrdp's) |
| FreeRDP | Declines (`multitransport_no_udp` answers `E_ABORT`); the session stays on TCP. Checked on 2026-10-05 |
| Windows App on macOS | Expected to stay on TCP: macrdp found that it uses UDP only for Azure Virtual Desktop and Windows 365 |
| Windows App on iOS and Android | Not tested |

## How to tell

Clients that decline UDP log nothing at info level; with `RDPMAC_LOG=info,ironrdp_server=debug`
the log says `Client could not establish the UDP multitransport connection, continuing TCP-only`.
A client that takes UDP up makes IronRDP log `Sideband UDP transport established`. rdpmacd shows
that line by default. Failures are logged as warnings. In mstsc, the connection bar's signal icon
opens the connection information, which says whether UDP is in use.

To compare TCP and UDP on a poor link, add delay and loss with Network Link Conditioner on the Mac
(Additional Tools for Xcode). Run the same session with UDP on and off.

## Limits

- One UDP socket for each connection, bound to the listening address and port. The first datagram
  to arrive takes it. Someone on the network who sends first pushes the client back to TCP; TLS and
  the cookie keep them out of the session.
- At most 64 datagrams of 1,232 bytes in flight. That caps the picture at about 31 Mbit/s with a
  20 ms round trip and 12 Mbit/s with 50 ms. The window is a constant in IronRDP's connection
  settings, which `with_udp_transport` does not expose yet.
- If the tunnel dies after the picture moved onto it, the connection ends and the client
  reconnects.
- No lossy mode, forward error correction or DTLS: only the reliable mode exists.
- A network firewall must let UDP through on the listening port; the Mac's own firewall allows
  rdpmacd by application, for both protocols.
