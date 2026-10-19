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
output, which means holding frames back). NVENC also gets `delay=0`: FFmpeg's
wrapper otherwise holds up to three frames before releasing a packet
(`zerolatency` does not change that), and because capture only encodes when
the screen changed, a held frame waited for the next change — the opening
keyframe of a view sat in the encoder for 5 s on a still desktop, and the last
frame of each burst of motion never left at all (measured 2026-09-27: frame 0
of a fresh NVENC encoder produced no packet). Encoder choice is NVENC → VAAPI →
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

What the loop moves today (`frame_pipeline::AdaptiveQuality`):

- **The lossless/lossy threshold** (`ThresholdLadder`) — how large a change
  must be before it goes to video instead of a tile (15–45% of the window).
  Down 10 points on any ack showing more than 3 frames outstanding; up 10
  only after 8 caught-up acks in a row (~2 s) and never within 4 s of a step
  down. Moving both ways on single acks flapped 0.35↔0.25↔0.15 every 250 ms
  on a live view: each raise sent the next repaint lossless, and its size was
  the next backlog.

The backlog is sent-minus-acked counted in frames: the pipeline numbers a
frame only once the capture queue has taken it, so a refused update never
reads as one the link failed to carry. Only frames queued more than 100 ms
before the ack count (`IN_FLIGHT_GRACE`): encode, wire, decode, paint and the
ack's return keep four or five frames of a healthy 60 fps stream in flight,
and counting them cut the bitrate to its floor exactly when motion was
smoothest. An ack no newer than the last, or
numbered past anything the capture sent (a leftover of the previous stream),
is ignored rather than read as the link calming down.
- **The lossy stream's bitrate** (`BitrateLadder`, 2026-09-27), as a
  percentage of `lossy_bitrate(w, h)` (≈12 Mbit/s at 1080p): 20–350%.
  Down 30% at once on a backlog of more than 3 frames, then no climbing for
  4 s; up 15% only after 6 consecutive caught-up acks *while video is
  flowing* — a source that stopped changing sends no frames, and its acks say
  nothing about the link. The rate changes on the live encoder: x264 and
  Media Foundation (`CODECAPI_AVEncCommonMeanBitRate`) reconfigure in place;
  NVENC through FFmpeg forces an IDR on any rate change (`nvenc.c`
  `reconfig_encoder` sets `resetEncoder`+`forceIDR`), so NVENC only moves for
  a change of 25% or more. A lossless stream has no rate and ignores it.

Resolution and frame rate do not adapt yet; below the bitrate floor the
threshold keeps pushing more of the window to video.

The Windows encoder was never on the video path at all before this: hardware
H.264 MFTs are asynchronous, and calling one per frame like a synchronous
transform was accepted once and refused (`MF_E_NOTACCEPTING`) ever after, so
every frame fell back to lossless tiles. It is now driven by its
`METransformNeedInput`/`HaveOutput` events. Each failed reopen also leaked an
NVENC session — releasing the transform does not end it, only
`IMFActivate::ShutdownObject` does — and the Windows peer was found holding
all 12 of its sessions (`nvidia-smi encodersessions`), after which no encoder
opened at all. Encoders are now shut down on drop, and `open` tries every
encoder through to a started stream, so an exhausted GPU falls back to
Microsoft's software encoder (measured there: 1080p at ~20 ms/frame).

## Keeping the viewer's picture right (2026-09-27)

A Windows view of a GNOME monitor showed black on first connect, then
"imprints" of earlier frames that updated only where the pointer passed, and a
closed menu stayed burnt into the picture. Several causes, each fixed where it
lives:

**Only video resizes the picture.** A frame carries its `dirty` rect and no
window size, so viewers inferred the size: a frame at the origin whose size
differed from theirs meant "the host resized". But a tile is only the region
that changed, and any change touching the top-left corner (GNOME's Activities
button, the pointer in the corner) starts at the origin — so the viewer
cleared its picture to black at that tile's size, and only later tiles were
drawn back. `framebuffer::geometry_of` now resizes only on a video frame (the
stream is the whole window by construction) and only *grows* on a tile that
reaches past the edge. A host resize reaches the viewer as video — the first
frame at a new size is a full-window change — so what is lost is shrinking
from tiles alone, when no encoder opens.

