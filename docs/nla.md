# NLA (M4 item 1)

Under ADR-0001 D4, NLA supports two modes: a per-user RDP credential store for standalone Macs,
and Kerberos with a keytab for Macs joined to a domain. The credential store mode was implemented
and passed local tests on 2026-09-24, and was verified with mstsc on 2026-09-25. Kerberos is the
second step and waits for an AD domain to test with.

## Verification before changing IronRDP

IronRDP 0.13 has server-side NLA: `RdpServerBuilder::with_hybrid(acceptor, pub_key)` makes the
negotiation accept only HYBRID and HYBRID_EX, and CredSSP (sspi's `CredSspServer`) runs after TLS.
With rdpmacd presetting the credentials of the static account and calling `with_hybrid` with
`TlsIdentityCtx::pub_key`, sdl-freerdp `/sec:nla` over loopback gave:

| Case | Result |
|---|---|
| Correct password | CredSSP completes and the session is established; H.264 at 26.7 fps, no dropped frames |
| Wrong password | Rejected during CredSSP: server `LogonDenied: no candidate credential matched`, client `ERRCONNECT_AUTHENTICATION_FAILED` |
| Client that supports only TLS | Rejected during negotiation: `server requires SecurityProtocol(HYBRID \| HYBRID_EX)` |

There are three gaps, and upstream master still has all three:

1. **Only one account.** The acceptor's `CredentialsProxyImpl` holds a single `AuthIdentity`, which
   comes from `RdpServer::set_credentials`. NTLM has to complete before the client delegates the
   password, so the server must know the key of every account in advance.
2. **No way to pass Kerberos in.** `CredsspSequence::init` accepts a `KerberosServerConfig`, but
   ironrdp-server hard-codes `None` when it calls `accept_credssp`.
3. **The delegated password is discarded.** When CredSSP ends, sspi returns the credentials the
   client delegated (`ServerState::Finished(identity)`), but the acceptor does not hand them to the
   caller, so the server cannot also check with PAM whether the account is valid now.

## Implementation

### IronRDP patch

The patch is on branch `rdpmac/nla` of the IronRDP fork. It starts from 11a0810, the commit the
ironrdp 0.13 crates on crates.io were published from, whose source is identical to what was
published. rdpmac's `Cargo.toml` points all 17 ironrdp crates at a checkout of the fork in
`../IronRDP` with `[patch.crates-io]`: if only acceptor and server pointed there, the sibling
crates they depend on by path would exist twice, once from the checkout and once from crates.io,
and the types would not match. The checkout locks sspi to 0.21.3, the same as rdpmac and upstream
master. The patch only adds interfaces. Gaps 1 and 3 are closed; gap 2 (Kerberos) is left for the
second step:

- ironrdp-acceptor: `CredsspSequence::init_with_lookup` and `accept_credssp_with_lookup` take an
  sspi `CredentialsProxy`, which returns the password or the NT hash (`$NTLM$:` followed by hex)
  for the username the client gives; the original single-account entry points are unchanged. When
  CredSSP ends, the credentials the client delegated go into `AcceptorResult::credentials`, shaped
  as in ClientInfo: a UPN is the whole username, and `DOMAIN\user` is split into username and
  domain.
- ironrdp-server: `RdpServer::set_credentials_lookup`; a Hybrid connection uses the lookup when one
  is set. The existing `CredentialValidator` now also validates the delegated credentials, and its
  documentation is rewritten to match; `pub use sspi` gives callers these types.
- Tests: three new end-to-end tests in ironrdp-testsuite-extra check that an enrolled account
  connects and the validator receives the delegated password, that a wrong password is rejected,
  and that an unknown account is rejected. These three and the existing 17 all pass, and the
  changed crates have no new clippy warnings.

The acceptor's CredSSP code on upstream master has not changed since 11a0810, so this part of the
patch carries over as it is; server.rs has changed a lot and needs reworking against master.
Upstream pull requests are still to be opened.

### Credential store

- The store keeps the NT hash (the MD4 of the password in UTF-16LE, 16 bytes), not the password.
  The hash is a generic password in the login keychain, with service `com.rdpmac.nla`, the macOS
  short user name as the account, and "rdpmac network level authentication" as the name shown.
- The keychain lets only the program that last wrote an item read it without a prompt, so other
  programs of the same user cannot get the hash. rdpmacd turns off keychain dialogs: when it cannot
  read an item it fails at once and logs it, instead of putting up a dialog on an unattended Mac and
  waiting for someone.
- Without an Apple Team ID in the signature, every build counts as another program, even when
  signed with the same self-signed certificate. Measured on the login keychain on 2026-09-25 with
  two builds (same designated requirement, different cdhash):

  | What the new build does to the old build's item | Result |
  |---|---|
  | Read | errSecAuthFailed (-25293) |
  | Overwrite | Succeeds; afterwards the new build can read it and the old build cannot |
  | Delete | errSecInvalidOwnerEdit (-25244); only the build that created the item can delete it |

  So after an update an enrolled account has to be enrolled again once, and NLA logons fail until
  it is. The earlier statement that "a program re-signed with the same certificate reads as before"
  came from a test in a temporary keychain made with `security create-keychain`. Temporary
  keychains do not make this check, so the conclusion does not hold for the login keychain.
- How rdpmacd handles this:
  - When it cannot read the hash, the startup log names the account, the `nla` field of `status`
    reports `stale: true` and `enrolled: false`, and the app's menu and settings ask to enroll
    again after the update.
  - Enrolling again overwrites the old item; there is no need to delete it in Keychain Access
    first.
  - When removing an enrollment whose item an earlier build created and this build cannot delete,
    rdpmacd overwrites the hash with the marker `removed`, which reads as no enrollment. The item
    itself stays in Keychain Access and can be deleted there.
- Once builds are signed with a Developer ID, the keychain recognises the program by its team, and
  updates no longer need enrolling again. The one update that switches the signing identity still
  needs it.
- An account is enrolled by entering the Mac password in the app's settings, or with the control
  command `{"cmd":"nla_enroll","password":"…"}`. rdpmacd first checks the password with the
  configured PAM service (an empty password is never sent to be checked), then computes the hash
  and stores it in the keychain. Only the user rdpmacd belongs to is enrolled: it is that user's
  console session rdpmacd serves.
- `nla_remove` removes the enrollment. The `nla` field of `status` reports the user, whether it is
  enrolled, when it was enrolled (the modification time of the keychain item), and `stale`: an
  earlier build wrote the item, this build cannot read it, and the account needs enrolling again.
- After the Mac password changes, the old hash still passes NTLM, but the delegated old password
  fails PAM and the connection is rejected; the account then has to be enrolled again.

### Connection flow

1. X.224 negotiation accepts only HYBRID and HYBRID_EX; TLS is set up after it.
2. CredSSP: `NlaLookup` first asks the lockout whether the account may try, then fetches the NT
   hash from the keychain by username (case-insensitive).
3. sspi completes NTLMv2 authentication and the public key binding; the client then delegates the
   password.
4. The validator (lockout plus PAM) checks the delegated password; once it passes, the session
   starts.

An account that is not enrolled fails at step 2, and the log says why.

### Lockout

When NTLM authentication fails, the validator is never called, so `NlaLookup` records an attempt
with the lockout before NTLM, counting it as a failure up front. The count is cleared once the
validator accepts the delegated password; a rejection is not counted a second time. The lockout
counts by username with the domain removed and in lower case, so changing the case gets no extra
attempts; TLS mode now does the same. The rules are the same as in TLS mode: 5 failures within 5
minutes lock the account for 5 minutes.

### Settings

`security = "tls" | "nla"`, set with `--security`, config.toml or the app's settings; the default
is `tls`. When `nla` is chosen but no account is enrolled, the startup log warns and the menu shows
an item for enrolling. With `--auth static`, NLA uses the hash of the static password directly and
needs no enrollment, which makes development and integration testing easier.

## Tests (2026-09-24)

The server for the local loopback tests was `rdpmacd --auth static --security nla --test-pattern`,
with its own data directory and port. The client was `sfreerdp`, the headless sample client of
FreeRDP 3, with no window and no clipboard:

| Case | Result |
|---|---|
| Correct password | CredSSP passes, the validator accepts the delegated password (`Credential validation accepted`), and the H.264 session lasts until the client exits |
| Wrong password | `LogonDenied: no candidate credential matched`, client `ERRCONNECT_AUTHENTICATION_FAILED` |
| Account not enrolled | As above; the log says `not enrolled` |
| Client that supports only TLS | Rejected during negotiation |
| 5 failures in a row | The account is locked for 300 seconds; while it is, the correct password is rejected too (also with the username in upper case) |

Also:

- rdpmac-auth's keychain test writes to the login keychain and is ignored by default. Run as a
  one-off LaunchAgent in a graphical session, it passes enrolling, case-insensitive reading, listing
  and removing. Over SSH or in a background session the login keychain is locked, and writing
  fails.
- Control commands in PAM mode: the `nla` field of `status`, `nla_remove` and the rejection of an
  empty password are verified. Enrolling with the correct password needs the real password and was
  left to be done in the app.

### After an update (2026-09-25)

After the AVC444 test build was installed, mstsc could not connect (0x904, extended error code
0x7): the new build could not read the hash 0.3.1 had written, CredSSP failed, and mstsc's retry
without NLA was rejected too. Verification after the fix:

- Two builds (same designated requirement, different contents) went through an update under a
  test service in the login keychain: the new build first read `Unreadable`, and the hash lookup
  reported "another build of rdpmacd stored the hash"; after enrolling again it could read, while
  the old build no longer could; removing wrote the marker, which then read as no enrollment, and
  enrolling again worked again; finally the build that created the item deleted it.
- A server started from the new build with its own data directory and port only read the real
  item: `status` reported `enrolled: false, stale: true`, and the startup log named the account
  that needs enrolling again. No enroll or remove command was sent.

## Known limitations

- With self-signed builds, an account has to be enrolled again once after every update; see
  "Credential store". The app asks for it.
- HYBRID_EX's Early User Authorization Result reports success as soon as NTLM passes. If PAM then
  rejects, the client gets a ServerDeniedConnection disconnect rather than "wrong password". This
  happens only when the password changed after enrolling, or when the account is disabled.
- ironrdp-server handles one connection at a time, and the keychain is read synchronously on the
  connection's runtime thread, which takes a few milliseconds.

## Next steps

1. Done: after enrolling in the app and turning NLA on, mstsc connects normally (2026-09-25). What
   happens after a password change has not been tried yet.
2. Kerberos: find out where the machine account's keys come from on a Mac bound to AD and how to
   produce a keytab, pass them in through `KerberosServerConfig`, and check the delegated
   credentials with PAM as well. Needs an AD domain.
3. Upstream pull requests: rework the patch against master and open them.
4. Developer ID signing: updates would no longer need enrolling again, and notarization needs it
   too. It needs an Apple Developer account.
