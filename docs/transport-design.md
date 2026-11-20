# Independent delivery lanes (session protocol v23)

Cursio keeps one authenticated, resumable **session** per peer, with four
independent Noise-over-TCP connections. Connections use the existing advertised
port, pairing key and mesh dial/admission path; no auxiliary listening ports or
firewall exceptions are needed. The registry keeps dialling an online peer until
all four network lanes exist, and redials an individual missing lane without
rebuilding modules.

| Lane | Priority classes | Purpose |
| --- | --- | --- |
| Interactive | Control, Input, Metadata | Session state, cursor, keys, small requests |
| Audio | Audio | Stream descriptions and audio chunks |
| Video | Video | Window descriptions, pixels and presentation acknowledgements |
| Bulk | Bulk | Files, clipboard payloads, gallery bytes, updates |

Each connection has its own Noise state, writer and bounded inbox. Each lane has
its own sequence numbers, cumulative acknowledgements, reassembly and resend
buffer. A blocked bulk writer cannot park the interactive worker; a higher
sequence received on one lane cannot discard an older frame on another.
Ordering is guaranteed within a lane, not across lanes. A module's stream
initialisation and its stream data must use the same lane, or an explicit
request/response dependency. Audio and window modules already keep their stream
control in their corresponding classes.

## Bounded memory and queueing

The scheduler admits complete messages atomically. A cancelled admission queues
nothing. Reliable producers await capacity rather than dropping data or growing
an unbounded queue. Limits per priority class are:

| Control | Input | Audio | Video | Metadata | Bulk |
| --- | --- | --- | --- | --- | --- |
| 256 KiB | 256 KiB | 512 KiB | 4 MiB | 1 MiB | 8 MiB |

For an individual message larger than its class budget, the empty class admits
that one message; later messages wait. The existing logical-message limit still
applies. Consequently these are normal queue budgets, not a claim that a 64 MiB
clipboard object occupies only 8 MiB. Admission does not queue part of a message
and then await the rest, which would break reassembly when cancelled.

Each lane keeps at most 16 MiB of unacknowledged reliable frames. At that limit
its drainer waits for acknowledgements; it never silently abandons resumability.
There are 16 incoming frames per lane and two frames per socket writer, plus the
frame currently being written. Reassembly has the existing bounded-message
checks, independently per lane. Kernel unsent-backlog limits remain in force on
every TCP socket (`channel/src/tcp/send_backlog.rs`).

`queue_age` reports the oldest queued or currently sending frame. Completed
queue-delay histograms and byte counters remain available. `flush` waits for
frames already removed from the queues as well as frames still queued.

Capture already uses a bounded channel and resets its encoder/differ when a
capture update cannot be queued. The window pumps stop pulling at their video
high-water mark. Encoded reference frames are not discarded in the transport,
not even for age (see "Delivery" below): their dependants make a dropped one
cost a whole picture, which not every host can send. After a connection gap,
stale lossy data is not replayed; every viewer — a surrogate window, a
remote-screen node, the phone screen kept for `cursio phone` — sees its frame
numbers skip and asks for the whole picture (`frame_gap.rs`).

## Process queues and input overload

Transport budgets alone do not bound the work waiting inside the process.
`cursio-queue` also bounds these application queues by both count and estimated
bytes (message estimates include 256 bytes of envelope overhead):

| Queue | Items | Bytes |
| --- | ---: | ---: |
| Raw desktop input capture | 4096 | 512 KiB |
| Captured input for a peer, including Android | 4096 | 512 KiB |
| Session inbound control/input | 4096 | 4 MiB |
| Session outbound control/input | 512 | 1 MiB |
| Each module inbox | 4096 | 64 MiB + 256 bytes |
| Parked messages across missing modules | 256 | 64 MiB + 256 bytes |

Adjacent relative motion is added with checked arithmetic at capture, peer
input and session queues. Keys, buttons, scroll and ownership messages remain
boundaries; absolute coordinates are not replaced. An integer overflow leaves
separate events instead of saturating and losing displacement. Aggregation
preserves displacement and ordering boundaries, not the original trajectory.

Admission never awaits network capacity in the main cursor/ownership loop.
A separate session sender waits on transport backpressure. Each input turn
drains at most 128 events, with a 2 ms capture-drain budget; local and incoming
input alternate when both are ready. Shutdown restores local cursor ownership
before waiting for modules, and final outbound drain has a two-second deadline.
Blocking operating-system calls can still take longer than an input turn.

