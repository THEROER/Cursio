# Cursio — Audio Design (shared, per-application audio between machines)

Status: **implemented on Linux (PipeWire); partial on Windows (WASAPI).** There
is no macOS audio backend at all, so a Mac does not advertise the audio module.

The Windows gap is **endpoint parity**. Linux publishes a node per remote
application through PipeWire; Windows has no user-space way to publish an audio
endpoint, so making a peer's devices appear as native Windows endpoints needs a
kernel driver. `native/cursio-vad` — a fork of Microsoft's ACX AudioCodec sample
— is that driver, and it is **written but not yet in service**:

- both halves of the data path exist — `vad_client.rs` (IOCTL + ring) in user
  mode, and `CursioRing`/`CursioBridge`/`CursioIoctl` in the driver, with the
  stream hooks wired into `StreamEngine`, `RenderCircuit` and `CaptureCircuit`;
- dynamic endpoints are implemented, not planned: `Cursio_CreateEndpoint` /
  `Cursio_RemoveEndpoint` (`CursioEndpoint.cpp`) hang one render+capture child
  PDO per remote device off the parent via `AcxDeviceAddCircuitDevice`;
- what is missing is the last mile — no built and signed `.sys` ships, so none of
  it has been loaded and exercised on a real Windows box.

Until that lands, capture and render over WASAPI work, but a peer's device does
not appear in Windows' own device list. (`native/cursio-vad/CURSIO-FORK.md` still
describes these changes as planned; it predates the implementation.)

This document is the source of truth for how audio sharing should behave; code
should match it or this doc should be updated. Companion to `input-design.md`
(shared cursor/keyboard) and `window-transfer-design.md` (drag a window); audio
reuses their transport, pairing, and adaptive-rate machinery.

## What this replaces — and what it does NOT

This design comes from a concrete setup, and the boundary matters:

**Today (the thing being replaced):**
```
PC ──AudioRelay──► Laptop ──┐
PC ──Bluetooth(mic)──► Laptop │
                              ├─► VoiceMeeter Potato (mix / customize)
                              │        └──Bluetooth──► headphones
```
The transport is a zoo — AudioRelay + Bluetooth + a chain of hops — each adding
latency and a failure point. And Ubuntu's system volume is **ignored**, because
audio leaves via AudioRelay as a stream *outside* the system mixer: the volume
knob on the keyboard (plugged into the PC) controls nothing you actually hear.

**Cursio replaces the transport zoo (AudioRelay + Bluetooth), NOT the mixer.**
VoiceMeeter (Windows) and Carla (Linux) stay — they are the user's audio
processor with many tuned chains, and Cursio does not reinvent them. Cursio is a
**networked audio cable plus virtual devices**: on each machine it presents
sinks/sources that VoiceMeeter/Carla see as ordinary ins/outs and wire into
their existing graphs however they like. **Mixing and routing stay with the
user, in VM/Carla.** Cursio provides the pipes; the graph is yours.

## Model: MWB for audio, per application

Same shape as the input model ("the peer's mouse appears as an extra device"):
**the peer's audio appears on this machine as extra audio nodes**, and vice
versa. Two differences make it actually solve the user's pain:

1. **Devices, both directions.** Cursio publishes a virtual **sink** (things
   play into it → streamed to the peer) and a virtual **source** (audio arriving
   from the peer → published as a capture device the graph can pull from). On
   Linux this is the PipeWire null-sink pattern (the dev box's default sink is
   already a `virtual_speaker` null-sink — the exact mechanism).

2. **Per-application nodes across the network (the key requirement).** Cursio
   does not merge everything into one stream. Each application on a peer
   (Spotify, Discord, a game…) appears on the other machine as its **own named
   node** — "Cursio: Spotify (PC)", "Cursio: Discord (PC)". VoiceMeeter/Carla
   then see and route each app independently, and the keyboard knob can control
   any one of them. This is precisely what the system mixer cannot do today:
   AudioRelay delivers one merged blob, so per-app control across machines is
   lost. Cursio preserves the separation over the wire.

