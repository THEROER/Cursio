Cursio is a LAN-first, cross-platform seamless workspace tool that allows you to move application windows between machines while the applications continue running on their original host.

The goal is simple:

It should not matter where an application is launched.

What it does

Stream individual application windows across machines on the same LAN

Proxy mouse and keyboard input back to the source system

Treat remote windows as part of a single logical workspace

Integrate naturally with cursor boundaries and window movement

What it is not

Not full remote desktop

Not application or process migration

Not virtualization

Not designed for Internet/NAT traversal (LAN only in v0.x)

Supported platforms (v0.1)

Windows 11

Linux (Wayland)

Wayland support depends on compositor security policies and may vary.

Core concepts

Applications always run on the source machine

Only the window surface and input are transferred

When a window is moved:

it is hidden or minimized on the source (configurable)

and appears as a native window on the target

Input model

Mouse input is sent as relative motion (dx/dy)

Keyboard events are proxied to the source

System shortcuts (e.g. Alt-Tab) remain local

Performance

Hardware encoding via NVIDIA NVENC

Focus on low latency and high visual quality

LAN-only transport

User interaction

Move windows by dragging them across screen edges

Or via configurable hotkeys (e.g. Super + Arrow)

System tray + GUI for control and settings

Device pairing with password authentication

Limitations (v0.x)

DRM/protected surfaces may not be capturable

Fullscreen applications and games are experimental

Wayland compositors impose restrictions on capture and window control

Latency depends on network and hardware capabilities

Roadmap

v0.1: single-window streaming, input proxy, pairing

v0.2: multiple windows, bi-directional sessions

v0.3: seamless drag-to-edge window movement

v0.4: optional audio streaming and A/V sync

Later: encryption, adaptive bitrate, compositor integrations

## Project layout

Cursio is a Cargo workspace:

- `crates/protocol` — wire messages and length-prefixed framing
- `crates/transport` — the `Transport` trait + auto-selection, TCP transport,
  mDNS discovery, and password pairing (SPAKE2 + key confirmation + trust store)
- `crates/core` — node identity, session handshake/heartbeat, and the `cursio` CLI
- `crates/logging` — `cursio-log`: console + rotating-file logging shared by every binary
- `crates/xtask` — build/install/cross-compile automation (`cargo xtask`)
- `apps/clipd` — clipboard history popup (GTK4/libadwaita)
- `app` — the Tauri desktop UI (builds on `cursio-core`)

## Building & running (networking milestone)

The current milestone is discovery + pairing + a live heartbeat session. The
`cursio` CLI drives it; no Tauri toolchain is needed for this part.

Build the networking crates and CLI:

```sh
cargo build -p cursio-core
```

Run tests:

```sh
cargo test -p cursio-protocol -p cursio-transport -p cursio-core
```

### AirPlay receiver

Start the receiver with `cursio mesh --airplay`. On an iPhone, open Control
Center → Screen Mirroring and select `Cursio - <computer name>`. Without the
`--airplay` flag Cursio does not bind or advertise an AirPlay listener.
Use `--airplay-name <name>` and `--airplay-port <port>` to change the
announcement.

The `Cursio - AirPlay Screen` window opens only after an iPhone connects and
requests receiver information; starting `mesh --airplay` alone does not open
an empty window.

Current AirPlay scope:

- iPhone/iPad screen mirroring into a native Cursio video window;
- transient AirPlay pairing, FairPlay session setup, encrypted H.264 transport,
  and hardware-backed presentation;
- the audio that accompanies screen mirroring is planned but is not decoded
  yet;
- arbitrary file transfer is not an AirPlay feature. Cursio's own encrypted
  mesh file channel is the intended path for sending files between computers.

Platform video requirements:

- macOS: the receiver includes its native AppKit/AVFoundation presentation
  helper and uses Apple's hardware H.264 decoder; no GStreamer installation is
  required;
- Linux: GStreamer 1.x plus a hardware H.264 decoder (`nvh264dec`, `vah264dec`,
  or `vaapih264dec`) and a compatible Wayland/GL sink;
- Windows: GStreamer 1.x with `d3d11h264dec` and `d3d11videosink`.

At startup the Rust CLI checks the platform video path and prints a concrete
warning when GStreamer, a decoder, or a GPU-compatible sink is missing. AirPlay
discovery remains available so the diagnostic is visible without taking the
rest of the mesh offline.

### Build & install (`cargo xtask`)

Project automation lives in `cargo xtask`, which wraps the env vars and target
triples each build needs:

```sh
cargo xtask build              # release build of supported binaries for this host
cargo xtask install            # build + copy cursio, clipd, cursio-app to ~/.local/bin
                               # (+ .desktop launchers for the GUI apps on Linux)
cargo xtask windows            # cross-compile the .exe(s) from Linux (mingw)
cargo xtask logs --follow      # print the log dir and tail the current log
```

`install` also installs `.desktop` files under `~/.local/share/applications`.
Add `~/.local/bin` to your `PATH` if it isn't already. `clipd` is Linux-only,
so macOS builds `cursio` and `cursio-app`. Android remains a placeholder.

> On this Ubuntu box `cargo xtask` sets `OPUS_TARGET=ubuntu-24.04_x86_64`
> automatically for native builds (a plain `cargo build` still needs it set by
> hand). Windows cross-builds compile libopus from source and don't need it.

### Logging

Every binary logs to **both** the console and a rotating file:

- Console (stderr): coloured, `info` by default.
- File: plain, `debug` by default, rotated daily, under the platform data dir
  — `~/.local/share/cursio/logs/` on Linux, `%APPDATA%\cursio\cursio\data\logs`
  on Windows. Each binary writes its own file (`cursio.log.<date>`, etc.).

Override the levels via env vars:

- `RUST_LOG=debug` — raises **both** sinks (e.g. `RUST_LOG=trace`).
- `CURSIO_LOG_FILE=trace` — raises only the **file**, leaving the console quiet.

`cargo xtask logs` prints the directory; add `--follow` to `tail -f` the newest.

### Pairing two machines on your LAN

On machine A (e.g. Ubuntu), start a node that announces itself and waits:

```sh
cargo run -p cursio-core -- serve --password <shared-secret>
```

On machine B (e.g. Windows), discover A on the LAN and connect:

```sh
cargo run -p cursio-core -- connect --password <shared-secret>
```

Both must use the **same password**. On first contact they run SPAKE2 pairing,
confirm the derived key, remember each other, and hold a heartbeat session. On a
later run they skip pairing (`peer already trusted`). A wrong password is
rejected at key confirmation and no trust is stored.

Useful flags:

- `connect --addr <ip:port>` — skip discovery and dial a peer directly.
- `peers` — list peers discovered on the LAN and exit.
- `serve --port <n>` — pin the listening port (default: OS-assigned).
- `RUST_LOG=debug` — verbose logging (heartbeats etc.).

Identity and trusted peers are persisted under the platform config directory
(`~/.config/cursio` on Linux; `%APPDATA%\cursio` on Windows).

> Note: post-pairing session traffic is not yet encrypted — the derived key is
> stored for the upcoming Noise channel. Encryption is on the roadmap (below).

License

TBD.
