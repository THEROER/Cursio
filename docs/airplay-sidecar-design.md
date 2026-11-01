# AirPlay as a process of its own

Status: **design**. Nothing here is built yet.

## Why

`cursio-airplay` compiles playfair — the FairPlay setup an iPhone requires
before it mirrors — and playfair is GPL-3.0 (`THIRD-PARTY.md`). Any
executable that links `cursio-airplay` is therefore a GPL work as a whole,
whatever the rest of its source says. Today that executable is the node:
`cursio` links it through the default `airplay` feature.

Two ways out were weighed:

- **A crate boundary** — keep `cursio-airplay` a crate, everything else
  under another license. That frees the *source*: a product may take
  `cursio-transport`, `cursio-protocol` or `cursio-core` without `airplay`.
  It does not free a binary that has AirPlay in it: linking is what the GPL
  reaches through, and a crate is still linked.
- **A process boundary** — the receiver runs as `cursio-airplay`, a separate
  executable, and the node talks to it over a local socket. Two programs
  exchanging data over a socket, shipped side by side, are an aggregate, not
  one program: the receiver executable is GPL, the node is not, and the node
  keeps AirPlay. This is how the Windows audio driver already stays MS-PL
  next to a GPL node (`native/cursio-vad`, IOCTL).

Only the second keeps AirPlay *and* frees the node, so it is the one designed
here. The first is what `cargo xtask license-check --no-default-features
--features …` already proves for a build without AirPlay.

The exchange must stay a plain protocol between programs — control messages
and H.264 access units — and not a channel for the node's internal data
structures; that is what keeps them two programs and not one split in two.

## What AirPlay touches today

| Code | What it does | After the split |
|---|---|---|
| `crates/airplay` (receiver, RTSP, crypto, playfair, mirror framing, video) | the iPhone side | **moves** into the `cursio-airplay` executable, unchanged |
| `core/src/bin/cursio/airplay.rs` (`Receiver` setup, `Announcer`, bind probe, quality) | starts and announces the receiver | **splits**: starting and announcing move to the executable; the node keeps deciding *whether* this machine hosts (placement) and asks the executable to start, stop, withdraw or re-announce |
| `core/src/service_session.rs`, `protocol/src/service.rs` | placement across the mesh, refusal and fallback | **stays** in the node: it is mesh policy, and it never touched playfair |
| `core/src/airplay_window.rs` (`MirrorTap` → window frames) | the iPhone screen as a transferable window | **stays** in the node, reading access units from the socket instead of an in-process `MirrorTap` |
| `window_session/factory.rs` (`WindowFactory`'s AirPlay field) | gives the window module the tap | **stays**, holding the socket-backed tap |

## The boundary

One local socket per node, private as the control socket is (owner-only
Unix socket; named pipe with this user's DACL on Windows). The node starts
the executable when placement first chooses this machine, and ends it with
the node. Two kinds of traffic:

**Control (JSON lines, both ways).**

- node → receiver: `Start { name, port, quality, window }`, `Withdraw`,
  `Announce`, `Stop`.
- receiver → node: `Bound { addr }` or `BindFailed { reason }` (the
  availability probe the placement offer depends on), `Mirroring { device,
  width, height }`, `Ended { reason }`.

**Video (length-prefixed binary, receiver → node).** Each H.264 access unit
exactly as `MirrorTap` hands it out today — Annex-B, with the retained
SPS/PPS sent first to a late subscriber — so `airplay_window.rs` and every
viewer's decoder are unchanged. Nothing is decoded or re-encoded on either
side, as now.

The local window (`--airplay-window`) is drawn by the receiver executable
itself, which already owns its GStreamer pipeline; only frames bound for
peers cross the socket.

## What stays the same

- The picker name, one receiver per mesh, placement by decoder fitness,
  failover and refusal-with-reason (`CLAUDE.md`, "AirPlay is deliberately
  outside the mesh") — all node-side, all unchanged.
- The wire between machines: AirPlay never crossed it, and the window module
  carries the frames as before.

## Building and shipping

- `cursio-airplay` becomes a `[[bin]]` of `cursio-airplay` (the crate), GPL,
  staged as its own component (`manifest.json`), installed beside the node
  in `versions/<v>/`, found there by the node the way the drive helper is
  (`mount_helper.rs`).
- The node's `airplay` feature then means "speak the sidecar protocol", which
  links no playfair; `cargo xtask license-check` must pass for the node with
  it on.
- A product embedding Cursio ships the executable beside itself to have
  AirPlay, or leaves it out and has none.

## Order of work

1. Define the control and video messages in a small crate of their own.
2. Build the executable around the existing `Receiver`, `Announcer` and
   `MirrorTap`.
3. Point `bin/cursio/airplay.rs` and `airplay_window.rs` at the socket.
4. Stage and install the executable; drop `cursio-airplay` from the node's
   dependencies.
5. Live: mirror an iPhone to Nocturne, move placement to Lightmoon, watch the
   window on a third machine.