On application-queue overflow the stream fails explicitly and clears stale
work. The affected session ends and releases its held input; it does not drop a
key release and continue a partially valid stream. Raw capture overflow disables
the grab and reports a health problem requiring a Cursio restart. These are
overload recovery policies, not a guarantee of uninterrupted input under
arbitrary load. Low-rate ownership, edge and other internal command queues are
not all bounded yet.

`process queue health` reports count/byte peaks, merged events and observed
event ages over ten-second windows while the queue is active, at debug level
when something waited (a second item, a merge, or an age of 10 ms) and at
trace level otherwise. `captured->queued`
measures admission to the local session sender; it is not wire delivery.
`received->injected` and `injector call` cover receiver-side work. Separate
percentiles cannot be added to claim an end-to-end percentile; hardware-to-display
latency still needs a dedicated measurement. See `docs/live-test.md`.

## Background bandwidth and bufferbloat

All producers in the bulk lane share a pacer, including retransmissions. It
starts at 1 MiB/s, has a 64 KiB/s floor and a 128 MiB/s ceiling. At most one frame
is released at a time; there is no accumulated idle token allowance to dump into
the network when a transfer starts. Each frame's slot follows the previous
slot rather than the moment the pacer woke, catching up at most 20 ms: Windows
wakes a sleeper on its 15.6 ms tick, and counting from the wake capped every
Windows machine's bulk at one 64 KiB frame per tick, about 4 MB/s.

Authenticated ping/pong probes travel on the interactive connection and are
answered by its reader, independently of application work. They run once per
second when idle and every 200 ms while bulk traffic is active. The controller
tracks the minimum RTT observed on the current control path over the last
three minutes, kept as 10-second bucket minima — not over the whole session,
which pinned bulk near its floor once a Wi-Fi link's quiet RTT rose after a
roam and every reading looked like queue. The queue is the lowest of the last
three readings above that baseline: a Wi-Fi link's quiet RTT spikes by tens of
milliseconds on its own, and each lone spike used to cut bulk by 30%. Queueing above
that baseline by more than 15 ms reduces the bulk rate by 30%; a clear path
raises it by 10% plus 16 KiB/s, at most once per 200 ms — doubling instead
until the first sign of a queue since bulk started, as TCP's slow start does:
at 10% from 1 MiB/s a transfer took six seconds to reach the 18 MB/s a Wi-Fi
link carried, and bursty reads that start over after each pause never left the
bottom of the ramp. Each back-off is logged at debug with the session, the
reading and the baseline it stood above. An overdue probe is also
a congestion signal. Idle observations update the baseline without raising the
next transfer's initial rate. A replacement control path resets the estimate.

The 15 ms target holds only while a person's input crosses the link, either
way, and for two seconds after; with nobody at the input bulk may queue 60 ms.
Measured to a Windows laptop on Wi-Fi, 15 ms all the time carried files at a
third of the link, while a generous target all the time carried them at
12–14 MB/s but let the laptop's radio lose the interactive lane's packets, each
loss a 300 ms retransmission timer, putting the pointer's 95th-percentile
round trip at 50–130 ms. So the first input event after a quiet spell halves
bulk at once, before any reading could show the queue, and the strict target
returns with it. Each end judges from what it sees: input it sends, and input
it receives and injects (`TransportTx::note_input`). The link test's pings
ride the input class but are not input, and do not count.

These are application-level heuristics on top of TCP congestion control. They
protect latency but cannot guarantee a latency bound through arbitrary router
queues, radio interference or a saturated external sender. They need real LAN
measurements before their constants can be treated as tuned defaults.

## Video adaptation

Presentation acknowledgements continue to drive each desktop encoder's existing
quality/bitrate ladder. In addition, the window module maintains one aggregate
video budget per peer connection, shared by active transferred windows and the
remote-screen capture. It begins at 12 Mbit/s, bounded to 256 Kbit/s–200 Mbit/s.
Every 500 ms, a queue older than 75 ms reduces the budget, bounded also by the
observed departure rate. While data flows without a backlog it grows by 10%.
Idle periods do not raise it. Congestion disables lossless H.264 for four seconds
beyond the last pressure reading, avoiding rapid codec changes as a queue
briefly empties. Linux and Windows adaptive encoders cap their per-window
bitrate by this allocation as well as their own presentation feedback.

The Android screen receives the allocation too. Its one shared encoder uses the
minimum target of its live viewers; ending a slow view releases its cap. Existing
presentation feedback remains another bound. Forwarded AirPlay is already
encoded upstream and cannot obey a local encoding target without transcoding;
the macOS capture helper likewise has no live encoder control yet. They still
use bounded queues and independent video delivery.

