//! Linux input capture via evdev.
//!
//! Reads raw events from the machine's real mouse/keyboard devices
//! (`/dev/input/event*`) and translates them into platform-neutral
//! [`InputEvent`]s, delivered on an async channel. The source side of input
//! sharing: these events are what we stream to the peer for injection.
//!
//! Requires read access to `/dev/input/event*` — typically membership in the
//! `input` group (see `docs/input-design.md`).
//!
//! Pointer motion is captured as **relative** deltas (`REL_X`/`REL_Y`), matching
//! the injection side and the "relative, not absolute" design requirement.
//!
//! ## Edge crossing needs the display server
//!
//! Every other backend can say where the pointer is: the Wayland portal fires a
//! barrier, the Windows hook reads the cursor, the macOS tap carries a point.
//! evdev reports `REL_X` and `REL_Y` and nothing else — it is the *device*, not
//! the desktop, so "the cursor reached the screen edge" is not a question this
//! layer can answer. Summing deltas does not recover it either: the compositor
//! applies acceleration and clamps at the edge, so the sum drifts from the real
//! position within seconds and never comes back.
//!
//! On an X11 desktop the X server answers it ([`crate::x11_edges`]): motion is
//! still read here, and each batch of it while this machine holds the cursor
//! is followed by a `QueryPointer`. This was once left out as a dependency
//! nobody's session needed; then an old MacBook whose GPU keeps GDM on Xorg
//! joined the mesh, and the cursor could reach it and never come back. With no
//! X server either, the edges channel stays silent and
//! [`crate::spawn_capture`] says so.

use cursio_protocol::{InputEvent, MouseButton};
use evdev::{Device, EventSummary, KeyCode, RelativeAxisCode};
use tokio::sync::mpsc;

use crate::{CaptureHandle, InputError};

/// Discover local mouse/keyboard devices, start reading them, and return a
/// [`CaptureHandle`] whose channel yields neutral [`InputEvent`]s.
///
/// Opens every device that looks like a pointer (has relative axes) or a
/// keyboard (has keys), spawning one async reader task per device. Skips
/// Cursio's own virtual devices so we never capture what we inject (which would
/// loop). Returns an error only if no capturable device could be opened.
pub fn spawn_capture() -> Result<CaptureHandle, InputError> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (grab, grab_rx) = crate::GrabControl::new();
    let (edges_tx, edges_rx) = mpsc::unbounded_channel();
    // Where the pointer is comes from the X server when there is one; without
    // it this backend cannot see an edge, and the edges channel stays silent.
    crate::x11_edges::spawn(edges_tx, grab_rx.clone());
    let mut opened = Vec::new();

    // Enumerate paths, but (re)open each via `Device::open` so we drive the
    // high-level `Device` event stream — the same path that reliably yields
    // events. `enumerate()` hands back lower-level `RawDevice`s whose async
    // stream did not deliver events here.
    for (path, probe) in evdev::enumerate() {
        let name = probe.name().unwrap_or("<unnamed>").to_string();

        // Never capture our own injected input — that would feed events back
        // into the loop.
        if name.starts_with("Cursio") {
            continue;
        }

        let is_pointer = probe.supported_relative_axes().is_some();
        let is_keyboard = probe
            .supported_keys()
            .map(|keys| keys.iter().any(is_keyboard_key))
            .unwrap_or(false);

        if !is_pointer && !is_keyboard {
            continue;
        }

        let device = match Device::open(&path) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(device = %name, ?path, error = %e, "could not open device");
                continue;
            }
        };

        match start_reader(device, tx.clone(), grab_rx.clone()) {
            Ok(()) => {
                tracing::info!(device = %name, ?path, "capturing device");
                opened.push(name);
            }
            Err(e) => {
                tracing::warn!(device = %name, error = %e, "could not start reader");
            }
        }
    }

    if opened.is_empty() {
        // Names the remedy, not just the group: every device was skipped or
        // refused, and on a machine that has a mouse plugged in the reason is
        // almost always that this user cannot read /dev/input/event*.
        return Err(InputError::Unavailable(
            "no capturable input devices — this user probably cannot read /dev/input/event*. \
             Add them to the group that owns it with `sudo usermod -aG input $USER`, then log \
             out and back in"
                .into(),
        ));
    }

    Ok(CaptureHandle {
        events: rx,
        edges: edges_rx,
        devices: opened,
        grab,
    })
}

