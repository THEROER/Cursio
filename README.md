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

macOS 13+ — input, clipboard, AirPlay and window *hosting*. A Mac cannot yet
display a peer's window, and has no audio or file-sharing backend.

Wayland support depends on compositor security policies and may vary.

Core concepts

Applications always run on the source machine

Only the window surface and input are transferred

When a window is moved:

it is hidden on the source (Windows), so the transfer feels like a move

and appears as a native window on the target

Input model

Mouse input is sent as relative motion (dx/dy)

Keyboard events are proxied to the source

System shortcuts (e.g. Alt-Tab) remain local

Performance

Hardware H.264 encoding where the platform provides it — ffmpeg on Linux, Media
Foundation on Windows

Focus on low latency and high visual quality

LAN-only transport

User interaction

Move windows by dragging them across screen edges — the only way to offer one,
so nothing is transferred by accident

Take the cursor back with Ctrl+Alt+G (in the wire key namespace, so a Mac's own
mapping is logged at startup)

Device pairing with password authentication

Limitations (v0.x)

DRM/protected surfaces may not be capturable

Fullscreen applications and games are experimental

Wayland compositors impose restrictions on capture and window control

Latency depends on network and hardware capabilities

Roadmap

Done: encrypted mesh, input proxy, drag-to-edge window movement, per-application
audio, clipboard, file sharing, AirPlay receiver

Next: displaying a peer's window on macOS, VideoToolbox encoding, adaptive
bitrate, compositor integrations

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

`cursio mesh` participates in one AirPlay receiver for the whole mesh. Open
Control Center → Screen Mirroring and select `Cursio`. The mesh chooses a host
with a free control port and prefers one with hardware H.264 decoding.

`--airplay 1440p:30fps` asks the mesh for that quality without implying a local
window; a better request rebuilds the selected receiver. Add
`--airplay-window` on the machine where the mirrored screen should appear, even
when another machine hosts the receiver. `--airplay off` removes this machine
from hosting, while `--airplay-port <port>` changes its control port.

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

At startup the Rust CLI checks the platform video path and port availability.
Those results and the requested quality travel in the mesh greeting, so every
node computes the same placement and failover when a host disconnects.

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

`windows` links AirPlay video against GStreamer, so it needs that project's
MSVC *development* package unpacked on the build host and pointed at with
`GSTREAMER_WINDOWS_SYSROOT` (the prefix holding `lib/pkgconfig`, typically
`.../1.0/msvc_x86_64`). `xtask` checks for it before building and wires up
`PKG_CONFIG_*` itself.

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

Run the *same* command on **both** machines, with the same password:

```sh
cargo run -p cursio-core -- mesh --pair
```

It prompts for the password without echoing it. There is no separate "listen"
and "join" side: a mesh node already listens and dials at once, and the two ends
sort out which one places the call. `--pair` only opens the ten-minute window in
which a machine neither has met is allowed to be called at all.

On first contact they run SPAKE2 pairing, confirm the derived key, remember each
other, and hold a session. Later runs skip pairing (`peer already trusted`), so
plain `mesh` is enough. A wrong password is rejected at key confirmation and no
trust is stored.

Useful flags:

- `mesh --pair-addr <ip:port>` — name the machine to pair with when mDNS will
  not; also means this side places the call.
- `mesh --port <n>` — pin the listening port.
- `peers` — list peers discovered on the LAN and exit.
- `RUST_LOG=debug` — verbose logging (heartbeats etc.).

Identity and trusted peers are persisted under the platform config directory
(`~/.config/cursio` on Linux; `%APPDATA%\cursio` on Windows; `~/Library/
Application Support/cursio` on macOS).

Session traffic runs over a Noise `NNpsk0` channel keyed by the paired secret —
input, audio, clipboard, windows and file requests all share that one encrypted
link.

License

TBD.
