# Cursio — Platform & Module Design

**Goal:** Cursio is not "an input sharer that also does audio". It is a mesh of
trusted personal devices with a shared encrypted channel, and *everything else*
— input, audio, clipboard, window transfer, and whatever comes next (KDE-Connect
style notifications/files/SMS, a phone-as-touchpad, remote media control) — is a
module on top of it.

This document is about the **platform**: what a module is allowed to assume,
what it must never do, and which parts of today's code stop working when the
fifth module (or the first phone) shows up.

Read [mesh-design.md](mesh-design.md) for the peer/reconnect model and
[input-design.md](input-design.md) / [audio-design.md](audio-design.md) /
[clipboard-design.md](clipboard-design.md) / [window-transfer-design.md](window-transfer-design.md)
for the individual modules. This document does not restate them.

## The layer cake

Every layer below already exists. The point of this document is that the
boundaries stop being clean at the module layer, and that is fixable now while
there are four modules instead of ten.

```
  modules      input | audio | clipboard | window | notify | touchpad | files
  ------------------------------------------------------------------------
  session      run_session: handshake, liveness, ownership, module multiplex
  transport    EncryptedTransport (Noise) + priority scheduler + chunking
  discovery    mDNS sightings -> PeerRegistry
  identity     NodeIdentity (stable PeerId) + TrustStore (durable pairing)
```

The invariant that makes this work, and that must survive every new module:

> **One link per peer pair, one encrypted channel on it, every module
> multiplexed onto that channel.** A module never opens a socket, never does its
> own handshake, never holds the transport.

This is not an aesthetic preference. Pairing is password-authenticated and
explicit (`mesh --pair`, with a hidden terminal prompt); a module with its own socket would be a second,
unauthenticated way into the machine. It is also why the priority scheduler
works at all — it can only rank traffic it can see.

## What a module is today

`SessionModule` (`crates/core/src/module.rs`) is four calls: `start`, `pump`,
`handle_message`, `shutdown`. `run_session` owns the transport and lends a `tx`
at each. Multiplexing is by message *type*: modules are asked in turn whether an
inbound message is theirs, first claim wins.

That shape is right and should not change. What needs to change is everything
around it.

## Five things that break, in the order they will bite

### 1. `Message` is a flat enum, and every module widens it — **done**

*Implemented in protocol v13. `Message` went from 26 variants to 14.*

`Message` had 26 variants; audio alone contributed 10. Adding notifications,
file transfer, phone touchpad and media control the same way would have put it
past 50, and each addition:

- bumps `PROTOCOL_VERSION` for *everyone* — a phone that only speaks
  notifications is refused by a peer whose audio module changed shape;
- forces every `match` on `Message` to be re-checked (the `priority()` match is
  already 40 lines);
- means `cursio-protocol` depends on the union of every module's vocabulary, so
  the phone client links the window-transfer types it will never use.

**Fix — envelope by module.** Keep the small set of variants the *session* truly
owns (`Hello`, pairing, `Heartbeat`, `Bye`, `Opaque`, and — for now — the
input/ownership variants) and route everything else through one variant:

```rust
Message::Module {
    module: ModuleId,      // u16, stable, registry in one file
    priority: Priority,    // declared by the module, scheduler needs it
    payload: Vec<u8>,      // bincode of the module's own enum
}
```

Each module crate then owns its own message enum, versioned independently:
`cursio-protocol` stops being a dumping ground and `PROTOCOL_VERSION` goes back
to meaning "the session handshake changed".

The cost is one extra serialize/deserialize hop per message and losing
compile-time exhaustiveness across module boundaries. Both are acceptable —
except on the hot paths, where a per-frame `Vec<u8>` copy is real. So the
migration is **tiered**, not uniform:

| Module | Enveloped | Kept first-class | Why |
|---|---|---|---|
| clipboard | all of it (`ClipboardMsg`) | — | pure bulk, no per-frame path |
| audio | control (`AudioMsg`) | `AudioChunk` | a chunk per ~10 ms per stream per app |
| window | control (`WindowMsg`) | `WindowFrame`, `WindowInput` | every frame and every pointer move |
| input | — (still session-owned) | `Input`, `Grab`, `CrossEdge` | intertwined with the ownership loop |

That is not a half-finished migration; it is the same tiering the priority
scheduler already assumes. Two rules keep it honest:

- **Hot variant checked first.** Each module's `handle_message` matches its hot
  variant before attempting an envelope decode, so the common case costs one
  discriminant compare rather than a failed deserialize.