**Keyframes by the clock.** A decoder can start only at an IDR, and drops
everything before one. Both encoders keyed on their own GOP — 120 *frames* —
but capture encodes a frame only when the screen changed, so on a desktop
where a clock ticks and the pointer moves (tiles, not video) the next IDR could
be minutes away, and a viewer that missed the first saw black plus whatever
tiles landed. The encoders' GOPs are now effectively infinite and the capture
loop's `frame_pipeline::KeyframeClock` forces an IDR on the first video frame
at least `KEYFRAME_INTERVAL` (2 s) after the last one it saw go out — read
from the bitstream, so an IDR the encoder chose itself counts. FFmpeg marks the
frame `I` with `forced-idr=1` (NVENC and x264 otherwise make it a plain intra
frame, which a fresh decoder cannot enter on); Media Foundation takes
`CODECAPI_AVEncVideoForceKeyFrame` before the sample. A rate change still costs
no keyframe on x264 and Media Foundation.

**Resync on request, from the last frame.** The clock still needs a change to
encode. So a viewer whose decoder is waiting for a keyframe — its first, or a
fresh one after a decode error, which now drops the decoder rather than
building on broken references — raises a flag its surrogate reports through
`Surrogate::take_keyframe_request`, and the session sends
`WindowMsg::KeyframeRequest { id }` (window module v5), at most once a second
per stream (`KeyframeRequests`). A remote-screen view also asks on every
`SourceShown`. The host's `CaptureSession::request_keyframe` reaches the
capture thread, where the shared `frame_pipeline::CapturePipeline` (both
backends now feed it; each only supplies an `EncoderSource`) marks the stream
for *resync* and answers from the last captured frame between captures — on
Linux after each PipeWire loop iteration, on Windows whenever WGC has nothing
new — as an IDR, or as the whole picture in four lossless bands when no
encoder opens. The encoder stays on the PipeWire thread; nothing is shared
across threads. An update the capture queue could not take marks a resync too,
instead of only resetting the differ: the old reset made the *next* change a
full frame, but on a screen that then stood still there was no next change,
and the viewer kept the gap.

**Refine lossy video once the source settles.** A lossy stream spends few bits
per frame of motion because the next frame corrects it; when the source stops
there is no next frame, and the last one's blur stays — GNOME's screenshot
toolbar left a faint imprint after it closed. Once lossy video has stood still
for `SETTLE` (400 ms), `CapturePipeline::idle` sends the last captured frame
once more as lossless tiles (four bands, zstd). Tiles rather than a
higher-quality IDR: exact, no encoder retuned for one frame, and a viewer
blits them over whatever its decoder produced. The whole window, because each
video frame replaced the whole window. Once per settle — only new lossy video
arms it again, so a still screen never loops — and never for a lossless
stream. The next video frame is a P-frame off the decoder's lossy reference, so
motion is lossy again until it settles again; that is the trade the lossy
stream already made.

**Direct scanout freezes a GNOME monitor cast.** A fullscreen window Mutter
scans out directly is never composited, and a monitor ScreenCast records only
what is composited. Measured on GNOME 49 (NVIDIA) by counting buffers from
Mutter's own ScreenCast API: a looping video in Chrome delivered ~35 buffers/s
windowed on the captured monitor, and in kiosk fullscreen a few seconds of
buffers followed by none at all while the video kept playing — the view froze
whenever the pointer left or the player hid it. (Fullscreen mpv through
`gpu-next` kept streaming at 30/s, so not every fullscreen client is scanned
out.) Asking for a frame rate cannot help: Mutter has nothing to record. So
while a remote-screen view streams from this machine, the capture asks the
Shell extension (`dev.cursio.Shell.InhibitDirectScanout`) to disable
unredirection, held per D-Bus sender and released when that sender leaves the
bus, so a crash cannot leave scanout off. Without the extension (or before
re-login loads the new version) the capture logs why and the fullscreen case
still freezes.

