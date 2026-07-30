# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Always scope cargo to a package. **`--workspace` fails on macOS**: `apps/clipd` depends on GTK4/libadwaita, and `gdk-pixbuf-sys` has no macOS pkg-config.

```sh
cargo build -p cursio-core              # the `cursio` CLI (the usual build)
cargo test -p cursio-airplay            # one crate's tests
cargo test -p cursio-airplay decodes_   # tests whose name contains this
cargo test -p cursio-airplay -- --exact media::tests::rejects_partial_key_setup
cargo clippy -p <pkg> --all-targets     # must be clean before committing
cargo fmt -p <pkg>
```

Linux native builds need `OPUS_TARGET=ubuntu-24.04_x86_64` (`cargo xtask` sets it; a bare `cargo build` does not).

Automation — prefer these over hand-rolled commands:

```sh
cargo xtask build | install | macos | logs --follow
cargo xtask windows     # cross-compile; needs GSTREAMER_WINDOWS_SYSROOT
```

Running:

```sh
cargo run -p cursio-core -- mesh --airplay-screen
RUST_LOG=cursio_airplay=debug,info cargo run -p cursio-core -- mesh --airplay-screen
```

`RUST_LOG` raises console + file; `CURSIO_LOG_FILE` raises only the file. Logs rotate daily under the platform data dir.

## Commits

Conventional Commits, **subject line only**. No body, no `Co-Authored-By`, no emoji, no issue refs.

```
type(scope): imperative lowercase summary
```

- Types in use: `feat`, `fix`, `perf`, `refactor`, `test`, `chore`, `style`.
- Scope is the crate or subsystem, not a path: `airplay`, `transport`, `window`, `input`, `audio`, `mesh`, `cli`, `xtask`, `build`, `security`, `deps`.
- Say what the change *does to behaviour*, not what you edited. `fix(airplay): keep cipher aligned for IDR frames` — not `fix: update mirror.rs`.
- One commit per issue, containing **every** file that issue touched. Never one commit per file; never one commit spanning unrelated fixes.
- Order commits so each one compiles: a helper lands before its caller.
- Never amend or force-push unless asked.

## Code conventions

**Comments explain why, and only where the reason is not recoverable from the code.** This codebase is unusually heavy on rationale and near-empty of narration. Module-level `//!` blocks carry the design argument; `///` covers non-obvious invariants; inline `//` is reserved for protocol quirks, platform bugs, and decisions a reader would otherwise "fix" wrongly:

```rust
// Byte 4 selects the payload kind. Byte 5 is a video subtype flag (notably
// 0x10 for an IDR frame), not the high byte of a u16 type. Treating 00 10 as
// type 0x1000 skips the first encrypted IDR payload and permanently
// desynchronizes the continuous AES-CTR stream.
```

Do not write: `// increment counter`, `// loop over items`, restatements of the signature, changelog notes, or your own reasoning about the edit.

Never commit `TODO`/`FIXME`/`XXX`, placeholder values, fake data, or commented-out code.

**Tests** live in `#[cfg(test)] mod tests` at the bottom of the file they cover. Names are behavioural sentences, not `test_foo`:

```
decodes_fragmented_request_and_body
rejects_control_characters_in_header_values
mirror_ctr_state_is_continuous_across_calls
undecodable_packets_are_skipped_without_desyncing_the_cipher
```

**Errors**: one `thiserror` enum per library crate (`AirPlayError`, `ProtocolError`, `MirrorError`, `ConfigError`), `anyhow` in binaries and `xtask`. Error strings are lowercase, name the concrete cause, and say what to install or fix when it is actionable. Never swallow an error silently; degrade with `tracing::warn!` and an explicit reason.

**Dependencies** are declared once in the root `[workspace.dependencies]` and referenced as `foo.workspace = true`. Platform-only deps go under `[target.'cfg(target_os = "...")'.dependencies]`. Do not add a dependency without a clear justification.

**Platform code**: gate with `#[cfg(target_os = ...)]` and always provide a fallback arm that returns an explicit error rather than silently degrading. Hardware paths deliberately refuse to fall back to software (`PipelinePlan::detect` will not accept a software decoder).