## Delivery

A message's kind declares two things, not one: its `Priority`, which says what
goes first, and its `Delivery` (`protocol/src/lib.rs`, `Message::delivery`),
which says what a late copy is worth. They are independent — sound is urgent
and worthless once late, a directory listing urgent and needed however late, a
file chunk neither — and the module that owns a message is the only party that
knows the second. Delivery is not on the wire: each end decides for what it
sends, so a message's choice changes without a protocol bump.

- `Reliable`: exactly once, in order within its lane, resent after a dropped
  connection. The default.
- `Fresh { within: None }`: never resent after a dropped connection (Mosh's
  lesson). Pointer motion and scroll, local and in a transferred window. A move
  waiting behind a stalled one is folded into it rather than dropped for age:
  on a Bluetooth path, where a phone's audio pauses the radio for a second at a
  time, dropping would lose motion the hand really made.
- `Fresh { within: Some(t) }`: also dropped whole, unsent, by a sender that
  reaches it more than `t` after it was queued (`channel/src/scheduler.rs`). Sound, at
  `audio::FRESH_FOR` (250 ms, the same bound the listener's playback feed
  applies on arrival). Only a message's first frame is judged; one part-way out
  is finished. Drops are counted per class and logged with the queue-delay
  window.

Window frames are `Fresh { within: None }`, though a late picture is as
worthless as late sound. A chunk of sound stands alone; a frame does not, and a
dropped one costs a whole picture. AirPlay and a Mac host cannot send one on
request. A tile update cannot even be seen to need one when what went missing
was one of its bands sharing a number, or its last frame before the window
went still. And on a slow path the resent picture outlives the deadline of
every frame behind it, so each drop asks for the next. Dropping video for age
needs the host to say it can resync and each update to say how many frames it
has; until then video is held back by its bitrate budget and the pump's
high-water mark.

The limit of doing this over TCP is plain: the choice can be made only until a
frame is handed to the socket. After that the kernel resends a lost segment
however late, and everything behind it in that lane waits. Lanes keep one
class's loss from holding another; nothing keeps a loss from holding its own
lane. That is the cost a datagram transport would remove, for exactly the
`Fresh` messages.

### Measuring before choosing a transport

Three figures decide whether that cost is worth a new transport, and each is
now logged:

- **Retransmissions per lane** (`channel/src/tcp/info.rs`, every 10 s at debug): the
  kernel's own count of what it sent and resent on each network path — Linux
  `TCP_INFO` in segments, Windows `SIO_TCP_INFO` in bytes. A lane that resends
  nothing gains nothing from skipping a loss.
- **Video frame round trip** (`frame_pipeline/quality.rs`, every 10 s at
  debug): from a frame being queued to the viewer's ack naming it — queue, wire,
  decoder, paint and the ack's return. An upper bound by up to one frame
  interval, taken only while video flows.
- **Sound** already reports arrival gaps and lateness (`audio health`), and the
  link's interactive round trip is logged by the session.

### Datagrams: QUIC or our own UDP

If the figures show lanes stalling behind resends, `Fresh` traffic moves to a
transport that may skip a loss. Two shapes were weighed:

| | A UDP lane beside the TCP ones | QUIC (`quinn`) as the network path |
|---|---|---|
| `Reliable` | stays on the four TCP lanes | QUIC streams, one connection |
| `Fresh` larger than a datagram | ours to write: fragmenting, reassembly, deadline, loss report | a stream per message, reset at its deadline |
| Small `Fresh` | ours to write | RFC 9221 datagrams |
| Congestion control | the kernel's for TCP beside ours for UDP, contending for one radio | one controller for everything |
| Loss detection, MTU, replay window | ours to write | done |
| Keys | Noise as now | TLS 1.3; see below |
| Bluetooth | unchanged | unchanged |

Either way some things do not move. TCP stays: every connection opens with the
frozen `Preamble`, and that is how a build of any age is updated, so a TCP
listener is permanent, and it is the fallback where UDP is blocked. Bluetooth
stays a byte stream under `ResumableLink`; on it `Fresh` is honoured the only
way a stream allows, by dropping at the sender, which is what the TCP lanes do
today. And Windows needs an inbound UDP firewall rule, whose absence must show
in `cursio path` rather than as a quiet fall back to TCP.