**Corrupted PipeWire buffers are skipped.** A buffer whose chunk carries
`SPA_CHUNK_FLAG_CORRUPTED` holds no new picture — its memory is whatever an
earlier frame left in that pool slot — and was diffed as current, sending the
viewer an old picture. The capture never negotiated `SPA_META_VideoDamage`, so
every other buffer is a whole frame and skipping one loses nothing.

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
   - The same extension also carries Cursio's quick-settings tile
     (`menu.js`, a client of the control socket); a tile that fails to build
     is logged and leaves the drag bridge running.
   - The extension is **optional**, and without it a GNOME machine cannot
     *start* a transfer — it still receives one. Nothing in the daemon may
     depend on it being present.
6. **Windows backend** — Graphics.Capture host + Win32 surrogate; verify true
   cross-OS Windows↔Linux transfer.
7. **Remote-screen views — windowless (2026-09-27)** — see below. Verified
   live: the Windows unseen view is listed by Chromium's and OBS's filters and
   recorded by WGC on the Windows peer (`screen_view_probe` +
   `wgc-probe --inspect`); the Linux chain capture-app → real
   `xdg-desktop-portal` 1.20.3 → Cursio backend → peer node → PipeWire remote
   delivered frames (`portal_probe` on a private bus). Not yet run: a real
   Discord/OBS share between the two machines, and the installed backend on
   the live GNOME session (RemoteDesktop forwarding is argued from
   `screen-cast.c`, not observed).

## Remote-screen views: a peer's app or monitor, for screen-sharing software

The need: run Discord (or OBS, or a browser call) on one machine and share, from
its ordinary picker, an app or a whole monitor of the **other** machine — in
both directions (Discord on Nocturne sharing Lightmoon, and the reverse).

…and **without drawing it on this desktop**. The first version opened a
visible `Cursio — <peer>` window per peer purely so it could be captured —
rendering for nobody. A screen-sharing application can only offer what its own
machine has, and what "has" means differs per OS, so the view does too.

### Windows: a window nobody sees

Discord, OBS and browsers list top-level windows, so the view stays one, one
per peer, kept for the life of the link — but never on screen. Measured on the
Windows peer (`tools/wgc-probe --selftest`, a window painting a known colour):

| state | Chromium list | OBS list | WGC frames/2 s | picture |
|---|---|---|---|---|
| on screen | yes | yes | 43 | yes |
| off the virtual desktop | yes | yes | **1** | stale |
| layered alpha 0 | yes | yes | 43 | yes |
| **layered alpha 0 + off desktop + `WS_EX_NOACTIVATE`** | yes | yes | 44 | yes |
| `WS_EX_TOOLWINDOW` | **no** | **no** | 43 | yes |

The filters are re-stated from Chromium's `window_capture_utils.cc` (visible,
not minimised, unowned or `APPWINDOW`, titled, not cloaked, `TOOLWINDOW`
excluded for WGC) and OBS' `window-helpers.c` (visible, not minimised or
cloaked, not `TOOLWINDOW`/`WS_CHILD`, non-empty client). Off the desktop alone
stops DWM refreshing the window; layered alpha 0 keeps it composed with its
picture intact in the capture; `NOACTIVATE` keeps it off the taskbar and
Alt+Tab. `PrintWindow` returns nothing for it, so a GDI-only capturer (OBS'
"BitBlt" method) would record black — every capturer measured here uses WGC.
Discord's own native capture engine is not open source; that it lists the
view is inferred from it listing the same windows Chromium does.

