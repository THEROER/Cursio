//! Driving a peer's cursor from a phone's touch surface.
//!
//! The whole feature is a channel. A phone never takes the cursor, so its
//! session with each peer stays dormant for the life of the link, and a
//! dormant session forwards everything on its capture channel to the peer —
//! so the touch surface simply pushes [`InputEvent`]s into the session of the
//! machine it is pointed at, and that machine's pointer moves. There is no
//! touch vocabulary on the wire and none is needed: a touchpad produces
//! *relative* motion, which is exactly what [`InputEvent::PointerMotion`]
//! carries.
//!
//! It used to bring up a link of its own, with a password, beside the node's.
//! On a phone the node already holds a paired session with every machine, so
//! that second link asked the user for what the pairing had settled.
//!
//! ## Why the UI sends deltas rather than positions
//!
//! A touchpad is not a digitiser: the finger's position on the glass means
//! nothing, only how far it moved since the last frame. Sending positions would
//! also mean deciding what they are relative to — which of the peer's screens,
//! at what scale — and that is a question a touchpad never has to ask.

use std::sync::Mutex;

use cursio_core::session::CapturedInput;
use cursio_protocol::{GesturePhase, InputEvent, MouseButton, GESTURE_SCALE_ONE};
use tokio::sync::mpsc::UnboundedSender;

/// What the UI is told, so the screen can say something true rather than
/// pretending every tap landed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    Idle,
    Connected { peer: String },
}

/// A machine the touchpad can drive.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Target {
    pub id: String,
    pub name: String,
}

/// Shared state behind the Tauri commands.
pub struct Touchpad {
    /// The driven peer's session channel. Present only while that link is up:
    /// quietly buffering strokes that will never arrive is worse than dropping
    /// them.
    events: Mutex<Option<UnboundedSender<CapturedInput>>>,
    status: Mutex<Status>,
}

impl Default for Touchpad {
    fn default() -> Self {
        Touchpad {
            events: Mutex::new(None),
            status: Mutex::new(Status::Idle),
        }
    }
}

impl Touchpad {
    pub fn status(&self) -> Status {
        self.status.lock().expect("touchpad status").clone()
    }

    fn set_status(&self, status: Status) {
        tracing::info!(?status, "touchpad status");
        *self.status.lock().expect("touchpad status") = status;
    }

    /// Hand an event to the driven peer's session, if it is still up.
    ///
    /// Returns whether it went anywhere, which is what lets the UI stop
    /// pretending: a surface that swallows strokes into a dead link looks
    /// identical to one whose peer has frozen. A link that has ended is
    /// let go of here, so the UI learns it on the next stroke.
    fn send(&self, event: InputEvent) -> bool {
        let mut guard = self.events.lock().expect("touchpad channel");
        let sent = guard
            .as_ref()
            .is_some_and(|tx| tx.send(CapturedInput::from(event)).is_ok());
        if !sent && guard.take().is_some() {
            drop(guard);
            self.set_status(Status::Idle);
        }
        sent
    }

    fn attach(&self, tx: UnboundedSender<CapturedInput>) {
        *self.events.lock().expect("touchpad channel") = Some(tx);
    }
}

/// Translate the UI's button name into the wire vocabulary.
///
/// Unknown names are refused rather than guessed at: a button that arrives as
/// something else is a click in the wrong place on someone's machine, which is
/// worse than a click that does not happen.
pub fn button_named(name: &str) -> Option<MouseButton> {
    match name {
        "left" => Some(MouseButton::Left),
        "right" => Some(MouseButton::Right),
        "middle" => Some(MouseButton::Middle),
        "back" => Some(MouseButton::Back),
        "forward" => Some(MouseButton::Forward),
        _ => None,
    }
}

/// Translate the UI's gesture into the wire vocabulary. Unknown names are
/// refused, as for buttons: a gesture that is never ended leaves the peer
/// holding virtual fingers down.
pub fn gesture(
    kind: &str,
    phase: &str,
    fingers: u8,
    dx: i32,
    dy: i32,
    scale: f64,
    rotation: i32,
) -> Option<InputEvent> {
    let phase = match phase {
        "begin" => GesturePhase::Begin,
        "update" => GesturePhase::Update,
        "end" => GesturePhase::End,
        "cancel" => GesturePhase::Cancel,
        _ => return None,
    };
    match kind {
        "swipe" => Some(InputEvent::GestureSwipe {
            phase,
            fingers,
            dx,
            dy,
        }),
        "pinch" => Some(InputEvent::GesturePinch {
            phase,
            fingers,
            scale: (scale * f64::from(GESTURE_SCALE_ONE)).round() as i32,
            rotation,
            dx,
            dy,
        }),
        _ => None,
    }
}

/// Tauri commands. Thin by design: everything they do is push an event or
/// pick the session to push into.
pub mod commands {
    use super::*;
    use tauri::State;

    #[tauri::command]
    pub fn touchpad_status(pad: State<'_, Touchpad>) -> Status {
        pad.status()
    }

