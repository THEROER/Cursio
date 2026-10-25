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
only the *connection* state is transient. The user removing a machine is the
one thing that takes a node out (see "Removing a machine").

### 5. N simultaneous sessions

`run_session` already takes a transport and a module list, and each session owns
its own state — so N sessions is N tasks, one per peer, no shared mutable state
between them. The registry supervises: spawn on connect, mark offline on end,
retry with backoff.

A node dials every known peer and listens at the same time. Two nodes dialing
each other simultaneously is resolved by the same PeerId rule: the lower id
keeps its outbound attempt, the higher one drops it in favour of the inbound.

That rule only says who *should* call; it cannot stop two links forming. The
side told to wait calls anyway after `YIELD_GRACE`, so a slow side and an
impatient one meet halfway, and a phone moving from Bluetooth to the network
briefly has both. Two links ran two whole sessions — the phone's screen opened
twice, its microphone was offered twice, and either link ending marked the
peer offline while the other was still up, which had the mesh dial it again.
So each session, the moment it learns who it talks to, claims the peer's one
link (`PeerRegistry::claim_link`), and both ends keep the same one without
asking each other (`LinkShape::beats`): the network over Bluetooth, then the
call the lower id placed, then — two calls from one side — the newer. A link
that loses closes at once; a held link that is beaten is woken and closes too.
Only the link that holds the peer takes it offline when it ends.

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

The same window can instead be opened with a **one-time code**
(`core/src/pairing_window.rs`): one machine shows eight characters (`ABCD-EFGH`,
no `0 O 1 I L`), the other types them, and the code *is* the SPAKE2 password —
the protocol is unchanged. A short secret is safe here because SPAKE2 gives an
eavesdropper nothing to test guesses against offline; every guess is a live
handshake. So the window budgets those: three failed key confirmations shut it
(for a password window too), a code lasts five minutes, and the first
successful pairing spends it. A dropped socket does not count as a failure.

## Removing a machine

Pairing is the only way in, so removal (`core/src/forget.rs`) undoes what it
made durable: the trust-store entry with its shared key and cached address,
and builder trust for updates, which names the machine by id. That half is
plain files, so `cursio forget <machine>` does it even with no node running.
With one running it goes through the control socket's `forget`, and the node
also drops what it holds: the registry entry (the one removal the registry
makes — a machine that is *away* only ever goes `Offline`), the machine's
problems, and the link. The link is ended from inside its own session through
`LiveSettings::forgotten`, the same watch every setting change travels on: it
sends `Bye` and runs the ordinary teardown, so modules stop and the peer's
disks unmount exactly as when a link ends any other way. The peer hears a
goodbye rather than a timeout.

The other machine still trusts this one and keeps dialling. Without the key its
handshake fails here the way any stranger's does; the node says so once
(`forget::note_refused_call`) and not every sweep. Pairing the two again, from
either side, takes the machine off `forgotten` so its new link stays up.

## Where a machine stands

Placement is a **grid cell**, `--at X,Y`, carried in `Hello` — cells, not pixels,
so nothing needs to know how wide anybody's screen is. Each node derives for
itself which of its edges faces which peer, and which peer is nearest in that
direction. Left unset, every machine sits at `0,0`, where nothing borders
anything and the hotkey is the only handoff. The reasoning behind grid
coordinates rather than "left"/"right" is in `input-design.md`.

## What a machine runs: `config.toml`

The set of modules, the grid cell, AirPlay mode and the rest live in
`config.toml` in the platform config dir (`~/.config/cursio/config.toml`,
`%APPDATA%\cursio\cursio\config\config.toml`; `--config PATH` points elsewhere).
`cursio mesh` with no flags runs what it says; `cursio config` prints the
effective result. Precedence is **defaults < file < flags**: the defaults are
the everyday set (input, clipboard, audio, files; AirPlay `on`; `0,0`), a file
overrides only the fields it names, and each flag overrides the file for that
run. Module flags come in pairs (`--audio`/`--no-audio`) because a lone switch
that is false when absent would erase the file's answer.