`docs/*.md` are the source of truth for their subsystems. If code diverges from a design doc, either fix the code or update the doc in the same change.

## Architecture

### Mesh: no server, no hub

Every node does both halves at once — listens for peers dialling in, and dials the known peers that are down. `serve`/`connect` were two sides of one behaviour; `mesh` is that behaviour.

- `crates/core/src/registry.rs` — the durable set of known peers (seeded from the trust store) plus transient `PeerState`. A peer going away flips it to `Offline`; it is never removed. Each node observes liveness for itself.
- `crates/core/src/mesh.rs` — the supervisor: dial targets, sweep interval, reconnection.
- `crates/core/src/link.rs` — everything between "there is a socket" and "a session can start": `Hello` → SPAKE2 pairing on first meeting (or the key from the trust store) → Noise channel → module negotiation → cursor ownership.

**Sessions are `!Send`.** The input path shares its injector and cursor via `Rc` on purpose. Peer sessions are spawned with `spawn_local` onto a `LocalSet`, and the registry is `Rc<RefCell<_>>`. Do not "fix" this by adding `Arc`/`Mutex` — it is single-threaded by design.

### Modules are the extension point

A feature is a `SessionModule` (`crates/core/src/module.rs`), built by a `ModuleFactory` and assembled in a `ModuleRegistry`. `ModuleId` constants live in `crates/protocol/src/capability.rs` (input 1, audio 2, clipboard 3, window 4, files 5). Both sides advertise, negotiate, and only build what they agreed on. The CLI's only job is to construct the registry — after that nothing names a specific module. Add capability by adding a module, not by branching in the CLI.

`crates/protocol` is transport-agnostic: message types plus length-prefixed framing. Bump `PROTOCOL_VERSION` (currently 13) whenever the shared vocabulary changes; a module's own messages are versioned separately.

### AirPlay is deliberately outside the mesh

An iPhone speaks Apple's protocols, not Cursio's Hello/Noise session. `crates/airplay` therefore owns its own listener and crypto:

- `receiver.rs` — accepts control connections, one `MediaSession` per connection; survives transient accept failures; closes sessions that go silent for 30 s.
- `rtsp.rs` — bounded HTTP/RTSP framing plus discovery routes (`/info`, `OPTIONS`). Anything unimplemented returns 501 **on purpose**: acknowledging a stage whose state machine does not exist desynchronizes sender and receiver.
- `media.rs` — `SETUP` (timing + mirror stream), `TEARDOWN`, feedback. A `TEARDOWN` carrying `streams` ends that stream only and keeps the session key; a bodyless one ends the session.
- `mirror.rs` — mirror packet framing and AVC → Annex-B conversion. Both decoders consume with a cursor and compact only when the consumed prefix outgrows the remainder.
- `crypto/apple.rs` — transient Ed25519/X25519 pairing and FairPlay (native C in `native/playfair`, built by `build.rs`).
- `video/` — presentation. **macOS**: a small AppKit/AVFoundation helper compiled by `build.rs`, embedded in the binary, written to a temp path and spawned; frames cross a bounded queue on a dedicated writer thread so a stalled window never blocks a Tokio worker. **Linux/Windows**: GStreamer with a zero-copy plan (`vah264dec`/`nvh264dec`/`d3d11h264dec`).

The AES-CTR keystream is continuous for the lifetime of a mirror data connection. Every video payload must pass through the decryptor **in wire order**, even one that will be discarded — skipping before decryption corrupts every later frame. Framing errors end the stream; a single undecodable payload is logged and skipped.

`--airplay-screen` (alias `--airplay`) gates video; without it the control receiver still runs and mirror `SETUP` answers 501. `--no-airplay` disables the listener. A busy port 7000 (macOS' own receiver) logs a warning and leaves the mesh running.

### Other crates

`transport` (Transport trait, TCP, mDNS, pairing) · `input` (uinput/Wayland/Win32 capture and injection) · `window` (capture, encode, decode, surrogate windows, drag) · `audio` (PipeWire/WASAPI, Opus, VAD) · `files` (FUSE/WinFsp bridge) · `clipboard-core` + `apps/clipd` (Linux-only GTK4 clipboard history) · `app` (Tauri UI over `cursio-core`) · `logging` (console + rotating file).
