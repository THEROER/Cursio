# Cursio

A LAN-only mesh of trusted personal devices sharing one encrypted channel.

Input, audio, clipboard, window transfer and file sharing are **modules** on that
channel — not separate products. Applications never migrate; only pixels,
samples, events and file requests cross the link.

The goal is simple: **it should not matter which machine an application was
launched on.**

## What it does

- Share one mouse and keyboard across every machine, moving between them by
  pushing the cursor off a screen edge
- Stream an individual application window to another machine while the app keeps
  running on its original host
- Carry the clipboard across machines, with formatting and images intact
- Route audio **per application**, so each remote app appears here as its own
  named node
- Mount a peer's disks locally, on demand
- Receive iPhone/iPad screen mirroring as an AirPlay target for the whole mesh

## What it is not

- Not a remote desktop — a window is transferred, not a screen
- Not application or process migration — the app never leaves its host
- Not virtualization
- Not built for the Internet or NAT traversal (LAN only in v0.x)
- Not a replacement for your mixer — Cursio replaces the network hop, not
  VoiceMeeter or Carla

## Supported platforms (v0.1)

| | Input | Clipboard | Window host | Window view | Audio | Files | AirPlay |
|---|---|---|---|---|---|---|---|
| **Linux** (Wayland) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Windows 11** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **macOS 13+** | ✅ | ✅ | ✅ | ❌ | ❌ | serve only | ✅ |

A Mac can host a window but cannot yet display a peer's; it serves its own
volumes but cannot mount a peer's. Wayland support depends on compositor
security policy and varies between compositors.

## Core concepts

**No server, no hub.** Every node listens for peers dialling in *and* dials the
known peers that are down. A node dying doesn't make the others forget it — they
mark it offline, carry on with each other, and reconnect when it returns.

**Machines are placed on a grid**, not by naming a side. `--at 1,0` is one
machine to the right of `0,0`; four in a row are `0,0 1,0 2,0 3,0`. Cells, not
pixels — nothing needs to know how wide anybody's screen is. A side cannot
express order, and a designated centre would make every machine depend on one
being up, which is what the mesh exists to avoid.

**One encrypted channel, five priority classes.** Everything rides one Noise
link, scheduled `Control` > `Input` > `Audio` > `Video` > `Bulk`, so an input
event queued mid-file-transfer jumps ahead of that transfer's remaining chunks.

**Input is relative.** Mouse motion crosses the wire as `dx`/`dy`, never as an
absolute position — absolute positioning teleports the cursor and breaks games
reading raw input. System shortcuts such as Alt-Tab stay local.

**A drag is the only way to start a window transfer.** There is no flag for it,
so nothing is ever transferred by accident. Receiving stays passive: a peer's
window always arrives.

## Quick start

Build the CLI:

```sh
cargo build -p cursio-core
```

Pair two machines by running the *same* command on **both**, with the same
password:

```sh
cargo run -p cursio-core -- mesh --pair
```

Then run the mesh with the modules you want:

```sh
cursio mesh --at 0,0 --send-input --clipboard
```

## Pairing

Pairing prompts for the password without echoing it. There is no separate
"listen" and "join" side: a mesh node already listens and dials at once, and the
two ends sort out which one places the call. `--pair` only opens the ten-minute
window in which a machine neither has met is allowed to be called at all.

On first contact the two run SPAKE2 pairing in symmetric mode — the password
never crosses the wire — confirm the derived key, remember each other, and hold a
session. A wrong password yields two *different* keys rather than an error, so an
explicit key-confirmation step follows; only after it is the peer stored. Later
runs skip pairing (`peer already trusted`), so a plain `mesh` is enough.

Identity and trusted peers are persisted under the platform config directory
(`~/.config/cursio` on Linux, `%APPDATA%\cursio` on Windows, `~/Library/
Application Support/cursio` on macOS).

## CLI

```
cursio mesh      # run as a mesh node — this is how everything runs
cursio peers     # list peers discovered on the LAN, then exit
```

Modules are off by default; each flag opts this machine in.