- **Control shares its hot path's `Priority`.** `AudioMsg::PRIORITY` is `Audio`
  and `WindowMsg::PRIORITY` is `Video`, so an advertisement cannot overtake the
  chunks it introduces and a close cannot overtake the frames it ends. Splitting
  a module across two classes would reorder its own traffic against itself.

### 2. There is no capability negotiation — **done**

*Implemented in `crates/protocol/src/capability.rs` (protocol v12).*


`handshake` exchanges `protocol_version`, `peer_id`, `display_name` — and
nothing about what either side can *do*. Modules are enabled by CLI flags on
each machine independently, so `--audio` on one side and not the other is
silently half-configured, and a phone that has no window-transfer backend would
still be sent `WindowOffer`.

**Fix — `Hello` carries a capability set.**

```rust
pub struct Hello {
    pub protocol_version: u16,     // session layer only
    pub peer_id: PeerId,
    pub display_name: String,
    pub device_kind: DeviceKind,   // Desktop | Laptop | Phone | Tablet | Headless
    pub modules: Vec<ModuleAdvert>, // { id, version, role }
}
```

Rules that follow from it, and that make the failure modes legible:

- A module runs on a link only if **both** sides advertise it. One-sided
  configuration becomes a startup log line, not a mystery.
- `version` is per-module, so an old phone keeps notifications working while its
  audio module is skipped as too old.
- `role` matters for asymmetric modules: a phone touchpad is `Source`-only, the
  desktop is `Sink`-only. Symmetric modules (clipboard) advertise `Both`.
- Unknown `ModuleId`s are **ignored, not rejected**. This is the whole point of
  the envelope: forward compatibility becomes the default rather than a version
  bump.

`device_kind` is what lets the input module skip edge geometry for a phone (a
phone has no screen edge bordering the desktop) without a special case bolted
into the ownership loop.

### 3. Module construction is hard-coded in `main` — **done**

*Implemented as `ModuleFactory` / `ModuleRegistry` / `SessionCtx` in
`crates/core/src/module.rs`, with a factory beside each engine.*


`crates/core/src/bin/cursio.rs:607-675` builds the module vector inline: an `if`
per module, `#[cfg]` blocks per platform, constructor arguments threaded through
`run_pairing_and_session`'s already-`too_many_arguments` signature. Adding a
module means editing the binary, the CLI enum, and that function's parameter
list.

**Fix — a `ModuleFactory` registry.**

```rust
pub trait ModuleFactory {
    fn id(&self) -> ModuleId;
    fn advert(&self) -> Option<ModuleAdvert>;  // None = unavailable here
    fn build(&self, ctx: &SessionCtx) -> anyhow::Result<Box<dyn SessionModule>>;
}
```

`SessionCtx` carries what constructors used to take by hand: local
`NodeIdentity`, the peer's `Hello` (so a module can adapt to `device_kind`), and
the peer edge. Node startup builds the registry once; each session asks the
factories whose ids were negotiated to build.

What it actually removed, measured: `run_pairing_and_session` went from 13
arguments to 9 (the four dropped were all per-module flags; the service work has
since added `offer`, taking it back to 10), ~75 lines of
per-module `if` plus three `#[cfg]` blocks collapsed to one `build_agreed` call,
and `spawn_drag_trigger` moved out of the binary into `window_session` — it was
platform-specific window-transfer wiring living in `main` for no reason. The
`#[cfg]` blocks still in `cursio.rs` are all about the *input injector*, which is
correct: input is not a `SessionModule`.

Two consequences worth taking deliberately:

- **Platform availability moves into `advert()`.** `#[cfg]` lives in the module
  crate, not in `main`. A module unavailable on this OS returns `None` and never
  reaches the wire.
- **CLI flags stop being the source of truth.** They become overrides of a
  config file. This is a prerequisite for a GUI toggle and for the phone, which
  has no command line at all. *Not yet done* — flags still feed the registry
  directly; the registry is simply the only thing that reads them now.
- **A failing module no longer takes the session with it.** `build_agreed` logs
  and skips, so losing clipboard doesn't cost you the cursor (rule 6).

### 4. `run_session` is single-threaded and pumps in lockstep

Sessions are deliberately `!Send` (`Rc` in the input path) and spawned on a
`LocalSet`. Every module is pumped every 5 ms on the same task, and a module
that blocks freezes input, audio and clipboard together — the code already knows
this, which is why `SLOW_PUMP_WARN` exists.

This holds for four modules. It will not hold for file transfer (disk I/O),
notification sync (D-Bus round-trips) or SMS bridging, all of which block for
tens of milliseconds at a time.