```toml
position = { x = 1, y = 0 }

[modules]
input = true
clipboard = true
audio = true
files = true
usb = false
remote_screen = false
screen_host = true

[airplay]
mode = "on"          # off | on | a profile such as 1080p60
window = false

[input]
mac_modifiers = "positional"

[files.share]        # opaque here: whatever the files module's SharePolicy reads
```

Unknown keys are refused rather than ignored — a misspelt module is otherwise a
module that silently stays off. A change is applied to the running node, not by
restarting it (next section): the control socket applies it before replying,
and the node polls the file every two seconds for changes made any other way.
Fields a flag pinned stay pinned and are reported as such.

## Changing settings on a running node

This used to restart the process: capture, the listener and every link's
negotiated modules are settled at startup, and a fresh start is the one reload
guaranteed correct. For a toggle in the app that price was wrong — every link
dropped, the peer's disks unmounted, and on Windows the new process asked for
elevation again. Now a change lands where it belongs (`core/src/live.rs`,
`core/src/reconfigure.rs`):

| Setting | Applied |
|---|---|
| `modules.clipboard`, `audio`, `files`, `usb` | live: a switch the registry reads when it advertises, then renegotiated on every link |
| `position` | live: sent to every peer; both sides re-derive the edge between them |
| `modules.input` off | live: input stops being forwarded and a cursor that was away comes home |
| `modules.input` on | live when capture exists (input was on at startup); otherwise at the next restart |
| `modules.screen_host`, `files.share`, `audio.render_device` | live: the module that read it at build time is restarted on both ends of every link |
| `input.mac_modifiers` | live: the keymap is replaced |
| `port`, `modules.remote_screen`, `airplay.*` | **restart**: the listener owns the port, the portal backend claims a D-Bus name, the receiver binds and announces at startup |

A running link renegotiates with `Message::Reconfigure` (protocol v18): the
sender's full advert set and position, plus any modules to restart. Both sides
recompute `negotiate` from the latest adverts each holds of the other, stop
what is no longer agreed, and start what now is; input, the cursor and every
other module carry on. The sender announces *before* it starts anything, at
control priority, and the reader holds messages for a module that is not
running yet, because the peer's first message for a new module can be routed
before the loop has read the announcement. A module being restarted is not
started again until its old instance has shut down — two files modules would
mount the peer's volumes twice.

With input off, the loop ignores the hotkey and edge crossings, forwards
nothing, and refuses a peer handing it the dormant side: dormant with nothing
to forward would leave this machine's own keyboard grabbed and going nowhere.
The peer then holds its cursor too, which is what two machines that share no
input look like.

What is still waiting for a restart is reported as `restart_needed` — in the
status and in every save reply — measured against the config the process
started with, so undoing a change takes it off the list. The node never
restarts unasked; the app offers the `restart` command.

## Control socket

