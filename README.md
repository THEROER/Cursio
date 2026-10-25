# Cursio

A LAN-only mesh of trusted personal devices sharing encrypted, resumable sessions.

Input, audio, clipboard, window transfer and file sharing are **modules** on those
sessions — not separate products. Applications never migrate; only pixels,
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
- Use a peer's USB devices as if they were plugged in here (USB/IP)
- Offer a peer's windows and monitors to Discord, OBS or a browser call here,
  without drawing them on this desktop
- Receive iPhone/iPad screen mirroring as an AirPlay target for the whole mesh
- Bring an Android phone in as a full peer: its notifications where you are,
  its SMS, its calls with the voice on the computer (Cursio is the hands-free
  unit, over Bluetooth), its sound and microphone, its storage and gallery, and
  its whole screen in a window you drive with the mouse — scrcpy without ADB
- Update itself: a signed build offered by a machine you trust installs side by
  side and rolls back if it does not come up

## What it is not

- Not a remote desktop — you work on a peer by moving the cursor onto its own
  screen; windows and screens cross the link only as pictures
- Not application or process migration — the app never leaves its host
- Not virtualization
- Not built for the Internet or NAT traversal (LAN only in v0.x)
- Not a replacement for your mixer — Cursio replaces the network hop, not
  VoiceMeeter or Carla

## Supported platforms (v0.9)

| | Input | Clipboard | Window host | Window view | Remote screen | Audio | Files | USB | AirPlay |
|---|---|---|---|---|---|---|---|---|---|
| **Linux** (Wayland, X11) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Windows 11** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **macOS 13+** | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ | serve only | ❌ | ✅ |
| **Android** (arm64) | touchpad | share sheet | its screen | ❌ | ❌ | ✅ | serve only | ❌ | ❌ |

- **Linux**: capture goes through the `InputCapture` portal + libei on
  Wayland and evdev on X11. Window capture uses the ScreenCast portal, so what
  works depends on the compositor; drag-to-transfer on GNOME needs the bundled
  Shell extension. Audio needs PipeWire, file mounts need FUSE, USB needs
  `usbip-host`. Tested on Ubuntu 24.04 and 25.10: a build runs only against the
  FFmpeg it was linked with, so each is built separately (`cargo xtask stage
  ubuntu24`).
- **Windows**: mounts need WinFsp, USB needs `usbipd-win`, audio endpoints
  need the bundled driver (`native/cursio-vad`). Cursio asks for UAC at
  start, since Windows silently drops input injected into elevated windows;
  declining leaves everything else working.
- **macOS**: a Mac can host a window but cannot yet display a peer's, refuses
  peers' remote-screen requests, and serves its own volumes but cannot mount a
  peer's. Input needs the
  Accessibility permission, window capture the Screen Recording one.