**Fix — split modules into two classes, not one.**

- **Realtime modules** (input, audio, window) stay exactly as they are: pumped
  inline on the session task, `!Send`, sub-millisecond pumps. Do not touch this;
  it is tuned and it works.
- **Async modules** (clipboard, notifications, files, media control) get a
  variant of the trait that owns a `tokio::task` and communicates with the
  session over channels: the session forwards claimed messages into an mpsc and
  drains an outbound mpsc during `pump`. The module's own work happens on the
  runtime, off the session task, and a slow module can no longer stall the
  cursor.

Concretely this is a second trait — `AsyncSessionModule` — plus one adapter that
implements `SessionModule` in terms of it. `run_session` learns nothing new.
Clipboard should be the first migration: it already does chunked bulk I/O and it
is the module most likely to block.

### 5. Modules will need each other, and there is no way to say so

A phone touchpad emits input events — it should feed the *existing* input path,
not reimplement injection. A notification module that mirrors a "now playing"
card wants the media-control module's state. Today a module can only talk to the
session, so the alternative is copy-paste, which is exactly the thing to avoid.

**Fix — a `NodeBus`, and keep it deliberately weak.** One process-local
broadcast of typed events (`InputEvent`, `PeerUp/PeerDown`, `MediaState`,
`ClipboardChanged`), passed to modules via `SessionCtx`. Modules publish and
subscribe; they never hold references to each other.

The constraint that keeps this from becoming a god object: **the bus carries
notifications, never commands.** A module reacting to a bus event does its own
work on its own link. Nothing on the bus crosses the network — network traffic
is always the session channel. Without that rule the bus quietly becomes a
second, unpriorit­ised transport.

## What the future modules actually need

Sketching the three named cases against the design above, to check it holds.

### KDE-Connect equivalent

Not one module — four, and they should ship separately rather than as a
monolith:

| Module | Shape | Notes |
|---|---|---|
| Notification mirror | async, `Both` | D-Bus (`org.freedesktop.Notifications`) on Linux, `UserNotificationListener` on Windows. Needs action round-trip (reply/dismiss). |
| File transfer | async, `Both` | Reuses the chunking already in the transport. `Priority::Bulk`. The clipboard module's chunked-offer/request protocol is the model to copy — see [clipboard-design.md](clipboard-design.md). |
| Media control | async, `Both` | MPRIS / SMTC. Tiny state object; the audio module's device-state replication (owner mutates, broadcasts whole object) is the pattern. |
| Phone sensors / battery | async, `Source` from phone | Pure state replication, no commands. |

**A phone's copy of what a desktop client already showed is dropped.**
Telegram, WhatsApp and Discord push every message to every client they run,
so the machine the user sat at showed its own client's notification and then
the phone's copy. That machine now watches its own notification server (a
D-Bus monitor on `Notify`, GTK's and the portal's `AddNotification`) and drops
a copy whose twin — same app, same text once emoji and punctuation are set
aside — appeared in the last 10 s. A copy that arrives first is shown and
taken down if the twin follows within 2 s; an app that has matched twice is
remembered (`notify-native-apps.json` in the config directory, forgotten after
14 days without a match) and its copies wait those 2 s instead, then show if
no twin came. Calls are never held. `crates/core/src/notify_dedup.rs` holds
the rest.

**On Windows a phone's notifications are toasts.** An unpackaged program has
no AppUserModelID to show them under, so the node registers `dev.cursio.Cursio`
under `HKCU\Software\Classes\AppUserModelId` with the name and logo; a reply
is typed into the toast's own text field, and closing it closes the phone's.
WinRT runs on one thread of its own in the multithreaded apartment, as D-Bus
does on Linux. Presence is `GetLastInputInfo` plus "not driving a peer", the
same test as Linux. Twins are read from `UserNotificationListener`, polled
once a second (its change event needs a packaged background task); it asks
the user's consent and may refuse a program without package identity, and
then copies show at once, as before deduplication existed
(`crates/core/src/notify_toast.rs`).

The reason this decomposes cleanly is that all four are *state replication or
bulk transfer* — the two patterns the audio and clipboard modules already
established. Nothing new is required at the platform layer.

### Phone as touchpad

The interesting case, because it is the first module that is **`Source`-only and
asymmetric**, and the first client that is not a desktop.

- Publishes `InputEvent` onto the `NodeBus`; the existing injector consumes it.
  No new injection code.