The view keeps the old rules: it **survives every source switch** (the host
swaps captures under one reserved `SCREEN_STREAM_ID` and sends `SourceShown`
ahead of the new source's first frame; the view restarts its decoder in
place), it follows the source's size (nobody sees it, so 1:1 is best for the
capture), and its selector — which cannot be a right-click on a window that is
nowhere — is the Cursio app's tray, a "Show in Discord/OBS" submenu with a
peer each (`screen_views`, `show_screen_source`). Each view also has a
**notification-area icon** with the same menu, shown only while no app is
attached (`tray_attached`): an icon per peer beside the app's own read as the
same program twice, and a node without the app still needs a selector. A new
view shows the peer's first monitor unasked. Painting fills only the letterbox
bars, never the whole client first: DWM composes a redirected window whenever
it likes, and a capture taken between a full black fill and the blit recorded
a black frame (measured: every other capture black).

### Linux (GNOME): the peer's source in the app's own share dialog

Discord, OBS and browsers on GNOME get pixels only from the ScreenCast
portal, so the view is not a window at all: it is a PipeWire `Video/Source`
node (`PipeWireScreenSource`), and it is offered through a **ScreenCast portal
backend of Cursio's own** (`linux_portal.rs`). Installed per user with
`cursio portal install`, it becomes `org.freedesktop.impl.portal.ScreenCast`:

1. A capture app asks to share. The backend shows its chooser —
   "This computer…", then each peer's screens and windows of the kinds the app
   asked for. With no peer offering anything it shows nothing and goes
   straight to GNOME's dialog.
