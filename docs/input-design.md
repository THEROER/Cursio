# Cursio — Input Design (mouse & keyboard sharing)

Status: **implemented on all three platforms.** Capture and injection ship for
Linux (evdev + the Wayland `InputCapture` portal), Windows (RawInput +
`SendInput`) and macOS (`CGEventTap` + `CGEventPost`). This document is the
source of truth for how input sharing should behave; code should match it or this
doc should be updated.

## Model: Mouse Without Borders, but relative

The behaviour model is **PowerToys "Mouse Without Borders" / mouseless-borders**,
NOT Barrier/Deskflow's "hand one device set across a screen edge".

- **One shared cursor** across all machines. It is logically single; at any
  moment it is "active" on one machine.
- **Shared mouse + keyboard.** Each machine's real mouse and keyboard always
  work locally *and* are streamed to the peer. To each machine the peer's
  devices appear as **an extra mouse/keyboard plugged in** (a second device),
  implemented as a persistent virtual device.
- **Keyboard follows the cursor.** Keys go to the machine where the shared
  cursor currently is (the receiver's focused window).

### The one deliberate difference from MWB

MWB moves the cursor by **absolute** positioning (teleport to x,y). Cursio uses
**relative `(dx, dy)`** motion instead, because absolute teleports break games
that read raw mouse input (camera jumps/sticks). Relative deltas behave like a
real second mouse. This is the key improvement and a hard requirement.

## Persistent virtual device + active/inactive

Decision: **keep the virtual input device alive for the whole session**, rather
than create/destroy it on every cursor transition (which is slow and causes the
OS to re-init the device, producing lag/flicker).

- The virtual device (uinput mouse/keyboard) is created once at session start.
- It carries an **`active` / `inactive`** flag.
  - **Active** → peer's motion/keys are injected (cursor moves here).
  - **Inactive** → we simply stop injecting motion; the cursor freezes in place.
- Grabbing local input so the real mouse can't nudge the frozen cursor is a
  capture-side concern, and landed with the transition milestone. What became of
  the "park it in a corner" half is under [Hiding this machine's
  cursor](#hiding-this-machines-cursor) below.

## Wire protocol

`crates/protocol/src/input.rs`:
- `InputEvent::PointerMotion { dx, dy }` — relative deltas (evdev REL_X/REL_Y sign convention: +x right, +y down).
- `InputEvent::PointerButton { button, pressed }` — `MouseButton` =
  Left/Right/Middle plus the two side buttons, `Back` (evdev `BTN_SIDE`, Windows
  `XBUTTON1`) and `Forward` (`BTN_EXTRA` / `XBUTTON2`).
- `InputEvent::PointerScroll { dx, dy }` — 120ths of a notch (`SCROLL_NOTCH`),
  +y = up (evdev `REL_WHEEL_HI_RES`, Windows `WHEEL_DELTA`, libei discrete).
  Fractions matter: a touchpad scrolls by them, and rounding each report to a
  whole notch turned a precision touchpad's two-finger scroll into nothing.
  Injectors that only take whole notches (evdev `REL_WHEEL`, macOS lines) add
  the fractions up first (`input/src/wheel.rs`).
- `InputEvent::Key { key, pressed }` — **key is a Linux evdev keycode** (`KEY_*`
  from `input-event-codes.h`) used as the neutral cross-platform namespace; each
  platform maps to/from it. Carried as `Message::Input(InputEvent)`.

## Injection backend

Decision: **uinput first, behind a `trait InputInjector`** so libei/portal
(Wayland "proper" path) and a Windows backend could be added without touching
callers. Platform-specific code lives in a dedicated `cursio-input` crate, kept
out of the transport crate.

All of them landed. Injection is two persistent uinput devices on Linux (pointer
and keyboard kept separate so the compositor doesn't misclassify them),
`SendInput` on Windows, and `CGEventPost` at the HID tap on macOS.

### Environment facts (dev laptop, Ubuntu GNOME 49 / Wayland, verified 2026-07-17)
- `/dev/uinput` exists and is **writable now** (no sudo needed on this host).
  On other machines this may need a udev rule or the `input` group.
- `libei.so.1` / `libeis.so.1` present; `xdg-desktop-portal` running with
  `org.freedesktop.portal.RemoteDesktop` + `ConnectToEIS` on D-Bus — so the
  libei/portal path is viable as a future backend.
- A newly created uinput device isn't picked up instantly; GNOME/libinput takes
  ~1-2s to enumerate it.

## Milestones for input

1. **Virtual mouse via uinput — DONE ✅ (2026-07-17)** — `cursio-input` crate,
   `UinputInjector` behind `trait InputInjector`; persistent relative-mouse
   device with active/inactive. Verified live: real cursor moved (square +
   click), OS enumerated it as `mouse2`/`event25`, inactive froze motion.
2. **Wire injection to the network — DONE ✅ (2026-07-17)** — `session::run_session`
   dispatches inbound `Message::Input` to a caller callback; the `cursio` binary
   wires it to `UinputInjector` (`make_injector`/`inject`). Verified live over the
   Noise channel (loopback): `connect --send-test-input` streamed a synthetic
   square, the receiver injected it, and the real cursor moved. Full path
   sender → TCP → Noise → receiver → uinput → cursor works.
3. **Capture side — DONE ✅ (2026-07-17)** — `capture.rs`: `spawn_capture()`
   opens local mouse/keyboard via evdev (`Device::open` per enumerated path;
   needs `input`-group / `sg input`), reads async event streams, translates to
   neutral `InputEvent`, delivers on a channel. `session::run_session` is now
   symmetric (forwards captured events as `Message::Input` while injecting
   inbound). CLI: `mesh --send-input` (was `serve/connect --send-input` before
   those two collapsed into the single mesh command). Verified end-to-end over Noise
   (loopback): real mouse → capture → TCP → Noise → receiver → uinput; receiver
   accepted 7374 real input events. THE FULL INPUT PIPELINE WORKS.
   - Gotcha: the active mouse was `event3` (`2.4G Wireless Device`), not the
     first mouse-named node; `spawn_capture` opens all, so this is handled.
   - Gotcha: `evdev::enumerate()` yields lower-level devices whose async stream
     didn't deliver here; reopening each path with `Device::open` fixed it.
4. **Virtual keyboard via uinput — DONE ✅** — a second uinput device, kept
   separate from the pointer so the compositor classifies each as the plain
   mouse/keyboard it is rather than as a touchpad.
5. **Transition logic — DONE ✅** — `CursorRole` in `core/src/session.rs` owns
   who holds the cursor and who is driving whom; edge crossings and the
   Ctrl+Alt+G hotkey are two ways to ask for the same handoff.
6. **Other backends — DONE ✅** — libei/portal (Wayland), `SendInput`
   (Windows), `CGEventPost` (macOS), each behind the same two traits.

## Where a machine stands, and which edge that makes

Machines are placed on a **grid**: `--at X,Y`, defaulting to `0,0`. A side
cannot express order — four machines in a row are all "to the right" of the
first, and that vocabulary cannot say which one the cursor reaches first — and a
designated centre would make every other machine depend on one being switched
on, which is the arrangement the mesh exists to avoid.

`Position::edge_toward` (`protocol/src/input.rs`) takes the dominant axis, ties
breaking horizontal; `neighbour_across` picks the nearest peer in a direction,
so a machine going away lets the one behind it become the neighbour.

Positions ride in `Hello`, so **the edge is derived per link**, during the
handshake, from the two positions. That is seconds after capture starts — one
portal permission, one set of hooks, for the life of the process — so capture
comes up watching nothing and is handed the edges afterwards by
`set_watched_edges`: every edge that borders some peer, as an `EdgeSet`. Each
backend follows that differently, and the difference is the whole reason the
call exists rather than a constructor argument:

| backend | how it follows the edges |
| --- | --- |
| Windows | a bitmask the low-level mouse hook reads; a hook may not lock. There is no barrier, so `input/src/edge_gate.rs` decides from positions: a pointer reaching a 1px zone on an armed edge crosses, unless it arrived in one jump of more than 60px (a game's flick). Such a fast arrival only arms the zone, and the next push while still against the edge crosses — a touchpad swipe lands exactly like a flick, and refusing it until the pointer left and came back slowly meant hammering the edge to cross. The capture thread is per-monitor DPI aware, so the desktop bounds are in the hook's physical pixels |
| macOS | read from the event tap on every event |
| Wayland | capture is taken down, barriers placed on every armed edge, capture re-armed — a barrier is a D-Bus object living on one edge. The portal session itself is opened only when the first edge is armed: GNOME's InputCapture (version 1) cannot remember a grant, so opening it at start-up asked on every restart, even with no machine to send the cursor to |
| evdev on X11 | the X server is asked where the pointer is (`QueryPointer`) after motion, while this machine holds the cursor; a push outward at a watched edge is a crossing, and the pointer is warped back inside |
| evdev, no display | **cannot**: `REL_X`/`REL_Y` is a device, not a desktop |

evdev alone cannot see an edge: summing deltas does not recover the pointer
position — the display server applies acceleration and clamps at the edge, so
the sum drifts within seconds. On X11 the server answers instead
(`input/src/x11_edges.rs`, x11rb). That was once left out as a dependency no
session needed, until a 2008 MacBook whose GPU keeps GDM on Xorg joined the mesh
and the cursor could reach it but never come back. The root window's bounds are
the desktop's outer edges, as the Windows virtual screen is. With no display
server at all the session says so at startup and hands over by hotkey.

## Where the pointer comes in

A pointer that leaves near the top of one screen should come in near the top
of the next. `CrossEdge` has always carried where along the edge it left
(`pos_norm`, a fraction of the whole desktop's edge on that side), but the
receiver ignored it: the pointer appeared wherever it had last been, and on
Wayland the portal handed it back at the spot it had left by, so going over at
the top and back at the bottom brought it home at the top.

The session now hands that fraction to capture with the ownership change
(`Ownership::entry` → `cursio_input::enter_at`), and the backend places the
pointer once, as it takes control, at the same fraction of its own edge and
12 px inside it so the jump zone or barrier does not send it straight back. A
hotkey handoff carries no entry and leaves the pointer alone. Motion after it
stays relative.

The rule that keeps this from jerking a game: **place it only by a means a
game does not read as motion, and not at all while something owns the
pointer.** A game reads relative device motion (raw input, Wayland's
relative-pointer); a jump made of injected motion is exactly a flick to it.

| backend | how it places | when it does not |
| --- | --- | --- |
| Windows | `SetCursorPos` — no `WM_INPUT`, no low-level hook event, so neither games nor our own jump-zone watch see it | cursor hidden (`GetCursorInfo` without `CURSOR_SHOWING`), confined by `ClipCursor` to less than the desktop, or a mouse button held (a drag the jump would tear) |
| Wayland | `Release` with `cursor_position` while the portal holds the pointer: the compositor moves it itself, no device motion, barriers stay armed | the portal is not capturing (the cursor left by hotkey), or the release fails — then the old nudge off the edge |
| macOS, X11 | not yet: the pointer comes back where it left | — |

A game that hides the cursor is steering with it, and has no free pointer for a
height to apply to. That is not a lost feature but the correct answer: the
crossing itself should not happen while a game holds the mouse, and the flick
filter in `edge_gate.rs` exists for the same reason.

On Wayland a pointer this machine's portal never captured cannot be placed:
uinput only moves relatively, which is the motion a game reads. In practice
that is rare: a cursor coming here from a peer almost always left here across
a barrier first, so the portal still holds this machine's pointer and releases
it at the entry point. That covers a peer's own mouse driving this machine too:
the cursor reached the peer by this barrier before the user picked that mouse
up.

Monitors of different heights on one side are measured as one span, leaving and
coming in alike (`fraction_along`, `entry_position`): two monitors stacked
against the peer map its top half to the upper one.

## Crossing on part of an edge

An edge that faces a peer is also an edge of this desktop, and the desktop may
want it. GNOME reveals its dock when the pointer presses the bottom edge; with
a peer below, the barrier there took the pointer at first touch, and the dock
never showed. `[input.crossing]` narrows any edge to the part that crosses,
in percent along it (left to right, top to bottom):

```toml
[input.crossing]
bottom = [67, 100]   # only the right third of the bottom edge crosses
```

The app's settings page offers halves and thirds of each edge. The change
applies at once (`live.rs`): Wayland re-places its barriers, covering only
that part, and the other backends check the crossing point against it on the
next move. The rest of the edge is a plain wall. A crossing is still measured
along the whole edge, so the peer still places the pointer by its true height.

A pressure threshold (beamer's "resistance") was weighed instead and left out
for this. On Wayland the portal grabs the pointer the moment it meets the
barrier, so the desktop never sees the press, however long crossing then waits.
A narrowed edge leaves that part to the desktop entirely.

## Screens on one desk

The grid says a laptop is below a desktop; it cannot say the laptop stands
under the right-hand of the desktop's two monitors. With the desktop's whole
bottom edge as the unit, pushing down on either monitor reached the laptop,
and a laptop under the seam between two monitors could not be described at
all. So each machine also says what its screens are, and where they stand on
one desk every machine shares (`protocol/src/layout.rs`, `core/src/desk.rs`).

**What each machine says.** Its screens, read from the desktop every 5 s
(`cursio_input::screens`): each one's rectangle in the pixels capture measures
a crossing in, its panel's size in millimetres from EDID, and an id from the
EDID's maker and serial — the operating systems do not even agree on a port's
name (GNOME's `HDMI-1` is the kernel's `HDMI-A-1`), so the panel is recognised
by itself. Sources: GNOME's `DisplayConfig` with the kernel's EDID on Linux
(Wayland and X11 alike), `EnumDisplayMonitors` with the registry's EDID on
Windows, CoreGraphics on a Mac. Plus `[layout] place`, its corner on the desk
in millimetres, and the seams it closed. All of it rides `Hello` and
`Reconfigure` (protocol v24).

**Millimetres, not pixels.** The desk is the desk: a 27-inch 4K panel and a
14-inch 1080p laptop are told apart by size, and a pointer leaving one comes
in beside where it left, physically. One machine's screens are joined where
their pixels join, each sized by its panel; a panel that reports no size is
taken at 96 dpi. Where the display settings and the desk disagree — a 22-inch
and a 24-inch monitor aligned at the top in pixels but standing on the desk by
their feet — a monitor can be moved on its own (`[layout] offsets`, protocol
v25): only the desk moves, and the pointer between a machine's own monitors
still goes where its operating system sends it. Each machine writes only its own place, so there is nothing
to reconcile; arranging another machine from the app asks that machine
(`Message::Arrange`), and every peer hears the result from it.

**Seams.** Where one machine's screen edge lies against another's — within
3 mm, overlapping by at least 5 mm — is a seam. The hub arms only those
pixels of each edge (`set_crossing_windows`), sends a crossing to the peer
whose screen meets that very pixel, and fills `CrossEdge::entry` with the
point on that screen beside it, in the peer's pixels; the peer places the
pointer there (Wayland's release position, Windows' `SetCursorPos`). A seam
closed on either side (`[layout] barriers`) is not crossed either way.

**The grid stays.** A pair where either machine has no place yet, or no
screens (a phone, a desktop without a layout interface), crosses by the grid
as before. A machine placed on the desk but touching nothing is reached by no
edge at all. `[input.crossing]` still narrows whatever a seam allows.

**Edges of monitors, not of the box.** A crossing happens on the side of a
monitor that no other monitor continues: Wayland places barriers that way,
and Windows' gate now checks the hook's position against each monitor
(`input/src/desk.rs`) rather than the virtual screen's bounds, whose bottom a
shorter monitor never reaches. macOS and X11 still watch the box around their
monitors, which is the same edge unless the monitors differ in height.

## Several peers: one mouse, one hub

A machine with a neighbour to the right and another below has one mouse and
two ways out. Capture used to be lent whole to the first peer session, so every
later peer ran without local input, and the single watched edge belonged to
whichever peer greeted last — pushing down sent the cursor right, and right
stopped working at all.

`core/src/input_hub.rs` is what knows more than one peer exists. Capture's
forwarders hand everything to it, and it passes each thing to exactly one
session:

- **an edge crossing** to the nearest peer beyond that edge that can hold a
  cursor (`neighbour_across`, with distances taken when each link came up and
  whenever a position changes);
- **captured input** to the peer this machine is streaming to — only ever one,
  even when two sessions each believe they drive;
- **the hotkey** to the peer being driven (which is how the cursor comes back),
  else to the peer it last went to, else to the first linked;
- **panic** to every session.

Ownership itself is unchanged: each link still runs the two-machine
`CursorRole` exchange, and no wire vocabulary was added. What moved is what was
always machine-wide and was being written by each session in turn — the grab
mode, the armed edges and the hidden pointer. The hub recomputes them on every
report (`MachineInput`) and they are written only when they change, since
re-entering Active on Wayland releases the portal grab and nudges the pointer.

The start of a link is settled by comparing ids, which knows only that link. A
machine that already has a peer therefore never starts a new link as the side
that drives: otherwise the cursor would leave the screen the user is working on
because another machine switched on. Only the first link keeps the id rule.

With several edges armed there is no longer one "peer edge" to pull a returning
pointer away from or to park furthest from; the backends use the edge the
cursor last left by.

A peer that cannot hold a cursor (a phone used as a touchpad) arms no edge,
wherever it was placed: on Wayland a barrier with nobody to hand to would still
grab the user's input.

Its input is one more mouse on the machine it drives, so it follows that
machine's cursor. Its motion reaches the edges as injected motion does (the
Windows hook watches injected moves, a Wayland barrier stops a uinput pointer),
and the crossing goes through the hub like any other. From then on the hub
routes what the phone sends to the machine being driven, exactly as it routes
the local mouse; it is injected locally only while no peer is driven. A phone
paired with one machine therefore reaches every machine beyond its edges, and
the cursor comes back by the same edge it left by.

## Where a peer's modifiers land on a Mac

A Mac's shortcuts live on Command; a PC keyboard has Control, Alt and a Windows
key. `ModifierMap` (`input/src/mac_keymap.rs`) says where each **physical**
modifier key lands, and `--mac-modifiers` takes either a preset or a list of
`from=to` pairs:

| value | effect |
| --- | --- |
| `positional` (default) | Ctrl is Control, Alt is Option, Win is Command |
| `swapped` | Control acts as Command, Win acts as Control — Ctrl+C copies |
| `ctrl=cmd,win=ctrl` | the swap, spelled out |
| `rightalt=globe` | the right Alt becomes the Globe key, which is how a modern MacBook changes input source |

Names are `ctrl`, `alt`/`option`, `win`/`cmd`, `shift`, `caps`, `globe`/`fn`,
each optionally prefixed `left` or `right`. Sides are separable for exactly the
Globe case: aiming both Options at it would cost every accented character the
user can type. A side-less target keeps the side it came from, so `ctrl=cmd`
sends left to left and right to right.

The map runs in both directions — applied on injection, undone on capture —
so a Mac sending Command+C puts on the wire whichever key the peer turns back
into Command. Undoing is why a collision has to be settled rather than ignored:
two keys the user **named** are refused with both named, while a named key beats
one still sitting at its default. That second rule is what makes
`rightalt=globe` mean something: Globe was already pointing at itself.

The ownership hotkey is Ctrl+Alt+G in the **wire** namespace, so it follows the
map: by position it is the Mac's own Control+Option+G, and under the swap it is
Command+Option+G — which is consistent, since the swap is a statement that
Command is where Control belongs. Startup logs the physical keys, because a
hotkey that moved silently is one the user cannot find.

## Hiding this machine's cursor

While a machine drives its peer, its own pointer should get out of the way.
What that means is wildly platform-dependent, and only one platform can do it
outright:

- **Windows** — swaps the system cursors for blank ones. Real, and it holds.
- **macOS** — `CGDisplayHideCursor` works only while Cursio is the *frontmost*
  application, and the window server restores the pointer the moment it is not,
  which for a daemon is nearly always. So the pointer is **also parked**: warped
  into the corner furthest from the peer-facing edge, and restored on takeback
  to where the user left it, pulled 12 px inside that edge. Landing exactly on
  the edge it crossed would read as a fresh crossing on the next small movement.
- **Linux** — neither half is available from a background process. Wayland lets
  only a focused client with its own surface touch the cursor, and there is no
  absolute warp to park with; X11 could hide via XFixes, but an X11 session
  cannot detect an edge crossing anyway. The pointer stops moving (local input
  is grabbed) and stays visible; the log says so once. The supported way through
  is for Cursio to own a fullscreen transparent GTK4 surface with an empty input
  region and its cursor set to `none` — real work in `cursio-window`, not a call
  to add in `cursio-input`.

The injector is told when it is allowed to act (`InputInjector::set_active`),
and that transition is where a backend re-reads what went stale while it was
idle — on macOS, the shadow cursor it must keep because `CGEvent` carries no
relative motion. Showing the cursor happens *before* activating the injector,
so the shadow is seeded from the restored pointer rather than the parking
corner.
