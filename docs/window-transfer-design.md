# Cursio — Window Transfer Design (drag a window to the other machine)

Status: **implemented, both halves.** Capture ships for Linux (ScreenCast portal
→ PipeWire), Windows (`Windows.Graphics.Capture`) and macOS (ScreenCaptureKit);
surrogate windows ship for Linux (GTK4) and Windows (raw Win32). A Mac can host a
window but cannot yet display a peer's — `MacWindowRenderer` says so rather than
failing obscurely. The protocol version quoted throughout this document is the
one each piece landed in, not the current one. This document is the source of
truth for how window transfer should behave; code should match it or this doc
should be updated. Companion to `input-design.md` (shared cursor & keyboard),
which this reuses for the transport, pairing, and the "drag past the edge"
trigger.

### First live capture (2026-07-20)

`capture_probe` against a 1920x1080 window on GNOME 49 / Wayland. What worked
on the first successful run:

- Portal session came up with `input_granted=true`, and the **restore token was
  saved** — the picker is a one-time cost, as designed.
- PipeWire negotiated **BGRA 1920x1080** exactly as requested, so frames need no
  colour conversion.
- The differ narrowed a full-screen first frame down to the window's real
  content box (`1344x756 at (16,183)`), i.e. dirty-region detection works.

**And it failed its traffic budget, informatively:** 244 frames cost **360 MiB**
— ~1.5 MiB/frame, with `seq` running unbroken (every single tick had changes).
The captured window was playing video/animation, which is the exact case the
budget section below predicts: a moving image dirties nearly every pixel, so the
bounding box degenerates to ~the whole window and zstd only reaches **~2.9x**
(median **1.40 bytes/pixel** out of 4). Lossless compression cannot rescue
motion — this measurement is the concrete argument for milestone 4, and it means
**`LossyVideo` is now the top priority, ahead of the adaptive controller**: no
amount of rate control makes 1.5 MiB/frame fit a LAN.

Not yet measured: a *static* window (should emit zero frames) and a
*text-editing* window (should be the few-hundred-byte case). Those two runs are
what would confirm the other half of the budget model.

### H.264 **lossless** settles it (measured 2026-07-20)

Benchmarked on this machine (GTX 1650, driver 580.126.09 — NVENC proven by a
real encode, not by the mere presence of `h264_nvenc`), 1080p60:

| Path | Realistic window (code + caret) | Worst case (full motion) | Text |
|---|---|---|---|
| zstd on dirty rects (before) | — | **742 Mbit/s** | exact |
| **H.264 lossless** | **2.6 Mbit/s** | **61 Mbit/s** | **exact** |
| H.264 lossy (`p1`/`ull`, 12M) | 0.3 Mbit/s | 15 Mbit/s | artefacts on text |

The decisive result is the middle row: **lossless H.264 is 12–280x cheaper than
zstd while staying pixel-exact.** Inter-frame prediction — not quantisation — is
what collapses the bitrate, because consecutive window frames are nearly
identical.