PipeWire fits this natively — every stream is a graph node with props
(`application.name`, `media.role`) — so per-app nodes are the natural unit, not
a bolt-on. Windows per-app capture uses WASAPI **process loopback** (Win10 2004+);
that is a backend concern, not a protocol one.

### Volume: logical control above the system

The knob on the keyboard is captured as an **input event** (Cursio already has
the input channel) and applied as a **logical gain** on Cursio's own nodes —
per-app or master — *above* the OS mixer. That is why it works when the system
mixer doesn't: Cursio owns these nodes, so it can attenuate the right stream
regardless of which machine the sound ends up on. (Whether a given knob turn
targets a specific app's node or a master is a routing/binding choice; the
protocol just needs to carry per-node gain.)

## Latency & jitter budget

Audio is less bandwidth-hungry than video but far less forgiving of timing:
gaps/underruns are instantly audible, and drift between a stream's clock and the
playback clock causes crackle or growing lag.

- **Codec:** raw PCM only for LAN bring-up (48 kHz stereo ≈ 3 Mbit/s per stream
  as f32 — fine on LAN, wasteful over Wi-Fi and multiplied per app). Steady
  streams use **Opus**, low-latency mode. The wire format tags the codec per
  stream so PCM↔Opus needs no protocol change, the same discipline as
  `TileCodec` for windows.

  The PCM on the wire is **f32, never s16**. PipeWire monitors are natively
  float, and an earlier bug had the wire claim s16 while the buffers were
  actually float32 — every backend now agrees on f32.
- **Framing:** small fixed audio chunks with a per-stream `seq`, so the receiver
  can order, detect loss, and run a **jitter buffer** — a few ms of slack traded
  for gap-free playback. Opus runs at **5 ms** frames (`FRAME_SAMPLES = 240`,
  `Application::LowDelay`), not the 10 ms this design first sketched.
- **Adaptive:** like windows, a lightweight ack (`AudioFrameAck`) lets the
  sender track fill/RTT and adjust Opus bitrate under pressure. The rule is the
  same — **drop/degrade rather than buffer unboundedly** — but audio keeps a
  *small* jitter buffer on purpose, unlike video which favors freshness.
- **Clock:** the receiver resamples to its own device clock to absorb drift
  (details are a backend concern; the protocol carries timestamps/seq to make it
  possible).

## Wire protocol

`crates/protocol/src/audio.rs`. OS-neutral, same discipline as `input.rs` /
`window.rs`.

**The shape changed after this was written.** Audio moved onto the module
envelope: everything below except the chunk itself is an `AudioMsg` variant
carried inside `Message::Module { module: ModuleId::AUDIO, .. }`, not a flat
`Message` variant. `Message::AudioChunk` stayed first-class because it is the hot
path. So `Message::AudioStreamAdvertise` below is really
`AudioMsg::StreamAdvertise`, and likewise for `FrameAck`, `Gain` and
`StreamClose`. The module carries its own `VERSION` (`ModuleId::AUDIO = 2`);
the `PROTOCOL_VERSION` bumps recorded further down are historical markers, not
the current value.

- `AudioStreamId` — session-scoped `u32`, one per logical stream (a device or an
  app node). Not any OS handle.
- `AudioFormat` — `{ sample_rate, channels, sample_kind }` (48000, 2, f32).
- `AudioCodec` — `RawPcm` (bring-up) / `Opus` (steady). Placeholder-extensible.
- `Message::AudioStreamAdvertise(AudioStreamMeta)` — announces a stream the peer
  can consume: `{ id, role (Playback/Capture), app_name: Option<String>,
  format, codec }`. `app_name` present ⇒ this is a per-app node; absent ⇒ a
  whole-device stream. Sent when an app starts playing / a device appears.
- `Message::AudioChunk(AudioChunk)` — `{ id, seq, timestamp, data }`: one small
  frame of samples encoded per the stream's codec.
- `Message::AudioFrameAck(AudioFrameAck)` — `{ id, acked_seq }`, sparse feedback
  for rate control.
- `Message::AudioGain { id, gain }` — logical volume for a node (per-app or
  master), driven by the knob. Applied above the OS mixer.
