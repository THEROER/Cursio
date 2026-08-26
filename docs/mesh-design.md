# Cursio — Mesh & Resilience Design

Status: **implemented.** Everything below ships; `serve`/`connect` were replaced
by the single `mesh` command this design argued for. This document is the source
of truth for how the mesh should behave; code should match it or this doc should
be updated.

**Goal (user, 2026-07-19):** no server. Every machine is a peer. If the laptop
dies, the phone keeps talking to the PC; if the PC dies and the laptop comes
back, the phone talks to the laptop. Nothing should "forget the world" because
one node went away.

## The three problems this solved

Long-term memory was never the problem — `TrustStore` persists to
`trusted_peers.json` under the platform config dir (`pairing.rs`), so a paired
peer survives a restart and never re-pairs. What was missing was **liveness** and
**plurality**:

1. **`connect` didn't survive a drop.** `serve` ran an accept loop and waited for
   the next peer, but `connect` was a straight call: peer dies → session ends →
   the process exits. One side waited, the other gave up.
2. **Death wasn't detected.** Heartbeats were *sent* every 5s but nobody checked
   they *arrived*. A yanked power cord sends no FIN, so the link looked alive
   until the OS TCP timeout — minutes of "everything is frozen".
3. **A node knew exactly one peer at a time.** Sessions were 1:1 and nothing held
   a picture of the wider set of machines.

The answer to all three is that `serve` and `connect` were two halves of one
behaviour. A node listens for peers dialling in *and* dials the known peers that
are down, and that single behaviour is `mesh`.

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

## The five parts

Written as the design argued for them; all five ship. Where the implementation
chose differently, the section says so.

### 1. Liveness: detect death in seconds, not minutes

Track the last inbound message time in the session loop. Any message counts as
liveness, not just heartbeats — traffic already proves the link. If nothing
arrives for `PEER_TIMEOUT` (15s = 3 missed heartbeats), end the session with a
distinct `SessionError::PeerTimeout` so the caller can tell "peer vanished"
from "peer said Bye".

Deliberately a multiple of the heartbeat: one missed beat is jitter, three is
gone.

### 2. Reconnect: both sides keep trying

Dialling is a loop, not a single shot. What shipped is simpler than the capped
exponential backoff this design first proposed: a **fixed sweep every 2 seconds**
(`DIAL_SWEEP_INTERVAL`, `mesh.rs`) that redials whatever is currently down. There
is no backoff state to reset, and a peer that returns is picked up by the next
sweep. Since the trust store already holds the key, reconnect skips pairing
entirely: it is a handshake and a Noise upgrade, no user interaction.

This alone turns "laptop died → everything is over" into "laptop died → a few
seconds of silence → back".

### 3. Identity-derived roles (kills the last server-ness)

Replace both leaks with functions of the stable `PeerId`:

- **Cursor ownership:** decided by `CursorSetup::negotiate` (`session.rs`), and
  the id is only the *tie-break*. What a machine **is** comes first: a device
  that cannot hold the cursor — an input source with no screen of its own —
  stays dormant, which is exactly what makes its input stream to the peer, and a
  machine facing one holds the cursor always. Only when both are ordinary
  machines with screens does the lower `PeerId` start active. Comparing ids
  rather than who dialled whom matters because the latter flips when the same two
  machines reconnect the other way round, landing the cursor somewhere different
  each time.
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

## Meeting a machine for the first time

The sweep above only auto-dials machines already in the trust store. Meeting a
new one stays a deliberate, password-gated act: `mesh --pair` is run on **both**
machines with the same password, and it opens a ten-minute window in which a
machine neither has met is allowed to be called at all. There is no separate
"listen" and "join" side, because a node already does both — the same
`should_dial` rule decides which end places the call.

SPAKE2 runs in symmetric mode, so the password never crosses the wire and `Role`
only orders the confirmation tags. A wrong password yields two *different* keys
rather than an error, which is why an explicit key-confirmation step follows; only
after it passes is the peer written to the trust store. `--pair-addr` names the
machine directly when mDNS will not, and naming it also means this side dials.

## Where a machine stands