| Flag | Effect |
|---|---|
| `--at X,Y` | Where this machine stands on the grid. Defaults to `0,0`, where nothing borders anything and Ctrl+Alt+G is the only handoff. Negative cells are fine (`--at -1,0`). |
| `--send-input` | Capture this machine's mouse/keyboard and share it with peers |
| `--clipboard` | Share the clipboard (text, images, other formats) |
| `--audio` | Share audio per application |
| `--files` | Share disks both ways; a peer's volumes appear under `~/Cursio/<peer>/` |
| `--pair` | Open the pairing window (run on both machines) |
| `--pair-addr <HOST:PORT>` | Name the machine to pair with when mDNS will not; also means this side places the call |
| `--port <n>` | Listening port. Defaults to **46073**, deliberately stable — peers cache the last address to reconnect when mDNS is slow, and a port that changed each restart left that cache pointing at a dead one. Pass `0` for an ephemeral port. |
| `--mac-modifiers <SPEC>` | Where the modifiers land on a Mac (see below) |

`--files` mounts read-write, so anything either machine can reach, the other can
change. Linux mounts with FUSE, Windows with WinFsp.

### Taking the cursor

Push the cursor off the edge facing a peer to hand it over. To take it back from
an idle machine, press **Ctrl+Alt+G**.

The hotkey is defined in the **wire** key namespace (Linux evdev), so on a Mac it
moves with `--mac-modifiers`: `positional` (the default) puts it on Control+
Option+G, `swapped` on Command+Option+G. Startup logs which physical keys, since
a hotkey that quietly relocated is one you cannot find.

`--mac-modifiers` takes a preset (`positional`, `swapped`) or `from=to` pairs
such as `rightalt=globe`. `swapped` makes a PC keyboard's Ctrl+C copy on a Mac.

## AirPlay

`cursio mesh` participates in **one AirPlay receiver for the whole mesh**, named
plainly `Cursio`. Open Control Center → Screen Mirroring and select it. The mesh
picks a host that has a free control port and prefers one with a hardware H.264
decoder; if that host goes away, the rest recompute placement and one takes over.

| Flag | Effect |
|---|---|
| `--airplay <MODE>` | `on`/`control` (default) takes part; a profile such as `1440p:30fps` raises the mesh's quality request; `off` steps out of hosting |
| `--airplay-window` | Show the mirrored screen on *this* machine, even when another hosts the receiver |
| `--airplay-port <port>` | Change the control port |
| `--share-airplay` | When hosting, offer the mirrored screen to every peer rather than only those asking for a window |

The picture appears wherever you started it: the hosting peer forwards the H.264
untouched through the ordinary window path, so nothing is re-encoded on the way.

Scope today: screen mirroring, transient AirPlay pairing, FairPlay session setup,
encrypted H.264 transport and hardware-backed presentation. The audio that
accompanies mirroring is not decoded yet. Arbitrary file transfer is not an
AirPlay feature — Cursio's own encrypted file channel is the path for that.

**Platform video requirements**

- **macOS** — bundles its native AppKit/AVFoundation helper and Apple's hardware
  decoder; no GStreamer needed
- **Linux** — GStreamer 1.x with a hardware H.264 decoder (`vah264dec`,
  `nvh264dec`, or `vaapih264dec`) and a compatible Wayland/GL sink
- **Windows** — GStreamer 1.x with `d3d11h264dec` and `d3d11videosink`

Software decoding is refused rather than silently used. At startup the CLI checks
the video path and port availability; those results travel in the mesh greeting,
so every node computes the same placement.

## Building & installing

Project automation lives in `cargo xtask`, which wraps the env vars and target
triples each build needs:

```sh
cargo xtask build              # release build of supported binaries for this host
cargo xtask install            # build + copy cursio, clipd, cursio-app to ~/.local/bin
cargo xtask windows            # cross-compile the .exe(s) from Linux (MSVC)
cargo xtask logs --follow      # print the log dir and tail the current log
```