- `Message::AudioStreamClose { id, reason }` — app stopped / device gone.

## Backends (behind traits, like `InputInjector`)

Crate `crates/audio`, OS-agnostic traits; platform code slots in without
touching the session. Channels, not callbacks (a slow link never stalls capture).

- **Capture / source side:** enumerate & advertise streams, grab each app/device
  stream, emit `AudioChunk`s. Linux = PipeWire (per-node capture, null-sink for
  the virtual output). Windows = WASAPI loopback + process loopback for per-app.
- **Playback / sink side:** create local nodes from advertisements, play inbound
  chunks through a jitter buffer, expose them so VM/Carla can route them. Linux =
  PipeWire nodes; Windows = WASAPI render, with the `native/cursio-vad` driver
  (not WASAPI) responsible for making those nodes visible as real endpoints.

  There is deliberately **no fallback** on Windows when that driver is absent.
  The old one was actively harmful: every "mirror" captured the same full system
  mix, so per-app routing silently became a copy of everything. It now returns
  `Unsupported` with a warning instead.
- **Gain:** apply `AudioGain` to the right node.

Cross-OS works because the wire is neutral (PCM/Opus + neutral stream metadata);
each side maps its own platform (PipeWire ↔ WASAPI) to/from the wire types.

## Milestones for audio

1. **Protocol + trait scaffolding** — `crates/protocol/src/audio.rs`
   (`AudioStreamMeta` / `AudioChunk` / `AudioFrameAck` / `AudioGain` / formats /
   codecs), `Message` variants, version bump; `crates/audio` crate with capture
   (source) and playback (sink) traits + device/stream advertisement. Per-app
   node model and adaptive/jitter design baked in from the start. No backend.
2. **Vertical slice Linux→Linux (whole-device)** — PipeWire null-sink capture →
   PCM/Opus chunks over Noise → PipeWire playback node on the peer, with a jitter
   buffer. One device each way, no per-app yet. Proves the timing model.
3. **Per-app nodes (Linux)** — advertise each PipeWire stream (by
   `application.name`) as its own node on the peer; the whole point for the
   user's per-app volume control.
4. **Volume knob → logical gain** — capture the keyboard knob (input channel) and
   apply `AudioGain` per-app/master above the system mixer.
5. **Opus + adaptive rate control** — wire `AudioFrameAck` into Opus bitrate
   control; degrade-not-buffer under bandwidth pressure.
6. **Windows backend** — WASAPI render/loopback + process loopback for per-app;
   verify true cross-OS PC(Windows)↔Laptop(Linux) with VoiceMeeter/Carla in the
   graph.
7. **Bind audio to a transferred window** — when a window moves (see
   `window-transfer-design.md`), route its app's audio node along with it.

---

## Redesign: "one system" — Intent Sinks + replicated device state (2026-07-21)

Status: agreed with the user (and reviewed by a peer) 2026-07-21. Supersedes the
parts of the model above that it contradicts. Requirement: **audio must look like
ONE machine, not two linked ones.** helvum (Linux) / volume mixer (Windows) on
either side shows the devices AND applications of both machines as native local
objects.

**Implementation status: all seven phases below are ✅ DONE** protocol-, session-
and routing-side, with one platform gap. The device-state monitor has a Windows
backend (`IAudioEndpointVolume`, polled), so device volume/mute replicate
cross-OS.

The gap is the **Intent Sink itself (phases 2–3), which is Linux-only**:
`IntentSink::create` is `#[cfg(target_os = "linux")]` and returns
`AudioError::Unsupported` everywhere else, because it needs a routing observer
the WASAPI backend does not have. Windows publishes a peer's devices through the
`native/cursio-vad` kernel driver instead — see the status note at the top of this
document for how far that has got.

The defining insight: **separate routing from transport.** In AudioRelay/Deskflow
a device is both the place you pick and the pipe the audio flows through. Here a
mirrored device is ONLY a control/declaration surface; the audio travels on a
separate per-app path.

### Core concepts