- Does **not** participate in cursor ownership. A phone has no shared cursor and
  no screen edge — it is an input *device* that happens to be on the network,
  not a machine the cursor travels to. `device_kind: Phone` is what tells the
  ownership loop to skip it, and this is precisely why `device_kind` belongs in
  `Hello` rather than being inferred.
- Absolute coordinates, not relative: a touchpad gesture is a delta, but a
  tablet stylus is absolute. `InputEvent` is relative-only today
  (see [input-design.md](input-design.md)); it will need an absolute variant.
  That is a change to the input module's vocabulary — under the envelope design,
  not a global `PROTOCOL_VERSION` bump.

### The phone client itself

Worth stating plainly since it constrains the crate layout: a phone client must
link `cursio-protocol` + `cursio-transport` + `cursio-core`'s session, and
**nothing else**. Today `cursio-core` pulls in audio, window, and input
unconditionally (`lib.rs` declares all of them as modules), so a phone build
would drag in PipeWire and Media Foundation.

The fix is mechanical and should happen with the factory registry: each module
becomes its own crate (`cursio-mod-audio`, `cursio-mod-clipboard`, …) depending
on `cursio-core`, not the reverse. `cursio-core` keeps identity, transport
glue, session, registry, mesh, and the module traits — and knows about no
specific module at all. The binary is what wires the set together.

This also fixes the dependency inversion already present: `cursio-core`
currently knows every module, which is why `main` can't add one without touching
core.

### The phone over Bluetooth

The phone's node opens the mesh's Bluetooth path itself (`crates/android/src/
bt.rs`, `BtLink.kt`). Android gives an app an RFCOMM socket only as Java
streams, never as a descriptor, so the node makes a socket pair, hands one end
to `BtLink.connect` — which owns it from then on and closes it on every way out
— and reads the other end as an ordinary socket; two Kotlin threads copy bytes
between the pair and the Bluetooth streams. Only computers are dialled
(`BluetoothClass.Device.Major.COMPUTER`): asking headphones for a service they
cannot have is seconds of radio time each. It needs the "nearby devices"
permission (`BLUETOOTH_CONNECT`) from Android 12, offered as a row of the
phone's setup card.

### The phone's storage on a computer

The phone runs the files module's server half and nothing else of it
(`ModuleRole::Source`; a computer is `Both`), so a computer mounts the phone's
storage under `~/Cursio/<phone>/` exactly as it mounts another computer's
disks, and two phones do not start the module at all. There is no Android
mount: a phone has nowhere to present one.

