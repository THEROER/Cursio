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
cargo xtask build | install | macos | windows | logs --follow
```

Running:

```sh
cargo run -p cursio-core -- join            # once per new machine, prompts for the password
cargo run -p cursio-core -- mesh --airplay-screen
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

- `registry.rs` — the durable known-peer set (seeded from the trust store) plus transient `PeerState`. A peer going away flips to `Offline`; it is never removed.
- `mesh.rs` — dial targets, sweep interval, reconnection. `should_dial` breaks the tie so two nodes don't dial each other simultaneously.
- `link.rs` — everything between "there is a socket" and "a session can start": `Hello` → SPAKE2 pairing on first meeting (or the stored key) → Noise `NNpsk0` → module negotiation → cursor ownership.
- `session.rs` — `Hello` version check, then the heartbeat/pump loop.

**Sessions are `!Send`.** The input path shares its injector and cursor via `Rc` deliberately, so peer sessions are spawned with `spawn_local` onto a `LocalSet` and the registry is `Rc<RefCell<_>>`. Do not "fix" this with `Arc`/`Mutex` — it is single-threaded by design.

### Modules are the extension point

A feature is a `SessionModule` (`core/src/module.rs`), built by a `ModuleFactory`, assembled into a `ModuleRegistry`. `run_session` keeps the transport and lends each module a `tx` at three points: startup, once per loop iteration to flush outbound work, and whenever an inbound message isn't one the session handles itself. Multiplexing is by message *type* — modules are asked in turn whether a message is theirs.

`ModuleId` constants live in `protocol/src/capability.rs` (input 1, audio 2, clipboard 3, window 4, files 5). Both sides advertise `ModuleAdvert` in `Hello`, negotiate, and build only what they agreed on — this is what fixed the old failure where `--audio` on one side silently half-configured the link. The CLI's only job is to construct the registry; after that nothing names a specific module. **Add capability by adding a module, not by branching in the CLI.**

A module `pump` that exceeds ~50 ms is logged as having blocked the session loop. The shared cursor rides the same channel, so disk I/O, compression and codec work must go to threads.

### One channel, five priority classes

Everything rides one Noise link, so `transport/src/priority.rs` chunks and queues by `Priority`: `Control` > `Input` > `Audio` > `Video` > `Bulk`. `send` does not touch the socket — a background drainer emits from the highest non-empty class and re-checks between every frame, so an input event queued mid-file-transfer jumps ahead of that transfer's remaining chunks. Any new message type must pick a class deliberately.

### Pairing and trust

SPAKE2 in symmetric mode: same password both sides, one message each, password never on the wire. A wrong password yields two *different* keys rather than an error, so an explicit key-confirmation step follows; only after confirmation is the peer persisted to the trust store. Identity (`PeerId` + display name) and trusted peers live in the platform config dir. `join` is the only place a password is used and reads it without echoing.

### Input

Wire events are OS-agnostic and **relative** (`dx`/`dy`, never absolute) — absolute positioning teleports the cursor and breaks games reading raw input. The wire keycode namespace is Linux **evdev**; Windows translates both directions through `win_keymap.rs` by physical position via PS/2 scancode set 1.

