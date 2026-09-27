# Security

Report security problems privately, through GitHub's private vulnerability reporting on this
repository (Security, then "Report a vulnerability"), not in a public issue. Include the rdpmac and
macOS versions, the client, and the steps that show the problem.

Keep rdpmac on networks you trust. Like any RDP server, it should not be reachable from the internet
directly; reach it through a VPN or an RD Gateway.

How rdpmac authenticates, as of 0.4.x:

- The session is always the console session of the user rdpmacd runs as. Only that user may log
  on, and the accounts listed in `allow-users`, which then take the session over. A logon by any
  other account, a directory account on a Mac bound to a directory included, is turned away before
  its password is checked, so rdpmac cannot be used to try the passwords of other accounts.
- With `security = "tls"` (the default), the client sends a user name and password inside TLS once
  the session is set up, and rdpmacd checks them with PAM.
- With `security = "nla"`, the client proves it knows the password before a session exists, against
  an NT hash kept in the login keychain, and only the account rdpmacd runs as can be enrolled. The
  delegated password is checked with PAM again (docs/nla.md).
- Five failed logons within five minutes lock the account out for five minutes.
- The TLS certificate is self-signed unless one is imported; clients should check its fingerprint,
  which the menu-bar app shows.