2. "This computer" forwards `Start` to `xdg-desktop-portal-gnome` unchanged
   (GNOME's own picker, restore tokens and all). A peer source goes through
   `ScreenHub` to that peer's link: `ShowSource`, then on `SourceShown` the
   node is created at the source's size and its id is `Start`'s answer.
3. The app closing the session stops the host's capture; the host ending the
   source (or the link dropping) emits `Closed` on the portal session.

`CreateSession` and `SelectSources` are always forwarded to GNOME too, so
GNOME has a twin session at the same path whatever the user picks. And every
call for a session Cursio did not create is forwarded verbatim — which is what
keeps **Cursio's own window transfer working**: `xdg-desktop-portal` routes
`ScreenCast.SelectSources` on a *RemoteDesktop* session to the ScreenCast
backend (one `impl` proxy, `screen-cast.c` 1.20.3), while `RemoteDesktop.Start`
goes to GNOME, which starts only what GNOME was told. `InputCapture` never
comes here.

Details that are load-bearing:

- **One configuration file.** `xdg-desktop-portal` reads the first
  `(<desktop>-)portals.conf` it finds, user config first, and ignores the rest,
  so `install` copies the one in effect and adds the ScreenCast line; writing
  only that line would drop every other portal's choice.
- **Not running is not broken.** D-Bus starts `cursio portal relay` (forward
  everything) when no daemon holds the name; the daemon takes the name over
  (`ReplaceExisting`), the relay allowed that and exits.
- **The chooser is a child process** (this executable, hidden argument): GTK
  initialises once per process, the daemon may already have spent it on a
  transfer surrogate, and a crashed chooser is a cancelled share, not a dead
  daemon.
- **The node's size is fixed** at creation; a source that changes size later
  is letterboxed into it (`framebuffer::scale_into`) rather than
  renegotiated, which not every consumer follows.
- **One stream per peer.** A second capture app asking the same peer takes
  the stream; the first is told its session ended.
- The portal reads its configuration only at start: the change applies after
  `systemctl --user restart xdg-desktop-portal` or a new login, which ends
  open portal sessions (Cursio's input capture included), so `install` does
  not restart it.

Rejected: a PipeWire node alone (neither Discord nor OBS can pick a node the
portal did not hand out); a v4l2loopback or PipeWire *camera* (appears as a
camera, not a screen, and v4l2loopback needs a kernel module).

### Why not a camera or a virtual monitor on Windows

- `MFCreateVirtualCamera` (Windows 11, user mode) would appear in Discord and
  OBS as a **camera**, not a screen — a fallback at best, and the window path
  already works.
- An **IddCx** indirect display (a real extra monitor, listed under Screens)
  needs a signed user-mode driver package installed with admin rights; out of
  scope, as on GNOME is Mutter's `RecordVirtual`.

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

### Protocol (window module v6, `WindowMsg`)

| Message | Direction | Class | Meaning |
|---|---|---|---|
| `ListSources` | viewer → host | Control | subscribe: send the list now and on every change |
| `Sources(Vec<ScreenSource>)` | host → viewer | Control | `{ id, kind: Window/Monitor, title, width, height, picked_on_host }` |
| `SourcesRefused { reason }` | host → viewer | Control | this host does not serve views |
| `ShowSource { source }` | viewer → host | Control | start **or** switch |
| `StopSource` | viewer → host | Control | stop streaming; the view stays |
| `SourceShown { source, title, width, height }` | host → viewer | Video | the frames that follow are a new source |
| `SourceEnded { reason }` | host → viewer | Video | stream ended (closed, failed, picker cancelled) |
| `KeyframeRequest { id }` | viewer → host | Control | send the whole current picture now (also used by transfers) |
| `RequestScreen { show }` | viewer → host | Control | a computer asking a phone for its whole screen as a window (see "A phone as host") |

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
versioned through its `ModuleAdvert` (2 → 3 for these, 3 → 4 for `VideoCaps`, 4 → 5 for `KeyframeRequest`, 5 → 6 for `RequestScreen`).

### What each host can offer

- **Windows** enumerates directly: every monitor (primary first,
  `CreateForMonitor`) and every visible, titled, unowned, non-tool,
  non-cloaked window (`CreateForWindow`, never hidden — the person at that
  machine is still using it). Views of this machine are left out of its own
  list. A new Windows view shows the first monitor without being asked; a
  Linux viewer never streams until a capture app asks.
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

On Linux there is no selector of Cursio's beyond the portal chooser: each pick
in the capture app is a new request. On Windows the menu (screens, windows,
"Show nothing") is in the app's tray, or on the view's own notification-area
icon when no app is attached; the icon's menu is
`TrackPopupMenu`, a window of its own, so it never reaches the pixels even if
the view were visible. An idle view paints one line of hint naming the icon —
seen only in the capture app's preview, which is exactly where someone wonders
why it is empty.

Rejected: a CLI subcommand (`cursio share-source`) — the daemon has no IPC and
adding one for this alone is a larger surface than a menu; the Tauri app has no
channel to the daemon either.

A link ending closes the view (or node) and stops the capture.

### Later: appearing under "Screens"

Some applications — Discord's own UI among them — separate windows from
screens. The Windows view can only be a window, and the Linux node is offered
as a monitor or window by the kind of its source. To appear as a real
*screen* on Windows the viewer would need a virtual monitor (IddCx, a signed
driver); out of scope, see above.

## A phone as host: its screen on a computer, driven with the mouse (2026-10-18)

What scrcpy does over ADB, done over the mesh: a phone's whole screen opens on
a computer as an ordinary transferred window titled `<phone> — screen`, and the
computer's mouse and keyboard work on it as a finger and a few keys. Nothing
needs ADB, a cable, or the phone in developer mode.

### Why it is a window transfer and not a module

It is the AirPlay case turned around. The phone's hardware encoder makes H.264
from the screen (MediaProjection → a virtual display whose surface is
MediaCodec's input), `TileCodec::LossyVideo` already carries H.264 between
Cursio machines, and every viewer's surrogate already decodes it. So the
access units go out exactly as the encoder made them — no pixel passes through
the app — and the computer opens the surrogate it opens for any window. Its
pointer comes back as the `WindowInput` every surrogate sends. The policy is
`core/src/phone_screen.rs` (who watches, when the window opens and closes) and
`core/src/phone_gesture.rs` (what a mouse means on a touch screen); the phone's
half is `ScreenShare.kt` (projection, encoder) and `ScreenTouch.kt` (touch).

The phone advertises the window module as `ModuleRole::Source`: it hosts its
screen and shows nothing, so two phones do not run the module, and its
renderer (`NoViewRenderer`) refuses an offer with that reason.

### Who starts it

- **The computer** asks with `WindowMsg::RequestScreen { show }` (window module
  v6) — from `cursio screen <phone>`, the GNOME menu's *Show Screen* or the
  app's peer menu, all through the control socket's `phone_screen`. Android
  lets nothing capture the screen without a consent shown on the phone, so
  the request becomes a notification there ("Nocturne wants to show this
  screen — Allow · Always allow"), or a sheet in Cursio's window when that is
  in front. With the phone's screen on and nothing to reuse, the system's
  dialog also opens at once, where Android lets the app start an activity
  from the background (a CompanionDeviceManager association, or drawing
  over other apps) — a notification alone went unseen under MIUI, which
  does not pop one up unless the user turned that on per app. Before
  Android 14 one system consent starts any number of captures, so it is
  kept for the process's life: "Allow" then starts the capture at once, and
  a computer always allowed is not asked at all — the system's recording
  indicator still says so. From 14 every capture needs the system's own
  dialog, which an allowed request goes straight to.
- **The phone**, from *Settings → This screen → computer*, shows it on the
  computers ticked there (`phone_screen::choose`, before the consent opens).
  The one the user was at last (`NotifyHub::last_used_desktop`, from the
  presence each desktop already reports for notifications) is ticked first;
  with nothing known of where the user is, every linked computer is. It was
  every linked computer always, which opened the screen on whichever machines
  happened to be up rather than the one being looked at.

Either way the consent is the system's own dialog for the whole display. Once
the capture is up, every computer that asked gets a window. When the last
window closes (or its link ends), the capture stops, and the system's
recording indicator goes with it. *Stop showing screen* on the service's
notification ends it from the phone.

### Size, rate and turning

The long side is held to 1920 px, each side the nearest multiple of 8 (of 16
where the encoder refuses that) so the stream keeps the screen's shape — sides
rounded down to 16 made a 1220×2712 screen 848×1920, letterboxed, with every
tap landing off by the band. 60 fps, starting at
6 Mbit/s. The encoder repeats the last frame after 200 ms of a still screen,
so a viewer joining late still gets pictures. Keyframes are sent on request —
a window that just opened, a viewer whose decoder lost its place, or one that
fell more than 64 units behind and skipped to the next keyframe. The bitrate
halves when more than 30 frames are unacknowledged and grows by a quarter
after five seconds of keeping up, within 1–12 Mbit/s: the phone's encoder takes
only a target, so frames in flight is the signal it can be steered by.

Turning the phone changes the display's size, and a transferred window's size
is fixed by its offer, so a turn rebuilds the encoder at the new size and the
computer's window closes and reopens at the new shape.

### Mouse as a finger

| Computer | Phone |
|---|---|
| left button down, move, up | one touch: a tap, a long press or a drag, by how long and how far |
| wheel | a short swipe at the last press (or the centre), the content following the wheel |
| right button, side "back" button, Esc | Back |
| middle button | Home |
| Ctrl + left drag | a pinch: a second finger mirrored about where the press began |
| keys | typed into the focused field (US layout); Backspace, Enter |

`dispatchGesture` is the one public way an app touches the screen, through an
accessibility service the user turns on once by hand ("Cursio: control from a
computer"). A held button is one continuous stroke (`continueStroke`); Android
takes a continuation only once the previous piece is dispatched, so moves
arriving meanwhile are folded into the next piece — the finger goes where the
pointer is now. Typing sets the focused field's text through the accessibility
tree: a character lands at the end of the field rather than at the caret, and
keys that are not characters (arrows, shortcuts) do nothing. That is the limit
of not being a keyboard; an input method would lift it and is not built.

### Limits

- One projection per app: showing the screen ends a playback capture that was
  running, and the other way round (Android stops an app's earlier projection
  when it starts the next).
- The capture covers the screen as the phone draws it, including what apps
  mark secure (banking, DRM video), which Android draws black.
- The phone's sound is not part of it; that is *Phone audio → computer*.
- A computer's capture apps can take the phone as a remote-screen source too
  (`PhoneScreenSources`); picking it waits up to 90 s for the phone's consent.
