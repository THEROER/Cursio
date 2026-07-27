# Clipboard Design (history, and sharing between machines)

Status: foundation + `clipd` implemented and verified on the dev box
(2026-07-19). Cross-machine sharing **implemented** (2026-07-19) as a session
module in `crates/core/src/clipboard_session.rs`; unit-tested, not yet
LAN-verified end to end. Companion to `input-design.md`,
`window-transfer-design.md`, and `audio-design.md`.

## Two products, one foundation

There are two separate things here, and keeping them separate is the point:

1. **`clipd`** — clipboard history for Linux. What Win+V is on Windows and what
   GNOME does not ship. Standalone: no pairing, no network, no Cursio.
2. **Cursio clipboard sharing** — copy on one machine, paste on the other.

They look unrelated but do the same underlying job: *get multi-format content
off the local clipboard, and put multi-format content back on it.* Only the
destination differs — a database, or a socket. So that job lives in
`crates/clipboard-core`, which depends on neither.

```
crates/clipboard-core/     the platform layer (read/write/watch, formats)
  ├── apps/clipd/          history + picker   (standalone)
  └── crates/core/         Cursio sharing     (not yet built)
```

**They compose for free.** The meeting point is the *system* clipboard, so when
both run, content Cursio receives from another machine lands on the local
clipboard, which `clipd` observes as an ordinary copy and records. There is no
integration code between them, and none is needed.

## The clipboard is a set, not a string

The single most important thing to get right: **one copy is several
representations of the same content.** Copying from a word processor puts HTML,
RTF and plain text on the clipboard at once; copying an image offers PNG and
often a raw bitmap. Whoever pastes picks what they understand — a browser takes
the HTML, a terminal takes the text.

Collapsing that set at capture time destroys formatting irreversibly. So
`ClipboardContent` keeps every representation, and the *consumer* chooses
(`best_match`, ordered by a richness score). Both platforms' native APIs already
work this way; preserving the set is also the least surprising choice.

Reads are **lazy** by default: `read()` reports formats and sizes without
fetching payloads, because a copy can be tens of megabytes and most copies are
never used. `load()` fetches one representation. The history daemon is the
deliberate exception — see "Capture must be eager" below.

## Platform: why GNOME forces the X11 path

Reading someone else's clipboard is privileged on Wayland: an ordinary client
only sees it while holding keyboard focus, which is useless for a background
daemon with no window. The `wlr-data-control` / `ext-data-control` protocol
exists exactly for clipboard managers.

**GNOME does not implement it.** Verified on the target system (GNOME Shell 49,
Ubuntu, Wayland): the compositor's registry advertises only `wl_seat` — neither
`ext_data_control_manager_v1` nor `zwlr_data_control_manager_v1` is present,
even though the protocol XML ships in `/usr/share/wayland-protocols/staging/`.
`wl-clipboard-rs` therefore fails with `MissingProtocol`, and `wl-paste` works
only because it is a focus-dependent client — which a daemon cannot be.

What *does* work is **XWayland**: GNOME bridges the X11 selection to the Wayland
clipboard **both ways**, including for native Wayland apps and including images
(verified by round-tripping a PNG between `wl-copy` and `xclip`). On GNOME this
is not a legacy fallback — it is the only way a background process can do this
job.

`arboard` drives it, rather than raw X11, because the alternatives cannot write
several representations at once: `x11-clipboard` and `xclip` hold a single
`(target, value)` pair per selection, which would make every copy lose all but
one of its formats.

`data-control` remains the right backend for wlroots compositors (Sway,
Hyprland) and can be added behind the same trait without touching callers.

### Consequences worth knowing

- **Images are transcoded, not passed through.** `arboard` decodes to raw RGBA
  on read and re-encodes on write. Lossless for PNG, but a JPEG comes back as
  PNG, and stored sizes are PNG-encoded — a 4K screenshot is ~2 MB as PNG versus
  ~33 MB as raw pixels, which is why history stores the encoded form.
- **Whoever writes must keep serving.** The X11 clipboard has no storage; content
  lives in the process that copied it. A process that writes and exits leaves an
  empty clipboard, so writes hand ownership to a thread that stays alive.
  `arboard`'s `wait()` blocks *inside* the set call until the clipboard is
  replaced, so the "ownership taken" signal must be sent before it — waiting on
  the set call's result deadlocks (found the hard way).
- **Change detection is polling.** X11 offers no notification without owning the
  selection, and `data-control` is unavailable. Polling is text-only at 400 ms:
  probing for an image every tick would make the owning app render and transfer
  a full bitmap several times a second. An image copy is detected by the text
  representation changing, after which a full read finds what is actually there.

## `clipd`

```
clipd daemon          # watch and record
clipd list            # print history
clipd pick            # choose an entry and put it back
clipd copy <id>       # restore a specific entry
clipd install-service # run the daemon at login (systemd user unit)
clipd install-hotkey  # bind the picker under GNOME
```

### Running by itself

Nothing is recorded unless the daemon runs, so `install-service` writes a
systemd **user** unit — not a system one, which would have no display to talk
to. It binds to `graphical-session.target` rather than `default.target`, since
the clipboard only exists once a display server does, and `PartOf=` stops it at
logout instead of leaving it running.

Installation copies the binary to `~/.local/bin` and points both the unit and
the hotkey there. A unit referencing `target/debug/clipd` would break on the
next rebuild or `cargo clean` — and break silently, as a shortcut that simply
stops working.

