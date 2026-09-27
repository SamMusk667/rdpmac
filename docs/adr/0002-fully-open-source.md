# ADR-0002: Fully open source, under MIT OR Apache-2.0

| | |
|---|---|
| Status | Accepted (2026-09-27) |
| Supersedes | ADR-0001 D6, and the Pro and commercial parts of ADR-0001 sections 1, 2 (D2, D8), 4 (M5), 10 and 11 |

## Context

ADR-0001 planned two editions, the model RustDesk uses: a free edition under AGPL-3.0, and a Pro
edition under a commercial licence, kept in a closed repository and linked in as a crate. Pro was to
carry licensing and activation, an update channel, multi-monitor and the enterprise features. A
contributor licence agreement was to keep the right to license contributions both ways.

A review of the market and of the other macOS RDP servers (2026-09-26 and 27) found that a paid
edition is unlikely to pay for itself:

- Demand for an RDP server on macOS is real but small. The best documented commercial precedent,
  iRAPP, reported licence revenue of about $174,000 to $223,000 a year in 2014 and 2015 and went
  bankrupt in 2016. NuoRDS has sold one for about ten years at $49 per server.
- The field filled up in 2026: at least 17 open-source macOS RDP servers besides rdpmac, the most
  complete being clintcan/macrdp (MIT OR Apache-2.0), and new commercial ones such as ProRDP and
  FornaX RDS AI. A working server is no longer scarce.
- A commercial edition needs more than code: Developer ID signing, sales and support, and a
  settlement of RDP patent licensing with Microsoft, whose pledge to open-source developers does not
  cover commercial distribution.

## Decision

1. rdpmac is free and open source, all of it. There is no Pro or commercial edition and no closed
   repository.
2. The licence is MIT OR Apache-2.0, at the user's option: the terms of IronRDP and of most of the
   Rust ecosystem. It covers libscreenio too. Code can move both ways between rdpmac and the
   permissively licensed projects around it, IronRDP and macrdp among them, and patches go upstream
   without relicensing.
3. Contributions come in under the same terms (CONTRIBUTING.md). There is no contributor licence
   agreement.
4. libscreenio moves into this repository, with its history, as `crates/screenio-core` and
   `crates/screenio`. It keeps its C ABI, the ABI's compatibility rules and its own version
   (docs/libscreenio.md).
5. The rule of ADR-0001 D6 stays: no rustdesk code (AGPL-3.0) enters this repository, nor any other
   code under a licence incompatible with MIT OR Apache-2.0.
6. The documentation is in English.

Alternatives were keeping AGPL-3.0 and using Apache-2.0 alone. AGPL-3.0 keeps derived versions open,
but it was chosen to protect a commercial edition that no longer exists; it would also stop code
from moving to the permissive projects around rdpmac, and some organisations do not allow AGPL
software. Apache-2.0 alone would differ from IronRDP's terms for no gain.

## Consequences

- All code so far has one author, so the change needed no one else's consent.
- Several virtual displays for mstsc's multi-monitor sessions become an ordinary roadmap item. They
  still wait for IronRDP's display control channel to accept more than one monitor.
- M5 "Pro and commercialisation" becomes M5 "Open-source release"; its items are listed in
  docs/milestones.md. M4's enterprise items (Kerberos, MDM managed preferences, audit records and a
  session recording interface, login-window research) stay in the plan but come after what
  individual users and Macs without a screen need.
- Releases still need a Developer ID and notarization. Without them Gatekeeper blocks a downloaded
  installer, the service is registered as a classic LaunchAgent instead of through SMAppService, and
  NLA has to be enrolled again after every update (docs/nla.md). The Apple Developer Program fee is
  the project's one running cost.
