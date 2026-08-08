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
- `InputEvent::PointerScroll { dx, dy }` — notches, +y = up (evdev REL_WHEEL).
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
comes up watching nothing and is pointed at an edge afterwards by
`set_peer_edge`. Each backend follows that differently, and the difference is
the whole reason the call exists rather than a constructor argument:

| backend | how it follows the edge |
| --- | --- |
| Windows | an atomic the low-level mouse hook reads; a hook may not lock |
| macOS | read from the event tap on every event |
| Wayland | capture is taken down, a new barrier placed, capture re-armed — a barrier is a D-Bus object living on one edge |
| evdev | **cannot**: `REL_X`/`REL_Y` is a device, not a desktop |

The evdev gap is not an unfinished backend. Summing deltas does not recover the
pointer position either — the compositor applies acceleration and clamps at the
edge, so the sum drifts within seconds. Answering it needs the display server,
and on X11 that means `XQueryPointer` and a dependency; deliberately not taken.
A non-Wayland Linux session says so at startup and hands over by hotkey.

Several peers still mean one watched edge, because there is one physical mouse
and the backends each hold a single edge. Arming a *set* is the change that
would let one machine border two peers at once.

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
