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
pub trait Envelope: Sized + Send + 'static {
    /// Index into the channel's classes, highest priority first.
    fn class(&self) -> ClassId;
    fn delivery(&self) -> Delivery;
    fn encode(&self) -> Result<Vec<u8>, EncodeError>;
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError>;
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
session, and delivered exactly once.

### Seal

Noise transport mode over each carrier, exactly as now, when the trust
produced Noise keys. When the carrier is already authenticated and encrypted
(TLS with mutual certificates, a WSS connection whose user presented a token),
sealing is the identity: frames go in the clear inside TLS. Frame tags and
chunking are the same either way.

### Carrier

```rust
pub trait Carrier: Send + 'static {
    fn split(self: Box<Self>) -> (Box<dyn CarrierTx>, Box<dyn CarrierRx>);
    fn kind(&self) -> CarrierKind;
}
#[async_trait] pub trait CarrierTx: Send {
    async fn send(&mut self, frame: &[u8]) -> Result<(), ChannelError>;
    async fn send_batch(&mut self, frames: &[Vec<u8>]) -> Result<(), ChannelError>;
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

```rust
#[async_trait] pub trait Trust: Send + Sync {
    /// Run the opening exchange over a fresh carrier, as the side that
    /// dialled or the side that accepted.
    async fn open(&self, carrier: &mut dyn Carrier, role: Role) -> Result<Opened, ChannelError>;
}
pub struct Opened {
    pub peer: PeerIdentity,       // opaque bytes plus a display name
    pub seal: Seal,               // Noise keys, or Identity for an authenticated carrier
    pub session_tag: SessionTag,  // same on both ends, unguessable
}
```

Shipped implementations:

- **`NoisePsk`** — `Noise_NNpsk0` keyed by a shared secret, Cursio's handshake
  byte for byte. SPAKE2 pairing, which produces the secret, stays in Cursio.
- **`NoiseKeys`** — `Noise_IK`/`XX` with static Ed25519/X25519 keys made on
  each node, the peer's key pinned by the caller or vouched for by a
  certificate the caller verifies (Leave's spiffe CA): what a server's
  agent and a control plane use when TLS is not underneath.
- **`CarrierAuthenticated`** — for a carrier TLS already authenticated: the
  caller says who the peer is (a verified client certificate, a validated
  JWT), the session tag is issued by the accepting side over that carrier
  and presented again to resume. A tag alone never authenticates: a joining
  connection must authenticate as the same peer first.

The session binds resumption to the trust: a connection joins a session only
when it opened as the same peer and offered that session's tag.

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
2. **Trust and carriers for the internet**: `NoiseKeys`,
   `CarrierAuthenticated`, the WebSocket carrier, and an example pair (a
   service and a client with a console lane and a bulk lane, killing the
   connection mid-transfer and resuming) that Leave can start from.
3. **Specification and test vectors** for implementations in other languages.
4. **QUIC carrier**, when the measurements say the lanes stall on resends.
5. **First Leave consumer**, chosen there: the panel's console and files, or
   resumption under the agent's command stream.
