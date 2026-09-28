# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What Cursio is

A LAN-only mesh of trusted personal devices sharing one encrypted channel. Input, audio, clipboard, window transfer and file sharing are **modules** on that channel — not separate products. Applications never migrate; only pixels, samples, events and file requests cross the link. AirPlay is the one deliberate exception (see below).

## Commands

Always scope cargo to a package. **`--workspace` fails on macOS**: `apps/clipd` depends on GTK4/libadwaita and `gdk-pixbuf-sys` has no macOS pkg-config.

```sh
cargo build -p cursio-core              # the `cursio` CLI (the usual build)
cargo test -p cursio-airplay            # one crate's tests
cargo test -p cursio-core peer_timeout  # tests whose name contains this
cargo test -p cursio-airplay -- --exact media::tests::rejects_partial_key_setup
cargo clippy -p <pkg> --all-targets     # must be clean before committing
cargo fmt -p <pkg>
```

Linux native builds need `OPUS_TARGET=ubuntu-24.04_x86_64` (`cargo xtask` sets it; a bare `cargo build` does not). Windows cross-builds need `GSTREAMER_WINDOWS_SYSROOT`.

```sh
cargo xtask build | install | macos | windows | android | logs --follow
cargo xtask deploy [--windows-only|--linux-only] [--no-restart]   # ship to Lightmoon, restart there
cargo xtask live-test [--seconds N] [--yes]                       # PASS/FAIL over both real machines
```

Builds install side by side under a per-user root, with a `current` pointer and rollback (`docs/update-design.md`):

```sh
cargo xtask stage [host|windows] [--install]         # versioned build dir + manifest.json
cursio update install <dir> [--activate] | status | prune --keep N
cursio update apply --version V --wait-pid PID [--stop] [-- mesh ...]   # switch, health-check, roll back on failure
cursio update rollback --wait-pid PID [--stop]
cargo xtask publish [--host-only] [--no-build]       # stage host + windows, sign, offer to the mesh (dev)
cursio update key | trust-builder <machine> [--key K] | untrust-builder <machine>
```

Builds cross the mesh signed: a receiving machine takes `dev` builds only from a builder it trusted, applies them per `[update] policy = "auto" | "ask" | "never"`, and `auto` switches only at a quiet moment (`core/src/update/quiet.rs`).