    /// The machines the touchpad can drive: every peer with a live link.
    #[tauri::command]
    pub fn touchpad_targets() -> Vec<Target> {
        cursio_android::input_targets()
            .into_iter()
            .map(|(id, name)| Target {
                id: id.to_string(),
                name,
            })
            .collect()
    }

    /// Point the touchpad at one linked peer — `peer` by id, or the only one
    /// when there is a single link.
    #[tauri::command]
    pub fn touchpad_connect(pad: State<'_, Touchpad>, peer: Option<String>) -> Result<(), String> {
        let targets = cursio_android::input_targets();
        let (id, name) = match peer {
            Some(peer) => targets
                .into_iter()
                .find(|(id, _)| id.to_string() == peer)
                .ok_or_else(|| "that machine is not connected to this phone".to_string())?,
            None => match targets.as_slice() {
                [only] => only.clone(),
                [] => return Err("no paired machine is connected to this phone".into()),
                _ => return Err("choose which machine to drive".into()),
            },
        };
        let tx = cursio_android::input_to(&id)
            .ok_or_else(|| format!("{name} is no longer connected"))?;
        pad.attach(tx);
        pad.set_status(Status::Connected { peer: name });
        Ok(())
    }

    /// Relative motion, already accumulated by the UI over one frame.
    #[tauri::command]
    pub fn touchpad_motion(pad: State<'_, Touchpad>, dx: i32, dy: i32) -> bool {
        if dx == 0 && dy == 0 {
            return true;
        }
        pad.send(InputEvent::PointerMotion { dx, dy })
    }

    #[tauri::command]
    pub fn touchpad_button(pad: State<'_, Touchpad>, button: &str, pressed: bool) -> bool {
        match button_named(button) {
            Some(button) => pad.send(InputEvent::PointerButton { button, pressed }),
            None => {
                tracing::warn!(button, "unknown button from the touch surface; ignoring");
                false
            }
        }
    }

    #[tauri::command]
    pub fn touchpad_scroll(pad: State<'_, Touchpad>, dx: i32, dy: i32) -> bool {
        if dx == 0 && dy == 0 {
            return true;
        }
        pad.send(InputEvent::PointerScroll { dx, dy })
    }

    /// A swipe of three or more fingers, or a pinch. `scale` is the ratio to
    /// the gesture's start; `rotation` this update's turn in hundredths of a
    /// degree.
    #[tauri::command]
    #[allow(clippy::too_many_arguments)]
    pub fn touchpad_gesture(
        pad: State<'_, Touchpad>,
        kind: &str,
        phase: &str,
        fingers: u8,
        dx: i32,
        dy: i32,
        scale: f64,
        rotation: i32,
    ) -> bool {
        match gesture(kind, phase, fingers, dx, dy, scale, rotation) {
            Some(event) => pad.send(event),
            None => {
                tracing::warn!(
                    kind,
                    phase,
                    "unknown gesture from the touch surface; ignoring"
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buttons the surface can send must map to the ones on the wire, and
    /// nothing else may be invented.
    #[test]
    fn only_known_buttons_are_accepted() {
        assert_eq!(button_named("left"), Some(MouseButton::Left));
        assert_eq!(button_named("right"), Some(MouseButton::Right));
        assert_eq!(button_named("middle"), Some(MouseButton::Middle));
        assert_eq!(button_named(""), None);
        assert_eq!(button_named("Left"), None, "names are exact, not guessed");
        assert_eq!(button_named("thumb"), None);
    }

    #[test]
    fn a_pinch_scale_reaches_the_wire_as_a_fixed_point_ratio() {
        assert_eq!(
            gesture("pinch", "update", 2, 1, -1, 1.5, 250),
            Some(InputEvent::GesturePinch {
                phase: GesturePhase::Update,
                fingers: 2,
                scale: GESTURE_SCALE_ONE * 3 / 2,
                rotation: 250,
                dx: 1,
                dy: -1,
            })
        );
        assert_eq!(
            gesture("pinch", "Update", 2, 0, 0, 1.0, 0),
            None,
            "names are exact"
        );
        assert_eq!(gesture("rotate", "end", 2, 0, 0, 1.0, 0), None);
    }

    /// With no link chosen, events must be refused rather than buffered.
    ///
    /// A queue that fills while nothing is connected replays a burst of stale
    /// motion the moment a session starts — the pointer jumps somewhere the
    /// user pointed at a minute ago.
    #[test]
    fn events_are_dropped_while_nothing_is_connected() {
        let pad = Touchpad::default();
        assert!(!pad.send(InputEvent::PointerMotion { dx: 5, dy: 5 }));
    }

    #[test]
    fn events_reach_a_live_session_and_a_dead_one_is_let_go() {
        let pad = Touchpad::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        pad.attach(tx);
        pad.set_status(Status::Connected {
            peer: "Nocturne".into(),
        });

        assert!(pad.send(InputEvent::PointerMotion { dx: 3, dy: -2 }));
        assert_eq!(
            rx.try_recv().expect("delivered").event,
            InputEvent::PointerMotion { dx: 3, dy: -2 }
        );

        drop(rx);
        assert!(!pad.send(InputEvent::PointerMotion { dx: 1, dy: 1 }));
        assert!(matches!(pad.status(), Status::Idle));
    }
}