TLS is not the obstacle it first looks like. Pairing stays the root of trust:
each node gains a long-term key (a self-signed certificate or a raw public
key), and its fingerprint is exchanged over the already-trusted Noise session
the first time two builds that both speak QUIC meet, so no machine is paired
again and TLS only checks a pinned key.

QUIC is the choice if the move is made. Video with a deadline needs
fragmenting, a deadline per message, a loss report to the encoder and a
congestion controller that does not fight the kernel's — about half of QUIC,
written fresh. Our own UDP lane wins only if the move stops at sound and small
datagrams. The `Delivery` API is the same for both, which is why it came first.

## Recovery and path authentication

The encrypted RESUME exchange includes the session tag, four delivered
watermarks and the proposed missing lane. Both ends select the same lane even
when only one has noticed its failure. A path for a different session is
rejected after authentication. Each socket is authenticated separately; the
initial plaintext greeting alone never grants attachment.

Reliable frames are resent in order on a replacement lane and deduplicated in
that lane's sequence space. Motion, audio and pixels are not saved for replay.
Acknowledgements are handled by socket readers, outside bounded application
inboxes, so an application sender waiting for resend space cannot prevent that
space being freed. ACK snapshots are cumulative and can be retried if a writer
inbox is full.

Bluetooth remains one shared standby, carrying the lane identifiers and the
same encrypted protocol; it does not provide TCP-like lane isolation. Its
writer keeps one inbox per lane and always takes the interactive one first,
then audio, then bulk, and the queue below it is kept small (8 KiB: the
socket's send buffer on Linux, the pump's pipe on Windows), because an
RFCOMM socket's default buffer held over a second of other traffic in front
of every pointer move. A frame already being written still finishes first, so
while Bluetooth carries the interactive lane a message is cut into 4 KiB
frames rather than 64 KiB ones: a full frame held input for most of a second
on the radio, a small one for tens of milliseconds. Receivers accept any piece
size, so this needs nothing of the peer. A carrying Bluetooth path is let go
of after 10 s of silence rather than 3 s, since it is usually the only path and
the radio pauses for seconds while it pages another device. Pixels are not
sent over it.

Classic Bluetooth carries well under the thousand packets a second a 1000 Hz
mouse produces, so on any link that cannot keep up a pointer move that finds
the previous move still waiting in the input queue is added into it (relative
deltas sum exactly; a button or key between them stops the merge). This is
what a Bluetooth mouse itself does per connection interval. A link that keeps
up never has a move waiting, so nothing changes there.

What leaves a path's writer leaves in as few writes as it can: everything
already waiting in its inboxes, most urgent lane first and up to 16 KiB, is
sealed and written at once (`TransportTx::send_batch`). On RFCOMM a write is a
frame of its own, and a frame of tens of bytes takes a radio packet nearly as
long as a full one; written one by one, a pointer's moves waited up to 49 ms
to reach the socket in a measured minute and arrived in jumps. The interactive
inbox holds 32 frames rather than 2 so a burst can travel together; it is
still emptied before any other lane. TCP keeps one write per frame. A fully idle Bluetooth standby keeps its sparse 15-second
keepalive. Loss of the interactive path holds the session even if a data socket
is still alive; a usable control path must return within 60 seconds. Modules,
mounts and capture objects remain intact while individual paths reconnect.

This changes session framing and increments `PROTOCOL_VERSION` to 23. Peers on
older session versions use the existing frozen update lane; ordinary sessions
require matching versions.

## Verification

Transport tests cover a permanently blocked bulk writer with working input,
audio and video; lane-local reconnect with reliable replay; independent sequence
spaces; rejection of a connection for another session; bounded admission and
cancellation; and flush including an in-flight frame. Existing encryption,
chunking, reconnect, Bluetooth fallback and loopback tests remain applicable.
Video tests exercise aggregate backoff, idle behaviour and encoder caps.
Hardware validation is still required on Windows, Android and congested Wi-Fi;
unit tests do not establish end-to-end latency or throughput on those systems.

## Measuring a link

`cursio link test <machine>` (module `probe`, id 17, `core/src/probe_session.rs`)
loads the bulk lane each way for a few seconds with requested 256 KiB pieces —
a mount's shape, with no disk on either end — and pings at the input class
throughout: at rest, then under each direction's load. The answer is speed
each way and the pointer's round trip under load, which is what decides
whether a file copy is felt in the mouse. One end of each pair of computers
also tests by itself every 12 h at a quiet moment, never over Bluetooth and
never with a phone; results are kept in the data directory
(`link-tests.jsonl`, `cursio link history`).