/// Turn a device into an async event stream and spawn a task that forwards
/// translated events onto `tx` until the stream ends.
///
/// The task also watches `grab_rx`: when it flips to `true` the device is
/// grabbed (`EVIOCGRAB`), so events keep reaching us but no longer act on the
/// local machine; flipping back to `false` ungrabs it.
fn start_reader(
    device: Device,
    tx: mpsc::UnboundedSender<InputEvent>,
    mut grab_rx: tokio::sync::watch::Receiver<crate::InputMode>,
) -> Result<(), InputError> {
    let mut stream = device.into_event_stream()?;
    tokio::spawn(async move {
        tracing::debug!("reader task started");
        let mut grabbed = false;
        loop {
            tokio::select! {
                // Mode changed: grab only in Dormant (evdev has no edge barriers,
                // so the Wayland arm-while-Active distinction doesn't apply here).
                changed = grab_rx.changed() => {
                    if changed.is_err() {
                        // GrabControl dropped; keep reading without grab.
                        continue;
                    }
                    if matches!(*grab_rx.borrow_and_update(), crate::InputMode::Shutdown) {
                        // Session over for good: stop reading this device so a
                        // later session doesn't end up with two readers on it.
                        break;
                    }
                    let want = grab_rx.borrow_and_update().grabbed();
                    if want != grabbed {
                        let dev = stream.device_mut();
                        let res = if want { dev.grab() } else { dev.ungrab() };
                        match res {
                            Ok(()) => grabbed = want,
                            Err(e) => tracing::warn!(error = %e, want, "grab/ungrab failed"),
                        }
                    }
                }
                event = stream.next_event() => {
                    let event = match event {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::debug!(error = %e, "device read error; stopping reader");
                            break;
                        }
                    };
                    if let Some(neutral) = translate(event) {
                        if let InputEvent::PointerMotion { dx, dy } = neutral {
                            crate::x11_edges::note_motion(dx, dy);
                        }
                        if !grab_rx.borrow().forwards_events() {
                            continue;
                        }
                        // Receiver gone => nothing left to capture for; stop.
                        if tx.send(neutral).is_err() {
                            tracing::debug!("capture receiver gone; stopping reader");
                            break;
                        }
                    }
                }
            }
        }
        // Best-effort release on exit so a crashed session doesn't leave the
        // device grabbed and the local input dead.
        if grabbed {
            let _ = stream.device_mut().ungrab();
        }
    });
    Ok(())
}

/// Map one evdev event to a neutral [`InputEvent`], or `None` if we don't
/// forward it (SYN, unsupported axes/buttons, etc.).
fn translate(event: evdev::InputEvent) -> Option<InputEvent> {
    match event.destructure() {
        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_X, v) => {
            Some(InputEvent::PointerMotion { dx: v, dy: 0 })
        }
        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_Y, v) => {
            Some(InputEvent::PointerMotion { dx: 0, dy: v })
        }
        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_WHEEL, v) => {
            Some(InputEvent::PointerScroll { dx: 0, dy: v })
        }
        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_HWHEEL, v) => {
            Some(InputEvent::PointerScroll { dx: v, dy: 0 })
        }
        EventSummary::Key(_, code, value) => {
            // value: 0 = release, 1 = press, 2 = autorepeat. Treat repeat as
            // press so held keys keep acting on the peer.
            let pressed = value != 0;
            if let Some(button) = mouse_button(code) {
                Some(InputEvent::PointerButton { button, pressed })
            } else {
                // Forward as a keyboard key using the evdev keycode namespace.
                Some(InputEvent::Key {
                    key: code.code(),
                    pressed,
                })
            }
        }
        _ => None,
    }
}

/// Map an evdev mouse button to a neutral [`MouseButton`], or `None` if it's
/// not one of the buttons we forward as a pointer button.
fn mouse_button(code: KeyCode) -> Option<MouseButton> {
    match code {
        KeyCode::BTN_LEFT => Some(MouseButton::Left),
        KeyCode::BTN_RIGHT => Some(MouseButton::Right),
        KeyCode::BTN_MIDDLE => Some(MouseButton::Middle),
        KeyCode::BTN_SIDE => Some(MouseButton::Back),
        KeyCode::BTN_EXTRA => Some(MouseButton::Forward),
        _ => None,
    }
}

/// Heuristic: does this key indicate a real keyboard (vs. a mouse's buttons)?
/// Mouse `BTN_*` codes are >= 0x100; ordinary keyboard keys are below that.
fn is_keyboard_key(code: KeyCode) -> bool {
    // BTN_0 (0x100) is the first mouse/gamepad button code; keyboard keys are
    // below it.
    code.code() < KeyCode::BTN_0.code()
}