Placement is a **grid cell**, `--at X,Y`, carried in `Hello` — cells, not pixels,
so nothing needs to know how wide anybody's screen is. Each node derives for
itself which of its edges faces which peer, and which peer is nearest in that
direction. Left unset, every machine sits at `0,0`, where nothing borders
anything and the hotkey is the only handoff. The reasoning behind grid
coordinates rather than "left"/"right" is in `input-design.md`.

## What a machine *is doing*

`ModuleAdvert` says a node has audio; `DeviceKind` says it is a laptop. Neither
says the laptop is at 9% and about to disappear, or that the desktop everything
routes through has moved onto a phone hotspot. That is **state**: it changes
while a session is up, and nothing on the wire carried it.

`NodeStatus` (`protocol/src/node.rs`) does — battery level, power source,
network kind, Wi-Fi signal. It rides `Hello` and is re-sent as
`Message::NodeStatus` on change.

**It is not a module, deliberately.** Every other subsystem is one because it is
optional: a machine with no clipboard backend simply does not advertise
clipboard. But every node has *some* answer to "are you on battery" — even if
that answer is "no battery, I am a desktop" — and the peers whose state is most
worth knowing are the ones sharing least with us. A module could be negotiated
away, and then a peer running only files could not be asked whether it is about
to vanish.

**Every field is `Option`, and absence is never zero.** A desktop has no
battery; a kernel may not expose one; a container has no Wi-Fi to ask about.
`None` means "this node cannot say". A peer rendering a missing battery as 0%
would show every desktop in the mesh as flat, and the mesh would route work away
from machines that were fine. The rule in the platform layer is the same:
**never substitute a plausible default for a failed read.**

**Sent on change, read by polling.** No interval is right for a poll on the
wire: battery percentage moves once a minute, but the switch to *discharging* is
the moment a peer cares and it falls between any two polls. Reading, on the
other hand, is polled — neither Linux nor Windows offers one event source
covering power *and* network, and four subscriptions (two needing a message loop
or a COM apartment) to learn a handful of integers costs more than reading a few
sysfs files every 30 s. `differs_meaningfully_from` decides what is worth
saying, so a plugged-in desktop sends nothing for the life of a session:
percentage is compared in 5% steps and Wi-Fi signal in bars, while a change of
power source or network kind is never rate-limited away.

**One monitor per machine, one cursor per link.** A node holds a link to every
peer and each session polls the same monitor, so a plain "changed since last
poll" flag would be consumed by whichever session polled first and no other peer
would ever hear it — the machine changed, not the link. Each reading therefore
carries a generation and each session remembers the last it sent, so a change
reaches every peer exactly once and a link that comes up later just greets with
the current reading.

Nothing routes by this yet; it is logged. The point of carrying it now is that
placement decisions already exist (`ServiceOffer`/`Fitness` rank nodes for
AirPlay) and have been ranking machines without knowing which of them is running
out of power. A laptop below 15% and discharging is `is_power_critical`; a peer
on cellular `is_metered`. Those are the questions the mesh will ask.

## Where it lives

All five parts shipped, and the code is laid out along the same seams:

| Part | Code |
|---|---|
| Liveness timeout | `PEER_TIMEOUT` (15s), `SessionError::PeerTimeout` — `session.rs` |
| Reconnect | dial targets, sweep interval, backoff — `mesh.rs` |
| Identity-derived roles | `CursorSetup::negotiate` — `session.rs` |
| Peer registry | `PeerState`, `should_dial` — `registry.rs` |
| N sessions | one task per peer, supervised by the registry |
| Node state | `NodeStatus` — `protocol/src/node.rs`; reading it — `core/src/node_status.rs` |

`PeerState` gained a third state the original design did not name: `Connecting`,
so the supervisor does not start a second attempt for a peer that already has one
in flight.

Everything between "there is a socket" and "a session can start" lives in
`link.rs`: `Hello` → SPAKE2 pairing on first meeting (or the stored key) → Noise
`NNpsk0` → module negotiation → cursor ownership.

**Sessions are `!Send`.** The input path shares its injector and cursor via `Rc`
deliberately, so peer sessions are spawned with `spawn_local` onto a `LocalSet`
and the registry is `Rc<RefCell<_>>`. This is single-threaded by design and is
not to be "fixed" with `Arc`/`Mutex`.