- **Android** (API 28+): the app in `app/` with the node linked in, kept alive
  as a `connectedDevice` foreground service. It plays peers' sound and offers
  its own and its microphone (AAudio), serves its shared storage and gallery,
  shows its screen on the computers you pick — the one you were at last
  first — and is driven from there through an accessibility service you turn
  on once. Notifications reach only the computer you are at; SMS are read and
  sent from a computer; calls ring there, and on Linux their voice too, with
  Cursio as the phone's Bluetooth hands-free unit and speaker (`cursio
  bluetooth install`). With the network gone it keeps the link over
  Bluetooth. It reports battery and network; it does not view windows, mount
  disks or share USB.

## Core concepts

**No server, no hub.** Every node listens for peers dialling in *and* dials the
known peers that are down. A node dying doesn't make the others forget it — they
mark it offline, carry on with each other, and reconnect when it returns.

**Machines are placed on a grid**, not by naming a side. `--at 1,0` is one
machine to the right of `0,0`; four in a row are `0,0 1,0 2,0 3,0`. Cells, not
pixels — nothing needs to know how wide anybody's screen is. A side cannot
express order, and a designated centre would make every machine depend on one
being up, which is what the mesh exists to avoid.

**One session, four independent delivery lanes.** Each pair of peers uses
separate encrypted TCP connections for interactive traffic, audio, video and
bulk data. The six priority classes remain `Control` > `Input` > `Audio` >
`Video` > `Metadata` > `Bulk`; a blocked file connection cannot block input.
Queues have byte budgets and backpressure, bulk traffic is paced against
interactive RTT, and video encoders share an adaptive budget. Connections can
be replaced without recreating modules. See [transport design](docs/transport-design.md).

**Input is relative.** Mouse motion crosses the wire as `dx`/`dy`, never as an
absolute position — absolute positioning teleports the cursor and breaks games
reading raw input. Keys cross the same way, so while you drive a peer its own
shortcuts (Alt+Tab included) act there.

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

With a node already running, `cursio pair` pairs without a password: it shows a
one-time code, and `cursio pair <CODE>` (or entering it in the app) on the other
machine within five minutes completes it.

Identity and trusted peers are persisted under the platform config directory
(`~/.config/cursio` on Linux, `%APPDATA%\cursio\cursio\config` on Windows,
`~/Library/Application Support/dev.cursio.cursio` on macOS).

## CLI

```
cursio mesh            # run as a mesh node — this is how everything runs
cursio status          # what the running node is doing: peers, modules, latency, problems
cursio pair [CODE]     # pair through the running node with a one-time code
cursio forget <NAME>   # remove a paired machine and its key
cursio peers           # list peers discovered on the LAN, then exit
cursio config          # print the config `mesh` would run with, and where it lives
cursio update ...      # install, switch and roll back versions
cursio portal install  # (Linux) make Cursio the ScreenCast portal for --remote-screen
```

What `mesh` runs comes from `config.toml` (`cursio config` prints its path);
every flag overrides the file for that run only, so each module switch comes as
a pair (`--audio` / `--no-audio`). Settings changed while the node runs are
renegotiated on the live links; the few that cannot be are reported as waiting
for the next restart.

| Flag | Effect |
|---|---|
| `--at X,Y` | Where this machine stands on the grid. Defaults to `0,0`, where nothing borders anything and Ctrl+Alt+G is the only handoff. Negative cells are fine (`--at -1,0`). |
| `--send-input` | Capture this machine's mouse/keyboard and share it with peers |
| `--clipboard` | Share the clipboard (text, images, other formats) |
| `--audio` | Share audio per application |
| `--files` | Share disks both ways; a peer's volumes appear under `~/Cursio/<peer>/` |
| `--files-share <PATH[:ro]>` | Offer only this root instead of every disk; repeat for several, `:ro` refuses changes |
| `--usb` | Share this machine's USB devices and use the peers' |
| `--remote-screen` | Offer peers' windows and monitors to screen-sharing apps here; `--no-screen-host` refuses peers' requests for this machine's |
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
| `--airplay-interface <addr>` | Pin AirPlay's mDNS to the LAN interface with this address |
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
cargo xtask run                # build and run `cursio mesh --send-input --clipboard`, restarting on crash
cargo xtask windows            # cross-compile the .exe(s) from Linux (MSVC)
cargo xtask macos              # build on a Mac
cargo xtask android            # the phone app as an arm64 APK (needs ANDROID_HOME, ANDROID_NDK_HOME, bun)
cargo xtask stage ubuntu24     # an Ubuntu 24.04 build, in docker
cargo xtask publish            # stage host + Windows + Ubuntu 24.04, sign, offer to the mesh
cargo xtask logs --follow      # print the log dir and tail the current log
```

Builds install side by side under a per-user root, with a `current` pointer
and rollback (`docs/update-design.md`). A machine takes a build from the mesh
only if it trusts the machine that built it (`cursio update trust-builder`),
and applies it per `[update] policy = "auto" | "ask" | "never"`.

`cargo xtask deploy` and `cargo xtask live-test` drive a real Windows peer over
ssh (`docs/live-test.md`). Where that peer is lives in `.env` at the root of the
checkout, which git ignores: copy `.env.example` and fill it in.

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
  `%APPDATA%\cursio\cursio\data\logs` on Windows,
  `~/Library/Application Support/dev.cursio.cursio/logs` on macOS

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
- `crates/usb` — USB/IP export and import, and who holds a device
- `crates/clipboard-core` — clipboard backends, shared with `clipd`
- `crates/android` — the phone's node and its JNI entry points, plus the
  Kotlin service, plugin and backends (`plugin/`)
- `crates/bluetooth` — Cursio as a phone's hands-free unit and A2DP speaker,
  and the mesh's Bluetooth channel
