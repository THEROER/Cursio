# One channel, many lanes: the transport as a crate of its own

Cursio's transport solves a problem that is not Cursio's alone: several kinds
of traffic share one link between two parties, and the urgent kind must never
wait behind the bulky kind — not in our queue, not in the kernel's, not behind
a lost segment of another stream — while the link itself drops, moves and
comes back. Leave (the hosting control plane, its future server panel) has the
same problem with different parties: a console that must stay interactive
while a backup uploads, command results that are lost today when the agent's
stream drops (the agent does not buffer them), a dead channel nobody notices.

This document moves the part of `cursio-transport` that answers that problem
into `cursio-channel`, licensed MIT OR Apache-2.0, and leaves everything that
is Cursio's own — its vocabulary, its pairing, LAN discovery, the frozen
update lane — where it is. It is the foundation Leave starts from; Cursio is
its first user and does not change on the wire.

## What it is not

- **Not a data plane.** Leave's game traffic rides kernel WireGuard with its
  own routing; nothing here replaces it, and NAT traversal there stays on
  WireGuard.
- **Not a replacement for gRPC/mTLS between Leave's agent and control.**
  Whether that stream gains resumption from this layer is Leave's call; the
  crate must make it possible, not require it.
- **Not discovery and not an identity system.** Who the peers are and why they
  are trusted is the caller's; the crate takes a [`Trust`](#trust) that answers
  it.

## Layers

From the application down, each layer knowing only the one beneath it:

| Layer | Answers | Today in Cursio |
|---|---|---|
| **Envelope** | what a message is: its class, its delivery, its bytes | `cursio_protocol::Message` (`priority`, `delivery`, `to_bytes`) |
| **Scheduler** | which waiting frame goes next | `priority.rs`: classes, byte budgets, chunking, starvation guard, `Delivery` expiry, motion folding, delay histograms |
| **Session** | that every reliable frame arrives once, across connections | `resume/`: `SEQ`/`ACK`/`RESUME`, per-lane numbering, kept frames, paths, silence, degraded paths, hold, pacing (`congestion.rs`) |
| **Seal** | that nobody else read or wrote it | `crypto.rs`: Noise transport, frame tags `WHOLE`/`PART` |
| **Carrier** | moving framed bytes | `tcp.rs`, `stream.rs` (RFCOMM), `send_backlog.rs`, `tcp_info.rs` |
| **Trust** | who the other end is, and the keys | `crypto.rs` handshake (`NNpsk0`), `pairing.rs` (SPAKE2) |

Everything in the first five rows moves, made generic. Of the sixth, the
handshake machinery moves and SPAKE2 pairing becomes one `Trust` among
several, kept in Cursio. `discovery.rs` (mDNS), `direct.rs` (Wi-Fi Direct
addresses), `dead_peer.rs`'s Cursio policy and the `Preamble`/`Hello`
exchange stay in Cursio.

## The pieces

### Envelope

```rust
pub trait Envelope: Clone + Send + 'static {
    /// Index into the channel's classes, highest priority first.
    fn class(&self) -> ClassId;
    fn delivery(&self) -> Delivery { Delivery::Reliable }
    fn encode(&self) -> Result<Vec<u8>, ChannelError>;
    fn decode(bytes: &[u8]) -> Result<Self, ChannelError>;
    /// A later message may fold into this one while it waits; only such
    /// messages are kept decoded beside their frame (a clone).
    fn folds(&self) -> bool { false }
    /// Fold `next` into `self` while both wait unsent (pointer motion in
    /// Cursio, a progress counter elsewhere); `false` queues it behind.
    fn fold(&mut self, _next: &Self) -> bool { false }
    /// A person is acting through this message: background classes yield
    /// while such messages flow (Cursio's input).
    fn is_activity(&self) -> bool { false }
}
```

`Delivery` (`Reliable`, `Fresh { within }`) moves here from
`cursio-protocol`, which re-exports it, so the choice stays the owning
module's (`docs/transport-design.md`, "Delivery").

### Classes and lanes

A channel is built from a list of classes, highest priority first, each with
a byte budget and the lane it rides. A lane is one carrier connection with its
own numbering and resend buffer, so a stalled bulk socket cannot hold the
interactive one. Cursio's six classes and four lanes become one such list,
declared by Cursio; Leave declares its own, for example:

| Class | Lane | Delivery |
|---|---|---|
| control (heartbeats, acks of commands) | interactive | reliable |
| console (keystrokes, output) | interactive | reliable |
| metrics, live log tail | interactive | fresh, 5 s |
| file and backup chunks | bulk | reliable |

The scheduler's guarantees hold per declaration: strict priority within a
lane with the starvation guard for the lowest class, budgets enforced at
admission, messages atomic under cancellation, expiry judged only at a
message's first frame.

### Session

Unchanged in behaviour (`docs/mesh-design.md`, "A session outlives its
connection"): frames numbered per lane, acknowledged, kept until acknowledged
and resent on a joining path; `Fresh` frames numbered but never kept; a path
silent past its limit passed over, then let go; a degraded path avoided for a
while; with no path left, the session held for `HOLD` and ended. What changes
is only what it names:

- `PathKind` becomes an opaque carrier kind with a lane, plus a flag for a
  standby carrier able to carry every lane (Cursio's Bluetooth).
- `PathChoice`/`PathReport` move here; `cursio_protocol::diag` re-exports
  them.
- The session tag is the trust's (below), not necessarily a Noise hash.

This is the piece Leave's command results need: a result sent while the
stream is down is kept and resent when the agent's next connection joins the
session.

**The guarantee is exactly once within one session**, and no more. Kept
frames live in memory: they are gone when the hold runs out, when either
process restarts, or when the trust ends the session. So when a session ends
for any reason, the application is handed every reliable message the peer
never acknowledged, decoded, to keep and send again in a session of its own
making; a durable truth (Leave's `command_id` and the agent's journal) stays
the application's, and so does deduplication across sessions.

**A reliable message may carry an absolute deadline** (`expires_at`, the
sender's wall clock), apart from `Fresh { within }`, which is judged against
the sender's own queue and suits metrics, not commands. A reliable message
past its deadline is not sent, not resent after a resume, and is reported to
the sender as expired — never lost quietly. A command run late is worse than
one refused.

### Seal

Noise transport mode over each carrier, exactly as now, when the trust
produced Noise keys. When the carrier is TLS that verified its peer (mutual
certificates, or a WSS connection whose user then presented a token), sealing
is the identity: frames go in the clear inside TLS. That mode is refused on
anything else — plain TCP, `ws://` — whatever the configuration says; only the
in-memory carrier of the tests is exempt. Frame tags and chunking are the same
either way.

### Carrier

```rust
#[async_trait] pub trait Carrier: Send {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError>;
    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError>;
    fn kind(&self) -> &'static str;
    fn local_ip(&self) -> Option<IpAddr> { None }
    fn split(self: Box<Self>) -> (Box<dyn CarrierTx>, Box<dyn CarrierRx>);
}
#[async_trait] pub trait CarrierTx: Send {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError>;
    async fn send_batch(&mut self, frames: Vec<Vec<u8>>) -> Result<(), ChannelError>;
    fn counters(&self) -> Option<std::io::Result<TcpCounters>> { None }
}
#[async_trait] pub trait CarrierRx: Send {
    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError>;
}
```

Frames, not bytes: TCP and RFCOMM carry the 4-byte big-endian length prefix,
a WebSocket carries one binary message per frame, a QUIC stream carries the
prefix. Shipped with the crate: TCP (with the unsent-backlog bound and
retransmission counters), WebSocket (client and server, `tokio-tungstenite`),
and an in-memory pair for tests. RFCOMM stays in Cursio, implementing the
trait. QUIC (one stream per lane; `Fresh` frames under a datagram's size as
datagrams) follows the measurements in `docs/transport-design.md`.

### Trust

Identity is never something a caller asserts. It is a value a verifier
produced from what it checked — a TLS handshake, a token, a Noise static
key — and it is typed, so a service cannot be confused with a user and a
display name never takes part in a decision:

```rust
pub enum PeerIdentity {
    /// A workload: `spiffe://leave/service/<id>` from the one URI SAN of a
    /// client certificate with clientAuth, never a CN or a header.
    Service { id: String },
    Host { id: String },
    /// A person, from a token whose audience was checked.
    User { sub: String, aud: String },
    /// A Cursio node, after pairing.
    Paired { id: [u8; 16] },
}
// Equality is on the canonical form only: a certificate rotated to a new
// serial is the same peer, so its session lives on.

#[async_trait] pub trait Trust: Send + Sync {
    /// The opening exchange over a fresh carrier, as the side that dialled
    /// or the side that accepted.
    async fn open(&self, carrier: &mut dyn Carrier, role: Role) -> Result<Opened, ChannelError>;
    /// Is `peer` still trusted now? Asked on every connection that joins
    /// the session (with the joining connection's own fresh proof), on a
    /// timer, and before each message is delivered. `Err` ends the session,
    /// held or not.
    fn check(&self, peer: &Verified, now: SystemTime) -> Result<(), Revoked>;
}
pub struct Opened {
    pub peer: Verified,           // the identity, its proof's serial, and not_after
    pub seal: Seal,               // Noise keys, or Identity for a verified TLS carrier
    pub session_tag: SessionTag,  // same on both ends, unguessable, never logged
}
```

Rules every trust and the session around it keep:

- **Continuous, not once.** A session has a deadline: the earliest
  `not_after` of what proved it (a certificate's, a token's `exp`). At the
  deadline it ends, unless a `REAUTH` frame carried a fresh proof of the same
  canonical identity first (a renewed JWT of the same `sub` and `aud`); a
  proof of another identity ends it. Revocation — a serial on a deny list, a
  token the issuer's introspection rejects — ends it at the next `check`,
  including a session only held and waiting for a path.
- **Resuming re-proves both ends.** A connection joins a session only after
  passing the full check again (expiry, deny list, audience) as the same
  canonical identity, and offering the session's tag. Both sides do it: the
  agent confirms the control it rejoins is the one it held results for, or
  they would go to another server. For a bearer token over WSS the tag is
  bound to the TLS connection (`tls-exporter`, RFC 9266), so a stolen tag
  cannot be replayed on another one.
- **Configured, never negotiated.** An endpoint names its trust in its
  configuration. There is no negotiation between trusts and no fallback: a
  TLS failure (wrong CA, name, expiry) fails closed and does not try Noise.
- **Tokens are in the opening exchange**, never in a URL.

Shipped implementations:

- **`NoisePsk`** — `Noise_NNpsk0` keyed by a shared secret, Cursio's handshake
  byte for byte. SPAKE2 pairing, which produces the secret, stays in Cursio.
  Not for Leave: a shared secret there is a shared token.
- **`CarrierAuthenticated`** — for a carrier TLS verified: the verifier of
  the caller's choosing (Leave's spiffe rules, a JWT validator with audience
  and introspection) turns that TLS context, and the token sent in the
  opening exchange, into a `Verified`, and answers `check`. Refused on a
  carrier that is not verified TLS.
- **`NoiseKeys`** — `Noise_IK`/`XX` with pinned static keys, where TLS is not
  underneath. Binding a static key to an X.509 certificate (Leave's ECDSA
  P-256 service CA) needs a specification of its own and is out of scope
  until someone needs it.

### Authorization

Who a peer is and what it may do are separate. An endpoint carries a policy
from identity to the lanes and classes it may open — a user's console lane,
a service's bulk lane — checked when a class is first used and enforced only
when the policy says `enforce`, the way Leave's toolkit checks a caller
against a method.

## Wire format, for implementations in other languages

Leave's services are Python and its browser speaks WebSocket only, so the
format is specified, not just implemented: frame tags (`WHOLE` 0x00, `PART`
0x01 with `[u32 message][u16 index][u16 count]`, `SEQ` 0x02 with
`[u8 lane][u64 seq][u8 flags]`, `ACK` 0x03, `RESUME` 0x04, `PING` 0x05,
`PONG` 0x06), the Noise parameters, chunk limits, and the resume exchange,
with test vectors generated from the Rust implementation and checked in. A
second implementation is conformant when it reproduces them.

## Cursio on top of it

`cursio-transport` keeps its name and becomes Cursio's binding:
`Message: Envelope`, Cursio's class list, `NoisePsk` after SPAKE2, RFCOMM as a
carrier, discovery, and the frozen `Preamble`/update lane, which never passes
through the channel at all. **Nothing changes on the wire**: the update lane's
pinned bytes, the resume and frame tags and the Noise parameters are covered by
the existing tests, which must pass unchanged at every step, and two builds
either side of the move must still form a session.

### What the extraction settled

- **Frames are owned.** A carrier takes `Vec<u8>`, not `&[u8]`: Cursio's
  carrier wraps each frame in a `Message::Opaque` without copying it again.
- **Cursio's frames still ride as `Message::Opaque`.** Every Noise frame has
  always crossed Cursio's TCP and RFCOMM transports as the bincode of a
  `Message::Opaque` inside the length prefix, so Cursio's carrier is its
  `Transport` behind an adapter (`OpaqueCarrier`) that keeps that shape. The
  channel's own TCP carrier sends the frame bare; the two are different wires
  and are not meant to meet.
- **The handshake keeps a length prefix of its own.** Each `NNpsk0` message
  travels as `[u32 len][message]` inside its frame, as Cursio has always sent
  it; `NoisePsk` specifies that, and it is part of the format of step 3.
- **`Message: Envelope` lives in `cursio-protocol`.** The orphan rule allows
  the impl only beside the trait or the type; the protocol crate depends on
  the channel for `Delivery`, `SessionTag`, `PathChoice` and `PathReport`
  anyway. The class order is `Priority::class`; the declaration — budgets,
  lanes, which classes are urgent — is `cursio_transport::lane::classes`.
- **A declaration is `Classes::new(lanes, classes)`.** A lane names itself
  and its path for logs, its writer depth, whether it is paced against the
  interactive round trip, and whether a standby skips its fresh frames; a
  class names its lane, its budget and whether it is urgent (never waits for
  a forced bulk release). The first lane is the interactive one. The
  scheduler's finished delay windows go to a report function the declaration
  may replace; Cursio's keeps its log lines and their target.
- **`PathKind` is `Lane(LaneId)` or `Standby`**, with a flag for a direct
  path beside it. Reports keep the names `network` and `bluetooth`, which
  Cursio's control socket and diagnostics already carry, for the lane carriers
  and the standby.
- **Hooks for step 2.** Every connection joins in `ResumeHandle::attach`, and
  every end goes through one place that carries its reason (`Inner::end`):
  where `Trust::check`, `REAUTH`, deadlines (`expires_at`) and handing back
  unacknowledged messages will sit. `Trust::open` returns the seal and the
  session tag only; the verified identity joins `Opened` with `check`.

### What step 2 settled

- **The canonical form is a string with the kind in front** (`service:`,
  `host:`, `user:<len of sub>:<sub>:<aud>`, `paired:<hex>`), compared exactly:
  no case folding, and the subject's length keeps a `sub` containing `:` from
  borrowing its audience. `Verified` carries the identity, an optional
  `serial` and an optional `not_after`.
- **`NoisePsk` proves `Paired` with the id the caller names**
  (`paired_with`; zeros until it does), and its `check` always passes. Its
  handshake bytes did not move.
- **A fresh proof is the trust's to verify**: `Trust::reverify(proof, tls)`,
  refused by default, so a `REAUTH` on a trust that takes none ends the
  session rather than passing unchecked.
- **A carrier says what its TLS verified** (`Carrier::tls`: the peer's chain
  and the RFC 9266 exporter); `None` is everything that is not verified TLS.
- **Binding is opt-in per session**: `ResumableLink::with_options(…,
  SessionOptions::bound_to(trust))`, the first path carrying its proof
  (`Path::proving`). Unbound — Cursio's — sessions behave as before.
- **A joining connection that fails is refused, not fatal.** Another identity,
  an expired or revoked proof, or no proof at all refuses that connection
  and leaves the session alone, so whoever saw the tag cannot end it by
  knocking; the session's own proofs are re-checked at the same moment, and
  those failing end it.
- **The session stands on a set of proofs**: the first path's plus each joined
  path's (at most eight, the first always kept); its deadline is their
  earliest `not_after`. A `REAUTH` (tag `0x07`, `[proof]`, written by
  `ResumeHandle::reauthenticate` on the interactive lane's path) replaces the
  whole set once `reverify` and `check` accept it.
- **When the trust is asked**: every join, every delivered message, every
  `check_every` (5 s by default) on the session's timer, and on any tick past
  the deadline — the timer runs with no path at all, which is how a held
  session hears of revocation.
- **The tag is bound only where there is TLS.** `RESUME` carries
  `SHA-256("cursio-channel resume" || tag || exporter)[..16]` on a path with
  an exporter and the bare tag on any other, so Cursio's `RESUME` bytes are
  unchanged.
- **Logs carry a hash of the tag** (`SHA-256("cursio-channel label" || tag)`,
  four bytes), never the tag's own bytes.
- **Endings are reported**: `ResumableLink::events` yields
  `SessionEvent::Ended { reason, … }` once, whatever ended it.
- **What is owed is kept as messages, not frames** (`session/ledger.rs`): a
  reliable message is entered whole, as its encoded body, when the scheduler
  admits it, and leaves when the frame that ends it is acknowledged. That
  covers the scheduler's queue, the frame in the drainer's hands and the
  resend buffer at once, and a chunked message's early parts need no keeping
  of their own. `SessionOptions::hand_back` enters every reliable message;
  without it only messages with a deadline are entered, so Cursio's sessions
  keep nothing and pay one branch per message. `Ended.unacknowledged` comes
  in admission order, decoded with `Envelope::decode`.
- **A deadline is `Envelope::expires_at`**, for reliable messages on a
  session. It is judged where the message can still be withheld whole: at
  the scheduler's queue, when the drainer hands it over, and when a new path
  resends — only while its first frame is unacknowledged, so the peer has
  none of it; later frames still queued are skipped. It is reported as
  `SessionEvent::Expired(message)`, and one still owed when the session ends
  past its deadline is reported expired rather than handed back. A message
  a ledger holds is never folded into.
- **Authorization is the receiver's**: `Authorization::new(enforce, |identity,
  lane, class| …)` on a bound session (`SessionOptions::authorize`; refused on
  an unbound one), decided once per class at the first message of it that
  arrives. A class this end never declared, or one arriving off its declared
  lane, is refused whatever the rule says. A refusal is reported as
  `SessionEvent::Unauthorized` and logged; not enforcing, the message is
  delivered; enforcing, it is not, and the session ends.
- **`CarrierAuthenticated` takes a `Verifier`** (`verify(PeerContext { tls,
  token, renewal })`, `check`) and refuses a carrier whose `tls()` is `None`
  before sending anything; only `insecure_in_memory_for_tests` lets the
  in-memory carrier by. `Seal::identity` is crate-private, so this trust is
  the only way to a seal that leaves frames to TLS. Its opening exchange is
  one frame each way inside TLS: `OPEN` (`"CA"`, version 1, a resuming flag
  with the session's exporter-bound offer, `[u32 len][token]`), then
  `ACCEPT` (`0x00`, a 16-byte tag the acceptor drew from the OS generator,
  its own token) or a bare `REFUSE` (`0x01`) — the reason stays in the
  acceptor's log. A token travels only there and in `REAUTH`, never in a
  URL. A dialler that wants an existing session says so
  (`CarrierAuthenticated::resuming(tag)`); the acceptor finds it with
  `ResumeHandle::named_by(offer, &path)` and `attach` still re-proves it.

## Licence

`cursio-channel` is MIT OR Apache-2.0, like `cursio-sdk`, so Leave's code
linking it keeps its own licence. Its code was written by the two authors of
`cursio-transport`, THEROER and IMDelewer, both of whom agreed to this licence
for it (2026-11-20). Nothing vendored and nothing GPL may enter it; its
dependencies (`tokio`, `snow`, `tokio-tungstenite`, `thiserror`, `tracing`)
are MIT or Apache. It is mirrored to its own public repository the way
`cursio-sdk` is (`cargo xtask sdk-mirror`).

## Order of work

1. **Extract** `crates/channel` with Envelope, classes and lanes, scheduler,
   session, seal, the TCP and in-memory carriers, `NoisePsk`.
   `cursio-transport` becomes the binding. Every existing transport, core and
   live test passes unchanged; nothing on the wire moves.
2. **Trust and carriers for the internet**: typed `Verified` identities,
   `Trust::check`, deadlines and `REAUTH`, `CarrierAuthenticated` over TLS and
   WSS, authorization per lane and class, the WebSocket carrier, and an
   example pair (a service and a client with a console lane and a bulk lane,
   the connection killed mid-transfer and resumed) that Leave can start from.
   Its tests: a resume by another peer is refused; a certificate expiring
   mid-stream closes the session; a serial added to the deny list closes a
   held session; `CarrierAuthenticated` over `ws://` is refused; a reliable
   message past its deadline is reported expired; a session that ends hands
   back what was never acknowledged.
3. **Specification and test vectors** for implementations in other languages.
4. **QUIC carrier**, when the measurements say the lanes stall on resends.
5. **First Leave consumer**, chosen there: the panel's console and files, or
   resumption under the agent's command stream.