**Intent Sink** (renamed from "mirror sink" — it plays nothing, but it is NOT a
mere placeholder). A real local `Audio/Sink` (Linux) / endpoint that is a **full
replica of a remote device's STATE**: it reproduces volume, mute, default/non-
default, name, icon, profile, connection state — everything about the device
EXCEPT that it does not play PCM. Audio routed into it goes NOWHERE locally;
Cursio only *observes which playback streams are connected to it* and reads that
as the user's intent: "everything in here should play on device X of the other
machine." So the Intent Sink is simultaneously (a) the device's local face
(replicated state) and (b) the routing declaration surface (observed membership) —
but never a transport.

**Remote Stream** (per-app transport). The actual audio path. Created lazily —
see lifecycle. Carries one playback stream A→B and is assigned, on B, to the real
target device.

**Route / Routing Group**. The logical object tying them together:
```
Route: target = Windows/VAIO3
  members = { Spotify, Firefox, Discord }   (all Ubuntu playback streams
                                              currently funnelled into the
                                              Intent Sink for VAIO3)
```
Operations (mute, disconnect, later latency/codec) apply to the whole group;
internally each member is its own Remote Stream.

### The rules (agreed answers to the open questions)

1. **Routing vs transport are separate.** Intent Sink = intent; Remote Stream =
   transport. A mirrored device never carries audio.

2. **Lazy Remote Streams (open question E → resolved).** Do NOT pre-create silent
   per-app streams. A Remote Stream exists only when a playback stream is BOTH
   producing audio AND funnelled into an Intent Sink. Disconnect it from the
   Intent Sink → its Remote Stream tears down after a couple seconds, like an
   ordinary audio session. No wall of idle "Chrome / Discord / Steam…" rows.

3. **Route by node TYPE, not app name (B → resolved).** Match on the PipeWire node
   being a playback stream, not on `application.name`. Anything representable as a
   playback stream is carried — Speech Dispatcher, Firefox, Wine, Flatpak, Snap,
   OBS, Steam — with no per-app special-casing. Nodes that can't be a stream are
   ignored. (On Windows the equivalent is a render audio session on the endpoint.)

4. **Multi-output fully supported (D → resolved).** PipeWire lets a stream fan out:
   `Spotify → { Headphones, Intent Sink }` means "play locally AND remotely." The
   double path here is INTENDED, not the duplicate bug.

5. **Pre-existing tap, lazy network (perf).** Every currently-playing stream
   already has a local tap (no network, no codec). When it's funnelled into an
   Intent Sink we just *open the network side* of the existing tap — minimal
   first-audio loss, versus building a tap from scratch on the link event.

### Device state is REPLICATED with a per-device owner (volume, mute, …)

This replaces "each machine fully owns its devices." A device is a **distributed
object** with two representations that stay in sync:
```
VAIO3   owner = Windows
  Windows view: volume 42%, mute false
  Ubuntu  view: volume 42%, mute false   (the Intent Sink shows the SAME value)
```

**A device has exactly ONE volume (open question C → RESOLVED, VCA dropped).** The
architectural rule, verbatim:

> A device has only one volume. It is replicated across all machines through the
> owner and is always identical everywhere. Any additional gain coefficient
> belongs to the ROUTE or the STREAM, not to the device. This keeps the model
> simple, predictable, and maximally like one physical computer.

Rationale: we are building the illusion that two machines are ONE computer. On one
computer, VAIO3 has one volume slider — not two. So the Intent Sink's volume IS
the real device's volume, replicated; there is no second slider and no separate
VCA on the device. The earlier "Intent Sink volume = a group VCA" proposal is
**rejected** — a VCA on the device would break the single-machine illusion.

**Where "make only the remote audio quieter" lives.** That is NOT a device
concern — it is a property of the **Route** (or an individual Remote Stream), to
be added later:
```
Route: Ubuntu → Windows/VAIO3   gain 80%   codec Opus   latency Low
```
and, if ever wanted, per-member:
```
Spotify → VAIO3  gain 100%
Discord → VAIO3  gain 60%
Game    → VAIO3  gain 40%
```
This keeps route/stream gain cleanly separate from the device's single replicated
volume, and never confuses the two.