- `crates/airplay` — the AirPlay receiver, deliberately outside the mesh, with
  the vendored FairPlay C in `native/playfair`
- `crates/logging` — console + rotating-file logging shared by every binary
- `crates/xtask` — build/install/cross-compile automation
- `app` — the Tauri UI: the desktop window and tray, and the whole phone app
- `apps/clipd` — standalone Linux clipboard history popup (GTK4/libadwaita)
- `native/` — the Windows audio driver and the GNOME extension (drag
  detection and the quick-settings tile)

Platform crates are **mechanism**; the matching `*_session.rs` in `core` is
**policy**. A feature is added as a module, never by branching in the CLI.

`docs/*.md` are the source of truth for their subsystems: [mesh](docs/mesh-design.md),
[platform](docs/platform-design.md), [input](docs/input-design.md),
[audio](docs/audio-design.md), [clipboard](docs/clipboard-design.md),
[window transfer](docs/window-transfer-design.md), [USB](docs/usb-design.md),
[messages](docs/messages-design.md), [calls](docs/calls-design.md),
[media](docs/media-design.md), [gallery](docs/gallery-design.md),
[updates](docs/update-design.md), [live testing](docs/live-test.md).

## Limitations (v0.x)

- DRM/protected surfaces may not be capturable
- Fullscreen applications and games are experimental
- Wayland compositors restrict capture and window control; drag detection on
  GNOME needs the bundled Shell extension
- Latency depends on network and hardware

## Roadmap

**Done** — encrypted mesh, input proxy, drag-to-edge window transfer,
per-application audio, clipboard, file sharing, USB sharing, remote screens
for screen-sharing apps, AirPlay receiver, signed self-update, the phone as a
full peer (audio, microphone, storage, gallery, notifications, SMS, calls,
media control, its screen driven from a computer, Bluetooth fallback).

**Next** — displaying a peer's window on macOS, macOS audio, the Windows audio
driver in service, calls' voice on Windows, the phone's clipboard in the
background, VideoToolbox encoding, compositor integrations.

**Mounting a peer's disks on macOS** — through a localhost NFSv3 server inside
Cursio, mounted by the system's own NFS client, rather than through FUSE:
nothing to install, no kernel extension, every macOS version. The FUSE routes
are all worse today: `fuser` is untested on macOS and lost its mount path in
macFUSE 5.3; macFUSE's kext needs Reduced Security set from Recovery on Apple
Silicon; its kext-free FSKit backend exposes no FUSE device fd (only the
still-changing `MFMount.framework`), mounts only under `/Volumes`, and is
slower; FUSE-T replaces libfuse rather than exposing an fd, and is free only
for personal use. A non-root `mount_nfs` of a localhost export is what
Hugging Face's hf-mount and `rclone nfsmount` already ship. The server side is
`nfs3_server` (its CI covers macOS arm64), with two gaps to patch: FSSTAT and
PATHCONF are constants in it (1 TiB free, case-sensitive), so a hook for each
goes upstream while a patched copy is vendored. NFS has no open/close, so the
adapter keeps peer handles per file id and closes them before a remove or
rename, which a Windows peer otherwise refuses. Mount options: `vers=3,tcp,
noresvport,locallocks` (`nolocks` makes `flock` fail with `ENOTSUP`), `intr`
and a `deadtimeout` so a crashed node does not wedge callers, and rsize/wsize
of 1 MiB.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security problems go through
[SECURITY.md](SECURITY.md), not public issues.

## License

Cursio is free software: you can redistribute it and/or modify it under the
terms of the [GNU General Public License](LICENSE), version 3 or (at your
option) any later version. It is distributed in the hope that it will be
useful, but WITHOUT ANY WARRANTY.

The GPL is not a preference stated on top of the code: the FairPlay setup an
iPhone needs and the WinFsp bindings a Windows mount needs are both GPL-3.0, so
every desktop binary already is. [THIRD-PARTY.md](THIRD-PARTY.md) lists the
vendored code and its licenses — the Windows audio driver in
`native/cursio-vad` stays under Microsoft's MS-PL, as a separate program.

AirPlay and FairPlay are trademarks of Apple Inc. Cursio is not affiliated with
or endorsed by Apple; it implements the receiver side of the protocol for
interoperability.