Anything registered outside (autostart, the app's "start node") names the root's stable `cursio`/`cursio-app`, which hand over to the current version; never register a `versions/<v>/` path.

`deploy` and `live-test` are described in `docs/live-test.md`. `deploy` never deletes a Windows `.exe` it did not name itself, and never stops the Linux daemon (it tells you to restart your `cargo xtask run`). `live-test` injects pointer motion only with `--yes`, and touches the peer mount only by exact path from a child process with a deadline.

`cargo xtask android` cross-builds **the mesh core only** for arm64 (`cursio-core --lib --no-default-features`), needing `ANDROID_NDK_HOME` and `cargo install cargo-ndk`. See "Android" below for why that is the whole core and not a subset of it.

Running:

```sh
cargo run -p cursio-core -- mesh --pair     # on BOTH machines once, prompts for the password
cargo run -p cursio-core -- mesh --airplay 1080p60 --airplay-window
cargo run -p cursio-core -- peers
RUST_LOG=cursio_airplay=debug,info cargo run -p cursio-core -- mesh
```

`RUST_LOG` raises console + file; `CURSIO_LOG_FILE` raises only the file. Logs rotate daily under the platform data dir.

Many crates ship `examples/` that probe real hardware (`capture_probe`, `wasapi_loopback_probe`, `grab_cycle`, `encode_bench`, `receiver_demo`). They exist because the thing they test needs a compositor, a GPU or an iPhone. Run one with `cargo run -p <pkg> --example <name>`; do not convert them into unit tests.

## Commits

Conventional Commits, **subject line only**. No body, no `Co-Authored-By`, no emoji, no issue refs.

```
type(scope): imperative lowercase summary
```

- Types in use: `feat`, `fix`, `perf`, `refactor`, `test`, `chore`, `style`, `docs`.
- Scope is the crate or subsystem, not a path: `airplay`, `transport`, `window`, `input`, `audio`, `files`, `clipboard`, `mesh`, `cli`, `xtask`, `build`, `security`, `platform`, `deps`.
- Say what the change does to *behaviour*, not what you edited. `fix(airplay): keep cipher aligned for IDR frames` — not `fix: update mirror.rs`.
- One commit per issue, containing **every** file that issue touched. Never one commit per file; never one commit spanning unrelated fixes.
- Order commits so each one compiles: a helper lands before its caller.
- Never amend or force-push unless asked.

## Code conventions

**Comments explain why, and only where the reason is not recoverable from the code.** This codebase is unusually heavy on rationale and near-empty of narration. Every module opens with a `//!` block that argues the design — what was tried, what the platform refuses to do, what breaks if you "simplify" it. Match that. `///` covers non-obvious invariants; inline `//` is reserved for protocol quirks, platform bugs, and decisions a reader would otherwise revert:

```rust
// Byte 4 selects the payload kind. Byte 5 is a video subtype flag (notably
// 0x10 for an IDR frame), not the high byte of a u16 type. Treating 00 10 as
// type 0x1000 skips the first encrypted IDR payload and permanently
// desynchronizes the continuous AES-CTR stream.
```

Do not write `// increment counter`, restatements of the signature, changelog notes, or your own reasoning about the edit.

Never commit `TODO`/`FIXME`/`XXX`, placeholders, fake data, or commented-out code.

**Tests** live in `#[cfg(test)] mod tests` at the bottom of the file they cover; integration tests that need two halves live in `crates/*/tests/`. Names are behavioural sentences, never `test_foo`:

```
decodes_fragmented_request_and_body
rejects_control_characters_in_header_values
mirror_ctr_state_is_continuous_across_calls
```

Anything requiring a compositor, a GPU or a peer is tested through in-memory fakes — `transport::test_util::MemTransport`, `window::window_test_support`. Reach for those before declaring something untestable.

**Errors**: one `thiserror` enum per library crate, `anyhow` in binaries and `xtask`. Messages are lowercase, name the concrete cause, and say what to install or fix when actionable. Never swallow an error; degrade with `tracing::warn!` and an explicit reason.

**Dependencies** are declared once in the root `[workspace.dependencies]` and referenced as `foo.workspace = true`. Platform-only deps go under `[target.'cfg(target_os = "...")'.dependencies]`.

**Platform code**: gate with `#[cfg(target_os = ...)]` and always give a fallback arm that returns an explicit error instead of degrading silently. Unimplemented codec paths are errors, not silent fallbacks, so premature use surfaces rather than shipping garbage.

`docs/*.md` are the source of truth for their subsystems (`mesh-design`, `platform-design`, `input-design`, `audio-design`, `clipboard-design`, `window-transfer-design`). If code diverges from a design doc, fix one or the other in the same change.

## Architecture

### The layer cake

`protocol` (wire vocabulary, no I/O) → `transport` (discovery, pairing, Noise, priority scheduling) → `core` (identity, link bring-up, mesh supervision, session loop, module implementations) → platform crates (`input`, `window`, `audio`, `files`, `clipboard-core`) → binaries (`cursio` CLI, `cursio-app` Tauri UI, `clipd`).

Platform crates are **mechanism**; the matching `*_session.rs` in `core` is **policy**. `cursio-files` knows how to serve and mount; `files_session.rs` decides what crosses the link.

### Mesh: no server, no hub

Every node listens for peers dialling in *and* dials the known peers that are down. `serve`/`connect` were two halves of one behaviour; `mesh` is that behaviour.

- `registry.rs` — the durable known-peer set (seeded from the trust store) plus transient `PeerState`. A peer going away flips to `Offline`; it is never removed. Only the user removing a machine (`cursio forget`, the app's peer menu, `core/src/forget.rs`) takes one out, together with its trust-store key.
- `mesh.rs` — dial targets, sweep interval, reconnection. `should_dial` breaks the tie so two nodes don't dial each other simultaneously.
- `link.rs` — everything between "there is a socket" and "a session can start": `Hello` → SPAKE2 pairing on first meeting (or the stored key) → Noise `NNpsk0` → module negotiation → cursor ownership.
- `session.rs` — `Hello` version check, then the heartbeat/pump loop.

**Sessions are `!Send`; modules are not on the session.** The input path shares its injector and cursor via `Rc` deliberately, so peer sessions are spawned with `spawn_local` onto a `LocalSet` and the registry is `Rc<RefCell<_>>`. Do not "fix" this with `Arc`/`Mutex` — it is single-threaded by design. What that loop holds is only what is `!Send` and ordering-sensitive: input forwarding and injection, `CursorRole`/ownership, edge crossing, the hotkey, heartbeat/liveness and `NodeStatus`. Every `SessionModule` runs on its own OS thread (`core/src/module_host.rs`), so nothing a module waits on can reach the cursor.

### Modules are the extension point

A feature is a `SessionModule` (`core/src/module.rs`), built by a `ModuleFactory`, assembled into a `ModuleRegistry`. `run_session` hands each built module to a `ModuleHost`: a dedicated OS thread that runs it with `Handle::block_on` on the link's runtime (so what a module spawns outlives its thread), calling `start`, then `pump` at the module's own pace (fast while it has traffic, idle after ~100 ms of quiet, `has_pending_work` keeps it fast) and `handle_message` for its inbox, and finally `shutdown`. Each module sends through its own clone of the prioritized sender (`TransportTx::clone_sender`), so its frames are scheduled against input exactly as before. Inbound routing is by id, not by asking: the reader task sends each message to the one module named by `Message::module_id()` (enveloped `Module` frames plus the `AudioChunk`/`WindowFrame`/`WindowInput` hot paths), and only the session's own vocabulary reaches the session loop.

`ModuleId` constants live in `protocol/src/capability.rs` (input 1, audio 2, clipboard 3, window 4, files 5, service 6). Both sides advertise `ModuleAdvert` in `Hello`, negotiate, and build only what they agreed on — this is what fixed the old failure where `--audio` on one side silently half-configured the link. The CLI's only job is to construct the registry; after that nothing names a specific module. **Add capability by adding a module, not by branching in the CLI.**

A slow `pump` (≥50 ms) or `handle_message` (≥20 ms) is still logged, now as delaying *that module's* own traffic — it no longer means input waited. Teardown asks every module to stop, waits at most `MODULE_SHUTDOWN_GRACE` (3 s) for all of them, then detaches a wedged one with a warning naming it and closes its sender so the link can close. A module's `start` failing or its send failing ends the session, as before. Keep pumps bounded anyway: while one runs, its own replies wait.

### One channel, five priority classes

Everything rides one Noise link, so `transport/src/priority.rs` chunks and queues by `Priority`: `Control` > `Input` > `Audio` > `Video` > `Bulk`. `send` does not touch the socket — a background drainer emits from the highest non-empty class and re-checks between every frame, so an input event queued mid-file-transfer jumps ahead of that transfer's remaining chunks. Any new message type must pick a class deliberately.

### Pairing and trust

SPAKE2 in symmetric mode: same password both sides, one message each, password never on the wire. A wrong password yields two *different* keys rather than an error, so an explicit key-confirmation step follows; only after confirmation is the peer persisted to the trust store. Identity (`PeerId` + display name) and trusted peers live in the platform config dir. `mesh --pair` is the only place a password is used and reads it without echoing. It is run on **both** machines rather than paired with a separate `join`: the mesh already listens and dials at once, and `should_dial` picks which side places the call to a machine neither has met. `--pair-addr` names that machine where mDNS will not.

### Input

Machines are placed by **grid coordinates** (`--at 1,0`), not by naming a side. A side cannot express order — four machines in a row are all "to the right" of the first, and nothing in that vocabulary says which one the cursor reaches first — and a designated centre would make every other machine depend on one being up, which is what the mesh exists to avoid. `Position::edge_toward` takes the dominant axis (ties break horizontal); `neighbour_across` picks the nearest peer in a direction, so a machine going away lets the one behind it become the neighbour. **Only the machine holding the cursor may send it away.** An idle machine looks identical whether the session just started or it has just handed the cursor back, so honouring a crossing from idle made a stray pointer movement start it driving a peer nobody was at — with no way back, since it then dismissed every incoming crossing as "already driving". Wayland and Windows never reported such a crossing at all; macOS did, and now matches them. Taking control from idle is the hotkey's job. `CursorRole::seize` is that path and is deliberately not `take_over`: the peer sending the cursor means the user is over there, pressing the hotkey means the user is here, and conflating them left the cursor on the far screen with no input reaching it.

Positions ride in `Hello`, so the edge is derived per link and handed to capture by `set_peer_edge` once the peer has greeted us — which is why capture starts watching nothing. Each backend follows that differently: Windows stores an atomic the hook reads, macOS' tap reads it per event, and Wayland has to take capture down and place a new barrier, since a barrier is a D-Bus object on one edge. Several peers still mean one watched edge; arming a set is the change that would let a machine border two at once.

Wire events are OS-agnostic and **relative** (`dx`/`dy`, never absolute) — absolute positioning teleports the cursor and breaks games reading raw input. The wire keycode namespace is Linux **evdev**; Windows translates both directions through `win_keymap.rs` by physical position via PS/2 scancode set 1.

Capture: evdev on X11-ish Linux, the `InputCapture` portal + libei on Wayland (pointer barriers on screen edges — this is how edge-crossing works there), RawInput on a message-only window on Windows, a `CGEventTap` on macOS (an *active* tap, since deleting the event is the only way to freeze local input; it runs its own `CFRunLoop` on a dedicated thread and must be re-enabled after the system times it out). Injection: two persistent uinput devices on Linux (pointer and keyboard kept separate so the compositor doesn't misclassify them), `SendInput` on Windows, `CGEventPost` at the HID tap on macOS. Cursor hiding: `SetSystemCursor` on Windows, `CGDisplayHideCursor` on macOS (real, but the window server restores it whenever Cursio is not frontmost), an honest logged no-op on Wayland.

macOS differs in three ways worth knowing before touching it. There is **no relative motion** — `CGEvent` carries an absolute point only, so the injector tracks a shadow cursor, re-seeded from the real one on activation, and attaches the raw delta as `kCGMouseEventDeltaX/Y` for anything reading raw input. Motion while a button is held must be posted as a *drag* event or the drag simply does not happen. And **modifiers are state, not keystrokes**: the full mask has to be stamped onto every event, keyboard and mouse alike, or Command-C is just a C. Both directions need the Accessibility permission, which macOS grants to the *responsible* application — a terminal, for a binary started from a shell — and without which every post silently succeeds and does nothing.

The wire namespace is evdev, and `mac_keymap.rs` also decides where the modifiers land. `ModifierMap` is keyed by *physical key* — both sides separately — and `--mac-modifiers` takes either a preset or `from=to` pairs: `positional` (the default: Ctrl is Control, Alt is Option, Win is Command), `swapped` (Control acts as Command, so a PC keyboard's Ctrl+C copies), or e.g. `rightalt=globe`, which puts the Globe key a MacBook changes input source with under a key a PC keyboard has. The map applies on injection and is undone on capture, so a Mac's Cmd+C arrives at a peer as whatever that peer turns back into Command. Two *named* keys on one target are refused (capture would have no answer to give); a named key beats one still at its default, which is what makes `rightalt=globe` work.

The ownership hotkey is Ctrl+Alt+G in the **wire** namespace, so it moves with the map: `positional` puts it on the Mac's own Control+Option+G, `swapped` on Command+Option+G. Startup logs which physical keys, because a hotkey that quietly relocated is one the user cannot find.

`elevate.rs` self-elevates on Windows because UIPI silently drops injected input into higher-integrity windows — `SendInput` returns success and nothing happens. Elevation is requested, not required by manifest, so declining UAC still leaves a useful daemon.

### Window transfer

One window's pixels stream to a peer while the app keeps running on its host (RemoteApp/Xpra model, bidirectional and cross-OS). One engine serves both roles at once, since either machine may host a window while viewing another.

Capture: `ScreenCast` portal → PipeWire on Linux, `Windows.Graphics.Capture` on Windows, ScreenCaptureKit on macOS. The macOS half is Objective-C **compiled into the crate**, not a spawned helper: macOS grants Screen Recording to the responsible application, and a separate unsigned binary in a temp directory is covered by no approval the user can give — it is refused with "the user declined" however many times they approve. A Mac can host but not yet view; `MacWindowRenderer` says so rather than failing obscurely. Both hand a full BGRA frame to the shared `frame_pipeline.rs`, which diffs against the previous frame and routes the change to a lossless tile or the video encoder. Surrogate (viewer) windows: GTK4 on Linux, raw Win32 on Windows, both blitting into the shared `Framebuffer`.

Video encoding exists because zstd on a *moving* window measured ~742 Mbit/s — past gigabit. H.264 via `ffmpeg-next` on Linux, Media Foundation on Windows. Decoders are stateful: one packet may yield zero frames or several, and "no output yet" is the ordinary case.

**A drag is the only way to start a transfer.** There is no flag: `--share-window` and its two companions were a second way of saying what the drag already says, and the two had to be kept in agreement. Receiving stays passive — every supported platform advertises the module, so a peer's window always arrives — while offering one is the deliberate act of dragging it across the peer-facing edge. The dragged window's title is carried to the capture through `WindowCapture::aim_at`, because the detector is the only thing that knows which window was grabbed — a backend left to guess picks the foreground one, which is right on Windows by coincidence and means the whole screen on macOS.

Drag detection is asymmetric by necessity: Windows asks `GetGUIThreadInfo`; Wayland forbids a client from knowing other windows exist, so GNOME needs the Shell extension in `native/cursio-shell-extension`; macOS answers no such question at all, so `window_drag.m` infers it — the button is down and a window has moved since it went down — reading the window server's own list rather than the Accessibility API, which would need a second permission and a round trip into an application that is busy being dragged. Both emit the same `DragEvent` vocabulary, and `drag_trigger.rs` decides when a drag becomes a transfer with no OS knowledge at all. Viewer input replays via `PostMessage`, not `SendInput` — the host's window is deliberately hidden and hijacking its real cursor would fight the user sitting there.

**Remote-screen views are the one pull, and draw nothing here.** `--remote-screen` offers each peer's windows and monitors to Discord/OBS on this machine (`core/src/remote_screen.rs`) without rendering them on this desktop. It is not a transfer: the viewer asks (`ListSources`/`ShowSource`), which the trusted mesh permits because each peer can already drive the other's input; `--no-screen-host` refuses with a reason. **Windows**: an *unseen* `Cursio — <peer>` window per peer — layered alpha 0, `WS_EX_NOACTIVATE`, parked past the virtual desktop — which Chromium's and OBS' window lists still include and WGC still records (measured, `windows_surrogate/unseen.rs`; off-desktop alone freezes it, `TOOLWINDOW`/cloaking drop it from the lists); it survives every source switch under `SCREEN_STREAM_ID`, and its selector is a notification-area icon. **Linux**: capture apps take pixels only from the ScreenCast portal, so Cursio *is* the ScreenCast portal backend (`window/src/linux_portal.rs`, `cursio portal install`): its chooser lists "this computer" (forwarded to `xdg-desktop-portal-gnome` untouched) plus peers' sources, and a peer pick becomes a PipeWire `Video/Source` node (`PipeWireScreenSource`) opened on demand through `core/src/screen_hub.rs`. Sessions it did not create — Cursio's own RemoteDesktop+ScreenCast transfer session — are forwarded verbatim, which is what keeps redirecting the interface safe. `cursio portal relay` (D-Bus-activated) forwards everything when no daemon runs. The window module announces its decoder (`VideoCaps`): Media Foundation cannot decode the High 4:4:4 lossless stream and says nothing, so hosts send such viewers lossy High profile.

### Audio

A networked **per-application** audio bus: each app on a peer appears here as its own named node, so VoiceMeeter/Carla can route it. Cursio replaces the network hop, not the mixer. Opus at low-latency settings — raw f32 stereo is 384 KB/s per stream, and a saturated link is felt as mouse lag because input shares the channel.

PipeWire and WASAPI both run thread-bound main loops, so each stream owns a dedicated OS thread bridged to async by channels. Windows has no user-space way to publish an audio endpoint, so `native/cursio-vad` is a kernel driver fork driven over IOCTL by `vad_client.rs`.

### Files

Request/response, never push: a volume is far larger than any link, so nothing crosses until a program actually asks. Requests are **untrusted input** — a path is a string off a socket, not something this process built.

FUSE calls block their own threads and expect an answer before returning; the session that fetches bytes is async. `bridge.rs` joins them **without** `block_on` (which deadlocks the runtime). Disk I/O and compression run on the dedicated pool in `background.rs`, deliberately kept out of the way of the cursor.

### Clipboard

A clipboard is a *set* of representations of one thing (HTML + RTF + text; PNG + bitmap), not a string — collapsing that set destroys formatting irreversibly, so `ClipboardContent` carries it whole. The exchange is **offer, then request**: a copy announces formats and sizes (a few bytes) and the peer pulls a representation only when pasting, because most copies are never pasted.

Backends: `data-control`/X11 via arboard on Linux, the Win32 clipboard with a message-only window on Windows, `NSPasteboard` on macOS. The macOS half names representations with UTIs (`public.png`, not `image/png`), so `macos.rs` holds that table the way the Windows backend holds its `CF_*` one, and carries anything unrecognised through verbatim. Two macOS quirks are load-bearing: nothing on the platform *announces* a copy, so the watcher polls `changeCount` (what every clipboard manager there does), and `NSPasteboard` cannot report a representation's size without materialising it — so `read` really does load each one to measure it, which is honest rather than lazy.

`clipboard-core` backs two unrelated products — `clipd` (a standalone Linux clipboard history, Win+V equivalent) and Cursio's cross-machine sharing. They never talk; the system clipboard is the meeting point. `clipd` keeps text in SQLite and image bytes in files beside it, drives an already-installed picker rather than shipping a GUI for the list, and registers its hotkey through GNOME's dconf custom keybindings because Wayland gives no app-side global shortcuts.

### AirPlay is deliberately outside the mesh

An iPhone speaks Apple's protocols, not Cursio's Hello/Noise session, so `crates/airplay` owns its own listener and crypto:

- `receiver.rs` — accepts control connections, one `MediaSession` each; survives transient accept failures; closes sessions silent for 30 s.
- `rtsp.rs` — bounded HTTP/RTSP framing plus `/info` and `OPTIONS`. Unimplemented stages return 501 **on purpose**: acknowledging a stage whose state machine does not exist desynchronizes sender and receiver.
- `media.rs` — `SETUP` (timing + mirror stream), `TEARDOWN`, feedback. A `TEARDOWN` carrying `streams` ends that stream only and keeps the session key; a bodyless one ends the session.
- `mirror.rs` — mirror packet framing and AVC → Annex-B. Both decoders consume with a cursor and compact only when the consumed prefix outgrows the remainder.
- `crypto/apple.rs` — transient Ed25519/X25519 pairing and FairPlay (native C in `native/playfair`, built by `build.rs`).
- `video/` — **macOS**: an AppKit/AVFoundation helper compiled by `build.rs`, embedded in the binary, written to a temp path and spawned; frames cross a bounded queue on a dedicated writer thread so a stalled window never blocks a Tokio worker. **Linux/Windows**: GStreamer with a zero-copy plan (`vah264dec`/`nvh264dec`/`d3d11h264dec`), which refuses to fall back to software decoding.

The AES-CTR keystream is continuous for the life of a mirror data connection. Every video payload must pass the decryptor **in wire order**, even one that will be discarded — skipping before decryption corrupts every later frame. Framing errors end the stream; a single undecodable payload is logged and skipped.

**Placement and actual state are both exchanged.** `Hello::service` carries each node's decoder fitness, requested quality and local-window request. Every node deterministically prefers a usable hardware decoder and uses peer id only to break equal ties. `ServiceMsg::Query`/`Status` still reports what is actually bound, including address and quality. `Announcer` lets a node withdraw when placement moves and re-publish after failover. Two processes on one machine remain a separate question, settled by a lock file in the log directory.

**When a machine cannot host it at all.** Ranking nodes against each other answers "who should host the receiver"; the question is "who *can*". A Mac can be reachable and willing while Apple's own receiver already holds port 7000, so availability is probed before the offer is sent. If a later bind still fails, request/refusal fallback asks connected peers; a peer accepts only when its hidden receiver is already bound. Refusal carries a reason, because silence is indistinguishable from a message that never arrived. The vocabulary (`protocol/src/service.rs`) names a `ServiceId` rather than AirPlay: the shape is the same for every duty that must live on exactly one machine. The picture still appears wherever the user started it — the hosting peer streams it back through the ordinary window path.

**One receiver per mesh, not per machine.** The picker name is plain `Cursio`. The selected host is the best connected offer that can bind its configured port; equal candidates use the lowest peer id. Losing that peer removes its offer, so the remaining nodes recompute placement and one announces. Stood-down nodes keep hidden receivers ready for failover.

`--airplay <MODE>` controls participation and requested mesh quality. `on`/`control` is the default, a profile such as `1440p:30fps` raises the mesh request, and `off` steps out of hosting. Window placement is orthogonal: `--airplay-window` is carried to the selected host, which forwards untouched H.264 through the window module when the viewer is another node. `--share-airplay` preserves the explicit broadcast-to-every-peer mode. A selected receiver with no local window uses a discarding sink after decrypting/tapping the access units, so it accepts mirroring without creating a decoder window.

**Reaching the rest of the mesh** is `tap.rs` plus `core/src/airplay_window.rs`. The phone never becomes a peer. The selected host offers its screen through ordinary window transfer; H.264 access units go out untouched and the viewer's surrogate decodes them. The tap retains SPS/PPS for late subscribers. A placement/quality change closes and recreates the surrogate with the newly agreed dimensions.

### Android: the JVM's way in

`crates/android` is a `cdylib` the JVM opens with `System.loadLibrary`, and the only thing Kotlin ever calls. The **node lives in Rust** — its own thread, its own current-thread Tokio runtime, the same `cursio-core` every desktop peer runs; Kotlin starts it, stops it, and asks for a status line. Putting the session loop on the JVM would expose a handshake to GC pauses and mean reimplementing `mesh.rs`.

Every `extern "system"` entry point catches its own unwinds: a panic crossing into JVM frames is undefined behaviour, not an exception — the process dies with nothing in logcat naming the cause. This is the one place in the codebase where catching a panic is correct.

Kotlin lives in `crates/android/java` (`CursioNative` declaring the six natives, `MeshService`, the manifest). The service is `foregroundServiceType="connectedDevice"` — that is what survives Doze; a battery-optimisation exemption helps only against manufacturer killers and is **not** the mechanism. `dataSync` would carry a timeout that stops a mesh link mid-session.

**Paths are handed in, not derived.** Android's writable directory is chosen by the system per install and reaches the app only through `Context.getFilesDir()`, so `nativeInit` must be called first and everything persistent resolves against `cursio_protocol::paths` (logs through `cursio_log::set_log_dir`, which stays independent so the logging primitive does not depend on the wire vocabulary).

**Depend on `cursio-core` by path, not `workspace = true`.** A workspace dependency carries its own `default-features`, and a `default-features = false` beside it is ignored — which silently pulled PipeWire, FUSE and libopus into a phone build.

### Android: the core crosses, the modules do not

`cursio-core`'s platform dependencies are **optional features**, all on by default (`desktop = ["audio", "input", "window", "files", "airplay"]`), so a desktop build is unchanged. `--no-default-features` leaves the mesh core — identity, link, session, mesh, registry, module dispatch, node status — and that builds for `aarch64-linux-android` as-is. Nothing in `link.rs`, `session.rs`, `mesh.rs` or `registry.rs` imports a platform crate; only the four `*_session.rs` policy files do, and each is gated by its feature.

The `cursio` binary is `required-features = ["desktop"]`: it drives capture, mounts and an AirPlay receiver, and a phone has no terminal to run it from.

**Do not switch a module's feature on for Android to "see if it works".** It will not fail at the feature — it fails deep inside PipeWire, FUSE or libopus, which is a far worse error to read. Each module needs an Android backend first. `DeviceKind::Phone`, `ModuleRole::Source`/`Sink` and `session::local_device_kind()`'s `target_os = "android"` arm already exist for that.

### Node state is not a module

`NodeStatus` (battery, power source, network kind, Wi-Fi signal) rides `Hello` and is re-sent as `Message::NodeStatus` when it changes. It is deliberately **not** a `SessionModule`: modules are optional and negotiable, but every node has an answer here, and the peer sharing least with us is exactly the one whose "about to vanish" is worth knowing. Every field is `Option` and `None` means *cannot say*, never zero — a missing battery rendered as 0% would show every desktop as flat. The wire is event-driven (sent on change); the *reading* is a 30 s poll, because no platform offers one event source covering power and network both. One monitor per machine with a generation per reading, one cursor per link: the machine changes, not the link, so every peer must hear it exactly once. See `docs/mesh-design.md`.

This is the second class of thing on the channel — **state and events of a device**, as opposed to a stream of data between machines. Notifications and media control belong to it too. Adding to that class is fine; conflating it with the module story is not.

### Wire compatibility

Bump `PROTOCOL_VERSION` in `protocol/src/lib.rs` (currently 19) whenever the shared vocabulary changes; a module's own message set is versioned separately through `ModuleAdvert`. Framing is a 4-byte big-endian length prefix plus that many payload bytes.

**Never** change the update lane (`protocol/src/update.rs`): its bytes are pinned by tests, `Message::Hello` stays variant 0 and `Message::Update` variant 17, and every connection opens with its `Preamble`. It is the one thing two builds of any age can still exchange, which is how the one that is behind gets updated. Add a new kind instead; old builds skip kinds they do not know.

Settings changes never restart the node: `Message::Reconfigure` renegotiates a running link and `core/src/live.rs` names what can only apply at the next restart (`restart_needed`). See "Changing settings on a running node" in `docs/mesh-design.md`.

### Native and tooling directories

`native/playfair` (FairPlay C, vendored) · `native/cursio-vad` (Windows audio driver fork, see `CURSIO-FORK.md`) · `native/cursio-shell-extension` (GNOME extension for drag detection) · `tools/wgc-probe` (settles whether Windows Graphics Capture keeps delivering frames from a hidden window) · `crates/xtask` (all build/install/cross-compile automation).