**All changes go THROUGH the owner** — never applied independently on both sides:
```
Ubuntu knob: VAIO3 42→35  →  RPC to owner (Windows)  →  Windows sets real VAIO3=35
          →  broadcast  →  Ubuntu (and any other node) reflects 35
```
This removes the volume-war race entirely: with two equal writers, simultaneous
`Win 40→50` and `Ubuntu 40→20` have no principled winner (last-write? by net time?
by RTT? → chaos). Funnelling through the owner makes it deterministic.

**Owner is per-DEVICE and not permanent.** First node to bring a device up owns
it; if the owner leaves, ownership migrates to a remaining node; a returning node
just syncs. Ownership is per device, not per machine — so with 3+ machines each
device has its own current authority and everyone else mirrors it. This is what
makes Cursio a distributed audio *network*, not a PC↔PC link.

**"Magic" media keys fall out for free.** At Ubuntu, default sink = an Intent Sink
that is really Windows/VAIO3. Press Volume+ → RPC to owner → real VAIO3 changes →
broadcast → both machines show 61%. Feels like one physical device.

**Replicated state (whole objects, idempotent — resend latest on reconnect):**
```
DeviceState: volume, mute, default, channel_volumes, balance
StreamState: volume, mute, target_device, playing
```

### What to remove

- ✅ DONE: the global stream (`global_capture`/`global_id`, `cursio_remote_out_*`
  virtual_sink, pump entry, `on_chunk` global branch). It shipped the whole default
  sink back — the duplicate-Spotify + stray-VAIO3-row source. Gone in the model:
  only apps (as Remote Streams) and devices (as Intent Sinks + replicated state)
  cross the boundary.

### Protocol additions

Existing (`crates/protocol/src/audio.rs`, `Message`): `AudioStreamMeta`,
`AudioChunk`, `AudioFrameAck`, `AudioGain { id, gain }`, `AudioStreamClose`.

To add:
- **Handle inbound `AudioGain`/`AudioFrameAck`/`AudioStreamClose`** —
  `handle_message` currently matches only advertise + chunk.
- **Device ownership + state RPC.** e.g. `DeviceStateSet { device, field, value }`
  routed to the owner, and `DeviceStateReplicate { device, state }` broadcast back;
  plus ownership announce/handover on connect/disconnect.
- **`AudioStreamRoute { id, device }`** — a stream's target device (None = the
  owner-side default). Set from the observed Intent-Sink membership.
- **Reconnect: replicate `RoutingState` as an object**, don't rely on PipeWire
  re-emitting link events:
  ```
  RoutingState: { Spotify→VAIO3, Firefox→Headphones, Discord→HDMI }
  ```

### Implementation phases (safe, incremental)

1. **Delete the global stream.** ✅ DONE 2026-07-21.
2. **Intent Sink = silent funnel + observer.** Stop the device mirror from
   capturing/shipping its own audio; instead watch which playback streams are
   linked into it. (Today `virtual_sink` captures and sends — that must become
   observe-only for device Intent Sinks.)
3. **Lazy Remote Streams driven by Intent-Sink membership.** A stream funnelled in
   AND playing ⇒ open its network path; unfunnelled ⇒ tear down after a grace
   period. Match by node type, not app name.
4. ✅ DONE 2026-07-21: **Inbound `AudioGain`/`AudioFrameAck`/`AudioStreamClose`**
   handling + apply. `handle_message` now matches all three. `AudioGain` applies a
   logical per-stream gain *above* the OS mixer on the receiving playback node
   (lock-free atomic into the RT thread, both PipeWire and WASAPI); it is the
   Stream/Route gain of the three-object model, never the device's replicated
   volume. `AudioFrameAck`: the receiver returns a sparse ack (~4/s) with the
   newest seq it has played; the sender tracks the sent-vs-acked gap and adapts
   the stream's Opus bitrate with hysteresis (degrade-not-buffer). A change is
   applied to the running encoder (`OPUS_SET_BITRATE`), never by rebuilding it:
   the receiver's decoder keeps the old encoder's state, and a fresh encoder's
   first packets decode against it as a loud burst (see `codec.rs`).
   `AudioStreamClose` already tore down playback; it now also clears the
   per-stream gain/rate-control state so a reused id starts clean.
