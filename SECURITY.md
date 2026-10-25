# Security

## Reporting a vulnerability

Report it privately through GitHub:
[Report a vulnerability](https://github.com/THEROER/cursio/security/advisories/new).
Please do not open a public issue for it. Say which build (the version
`cursio update status` shows, or the commit), which platforms, and what an attacker needs — a position on the
LAN, a paired machine, a local account.

Cursio is maintained by a small team in spare time. Expect an acknowledgement
within a week; a fix is released before the details are published.

## What Cursio protects, and what it does not

Knowing the model tells you whether something is a vulnerability or the design.

**A paired machine is fully trusted.** Pairing is the trust decision, and after
it a peer can, with the matching modules on: drive this machine's mouse and
keyboard, read and change every disk it shares (`--files` mounts read-write
unless a root is shared `:ro`), pull its windows and monitors for a screen-
sharing app (`--remote-screen`, refused with `--no-screen-host`), use its USB
devices, read and send a paired phone's SMS, and — if this machine ran
`cursio update trust-builder` for it — install a build here. Pair only machines
you would hand your unlocked laptop to. `cursio forget` removes one together
with its key.

**An unpaired machine on the LAN gets nothing.** Every link is Noise `NNpsk0`
keyed by the pairing secret; without it a connection ends at the handshake.
Pairing itself is SPAKE2 with an explicit key-confirmation step, so the password
never crosses the wire and a wrong one fails instead of producing a link. A
machine neither side has met can be called only during the ten-minute window
`--pair` opens. Anything that breaks one of these is a vulnerability.

**Requests from a peer are untrusted input** even though the peer is trusted:
a file path, a message page size or a window id is a string off a socket. A
crash, a path escaping a shared root, or memory growth a peer can trigger at
will is a vulnerability.

**Outside the model:**

- *The local network.* mDNS announces that a Cursio node is here, under which
  name and port. Cursio is built for a LAN you control; it does not try to hide
  from it and does not traverse NAT.
- *AirPlay.* The receiver accepts mirroring from any Apple device on the LAN,
  as an Apple TV does; it is outside the mesh and grants that device nothing
  but a picture on the screen.
- *The local machine.* Anyone with your account can read the trust store under
  the platform config directory, and with it impersonate this machine to its
  peers.