Capture: evdev on X11-ish Linux, the `InputCapture` portal + libei on Wayland (pointer barriers on screen edges — this is how edge-crossing works there), RawInput on a message-only window on Windows, a `CGEventTap` on macOS (an *active* tap, since deleting the event is the only way to freeze local input; it runs its own `CFRunLoop` on a dedicated thread and must be re-enabled after the system times it out). Injection: two persistent uinput devices on Linux (pointer and keyboard kept separate so the compositor doesn't misclassify them), `SendInput` on Windows, `CGEventPost` at the HID tap on macOS. Cursor hiding: `SetSystemCursor` on Windows, `CGDisplayHideCursor` on macOS (real, but the window server restores it whenever Cursio is not frontmost), an honest logged no-op on Wayland.

macOS differs in three ways worth knowing before touching it. There is **no relative motion** — `CGEvent` carries an absolute point only, so the injector tracks a shadow cursor, re-seeded from the real one on activation, and attaches the raw delta as `kCGMouseEventDeltaX/Y` for anything reading raw input. Motion while a button is held must be posted as a *drag* event or the drag simply does not happen. And **modifiers are state, not keystrokes**: the full mask has to be stamped onto every event, keyboard and mouse alike, or Command-C is just a C. Both directions need the Accessibility permission, which macOS grants to the *responsible* application — a terminal, for a binary started from a shell — and without which every post silently succeeds and does nothing.

The wire namespace is evdev, and `mac_keymap.rs` also decides where Command lands. `ModifierStyle::Pc` (the default) swaps Control and Meta so a PC keyboard's Ctrl+C copies on the Mac; the same swap runs in reverse on capture, so a Mac's Cmd+C arrives at a Linux or Windows peer as Ctrl+C. The swap is its own inverse, which is why one function serves both directions. `--mac-modifiers mac` keeps the names, for an Apple keyboard on the sending machine.

`elevate.rs` self-elevates on Windows because UIPI silently drops injected input into higher-integrity windows — `SendInput` returns success and nothing happens. Elevation is requested, not required by manifest, so declining UAC still leaves a useful daemon.

### Window transfer

One window's pixels stream to a peer while the app keeps running on its host (RemoteApp/Xpra model, bidirectional and cross-OS). One engine serves both roles at once, since either machine may host a window while viewing another.

Capture: `ScreenCast` portal → PipeWire on Linux, `Windows.Graphics.Capture` on Windows, ScreenCaptureKit on macOS. The macOS half is Objective-C **compiled into the crate**, not a spawned helper: macOS grants Screen Recording to the responsible application, and a separate unsigned binary in a temp directory is covered by no approval the user can give — it is refused with "the user declined" however many times they approve. A Mac can host but not yet view; `MacWindowRenderer` says so rather than failing obscurely. Both hand a full BGRA frame to the shared `frame_pipeline.rs`, which diffs against the previous frame and routes the change to a lossless tile or the video encoder. Surrogate (viewer) windows: GTK4 on Linux, raw Win32 on Windows, both blitting into the shared `Framebuffer`.

Video encoding exists because zstd on a *moving* window measured ~742 Mbit/s — past gigabit. H.264 via `ffmpeg-next` on Linux, Media Foundation on Windows. Decoders are stateful: one packet may yield zero frames or several, and "no output yet" is the ordinary case.

Drag detection is asymmetric by necessity: Windows asks `GetGUIThreadInfo`; Wayland forbids a client from knowing other windows exist, so GNOME needs the Shell extension in `native/cursio-shell-extension`. Both emit the same `DragEvent` vocabulary, and `drag_trigger.rs` decides when a drag becomes a transfer with no OS knowledge at all. Viewer input replays via `PostMessage`, not `SendInput` — the host's window is deliberately hidden and hijacking its real cursor would fight the user sitting there.

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

`--airplay-screen` (alias `--airplay`) gates video; without it the control receiver still runs and mirror `SETUP` answers 501. `--no-airplay` disables the listener. A busy port 7000 (macOS' own receiver) logs a warning and leaves the mesh running.

**Reaching the rest of the mesh** is `tap.rs` plus `core/src/airplay_window.rs`. The phone never becomes a peer — it has no identity, runs no modules, and is absent from the registry. Instead the receiving host offers its screen through the ordinary window-transfer path: AirPlay produces H.264 and `TileCodec::LossyVideo` already carries H.264, so access units go out untouched and the viewer decodes them with the surrogate it already has. The tap retains the last SPS/PPS and replays them to each new subscriber, because a machine that joins mid-stream would otherwise have nothing to configure a decoder with. `--share-airplay` (requires `--airplay-screen`) turns it on.

### Wire compatibility

Bump `PROTOCOL_VERSION` in `protocol/src/lib.rs` (currently 13) whenever the shared vocabulary changes; a module's own message set is versioned separately through `ModuleAdvert`. Framing is a 4-byte big-endian length prefix plus that many payload bytes.

### Native and tooling directories

`native/playfair` (FairPlay C, vendored) · `native/cursio-vad` (Windows audio driver fork, see `CURSIO-FORK.md`) · `native/cursio-shell-extension` (GNOME extension for drag detection) · `tools/wgc-probe` (settles whether Windows Graphics Capture keeps delivering frames from a hidden window) · `crates/xtask` (all build/install/cross-compile automation).