5. ✅ DONE 2026-07-21: **Device state replication with per-device owner**
   (volume/mute): RPC to owner, broadcast back. Protocol gained `DeviceState`
   (one volume + mute, whole-object, idempotent), `DeviceStateField`,
   `AudioDeviceStateSet { device, field }` (non-owner → owner request) and
   `AudioDeviceState { device, state }` (owner → all broadcast); `PROTOCOL_VERSION`
   → 9. Backend gained a `DeviceMonitor` (Linux/PipeWire): it binds every device
   node, observes `Props` (cubic↔linear volume + mute), and applies sets — used
   BOTH to watch our real devices (owner) and to reflect/observe our Intent Sinks
   (non-owner). Owner-side: a local device change broadcasts its `DeviceState`;
   an inbound `AudioDeviceStateSet` for an owned device is applied to the real
   device and the observed change propagates. Non-owner side: an inbound
   `AudioDeviceState` is applied to the matching Intent Sink (single-slider
   mirror); a local turn of that sink (a media key while it is the default sink)
   is sent to the owner as a request, with echo suppression so applied replicas
   don't ping-pong. Owner = the machine that advertised the device (physical
   reality: the device exists only there); this satisfies invariants #3/#4.
   Initial/reconnect resync falls out for free — the monitor emits current Props
   on subscribe, so the latest state is (re)broadcast without relying on deltas
   (#10). Windows now has a `DeviceMonitor` too: a `WasapiDeviceMonitor` that
   **polls** each render endpoint's `IAudioEndpointVolume` (scalar volume + mute,
   ~5/s) and applies sets via `SetMasterVolumeLevelScalar`/`SetMute`. Polling
   (rather than `RegisterControlChangeNotify` callbacks) sidesteps COM
   apartment/lifetime pitfalls; a knob turn is far slower than the poll. WASAPI's
   scalar is already linear `0..1`, so unlike PipeWire there is no cubic mapping.
   Windows owns its render devices (advertised by friendly name), so Windows
   volumes replicate to the Ubuntu Intent Sink and its media keys drive the
   Windows device through the owner RPC.
