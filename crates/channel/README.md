# cursio-channel

Several kinds of traffic over one link between two parties: the urgent kind
never waits behind the bulky kind, and the link survives its connection
dropping, moving and coming back.

You declare classes (highest priority first, each with a byte budget) and the
lanes they ride; implement `Envelope` for your message type; open each
connection with a `Trust` over a `Carrier`; and run a `ResumableLink` over the
paths. Reliable messages are numbered, acknowledged and resent on a connection
that joins the session, exactly once; fresh ones are never replayed.

Two trusts ship here:

- `NoisePsk` — Noise `NNpsk0` keyed by a secret both ends hold (Cursio's).
- `CarrierAuthenticated` — for a carrier whose TLS verified its peer: your
  `Verifier` turns the certificate chain and a token sent in the opening
  exchange into a typed `Verified` identity, and keeps answering whether it
  still stands. It refuses plain TCP and `ws://` outright.

Carriers: TCP, an in-memory pair for tests, and with the default features
WebSocket (`ws`, `ws://` and `wss://`, one binary message per frame) and TLS
over TCP (`tls`).

A session bound to an identity (`SessionOptions::bound_to`) admits a joining
connection only as the same identity, re-checks it on a timer and before each
delivered message, ends at the earliest `not_after` unless the peer proves
itself again (`ResumeHandle::reauthenticate`), and can hold the peer to an
`Authorization` policy over lanes and classes. With `SessionOptions::hand_back`
a session that ends hands the application every reliable message the peer
never acknowledged; a reliable message past its `Envelope::expires_at` is
reported expired rather than delivered late. Both arrive as `SessionEvent`s.

## The panel example

A server panel and its user over WSS in one process, with certificates made
on the spot: a console lane and a bulk lane, a 32 MiB upload, every
connection killed halfway and resumed.

```sh
cargo run -p cursio-channel --example panel
# with the session's own account of its paths:
RUST_LOG=cursio_channel=info cargo run -p cursio-channel --example panel
```

It prints that every chunk arrived once across the break, how fast the
console answered beside the upload, that no load sample sent while the
connections were down was replayed, and how many messages were handed back
when the session was closed.

Extracted from Cursio's transport, which is its first user. The design is
argued in `docs/channel-design.md` of the Cursio repository.

Licensed under either of Apache License 2.0 or MIT, at your option.