What it serves is the shared storage a person thinks of as the phone's files:
`/storage/self/primary` as **Internal storage**, and each card under
`/storage/<uuid>` as **SD card**, removable (`cursio_files::server`,
`detect_volumes`'s Android arm). It reads them as plain files, which is why the
grant is "All files access" (`MANAGE_EXTERNAL_STORAGE`) from Android 11 and the
legacy storage permissions with `requestLegacyExternalStorage` on Android 10,
rather than the Storage Access Framework. SAF answers through a content
provider: every entry of a listing is a JNI call and a provider query, a
directory of a few thousand photos takes seconds to list, and none of the
desktop server's code — chunked reads, write-back, rename, `statvfs` — would
apply. With real paths the server that answers for a desktop's disks answers
here unchanged. Google Play restricts that permission to file managers and
the like; Cursio's APK is installed directly.

Until the grant is made the storage is unreadable and no volume is offered —
a volume answering "denied" to everything would look broken. The module
rescans once a second, so the folder appears on the computer the moment the
user allows it, without a restart. The grant is a row of the phone's setup
card.

## Migration order

Each step is independently useful and independently revertable. Deliberately not
a big-bang refactor — the audio phase work is mid-flight and must not be
destabilised.

1. ~~**Capabilities in `Hello`**~~ — **done** (v12). `ModuleId`, `DeviceKind`,
   `ModuleAdvert` and `negotiate()` live in `capability.rs`; `handshake` takes
   the local advert set and `run_pairing_and_session` builds modules from
   `negotiated.agreed` rather than from flags. The silent half-configured
   `--audio` case now logs `module not running on this link` with a reason.
   Done first because it needed no changes to the 82 existing `Message::`
   match arms — unlike the envelope, which does.
2. ~~**`Message::Module` envelope**~~ — **done** (v13). Clipboard moved whole;
   audio and window moved their *control* traffic (`AudioMsg`, `WindowMsg`) and
   kept `AudioChunk`, `WindowFrame` and `WindowInput` first-class. `Message` is
   down from 26 variants to 14, and the four still-per-module ones are exactly
   the hot paths.

   The tiering held up in practice: `handle_message` in both modules now checks
   the hot variant *first* and only then attempts an envelope decode, so the
   common case costs one discriminant compare. The ordering guarantee that
   worried me — an enveloped offer racing a first-class frame — is preserved by
   giving each module's control tier the same `Priority` as its hot path, and
   `offer_precedes_the_first_frame` now asserts across that boundary.
3. ~~**`ModuleFactory` + `SessionCtx`**~~ — **done**. `run_pairing_and_session`
   is down from 13 arguments to 9 (10 today, since `offer` arrived with the
   service module), module `#[cfg]` moved into each factory's `advert`, and
   `main` names modules in exactly one place (`module_registry`).
   Steps 1–3 together mean a fourth module is now: write the engine, write a
   factory, add one line to `module_registry`.
4. **Split modules into their own crates**, core stops depending on them.
5. **`AsyncSessionModule`**, migrating clipboard first.
6. **`NodeBus`**, once there is a second consumer for it — not before. A bus
   with one publisher is speculative generality.

Steps 1–3 are what make the fourth module cheap. Steps 4–6 are what make the
*phone* possible. Nothing here needs to happen before finishing audio phase 7.

## Rules for new modules

Short enough to actually apply in review:

1. **Never own the transport.** No sockets, no handshake, no encryption. If a
   module needs bytes on the wire, it needs a `SessionModule`.
2. **Declare a priority honestly.** Bulk means bulk. A module that claims
   `Input` priority for a file transfer breaks the one guarantee the whole
   system rests on.
3. **Bounded pump.** Send a bounded amount per call and return, even for async
   modules. See `SessionModule::pump`'s contract.
4. **One owner per piece of state.** The audio module's rule — non-owners
   *request*, the owner mutates and broadcasts the whole object — generalises to
   every replicated thing (see [audio-design.md](audio-design.md), invariant #3).
   Two writers is always a race.
5. **Idempotent state replication.** Broadcast whole objects, not deltas, so a
   reconnect resyncs by resending the latest value. Learned the hard way in
   audio phase 6.
6. **Degrade, don't abort.** A module failing to start must not kill the
   session; input, audio and clipboard are still worth having. Log and carry on.
7. **Reuse the existing patterns.** Chunked bulk transfer, state replication,
   and adaptive rate control (ack-driven) all exist. A new module that invents
   its own is a bug, not a design choice.

## What a peer may reach

Pairing is trust, but not one amount of it. Every module that exposes this
machine to a peer answers two questions in the same place its requests are
already checked: *what* is offered, and *what the peer may do* with it. The
answer is enforced on the machine being exposed, never left to the peer's own
side — the peer is the party the rule constrains.

**Files** (`cursio-files::share`, `--files-share <path>[:ro]`, repeatable).
`SharePolicy { roots: Vec<ShareRoot { path, read_only }> }` is plain serde data
so a config file can carry it. An empty policy offers every detected volume
read-write — what sharing meant before a policy, so an unconfigured machine
behaves as it did. Naming any root turns it into an allow-list: a volume's root
(`/`, `C:\`) offers that disk; any other path offers that folder as a volume of
its own, named after it; a stick plugged in later is offered only if named. The
mode is read off the end of the argument because a Windows path has a colon of
its own (`C:\:ro`).

Enforcement is in `Namespace`, the one boundary every request already crosses:
read-only refuses every mutating open, `mkdir`, `remove` and `rename`, and a
folder root is *confined* — `..` was already refused, but a symlink inside the
folder pointing at `/etc` would still carry a request out, since the OS follows
links while opening. So the parent of every path must resolve (symlinks
followed) inside the folder, and an open or a listing, which go *through* a
final link, must land inside too. A link may still be listed and stat'ed as the
link it is. A whole volume has nothing outside it to escape to and pays none of
this.

**The network** (`core/src/firewall.rs`). On Windows the mesh and discovery
rules keep `profile=any` — Windows files many home LANs as Public, and a
private-only rule would leave exactly those machines unreachable — but carry
`remoteip=localsubnet`. The mesh is a LAN mesh found by mDNS, which never leaves
the link, so a connection from beyond the subnet is never a peer; and with IPv6
global addresses a permissive router no longer hides the machine, so an
any-address rule left the handshake reachable from the internet. The price is a
peer on a routed network (another VLAN, a VPN), which is refused. The rule name
carries the scope, so an old any-address rule is not mistaken for the current
one and is purged on the next start. `usbipd`'s own port is closed to the
network by the USB setup (see `usb-design.md`).