6. ✅ DONE 2026-07-21: **`AudioStreamRoute` + `RoutingState` replication** for
   reconnect and cross-machine assignment. Protocol gained `RouteEntry { id,
   app_name, device }` and `RoutingState { routes }`, plus `Message`s
   `AudioStreamRoute(RouteEntry)` (host → receiver, retarget one stream) and
   `AudioRoutingState(RoutingState)` (host → receiver, whole map); `PROTOCOL_VERSION`
   → 10. The host keeps a `routed_app_route` map alongside `routed_apps` and marks
   `routing_dirty` on any membership change (and at `start`); `pump` re-broadcasts
   the **whole** `RoutingState` when dirty, so a reconnecting peer gets the full
   map in one idempotent object rather than waiting on PipeWire to re-emit link
   events. The receiver applies a snapshot as a whole object (invariant #10): each
   listed stream's target is set, and any previously-targeted stream absent from
   the snapshot is reset to the default; a re-target drops an open playback so the
   next chunk reopens it on the new device. Only the stream's host writes routing
   (invariant #4); the receiver reflects it. NOTE: today an app moving between
   Intent Sinks is realised as close+reopen (a new stream id) plus the snapshot,
   which already survives reconnect; the `AudioStreamRoute` *inbound* path is wired
   and honoured, but the host does not yet emit per-stream re-targets in place —
   that is an optimisation to add if close+reopen ever proves too heavy.
7. ✅ DONE 2026-07-21: **Route/Routing-Group object** for group ops + future UI.
   Protocol gained `RouteOp::{SetGain, SetMute}`, `RouteState { target_device,
   members, gain, mute }`, and `Message`s `AudioRouteOp { target_device, op }`
   (receiver → host request) and `AudioRouteState(RouteState)` (host → receiver
   replicate); `PROTOCOL_VERSION` → 11. A Route is the group of Remote Streams
   funnelled into one target device; the host holds each Route's group gain/mute
   (`route_groups`, keyed by target device) — a Route property, kept separate from
   the device's single replicated volume (invariant #2). Only the host mutates it
   (invariant #4): the receiver (where the audio plays) sends `AudioRouteOp`, the
   host updates the group and, in `pump_routes`, fans the *effective* gain
   (`mute ? 0 : gain`) out to each member as a per-stream `AudioGain` (reusing the
   phase-4 path) and replicates the `RouteState`. New members joining a Route with
   a non-default group inherit it (the fan-out re-applies to all members). The
   receiver logs replicated `RouteState` for now — the future UI hooks here.

### Separation of responsibilities (three independent objects)

Dropping the VCA left three objects, each with a single clear responsibility.
Nothing else may hold these fields:

```
Device   → Volume, Mute, Default, Balance, ChannelVolumes, name/icon/profile
Stream   → PCM, Codec, Gain, Playing
Route    → Target device, Members, Latency, Codec policy, (future) Route gain
```

### Ownership rule (who may mutate what)

Each piece of state has exactly ONE authority. A change requested from anywhere
else is a *request*, not a write — the authority decides. Stated as invariants:

- **`DeviceState` may only be mutated by the device's OWNER.** Everyone else sends
  a request to the owner (see the volume-through-owner flow) and reflects the
  broadcast result.
- **`RoutingState` may only be mutated by the machine HOSTING THE SOURCE STREAM.**
  The receiver of a Remote Stream must never rewrite where that stream is routed.
  Example: Spotify plays on Ubuntu; Windows must not command
  `Spotify → Speakers`. Windows may only say *"I'd like Spotify on Speakers"* —
  Ubuntu, which hosts the stream, decides. (This is exactly what the Intent-Sink
  model already implies: intent is declared by connecting a stream to a sink on
  the stream's own machine.)
- **`StreamState` is owned by the machine HOSTING THE STREAM ITSELF** (the app's
  real machine).

Without this rule it becomes tempting, months later, to "just" set a route from
the receiving side — and the model quietly rots into two machines fighting over
one graph.

### Scaling note

The model is written in terms of **node** and **owner**, not "the two machines".
Nothing assumes exactly two peers: each device has a current owner, every other
node mirrors it, and Remote Streams/Routes are per source→target. So it extends to
N machines (desktop + laptop + Steam Deck) with no model change — Cursio becomes a
distributed audio *network*, not a PC↔PC link. A Mac is not in that list: with no
backend, it never advertises the audio module and so is not a node here at all.

## Invariants

Rules that must never be violated. If new code breaks one, the architecture is
starting to rot — stop and reconcile, don't work around it.

1. **An Intent Sink never carries PCM.** It replicates device state and observes
   membership; audio always travels on a Remote Stream, never through a mirrored
   device.
2. **A device has exactly one volume**, replicated identically across all nodes.
   No second slider, no per-device VCA. Extra gain belongs to a Route or Stream.
3. **Only the owner mutates `DeviceState`.** Changes from elsewhere are requests
   routed to the owner, then broadcast.
4. **Only the stream's host machine mutates its `RoutingState`/`StreamState`.** The
   receiving side may request, never command.
5. **A Remote Stream exists only while a *playing* stream is routed to an Intent
   Sink.** Not pre-created; torn down after a short grace once unrouted or silent.
6. **Routing is observed by node TYPE (a playback stream), never by app name.**
7. **Device ownership is per-device and transient** — it migrates when the owner
   leaves; it is never pinned to "the first/main machine" forever.
8. **No global/merged-mix stream.** Only apps (as Remote Streams) and devices (as
   Intent Sinks + replicated state) cross the boundary.
9. **Multi-output is legal.** A stream fanned out to a local device *and* an Intent
   Sink is intentional (plays both places), not the duplicate bug.
10. **Replicated state is whole-object and idempotent.** On reconnect a node
    resends the latest `DeviceState`/`StreamState`/`RoutingState`; it never relies
    on the graph re-emitting link events.

See memory `cursio-audio-unified-model` for the short statement, and
`cursio-audio-echo-and-mixer-fixes` for the echo/mixer/reconnect fixes that
preceded this.