### Capture must be eager

The daemon reads content eagerly (`read_loaded`), the opposite of the rest of
the system. X11 content is served by the process that copied it, so it vanishes
when that application exits. A history holding a lazy reference would find the
bytes gone precisely when the user reaches for them.

### Storage: text inline, blobs on disk

Text goes in SQLite; image bytes go in files beside it, referenced by row. A
screenshot is megabytes and a day produces dozens; as blobs, the picker's "list
the last 50" query would drag megabytes through SQLite just to render labels.
With files on disk, listing touches only small rows and eviction is one
`unlink`.

Blobs are named by SHA-256 of their content, so re-copying the same image costs
one file. Because rows share blobs, a blob is deleted only when no row
references it.

### Ordering: `seq`, not timestamps

`copied_at` has one-second resolution, and bursts of copying are common. With
ties unbroken, SQLite may return them in any order — which made eviction keep
the **oldest** items instead of the newest (caught by a test, fixed with a
monotonic `seq` column). Re-copying bumps `seq` so the item returns to the top
while keeping its identity — a plain timestamp update left it behind its
same-second peers.

### Limits

Bounded two ways, because either alone fails: a count limit lets 200 screenshots
fill a disk; a byte limit alone lets thousands of snippets make the picker
unusable. Both are enforced on insert. Pinned items are exempt — pinning is an
explicit "keep this".

### Picker and hotkey

`clipd pick` drives whichever dmenu-style picker is installed (`fuzzel`, `rofi`,
`wofi`, `tofi`, `dmenu`). They already solve the genuinely hard parts on
Wayland: an overlay that takes focus, fuzzy search, desktop-matching theming.
The cost is no image thumbnails — an image shows as `[image, 2.1 MiB]`. A native
GTK popup would do better and `picker.rs` is the seam where one slots in;
everything above it only asks "which id?".

Wayland gives applications no way to register a global shortcut — that is the
compositor's business. GNOME's answer is custom keybindings in dconf, so
`install-hotkey` writes one via `gsettings`.

**Not Super+V:** it is already bound to `toggle-message-tray` on GNOME
(verified). A custom binding that collides silently loses — GNOME keeps its own
action and the shortcut appears dead. The default is therefore
`<Super><Shift>v`, which is free; `--force` takes Super+V by clearing GNOME's
claim first, as an explicit and reversible choice.

## Cross-machine sharing (built)

Implemented as a [`SessionModule`] — `ClipboardEngine` in
`crates/core/src/clipboard_session.rs` — multiplexed onto the same encrypted
channel as input and audio, exactly like window transfer and audio. Enabled with
`--clipboard` on `serve` / `connect` / `mesh`. The blocking `clipboard-core`
backend runs on a dedicated worker thread (`clipboard_session/worker.rs`) so the
async session loop never blocks. The wire protocol reuses the disciplines
already proven in this workspace.

**Offer, then request.** The owner announces what it has — formats and sizes, a
few bytes — and sends content only when the peer actually pastes. Copies are
frequent and mostly never pasted on the peer; eagerly pushing every one would
waste bandwidth and stall the shared-cursor input stream sharing that link. The
native clipboards are lazy in exactly this way, so this matches the OS model.

**Chunked transfer.** Payloads exceed `MAX_FRAME_LEN` (16 MiB), so a requested
format streams as a sequence of chunks — the discipline `AudioChunk` and
`WindowFrame` already use. It also keeps the link responsive: input events
interleave between chunks instead of a large paste blocking the session.

**Generation ids.** Copying twice in quick succession means the peer may still be
pulling generation *n* when *n+1* is offered. Every request, chunk and error
carries the id, and the receiver drops anything that is not the newest offered,
so stale bytes are never pasted as current content.

**Explicit failures.** A request that cannot be served (stale generation, format
gone, read failed, too large) is answered with an error rather than silence, so
the requester can fall back to another format instead of hanging.

This is carried by the `Message` variants `ClipboardOffer` / `ClipboardRequest` /
`ClipboardData` / `ClipboardFailed` (in `crates/protocol/src/clipboard.rs`), with
`PROTOCOL_VERSION` bumped to 8. The module claims those variants via
`SessionModule::handle_message` and drives its outbound work from `pump`.

**Receiver pulls eagerly, once.** The offer→request split saves the bytes of
every copy that is never pasted on the peer — but this side has no "the user is
about to paste" hook, and content must already be on the local clipboard for a
paste to find it (which is also what lets a running `clipd` record it). So on
each offer the receiver pulls exactly *one* best format it can write back
(`PULLABLE`, ordered by richness), not all representations and not lazily. That
keeps the win — the *peer's* copies that go nowhere cost nothing — while making a
received copy instantly pasteable locally.

**Formats we can't write are not pulled.** The accept-set is limited to what the
platform backend can put back on the clipboard (image, HTML, text on `arboard`).
Pulling a representation with no write path would spend bandwidth to drop it, so
RTF and app-specific `Other` types are carried by the protocol but not pulled
until a backend can write them — adding the format to `PULLABLE` is then the
whole change.

### Open question: files

`text/uri-list` names paths on the *owner's* filesystem, which generally do not
exist on the receiver. Copying a file between machines therefore needs the file
*contents* transferred and materialised somewhere — a separate feature with its
own questions (where do they land, when are they cleaned up). For now the URI
list is carried as-is, which is correct locally and visibly incomplete across
machines.