`install` also writes `.desktop` files under `~/.local/share/applications`; add
`~/.local/bin` to your `PATH` if it isn't already. `clipd` is Linux-only, so
macOS builds `cursio` and `cursio-app`.

> Scope cargo to a package — **`--workspace` fails on macOS**, because
> `apps/clipd` needs GTK4/libadwaita and `gdk-pixbuf-sys` ships no macOS
> pkg-config.

Native Linux builds need `OPUS_TARGET=ubuntu-24.04_x86_64`; `cargo xtask` sets it
automatically, a plain `cargo build` does not. Windows cross-builds compile
libopus from source and don't need it, but do need GStreamer's MSVC *development*
package unpacked on the build host and pointed at with `GSTREAMER_WINDOWS_SYSROOT`
(the prefix holding `lib/pkgconfig`, typically `.../1.0/msvc_x86_64`).

Run the tests:

```sh
cargo test -p cursio-protocol -p cursio-transport -p cursio-core
```

Several crates ship `examples/` that probe real hardware (`capture_probe`,
`wasapi_loopback_probe`, `grab_cycle`, `encode_bench`). They exist because the
thing they test needs a compositor, a GPU or an iPhone:

```sh
cargo run -p <pkg> --example <name>
```

## Logging

Every binary logs to **both** the console and a rotating file:

- **Console** (stderr): coloured, `info` by default
- **File**: plain, `debug` by default, rotated daily, under the platform data
  dir — `~/.local/share/cursio/logs/` on Linux,
  `%APPDATA%\cursio\cursio\data\logs` on Windows

Override the levels:

- `RUST_LOG=debug` — raises **both** sinks
- `CURSIO_LOG_FILE=trace` — raises only the **file**, leaving the console quiet

```sh
RUST_LOG=cursio_airplay=debug,info cargo run -p cursio-core -- mesh
cargo xtask logs --follow
```

## Project layout

Cursio is a Cargo workspace, layered `protocol` → `transport` → `core` →
platform crates → binaries:

- `crates/protocol` — wire vocabulary and length-prefixed framing, no I/O
- `crates/transport` — discovery, password pairing (SPAKE2 + key confirmation +
  trust store), Noise, and priority scheduling
- `crates/core` — identity, link bring-up, mesh supervision, the session loop,
  the module implementations, and the `cursio` CLI
- `crates/input` — capture and injection per platform
- `crates/window` — window capture, encoding, and surrogate windows
- `crates/audio` — PipeWire and WASAPI backends
- `crates/files` — serving volumes and mounting a peer's
- `crates/clipboard-core` — clipboard backends, shared with `clipd`
- `crates/airplay` — the AirPlay receiver, deliberately outside the mesh
- `crates/logging` — console + rotating-file logging shared by every binary
- `crates/xtask` — build/install/cross-compile automation
- `app` — the Tauri desktop UI
- `apps/clipd` — standalone Linux clipboard history popup (GTK4/libadwaita)
- `native/` — vendored FairPlay C, the Windows audio driver, and the GNOME
  extension used for drag detection

Platform crates are **mechanism**; the matching `*_session.rs` in `core` is
**policy**. A feature is added as a module, never by branching in the CLI.

`docs/*.md` are the source of truth for their subsystems: [mesh](docs/mesh-design.md),
[platform](docs/platform-design.md), [input](docs/input-design.md),
[audio](docs/audio-design.md), [clipboard](docs/clipboard-design.md),
[window transfer](docs/window-transfer-design.md).

## Limitations (v0.x)

- DRM/protected surfaces may not be capturable
- Fullscreen applications and games are experimental
- Wayland compositors restrict capture and window control; drag detection on
  GNOME needs the bundled Shell extension
- Latency depends on network and hardware

## Roadmap

**Done** — encrypted mesh, input proxy, drag-to-edge window transfer,
per-application audio, clipboard, file sharing, AirPlay receiver.

**Next** — displaying a peer's window on macOS, VideoToolbox encoding, adaptive
bitrate, compositor integrations.

## License

Not yet chosen (`license = "TBD"`). Until one is added, no usage rights are
granted.