A running node answers on a local socket (`core/src/control/`): a Unix socket
at `$XDG_RUNTIME_DIR/cursio/control.sock` (0700 dir, 0600 socket; the temp-dir
fallback is refused unless it is this user's and private), or the named pipe
`\\.\pipe\cursio-control-<user>` with a DACL of this user's SID and SYSTEM only,
a medium integrity label (the node self-elevates, the app does not) and remote
clients rejected. Whoever reaches it can open a pairing window, so it is kept
exactly as private as the trust store.

Newline-delimited JSON, `{"id":1,"cmd":"status"}` → `{"id":1,"reply":"status",…}`.
Commands: `ping`, `status` (node — with its `pid`, crate `build` and the
`installed_version` it runs from, so the app can show and roll back what runs —
peers with presence/position/`NodeStatus`/
negotiated and skipped modules, configured modules, what flags pin, input
latency, `diagnostics` problems, the pairing window, and `update`: policy,
channel, a build being fetched and how far, one installed and ready, what an
`auto` switch is waiting for), `config`, `set_config`,
`set_module`, `set_position`, `pair_show`, `pair_enter`, `pair_cancel`,
`pairing`, `restart`, `subscribe`, `update_apply` (switch to the ready build
now; see `docs/update-design.md`), `forget` (remove a machine; see above).
After `subscribe` the node pushes the status
with id 0 whenever something in it changes (and checks every 2 s for what
changes elsewhere — latency, problems), only when it differs from the last
push. Requests carry ids and any number may be in flight on one connection;
`transport::Client` matches replies back and hands pushes to a channel — the
LeavePulse launcher's daemon client. The app keeps one subscribed connection,
re-emits pushes as Tauri events, and gives every request a deadline, so the
window never waits on a node that stopped answering or behind another request.

Changes go through `config.toml` rather than around it, so there is one path
for every change and the file always says what runs; the change is applied
before the reply, which names any field a flag pins and any setting that waits
for a restart. The mesh loop is `!Send`, so it *publishes* a snapshot every sweep
and each link reports itself up and down; the socket's handler only reads that.
A second node for the same user finds the socket answering and runs without
one, rather than being refused — two nodes on one machine on different ports is
a real test setup.

`cursio status` and `cursio pair [CODE]` are clients of it: `cursio pair` shows
a code, `cursio pair ABCD-EFGH` on the other machine enters it, and both wait
for the outcome. Every pairing reply, and the status, carry `nearby`: each
machine discovery has heard on the LAN, with its address, when, and whether it
is already in this mesh. Both clients show it while a code waits and say so
plainly when it holds no stranger — a code shown to a network where nothing can
be heard is never used, and without the list the only sign was the window
expiring. mDNS says nothing when a machine leaves, so the list is "heard", not
"reachable", and `last_seen` says how old. The Tauri app (`app/`) is another client and never runs the
mesh itself: the node self-elevates on Windows and drives GTK4 on Linux while
the webview is GTK3, and it has to keep running with no window open.

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

## What is wrong here, in words

The log says everything once, among thousands of lines. A user who opens it an
hour after the USB claim failed finds `no such device` and has to work out that
`adb` was holding the phone. `core/src/diagnostics.rs` is the other half: the
set of problems that hold **right now**, each a `Problem` with a stable `id`, a
`Severity`, a one-line `summary` naming the concrete cause, an optional `hint`
(the command that fixes it) and the `since` of the first time it was seen.

```rust
diagnostics::raise(problem);        // idempotent by id; keeps the first `since`
diagnostics::clear("usb.held.1-1"); // the condition ended
diagnostics::snapshot()             // most severe first, oldest first within it
```

**A set keyed by id, not a stream.** A problem is a state, so a producer that
sees it every second raises it every second and it stays one entry, dated from
when it began — "swap has been full for twenty minutes" is the useful sentence.
A producer never has to remember what it already raised, which is what keeps
every producer a one-line call at the place that already detects the condition.
A repeat that changes nothing logs nothing; a raise that changes what the
registry says logs once at the matching level, so the log and a status panel
never disagree.

**One per machine.** Problems come from module threads, FUSE and WinFsp
callbacks, a pressure monitor and the mesh supervisor, and the question the user
asks is about the machine, not about a link. The lock is held only to copy a few
small structs.

Ids are dotted by subsystem so a panel can group them: `usb.held.<busid>`,
`usb.usbipd-exposed`, `files.walk.<pid>.<peer>.<volume>`, `machine.memory` /
`machine.swap` / `machine.io` / `machine.commit`, `peer.<peer-id>.down`,
`module.<name>.<peer>.stalled`, `audio.virtual-devices`,
`window.shell-extension`, `portal.<name>`, `input.capture`. The producers that
live behind the platform crates (`cursio-usb`, `cursio-files`, `cursio-window`)
cannot name `cursio-core`, so they report
through their own errors or a reporter hook that the matching `*_session.rs`
turns into a `Problem`.

**A walk of a peer's disk** is the clearest case of a problem nobody sees. A
mounted peer volume looks local, so `find /`, a desktop indexer or a backup tool
walks it like any other — and every directory is a round trip, while the copy
the user did ask for crawls behind the metadata. The mount is the one place
that knows the caller: FUSE hands each request its PID, and WinFsp carries it on
the open that precedes a listing (a listing itself has no token). A process
that lists 200 *distinct* directories within 10 s is reported
(`cursio-files::walk`) with its command line — `/proc/<pid>/cmdline` on Linux,
the image path on Windows, where reading another process's command line needs
rights over its memory. Distinct directories, because a file manager re-listing
the folder it shows is not a walk. It ends after 20 s of quiet, noticed by the
files module's pump since a walker that stopped no longer calls the mount.

**The machine itself** (`core/src/pressure.rs`, a thread polling every 5 s).
Memory pressure does not fail anything; it slows everything, and from inside
Cursio that reads as a network fault in every module at once — seen live with
swap 100% full, each symptom chased as its own bug. Linux reads pressure stall
information (`/proc/pressure/{memory,io}` `avg10`: the share of time work
actually *waited*, not free memory, which a healthy page cache keeps near zero)
plus swap from `/proc/meminfo`; Windows reads `GlobalMemoryStatusEx`'s commit
charge against its limit, its equivalent of full swap. Each figure raises at one
level and clears at a lower one (memory full-stall 10%/5%, some-stall 20%/10%,
I/O full-stall 20%/10%, swap or commit 90%/80%), because a single line would
make a panel blink while pressure hovers at it.

**Links, modules and missing pieces** (`core/src/health.rs`) are one function
per condition, so the sentence, severity and id live in one place and a raise
cannot drift from its clear:

| Condition | Raised by | Cleared by |
|---|---|---|
| A greeted peer's session ended in an error | the mesh supervisor (`peer.<id>.down`) | that peer greeting again |
| A module's turn is too slow | `health::module_stalled(module, peer, took)` | `health::module_recovered` |
| No virtual audio driver (Windows: install `cursio-vad`) | the audio module, once | — (a capability, not an event) |
| The GNOME Shell extension is outdated or absent | `cursio-window::notice`, from the scanout inhibit call | the same call succeeding |
| A portal dialog was cancelled | `cursio-window::notice` (screen sharing); input capture failing to start | the next grant |

`module_stalled`/`module_recovered` are for the loop that already times every
module turn (`module_host.rs`); it is the one place that can tell a module that
fell behind from one that has caught up.

## One link, priority end to end

Every module rides one Noise-over-TCP link, so the order bytes leave in is the
latency each module gets. `transport/src/priority.rs` queues frames per class —
`Control` > `Input` > `Audio` > `Video` > `Metadata` > `Bulk` — and a drainer
writes the most urgent one next, re-choosing between frames. That choice only
reaches as far as the socket: whatever the kernel already holds goes out first,
in the order it was written. Left alone, a TCP send buffer autotunes to
megabytes, so a pointer event queued while the remote screen or a file transfer
was running waited behind a few megabytes of IDR frames and file chunks already
handed over — tens of milliseconds on gigabit, more on Wi-Fi, felt as a lagging
mouse however well the queue above was ordered.

So an input frame now waits behind at most two things, both bounded:

- **The frame being written.** Once started, a frame must finish or the stream
  is corrupt. Frames are at most one Noise message, 64 KiB: 0.5 ms on gigabit.
  They are not cut smaller for the lower classes, because a 64 MiB message would
  then need more parts than a receiver accepts.
- **What the kernel holds unsent** (`transport/src/send_backlog.rs`). On Linux,
  Android and macOS, `TCP_NOTSENT_LOWAT` = 32 KiB: the socket reports writable
  only below that many unsent bytes. Sent-but-unacknowledged bytes are not
  counted, so the congestion window — and throughput — is untouched; only the
  queue in front of the next write is capped. Windows has no such option and
  counts unacknowledged bytes in `SO_SNDBUF`, so a fixed cap would trade
  throughput for latency; instead the send buffer follows
  `SIO_IDEAL_SEND_BACKLOG_QUERY`, the stack's own bandwidth-delay estimate,
  re-read every 100 ms as the link ramps (clamped to 128 KiB–4 MiB).

32 KiB is two limits meeting: with the 64 KiB frame it is under 1 ms of wire
time at gigabit, and it lasts 260 µs at gigabit before the link would idle,
which a woken drainer meets easily. Over loopback in a release build, 16–128 KiB
all carried bulk at the same 130–180 MB/s as the unbounded socket (the cipher is
the limit there, and that is past gigabit); behind a 16 MiB backlog, input
arrived after 0–256 KiB of it bounded against 0.75–1.5 MiB unbounded.

**Lag is a number.** Each frame records how long it took from `send` to the
socket accepting it, per class, in ten-second windows. The drainer logs the
input p50/p99/max (with video and bulk p99 beside it) at `debug`, or `info` when
input p99 reaches 4 ms; `TransportTx::queue_delay` returns the last complete
window.

## Where a phone's notification goes

A phone's notification is shown on the computer the user is at, and only
there. Each computer says whether the user is at it (`NotifyMsg::Presence`:
holding its own cursor, input in the last five minutes); the phone sends to
those that are, and to all of them when none is.

Five minutes is right for a desk — reading a long page is not leaving — and
wrong the moment the user gets up and takes the phone: every message for the
next five minutes lit up an empty desk. So the phone answers one more question
itself, in `core/src/at_phone.rs`: **is the user holding me?**

- **The strong signal is order.** The phone's screen on and unlocked is
  someone using it — the screen times out without a touch, so this is also
  "touched recently", without reading anyone's input. A computer says when it
  is in use right now (`NotifyMsg::Activity`: input in the last 15 s, and how
  long ago when that changes). The user is at the phone when it was picked up
  after the last input on every computer; any computer that sees input after
  that has them back. Picking the phone up at the desk while still typing
  keeps the notifications on the computer; stopping to read the phone keeps
  them on the phone, where the user is looking.
- **The weak signal is movement.** The phone's own Wi-Fi signal falling by
  10 dB or more within 90 s while it is in hand looks like being carried out
  of the room. It only decides when a computer is too old to say whether it
  is in use; it never overrides one known to be in use, because a hand over
  the antenna moves the signal too.
- **Not used: Bluetooth distance.** Android has no public API for the signal
  strength of a connected classic link, reading it on Linux needs a raw HCI
  socket (`CAP_NET_RAW`), and Bluetooth Channel Sounding, which measures
  distance properly, is absent on the phones here (`pm list features` shows
  no `channel_sounding`). The decision does not depend on it.

`Activity` was added without bumping the module's version: a phone too old to
decode it drops it and keeps deciding by `Presence` alone, which is exactly
what it did before. Everything here rides the notify module; there is no
second channel.

## A session outlives its connection

A session used to be one TCP connection. Wi-Fi gone for two seconds meant
fifteen seconds of silence before the session noticed, then every module torn
down — disks unmounted, streams closed, the cursor sent home — and rebuilt once
the mesh dialled again. dark, on a b43 radio that loses its access point about
once an hour for a median of eight seconds, paid that every time.

Now the session rides a `ResumableLink` (`transport/src/resume.rs`) and
connections are *paths* under it:

- **Sequenced, acknowledged, resent.** XMPP's stream management (XEP-0198) is
  the model. Inside Noise, every frame of the session is numbered (`SEQ`); each
  side keeps saying the highest number it has delivered (`ACK`, also the
  path's keepalive, every second); a sender keeps what is unacknowledged. A
  path joining the session opens with `RESUME` and the number delivered, each
  side resends the rest, and the receiver drops anything not past what it
  delivered — so every frame reaches the session at most once, which for a key
  press is the difference between typing once and twice.
- **Fresh state is not replayed.** Pointer motion, scroll, sound and pixels
  (`Message::is_lossy`) are numbered but never kept: replayed after a gap,
  motion would jump the cursor by everything it moved meanwhile and old sound
  would play over new — Mosh's lesson. Presses and releases are never lossy.
- **Which connection joins.** `Hello::resumable` lists every session a node
  holds open (it cannot list the one with the caller: it does not know who is
  calling until the greetings cross). Each side looks for its session with the
  other in the other's list (`PeerRegistry::greeted`): present in both, the
  connection joins it (`link::join`, Noise with the stored key, then
  `ResumeHandle::attach`) and no module is built; missing from the peer's, the
  peer restarted or gave up, and a session of ours with no path left is ended
  so a fresh one starts. Session tags come from the Noise handshake hash, the
  same on both ends; they are not secret, since a joining connection must still
  complete Noise with the pair's key.
- **Path health.** A path silent for 3 s (45 s for a standby, whose keepalive
  is every 15 s) is let go of; so is any connection whose sent data stays
  unacknowledged for 10 s (`TCP_USER_TIMEOUT`, `transport/src/dead_peer.rs` —
  keepalive alone never fires while data is queued). A carrying path whose
  frames stay unacknowledged for 3 s while another path exists is *degraded*
  for 10 s and the session moves — the stale-subflow idea from multipath TCP,
  with a hold so a marginal Wi-Fi does not make it flap. The system's own word
  ends network paths at once: NetworkManager's state on Linux
  (`core/src/network_watch.rs`), `NetworkCallback.onLost` on Android
  (`resume::network_lost`).
- **Holding.** With no path the session keeps running and its loop does not
  time the peer out (`TransportTx::holding`); the registry offers the peer for
  dialling again (`needs_network_path`), and after 60 s (`resume::HOLD`) with
  nothing back the link ends the session as before.
- **Bounds.** At most one network and one Bluetooth path; a newer one replaces
  the older. Over Bluetooth, pixels are not sent at all. Past 64 MiB kept for
  resending, the peer has stopped acknowledging for far longer than any path
  survives, and the session can no longer be resumed.

Two halves of one session no longer race: a session both ends hold is never
also brought up fresh. `claim_link` still settles two *fresh* connections that
cross.

## Two paths, one mesh: the network and Bluetooth

A phone with Wi-Fi off, or out of the house's range, or on a hotspot the
computer is not on, is still the same peer — and so is a laptop whose Wi-Fi
card claims to be connected while passing nothing. Each keeps the same link,
carried by Bluetooth. **Bluetooth is a second path, not a second protocol**: the bytes
on it are the mesh's own, the same `Hello`, stored key (or pairing), Noise
`NNpsk0`, module negotiation and priority classes as over TCP. Only the socket
differs: `cursio_transport::stream::StreamTransport` frames messages over any
byte stream, and the session layer is written against `Transport`, so nothing
above it can tell. (HFP and A2DP, Cursio as a phone's headset and speaker, are
the deliberate exceptions, like AirPlay: Android hands a call's voice and an
unprompted copy of its sound only to those profiles. They are not this path.)

- **A phone dials; computers listen and dial each other.** Android keeps an app
  from learning its own Bluetooth address and keeps no listener, so a phone is
  never called. Every computer listens under Cursio's own UUID
  (`cursio_bluetooth::MESH_UUID`, `MeshChannel`), preferring RFCOMM channel
  23 (`MESH_CHANNEL`) and advertising the actual allocation through SDP.
  The mesh loop runs each accepted socket like an
  accepted TCP connection. On Linux that is a BlueZ external profile (only
  paired devices may open it, `RequireAuthentication`); on Windows a Winsock
  `AF_BTH` listener with an SDP record from `WSASetService`, its sockets pumped
  by two threads each rather than handed to Tokio, since nothing documents
  Winsock's Bluetooth provider under readiness polling. `WSASocketW` keeps both
  `WSA_FLAG_NO_HANDLE_INHERIT` and `WSA_FLAG_OVERLAPPED`: the first prevents
  child processes retaining listeners, while the second preserves concurrent
  send/receive capability that `socket()` enabled by default. The pump still
  uses blocking calls; omitting OVERLAPPED can stall a send behind a pending
  receive even after the greeting succeeded.
- **Discovering the channel.** Linux queries fresh records using BlueZ's
  `sdptool browse --xml`, then opens a raw RFCOMM socket on the channel in the
  Cursio service's protocol descriptor. Discovery is bounded to eight seconds
  and 64 KiB of output; discovery and connect share a fifteen-second deadline.
  The preferred channel is a fallback when the helper or query is unavailable.
  This bypasses `Device1.ConnectProfile`'s pairing-time service cache and reaches
  Windows listeners that allocate another channel. Windows queries SDP by UUID.
  Linux probes a channel before registering its profile because BlueZ can
  register an occupied channel without providing a working listener.
- **Finding the machine** needs no discovery protocol: each node lists the
  computers it is paired with in the system's Bluetooth settings (class of
  device "computer") and asks each for the Cursio service. Which mesh peer
  answered is learned from `Hello`, as on TCP, and the registry remembers it
  per address (`PeerRegistry::note_bluetooth`) — and so does the trust store
  (`TrustedPeer::bluetooth`), so a restarted node does not page every paired
  computer, the ones on the network included, to learn again whose radio is
  whose.
- **The network wins.** A known machine's address is called once *its* peer
  has been off the network for 20 s (`bluetooth_link::NETWORK_GRACE`); an
  address nobody has answered on yet, while any peer is missing, which is how
  it gets learned — or every one at once, as a standby, with
  `[bluetooth] standby = true`. Either way the Bluetooth connection is a *path*
  of the session, not a session of its own (see "A session outlives its
  connection" above): when either side dials over the network again, that
  connection joins the same session and carries it, and the side that placed
  the Bluetooth path lets it go unless it is kept as the standby.
- **Radio time is rationed.** Paging a device that is not there holds the radio
  for seconds, which on a shared antenna is seconds of Wi-Fi gone. A device
  that does not answer is retried after 30 s, doubling to 5 min; one that
  answers with no mesh channel after 30 min if unknown, or 30 s if already a
  known Cursio peer. A new OS Bluetooth connection clears the backoff once;
  one call runs at a time (`bluetooth_link::Dialer`). Noise establishment has a
  twenty-second deadline, so a stalled greeting cannot hold a path forever.
- **A machine can be left out.** `[bluetooth] mesh = "auto" | "on" | "off"`,
  read on every turn of the dialler. `auto` is on unless the Wi-Fi card is one
  known to drop off the network while Bluetooth is busy (Broadcom `b43` on
  Linux, `bluetooth_link::radio_conflict`); then it is off and a problem says
  so, with *OK* (`off`) and *Use bluetooth anyway* (`on`) as buttons
  (`Problem::actions`), so answering it also ends it. The other machines simply
  reach that one over the network only.
- **What fits.** RFCOMM carries a few hundred kilobytes a second. The priority
  classes already keep input, notifications, messages, calls' control and Opus
  audio ahead of screens and files, so those keep working and the bulk classes
  simply go slowly.
- **Visible.** A link's path is `LinkView::path` (`network` or `bluetooth`,
  control v12): `cursio status` shows `bluetooth` for such a peer, the GNOME
  menu a Bluetooth icon, the app a badge. It follows the session, not the
  connection that started it (`ResumeHandle::on_paths` →
  `NodeState::link_paths`): a session that moved to Bluetooth when the
  network went shows Bluetooth, and the network again once it carries it.

On macOS the computer's half is not built yet and says so in the log; peers
reach a Mac over the network only.

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
| Priority to the wire | drainer, queue delay — `transport/src/priority.rs`; kernel backlog — `transport/src/send_backlog.rs` |

`PeerState` gained a third state the original design did not name: `Connecting`,
so the supervisor does not start a second attempt for a peer that already has one
in flight.

Everything between "there is a socket" and "a session can start" lives in
`link.rs`: preamble → `Hello` → SPAKE2 pairing on first meeting (or the stored
key) → Noise `NNpsk0` → module negotiation → cursor ownership.

**The preamble is frozen.** Every connection opens with the update lane's
`Preamble` (`protocol/src/update.rs`), hand-encoded and pinned by tests, which
carries the sender's `PROTOCOL_VERSION`. Only a peer on the same version is
sent a `Hello`. A peer on another version still reaches Noise with the pair's
stored key and then carries the update lane and nothing else — it is how the
machine that is behind gets the build it needs (`docs/update-design.md`). A
peer from before v19 opens with its old `Hello`, is recognised by variant 0 and
its first field, and is told plainly that it has to be updated by hand once.

**Sessions are `!Send`.** The input path shares its injector and cursor via `Rc`
deliberately, so peer sessions are spawned with `spawn_local` onto a `LocalSet`
and the registry is `Rc<RefCell<_>>`. This is single-threaded by design and is
not to be "fixed" with `Arc`/`Mutex`.

## Inside a session: one loop for the cursor, one thread per module

A session used to be one loop per peer: each turn pumped every module, then a
`select!` over capture, ownership commands, the heartbeat and inbound messages,
with every inbound module message offered to the modules in turn. Modules are
logically independent but were not isolated, so any one of them waiting froze
the rest — and the cursor. In one day: audio joining a PipeWire thread
(380 ms), files holding its server lock across disk reads (0.3–1.9 s every
2 s), window listing windows through `GetWindowText` (50–220 ms), and every
input event pumping every module until pointer motion queued for seconds. Each
was fixed where it happened; the structure below is what makes the class of bug
impossible rather than rare.

**The session loop** (`session.rs`) keeps only what is `!Send` and
ordering-sensitive: forwarding captured input, injecting the peer's,
`CursorRole` and the handoff messages (`Grab`, `CrossEdge`), the hotkey,
heartbeat and liveness, and `NodeStatus`. The ownership logic itself is
unchanged — its ordering is what several past races were about.

**Each module** runs on a dedicated OS thread (`module_host.rs`), driven with
`Handle::block_on` on the runtime that owns the link. A thread rather than a
task, because blocking is exactly the failure being contained: a task that
blocks holds a runtime worker and whatever is queued behind it. The link's
runtime rather than a private one, because modules spawn things (a drag
watcher, an AirPlay forwarder, portal connections) that must not die with the
module's thread. The `SessionModule` trait kept its shape — `start`, `pump`,
`handle_message`, `shutdown`, `has_pending_work` — plus an `id`, since every
module was already `Send` and never needed the loop, only a `tx`. Pacing (5 ms
while a module has traffic, 50 ms after 100 ms of quiet) is per module now,
following that module's own sends, inbound and pending work.

**Sending**: each module has its own clone of the prioritized sender
(`TransportTx::clone_sender`), a handle onto the same queue, so its frames are
scheduled against input exactly as before and nothing serialises on a lock the
session also takes. `queue_depth` still reads the shared queue.

**Receiving**: the reader task routes by `Message::module_id()` straight into
the named module's inbox. Only the session's own vocabulary reaches the session
loop, so a flood of file or window traffic never sits between two input
events. A module the peer addresses but that did not negotiate here gets
nothing, and a module falling far behind logs its inbox depth rather than
failing silently. Inboxes are unbounded because the reader carries the
heartbeat and the cursor and must never block.

**Liveness** is stamped by the reader on every message, module traffic
included, and checked on the session's heartbeat tick — so a wedged module can
neither fake nor hide a dead peer.

**Teardown** stops every module at once and waits at most 3 s
(`MODULE_SHUTDOWN_GRACE`) for all of them; a module still stuck in a blocking
call is detached with a warning naming it and its sender closed, so the link
closes and the mesh can reconnect. A module is dropped on its own thread, so
the joins some modules do on drop no longer happen on the loop either.

The slow-pump (50 ms) and slow-message (20 ms) warnings survive, scoped to the
module: they now mean that module's own traffic waited, not that input did.

**Input lag is a number.** The session keeps latency windows and logs
p50/p99/max every 30 s when input moved (`warn` once p99 reaches 20 ms):
`received->injected`, from the reader taking an `Input` off the wire to handing
it to the injector, and `captured->sent`, from capture handing an event over
(`CapturedInput` is stamped as it leaves the hotkey filter) to the transport
accepting it — for coalesced motion, charged at its oldest part. The transport
measures the remaining stretch, queue to socket, per priority class. A third
window, `injector call`, is the injector's own share of `received->injected`;
the rest is queueing. Measured
over loopback with one module wedged in its pump for 30 s and another taking
5 ms per message while both were flooded: 2000 pointer events injected at p50
42 µs, p99 76 µs.

**Received motion is merged when it has queued.** The injector, not the link,
bounds how fast input lands: `SendInput` costs 1–2 ms per real pointer move on
the Windows laptop, and a 1000 Hz mouse sends one every millisecond. One call
per message let the backlog grow for as long as the hand moved — 24 ms median,
240 ms p99, the cursor replaying a movement after the hand had stopped. The
loop now drains every input message already queued behind the one it woke for
and sums consecutive motion, exactly as the sending side does with captured
input; a button, key or scroll keeps its place, and the first non-input message
ends the batch and is handled next, since a `Grab` decides whether the input
after it is injected at all. Over loopback with a 1.5 ms injector and 1000
moves a second: p50 3.3 ms, p99 3.5 ms, where one call per message gave p50
272 ms, p99 519 ms.

