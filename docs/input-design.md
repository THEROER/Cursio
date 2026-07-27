# Cursio — Input Design (mouse & keyboard sharing)

Status: design agreed 2026-07-17. Implementation in progress (virtual mouse via
uinput first). This document is the source of truth for how input sharing
should behave; code should match it or this doc should be updated.

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
- "Hiding" the cursor fully (parking it in a corner, and grabbing the local
  real mouse so it can't nudge the frozen cursor) is deferred to the
  **transition milestone**, because grabbing local input is a separate,
  capture-side concern. First milestone = active/inactive only.

### Why not fully hide the cursor now

On GNOME/Wayland an app cannot globally hide the system cursor. While the cursor
is logically "on the other PC", this machine's **real physical mouse still moves
its local cursor** (the OS doesn't know about our logical transition). Truly
making the cursor "disappear" here needs (a) parking the cursor and (b)
suppressing local input via an evdev grab — both belong to the capture/transition
work, not to basic injection.

## Wire protocol

`crates/protocol/src/input.rs`:
- `InputEvent::PointerMotion { dx, dy }` — relative deltas (evdev REL_X/REL_Y sign convention: +x right, +y down).
- `InputEvent::PointerButton { button, pressed }` — `MouseButton` = Left/Right/Middle for now.
- `InputEvent::PointerScroll { dx, dy }` — notches, +y = up (evdev REL_WHEEL).
- `InputEvent::Key { key, pressed }` — **key is a Linux evdev keycode** (`KEY_*`
  from `input-event-codes.h`) used as the neutral cross-platform namespace; each
  platform maps to/from it. Carried as `Message::Input(InputEvent)`.

## Injection backend

Decision: **uinput first, behind a `trait InputInjector`** so libei/portal
(Wayland "proper" path) and a Windows backend can be added later without
touching callers. Platform-specific code lives in a dedicated `cursio-input`
crate, kept out of the transport crate.

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
4. **Virtual keyboard via uinput (NEXT)** — keycode injection; `InputEvent::Key`
   is captured and carried on the wire but not yet injected (needs a virtual
   keyboard device).
5. Transition logic (whose cursor / when it crosses / parking + local grab).
6. Later backends: libei/portal (Wayland), Windows (SendInput).
