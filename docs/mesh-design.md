# Cursio — Mesh & Resilience Design

**Goal (user, 2026-07-19):** no server. Every machine is a peer. If the laptop
dies, the phone keeps talking to the PC; if the PC dies and the laptop comes
back, the phone talks to the laptop. Nothing should "forget the world" because
one node went away.

## Where we actually are

Long-term memory is **not** the problem — that part already works:

- `TrustStore` persists to `trusted_peers.json` under the platform config dir
  (`pairing.rs`), so a paired peer survives a restart and never re-pairs.

What is missing is **liveness** and **plurality**:

1. **`connect` doesn't survive a drop.** `serve` runs an accept loop and waits
   for the next peer (`cursio.rs`), but `connect` is a straight call: peer dies →
   session ends → the process exits. One side waits, the other gives up.
2. **Death isn't detected.** Heartbeats are *sent* every 5s
   (`HEARTBEAT_INTERVAL`) but nobody checks they *arrive*. A yanked power cord
   sends no FIN, so the link looks alive until the OS TCP timeout — minutes of
   "everything is frozen".
3. **A node knows exactly one peer at a time.** Sessions are 1:1 and nothing
   holds a picture of the wider set of machines.

## What `Role` really means (and what it doesn't)

`Role::Initiator/Responder` looked like client/server, but auditing every use
shows it is almost entirely legitimate:

- `pairing.rs` — orders the SPAKE2 confirmation tags. The exchange itself is
  symmetric; the role only says who speaks first.
- `crypto.rs` — picks the Noise initiator/responder half of the handshake.

Both are properties of **one connection**, not a hierarchy: in a mesh, whoever
dials is the initiator *for that link*, and the same node is a responder on the
next link. These stay.

Only two places leak actual "server-ness", and only these need to change:

- `start_active = matches!(role, Role::Responder)` (`cursio.rs`) — decides who
  holds the cursor. With N nodes "the responder" is meaningless.
- Audio stream-id namespacing `Initiator => 1000 / Responder => 2000`
  (`audio_session.rs`) — with three nodes, two of them would both pick 1000 and
  collide.

Fixing both by deriving from the stable `PeerId` instead of the link role is
what makes N nodes possible.

## Design

### 1. Liveness: detect death in seconds, not minutes

Track the last inbound message time in the session loop. Any message counts as
liveness, not just heartbeats — traffic already proves the link. If nothing
arrives for `PEER_TIMEOUT` (15s = 3 missed heartbeats), end the session with a
distinct `SessionError::PeerTimeout` so the caller can tell "peer vanished"
from "peer said Bye".

Deliberately a multiple of the heartbeat: one missed beat is jitter, three is
gone.

### 2. Reconnect: both sides keep trying

Make `connect` symmetric with `serve` — a loop, not a single shot. On session
end (any reason short of a fatal config error), redial with capped exponential
backoff (1s → 2s → 4s … 30s), resetting the delay after a session that actually
ran. Since the trust store already holds the key, reconnect skips pairing
entirely: it is a handshake and a Noise upgrade, no user interaction.

This alone turns "laptop died → everything is over" into "laptop died → a few
seconds of silence → back".

### 3. Identity-derived roles (kills the last server-ness)

Replace both leaks with functions of the stable `PeerId`:

- **Cursor ownership:** the node with the numerically lowest `PeerId` among
  connected nodes starts active. Deterministic, requires no negotiation, and
  every node computes the same answer independently.
- **Stream ids:** namespace by a hash of the owning node's `PeerId` rather than
  by link role, so N nodes never collide.

### 4. Peer registry: remembering the world

A `PeerRegistry` holds every *known* node (from the trust store) plus live
state: `Online { session } | Offline { last_seen }`. It is the answer to "the
PC shouldn't forget everything exists": the set of known nodes is durable, and
only the *connection* state is transient.

### 5. N simultaneous sessions

`run_session` already takes a transport and a module list, and each session owns
its own state — so N sessions is N tasks, one per peer, no shared mutable state
between them. The registry supervises: spawn on connect, mark offline on end,
retry with backoff.

A node dials every known peer and listens at the same time. Two nodes dialing
each other simultaneously is resolved by the same PeerId rule: the lower id
keeps its outbound attempt, the higher one drops it in favour of the inbound.

## Milestones

1. **Liveness timeout** — `PEER_TIMEOUT`, `SessionError::PeerTimeout`.
2. **Reconnect loop** — `connect` retries with backoff; symmetric with `serve`.
3. **Identity-derived roles** — ownership and stream ids from `PeerId`.
4. **Peer registry** — durable known-set + online/offline state.
5. **N sessions** — supervise many peers at once, dial-race resolution.

1 and 2 remove the pain today; 3 unblocks 3+ nodes; 4 and 5 are the mesh proper.
