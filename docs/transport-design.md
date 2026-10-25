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
every TCP socket (`send_backlog.rs`).

`queue_age` reports the oldest queued or currently sending frame. Completed
queue-delay histograms and byte counters remain available. `flush` waits for
frames already removed from the queues as well as frames still queued.

Capture already uses a bounded channel and resets its encoder/differ when a
capture update cannot be queued. The window pumps stop pulling at their video
high-water mark. Encoded reference frames are not arbitrarily discarded in the
transport: their dependencies matter. After a connection gap, stale lossy data
is not replayed, and the existing keyframe/resynchronisation path repairs video.

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
event ages over ten-second windows while the queue is active. `captured->queued`
measures admission to the local session sender; it is not wire delivery.
`received->injected` and `injector call` cover receiver-side work. Separate
percentiles cannot be added to claim an end-to-end percentile; hardware-to-display
latency still needs a dedicated measurement. See `docs/live-test.md`.

## Background bandwidth and bufferbloat

All producers in the bulk lane share a pacer, including retransmissions. It
starts at 1 MiB/s, has a 64 KiB/s floor and a 128 MiB/s ceiling. At most one frame
is released at a time; there is no accumulated idle token allowance to dump into
the network when a transfer starts.

Authenticated ping/pong probes travel on the interactive connection and are
answered by its reader, independently of application work. They run once per
second when idle and every 200 ms while bulk traffic is active. The controller
tracks the minimum observed RTT on the current control path. Queueing above
that baseline by more than 15 ms reduces the bulk rate by 30%; a clear path
raises it by 10% plus 16 KiB/s, at most once per 200 ms. An overdue probe is also
a congestion signal. Idle observations update the baseline without raising the
next transfer's initial rate. A replacement control path resets the estimate.

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
same encrypted protocol; it does not provide TCP-like lane isolation. Pixels
are not sent over it. A fully idle Bluetooth standby keeps its sparse 15-second
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