**Re-measured through Cursio's own encoder** (`cargo run -p cursio-window
--release --example encode_bench`, which includes the BGRA→YUV420P conversion
the ffmpeg CLI numbers skipped), 1920x1080:

| Content | Per frame | Bitrate | Encode speed |
|---|---|---|---|
| Static window | 0.1 KiB | **0.06 Mbit/s** | 2.3x real-time |
| Typing / caret | 0.1 KiB | **0.06 Mbit/s** | 2.4x real-time |
| Heavy motion (synthetic) | 281 KiB | **138 Mbit/s** | 2.5x real-time |
| *(raw BGRA, for scale)* | 8100 KiB | *3981 Mbit/s* | — |

**Correction to the earlier "61 Mbit/s worst case":** that figure came from
`testsrc2`. Measured across content types the ceiling depends heavily on what is
moving — smooth video-like gradients cost **34 Mbit/s**, `testsrc2` **61**, an
aggressive synthetic pattern **138**, and pure random noise **1532 Mbit/s**
(which fits nowhere, but never occurs on a real desktop). So: the common cases
are effectively free, ordinary video comfortably fits a gigabit LAN, and only
contrived worst cases strain it — which is what the adaptive controller
(milestone 3) is for.

**This simplified the design.** The original plan was a per-tile split — lossy
for motion, lossless for text — precisely to protect text from artefacts. A
lossless stream protects *everything*, so that machinery is unnecessary: one
encoder, no per-tile codec decision, no artefacts anywhere. Dirty-rect cropping
keeps its role for **small BGRA tiles** on an otherwise idle window, where
starting a video stream would be absurd. The `TileCodec` field stays on the wire
unchanged.

Verification worth repeating if these options change: all three NVENC presets
were confirmed lossless by decoding and comparing **byte-for-byte** against the
source (`cmp` on raw YUV), not by trusting the bitrate — `p1` produced a
suspiciously small stream that turned out to be genuinely exact.

Settings, and why: `preset=p4 tune=lossless` (`preset=lossless` is deprecated in
favour of p1–p7). p4 matches the old preset exactly on realistic content, sits
mid-pack on motion (60.9 Mbit/s vs p1 60.2, p7 51.3) and encodes 3 s of 1080p60
in 1.2 s. p7 compresses motion ~16% better for 40% more encode time — not worth
trading latency for bytes there is already room for. No B-frames (they reorder
output, which means holding frames back). Encoder choice is NVENC → VAAPI →
libx264 by what will actually **open**: this box lists `h264_vaapi` yet fails to
initialise it, since its only render node is NVIDIA. Software is a real
fallback, not a token one — x264 `ultrafast -qp 0` encoded the realistic
workload in 0.50 s per 3 s of video.

### …but only for a viewer that can decode it (2026-09-27)

Lossless H.264 exists only in the **High 4:4:4 Predictive** profile; NVENC and
x264 both switch to it for `qp=0` even from 4:2:0 input. FFmpeg decodes it.
Media Foundation — every Windows viewer — does not, and says nothing: measured
on the Windows peer with `examples/h264_decode_probe` (a High 4:4:4 stream from
each encoder, 30 packets in, **0 pictures out, 0 errors**; the same content as
High profile, 30 out). A Windows view of a Linux host therefore showed only the
lossless tiles that landed between video frames, over whatever the black
framebuffer held — the "patchwork, stale blocks, flicker" report.

So each window engine announces its decoder once per link, before anything is
offered (`WindowMsg::VideoCaps { lossless }`, window module v4). A host uses the
lossless stream only for a viewer that said it decodes it, and a lossy
High-profile stream at a bitrate (`frame_pipeline::lossy_bitrate`) for every
other. Until the announcement arrives it assumes High only: a stream that
cannot be decoded shows nothing, while a lossy one merely shows softer.

The same probe found two bugs in the Windows decoder itself, which had kept it
from showing even High-profile video: Microsoft's software decoder does not
allocate its output samples (handed an empty slot it fails with "null output
sample" exactly when a picture is ready), and by default it fills its whole
reorder buffer before releasing anything (`MF_LOW_LATENCY` turns that off). It
also reports the macroblock-padded size (1088 for 1080); the display aperture
is cropped so video frames and tiles agree on the picture's size.

## Model: one seamless remote window, app stays home

You grab a window's titlebar, drag it past the screen edge toward the peer, and
the window appears **as a window on the peer's desktop**. You interact with it
there; it behaves like a local window (move, resize, click, type).

The one thing that does **not** happen: the application does not move. Its
process, memory, and state stay on the **host** (the machine it started on).
What crosses the network is a *live view* — pixels out, input back:

- **Host** = where the app really runs. It captures that one window's pixels and
  streams them; it receives input and replays it on the real window.
- **Viewer** = where the window is dragged to. It shows a **surrogate** window
  from the streamed pixels and sends the user's input back to the host.

This is the **RemoteApp / Xpra "seamless single window"** model, with two
deliberate differences: it is **bidirectional** (either machine can be host for
a given window) and **cross-OS** (Windows ↔ Linux). It rides the Noise
transport, discovery, and pairing Cursio already has.

### One window, not the desktop

We transfer a **single window**, never the whole screen. This is what makes the
traffic budget realistic: a mostly-static window costs ~nothing to keep live
(only changed regions are sent), and there is no desktop, taskbar, or wallpaper
to stream. It also keeps the mental model clean — the surrogate is "that app's
window over there", not "a remote desktop".

### Why streaming, not state sync

The user was explicit: we do **not** synchronize application state/memory. That
would mean a shared or migrated process — a different, far harder project, and
impossible across unrelated OSes for an arbitrary app. Streaming the window's
output sidesteps all of it: the app never knows it's being viewed remotely.

## Traffic & latency budget (the core constraint)

This is the make-or-break engineering problem, so it is designed into the
protocol rather than bolted on.

**The hard number:** a raw 1080p BGRA frame is `1920 * 1080 * 4` ≈ **8.3 MiB**.
At 60 fps that is ~**4 Gbit/s** — past even gigabit LAN, and absurd over Wi-Fi.
So **raw pixels are for the first frame / bring-up only**; any steady stream
*must* be compressed. Not an optimization — a requirement.

**Goal: adaptive.** Aim for low latency, but never drown the channel. The
guiding rule under pressure: **degrade quality, never buffer stale frames.** A
growing send queue is unbounded latency; we would rather drop to a coarser
image that is *current* than deliver a sharp image that is late. (Same principle
as the input path's motion-coalescing in `session.rs`.)

Three independent levers, each attacking a different axis:

| Lever | What it saves | Where it lives |
|---|---|---|
| **Dirty regions** | send only the changed rectangle; static window ≈ 0 | `WindowFrame.dirty` |
| **Per-tile codec** | 4–20× on the changed pixels; lossless text, lossy motion | `WindowFrame.codec` |
| **Adaptive rate control** | size fps/quality to the link, don't queue | `WindowFrameAck` loop |

### Why the codec is per-tile, not per-session

Text/UI and video pull in opposite directions. A few small dirty rects of text
want **lossless** (crisp glyphs, cheap CPU). A playing video or a scroll produces
large frequent dirty areas that want **lossy** (or it saturates the link). One
global codec is wrong for one of them. So the host tags **each tile** with the
codec it chose for *that* region — sharp text and cheap motion can coexist in a
single visual update. A big change is split into several tiles (each its own
`WindowFrame` with its own `dirty` + `codec`), which also keeps any one frame
under the transport's `MAX_FRAME_LEN` (16 MiB).

### The adaptive loop

The host cannot fire frames blindly — if it out-runs the link, the queue grows
without bound. So the viewer periodically returns a `WindowFrameAck` with the
newest `seq` it has actually presented. The host compares **sent vs. acked** and
factors in **RTT** to estimate available bandwidth, then dials `fps`, tile
quality, and codec choice up or down. Acks are sent a few times a second (not
per frame — that would itself cost bandwidth). This is the closed loop that lets
us target latency without saturating the channel.

## Wire protocol

`crates/protocol/src/window.rs` (added in `PROTOCOL_VERSION = 4`). All types are
OS-neutral, the same discipline as `input.rs`.

- `WindowId` — `u32`, session-scoped, host-allocated. **Not** the OS window
  handle (that never leaves the host). Lets one session carry several
  transferred windows at once.
- `Message::WindowOffer(WindowMeta)` — host→viewer, sent once before any frame.
  `{ id, title, width, height }`: initial geometry + title so the viewer opens a
  matching surrogate. Title is display text only.
- `Message::WindowFrame(WindowFrame)` — host→viewer pixel update.
  `{ id, seq, dirty: Rect, codec: TileCodec, data }`. `seq` is a per-window
  monotonic counter (for acks + drop detection); tiles of one visual update may
  share a `seq`. `data` is the `dirty` sub-rect encoded per `codec`.
- `Message::WindowInput(WindowInput)` — viewer→host. Pointer/button/scroll/key in
  the **window's own absolute coordinates** (unlike the shared cursor's relative
  motion). The host maps `(x, y)` onto its real window and replays it.
- `Message::WindowFrameAck(WindowFrameAck)` — viewer→host rate-control feedback.
  `{ id, acked_seq }`. Sent sparingly.
- `Message::WindowClose { id, reason }` — either side ends a transfer.
  `WindowCloseReason` ∈ { `HostClosed`, `ViewerClosed`, `Returned` }.

### `TileCodec`

Ordered most-faithful → cheapest. Only the first two are implemented in the
first cut; the lossy variants are placeholders so the wire format is stable
before an encoder exists (callers never change):

- `RawBgra8888` — 4 B/px, uncompressed. First frame / bring-up only.
- `LosslessBgra8888` — lossless byte compression (zstd/lz4). Default for
  text/UI. Pixel-exact, cheap CPU.
- `LossyImage` — lossy still encoding (JPEG-class) for large photo-ish updates.
  *(placeholder)*
- `LossyVideo` — inter-frame video slice (H.264/VP9) referencing prior frames,
  for video/scroll/animation. The heaviest lever. *(placeholder)*

## Backends

Behind traits in `crates/window/src/lib.rs`, same split as `trait
InputInjector` — platform code slots in without touching the session:

- **Host:** `WindowCapture::begin(id) -> CaptureSession`. `CaptureSession` gives
  `meta()`, a `frames()` channel of `WindowFrame`, `apply_input()`, `on_ack()`
  (feeds the adaptive encoder), and `close()`.
- **Viewer:** `WindowRenderer::open(meta) -> Surrogate`. `Surrogate` gives
  `apply_frame()`, an `input()` channel of `WindowInput`, `take_ack()` (emits a
  due `WindowFrameAck`), and `close()`.

Channels (tokio mpsc), not callbacks, so a slow link never stalls the capture
thread — same reasoning as `CaptureHandle` for input.

### Per-platform capture / render (planned, not yet chosen in code)

- **Linux host capture:** `xdg-desktop-portal` `ScreenCast` with window
  selection → PipeWire stream. (Wayland forbids an app grabbing another window's
  pixels directly; the portal is the sanctioned path, consistent with the
  libei/portal direction already noted in `input-design.md`.)
- **Linux viewer surrogate:** a Wayland client window that blits decoded tiles.
- **Windows host capture:** Windows.Graphics.Capture (or Desktop Duplication)
  scoped to the window's HWND.
- **Windows viewer surrogate:** a Win32 window blitting decoded tiles.

Cross-OS works because everything on the wire is neutral (BGRA + neutral input
codes); each side only maps its own platform to/from the wire types.

## Hiding the window on the host ("it should feel like a third monitor")

The intended feel: you drag a window off the Windows screen, it **disappears
there** and appears on the Ubuntu desktop. Not "a remote window" — the two
machines should read as one continuous desktop. That goal creates a problem the
Linux side never had, because it requires the host window to keep producing
pixels **while being invisible**.

The obstacle, per Microsoft's own guidance: for continuous capture with
`CreateForWindow` the source window must be *"at least one pixel on screen and
must be layered"*, or DWM stops refreshing its offscreen content and capture
degrades to a **still image**. `ShowWindow(SW_HIDE)` and minimising are worse
still — both stop rendering outright, and a minimised window also stops
accepting `PostMessage`.

The partial-drag case (window straddling the seam between the two machines)
satisfies that constraint naturally and is expected to work with no tricks.

### Measured with real capture (2026-07-20, Windows 11 build 26200)

Settled by driving `Windows.Graphics.Capture` itself and **counting frames** —
the only measure that matters, since a window DWM has stopped refreshing simply
stops producing them. Target was a running **Minecraft** (LWJGL/OpenGL, the
hardest case), via `tools/wgc-probe`:

| Window state | Frames in 3 s | FPS |
|---|---|---|
| Normal, visible | 90 | 30.0 |
| **Layered `alpha=0` (invisible)** | **90** | **30.0** |
| **Layered, fully offscreen** | **90** | **30.0** |
| **Minimised** | **0** | **0.0** |

**An invisible window streams exactly as well as a visible one.** The "vanishes
from the host" behaviour the design wants is achievable, and even a fullscreen
OpenGL game does not notice.

Two corrections to earlier assumptions this produced:

- **Offscreen does *not* freeze capture** (30 fps), contradicting the "must be
  at least one pixel on screen" guidance. Do not design around that constraint
  without re-measuring.
- **Minimising is the one thing that kills it** — zero frames. Which is awkward,
  because minimising is the most obvious way to "put a window away"; it must be
  avoided explicitly. It also stops `PostMessage` delivery, so it fails both
  halves at once.

Two earlier probes gave *wrong* answers and were discarded; the mistakes are
worth not repeating:

1. `PrintWindow` reported every state as live — but it can itself provoke a
   repaint (`WM_PRINT`), which would mask a frozen window.
2. Sampling a DWM thumbnail with `GetPixel` reported every state as frozen — DWM
   composites previews outside GDI, so the pixels read were just the sink
   window's own background (verified: it stayed pure magenta).

**Approach:** hide with `WS_EX_LAYERED` + `SetLayeredWindowAttributes(alpha=0)`,
never `ShowWindow(SW_MINIMIZE)`. Position is then a free choice — offscreen also
works, so the window can simply be left where it was. Still to settle when the
backend is built: an `alpha=0` window continues to swallow real mouse input
(needs `WS_EX_TRANSPARENT`, which must be checked not to block our *synthesised*
input too), and applications that voluntarily throttle when they believe they
are hidden (Minecraft did not; Electron's occlusion tracking may).

Build note for anyone reproducing this: the mingw (`x86_64-pc-windows-gnu`)
build of the probe was **blocked by Windows Defender as a virus** — a false
positive on an unsigned binary that captures the screen. The MSVC target
(`cargo xwin build --target x86_64-pc-windows-msvc`) ran without complaint.

Note this is *not* how RemoteApp does it: RemoteApp runs the app in a separate
RDP session with its own virtual desktop, so "is it on a physical screen" never
arises. That is unavailable to us because we move a window out of the user's
own live session.

## The trigger: drag past the edge

Reuses the transition machinery from `input-design.md` milestone 5 (edge
detection / "whose cursor"). The difference: when the cursor crosses the edge
**while dragging a window's titlebar**, instead of (or in addition to) handing
the shared cursor over, the host starts a transfer — `WindowOffer` + frames for
the window under the drag. Dragging it back past the edge → `WindowClose {
Returned }`. Detecting "a window is being dragged" is platform-specific and part
of the capture backend work.

## Milestones for window transfer

1. **Protocol + trait scaffolding — DONE ✅ (2026-07-19)** —
   `crates/protocol/src/window.rs` (`WindowMeta` / `WindowFrame` / `WindowInput`
   / `WindowFrameAck` / `TileCodec` / `WindowCloseReason`), four new `Message`
   variants, `PROTOCOL_VERSION` 3→4; `crates/window` crate with `WindowCapture` /
   `CaptureSession` / `WindowRenderer` / `Surrogate` traits. Adaptive loop
   (per-tile codec, seq, ack) designed in from the start. No backend yet.
2. **Vertical slice Linux→Linux — CODE COMPLETE, NOT YET RUN (2026-07-20)** —
   the whole path exists and is unit-tested, but no frame has crossed a real
   link yet; live bring-up is the immediate next step (see *Status* above).
   - Host capture (`crates/window/src/linux_capture.rs`): portal session →
     PipeWire BGRA stream → `WindowFrame`s. Frames are **diffed** against the
     previous one (`FrameDiffer`) so only the changed bounding box is sent —
     PipeWire always hands over a full frame, so without this every tick would
     cost the full 8.3 MiB. Tiles over ~1 KiB are zstd'd
     (`LosslessBgra8888`), smaller ones ride raw.
   - Viewer surrogate (`crates/window/src/linux_surrogate.rs`): a GTK4 window
     (the toolkit clipd already uses) blitting tiles into a full-window
     framebuffer painted via `gdk::MemoryTexture` in `B8g8r8a8` — the wire
     format verbatim, so no colour conversion. Pointer/scroll/keys come back as
     `WindowInput`, rescaled into the **host's** pixel space.
   - Session wiring (`crates/core/src/window_session.rs`): `WindowEngine` as a
     `SessionModule` beside audio/clipboard, holding **both roles at once**.
     Offer strictly precedes the first frame; pumping is bounded per call so a
     busy window can't starve the session's inbound branch.
   - Input replay: the portal session is created on **RemoteDesktop** and
     `select_sources` is called on that same session (`RemoteDesktop`
     implements `IsScreencastSession`), so pixels out and input back cost the
     user **one dialog**, and `notify_pointer_motion_absolute` can address the
     cast's stream node directly — window coordinates need no translation.
   - Trigger at the time: a `--share-window` flag, since the drag trigger was
     still milestone 5. That flag is gone — the drag is the only way to start a
     transfer now, so there is no second way to say the same thing.
   - Not included by design: no adaptive controller (fixed rate, acks are
     accepted and ignored), no lossy codec, dirty regions are a single bounding
     box rather than several tiles.
3. **Adaptive rate control** — wire `WindowFrameAck` into a controller: measure
   sent-vs-acked + RTT, drive fps/quality; drop-stale-not-buffer under pressure.
4. **Lossy codecs** — implement `LossyImage` then `LossyVideo` behind the
   existing `TileCodec` tags; host picks per-tile by region kind.
5. **The edge-drag trigger — CODE COMPLETE, NOT YET RUN (2026-07-20)** — drag a
   window past the edge that borders the peer (derived from the two machines'
   `--at` positions) and it transfers; drag it back and it returns
   (`WindowCloseReason::Returned`).
   - Detection is *not* shared with the cursor's edge crossing, and cannot be:
     that signal carries no window, and on Wayland it only fires while input
     capture is armed. The trigger compares the drag's own pointer position
     against the screen bounds instead (`crates/window/src/drag_trigger.rs`),
     staying independent of who owns the cursor.
   - **Windows** asks the OS directly: `GetGUIThreadInfo` + `GUI_INMOVESIZE`,
     with `hwndMoveSize` naming the window (better than `WindowFromPoint` — the
     pointer leaves the window during exactly the drag we care about). A resize
     is excluded by sampling the window's size when the loop begins.
   - **Linux cannot do this at all from a client.** Verified on GNOME 49:
     `Shell.Eval` is dev-mode only, `Shell.Introspect.GetWindows` returns
     `AccessDenied`, Mutter doesn't implement `ext_foreign_toplevel_list`, and
     `xdg_toplevel_drag` only covers a client's *own* toplevel. So detection
     comes from a GNOME Shell extension (`native/cursio-shell-extension/`)
     re-publishing Mutter's grab-op signals on D-Bus. Note this is a GNOME
     limitation, not a Wayland one — wlroots compositors do offer
     `ext_foreign_toplevel_list`.
   - The extension is **optional**, and without it a GNOME machine cannot
     *start* a transfer — it still receives one. Nothing in the daemon may
     depend on it being present.
6. **Windows backend** — Graphics.Capture host + Win32 surrogate; verify true
   cross-OS Windows↔Linux transfer.
7. **Remote-screen views — CODE COMPLETE, NOT YET RUN (2026-09-27)** — see
   below. State machine unit-tested with fakes; Windows cross-build checked;
   no live Discord share yet.

## Remote-screen views: a peer's app or monitor, for screen-sharing software

The need: run Discord (or OBS, or a browser call) on one machine and share, from
its ordinary picker, an app or a whole monitor of the **other** machine — in
both directions (Discord on Nocturne sharing Lightmoon, and the reverse).

A screen-sharing application can only offer what exists on its own machine, and
the one thing every such application lists on every OS is a **window**. So each
machine, when started with `cursio mesh --remote-screen`, opens one ordinary,
visible top-level window per connected peer, titled `Cursio — <peer>`. Its
content is one source on that peer — any of its top-level windows, or one of its
monitors — and the user picks it inside the view. Discord lists the view under
its windows/applications like anything else.

### Why a kept window, and not a transfer surrogate

It *is* the transfer surrogate (GTK4 on Linux, raw Win32 on Windows, the shared
`Framebuffer` and decoders), opened through
`WindowRenderer::open_screen_view`, but with three differences:

- **It survives every source switch.** A screen share captures a window;
  recreate the window and the share ends. So the host swaps the capture under
  one reserved stream id (`SCREEN_STREAM_ID`) and sends `SourceShown` ahead of
  the new source's first frame. The view restarts its decoder and clears its
  picture on that marker (`Surrogate::restart_stream`) and keeps its window.
  The old capture keeps streaming until the new one is up — on GNOME "up" can
  mean a picker waiting on the host.
- **It letterboxes.** A transfer's surrogate resizes itself to its source; a
  view cannot, since the window is what is being captured. It scales the
  picture to fit (`framebuffer::letterbox`) and paints bars around it.
- **It is view-only.** The shared cursor already drives the peer directly, and
  a monitor has no window to replay a click into. Mouse and keys in the view
  drive the selector instead.

On Windows the view must be a normal window: the layered `alpha=0` trick a
transfer's *host* uses would be recorded by Discord as nothing.

### Pulled by the viewer — why that is acceptable

A transfer is pushed: the drag is the host's user choosing to show a window.
A view is pulled by the viewer. In a mesh of strangers that would be a spyware
primitive; in Cursio's it grants nothing new. Every peer is one of the user's
own machines, admitted by a password typed on both, and each can already drive
the other's keyboard and mouse and read its disks — watching its screen is
strictly less. Hosting still stays a choice: `--no-screen-host` makes a node
answer every request with `SourcesRefused` and a reason. The module keeps
negotiating either way, because refusal with a reason beats silence (the same
rule as the service module). Viewing stays passive in the other sense: nothing
opens without `--remote-screen`.

### Protocol (window module v3, `WindowMsg`)

| Message | Direction | Class | Meaning |
|---|---|---|---|
| `ListSources` | viewer → host | Control | subscribe: send the list now and on every change |
| `Sources(Vec<ScreenSource>)` | host → viewer | Control | `{ id, kind: Window/Monitor, title, width, height, picked_on_host }` |
| `SourcesRefused { reason }` | host → viewer | Control | this host does not serve views |
| `ShowSource { source }` | viewer → host | Control | start **or** switch |
| `StopSource` | viewer → host | Control | stop streaming; the view stays |
| `SourceShown { source, title, width, height }` | host → viewer | Video | the frames that follow are a new source |
| `SourceEnded { reason }` | host → viewer | Video | stream ended (closed, failed, picker cancelled) |

Frames are ordinary `WindowFrame`s under `SCREEN_STREAM_ID`, acks ordinary
`FrameAck`s. Requests and the list are `Control` so a switch asked for during a
full-rate monitor stream does not queue behind its frames; `SourceShown` and
`SourceEnded` stay `Video` because they are ordered *against* frames — the
decoder must restart exactly between the old source's last packet and the new
one's first. The reserved id works even with views running in both directions
on one link: frames only flow host → viewer, so a frame bearing it is always
ours to show, and an ack bearing it always for the stream we host. The host
re-reads its list every 2 s while subscribed and resends only on change.
`PROTOCOL_VERSION` is unchanged: this is the window module's own vocabulary,
versioned through its `ModuleAdvert` (2 → 3).

### What each host can offer

- **Windows** enumerates directly: every monitor (primary first,
  `CreateForMonitor`) and every visible, titled, unowned, non-tool,
  non-cloaked window (`CreateForWindow`, never hidden — the person at that
  machine is still using it). Views of this machine are left out of its own
  list. A new view shows the first monitor without being asked.
- **GNOME** cannot enumerate: `Shell.Introspect.GetWindows` is `AccessDenied`,
  and the portal is the only sanctioned way to name a source. So a Linux host
  offers two `picked_on_host` entries, "Choose a window…" and "Choose a
  screen…", and choosing one opens the ScreenCast picker **on the host**. That
  session is ScreenCast-only (no RemoteDesktop, so the dialog does not ask for
  keyboard control a view never uses), embeds the host's cursor (the audience
  watches that machine), and takes no restore token — the viewer asked to
  pick, and restoring the last source silently would make both entries show
  the same thing. No source is auto-selected from a Linux host, and ←/→ skip
  picker entries, so a dialog never pops up on the other machine unasked.
- **macOS** refuses with a reason for now.

### The selector, and why it is where it is

The choice must not end up in the pixels Discord sends. A control inside the
view (a GTK header button or popover *embedded in the content*) would be
captured, since window capture records the whole surface. So:

- **Right-click → context menu** listing screens, then windows, then "Show
  nothing". On Windows it is `TrackPopupMenu`, a separate `#32768` window that
  window capture does not record. On GNOME it is a `GtkPopoverMenu`, an
  `xdg_popup` surface of its own; a window screencast records the toplevel's
  surface, so it is not expected in the stream — **not yet verified live**.
- **← / → (Page Up / Page Down)** step between the host's direct sources.
  Draws nothing at all, which is why it exists beside the menu.
- An idle view shows one line of hint text; it disappears with the first frame.

Rejected: a CLI subcommand (`cursio share-source`) — the daemon has no IPC and
adding one for this alone is a larger surface than a menu; the Tauri app has no
channel to the daemon either.

Closing the view stops the host's capture and keeps the view closed for the
rest of that link (a list refresh must not pop it back up). A link ending
closes the view and stops the capture.

### Later: appearing under "Screens"

Some applications — and Discord's own UI — separate windows from screens. A
view can only ever be a window. To appear as a *screen*, the viewer would need
a virtual monitor: an IddCx indirect display driver on Windows, or Mutter's
`RecordVirtual` (a virtual monitor the compositor itself composites) on GNOME.
Both are deliberately out of scope here; the view path stays the same, only
its sink would change.
