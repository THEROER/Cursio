//! Driving a peer's cursor from a touch surface.
//!
//! The whole feature is a channel. A dormant session forwards everything on its
//! capture channel to the peer ([`Link::run_as_input_source`]), and a device
//! that cannot hold the cursor is dormant for the life of the link — so the
//! touch surface simply pushes [`InputEvent`]s into that channel and the peer's
//! pointer moves. There is no touch vocabulary on the wire and none is needed:
//! a touchpad produces *relative* motion, which is exactly what
//! [`InputEvent::PointerMotion`] carries.
//!
//! What lives here is the small amount of translation between a browser's
//! pointer events and that vocabulary, plus the task that holds the link up.
//!
//! ## Why the UI sends deltas rather than positions
//!
//! A touchpad is not a digitiser: the finger's position on the glass means
//! nothing, only how far it moved since the last frame. Sending positions would
//! also mean deciding what they are relative to — which of the peer's screens,
//! at what scale — and that is a question a touchpad never has to ask.

use std::sync::Mutex;
use std::time::Duration;

use cursio_core::link::{self, Link};
use cursio_core::module::ModuleRegistry;
use cursio_core::session::SessionError;
use cursio_core::NodeIdentity;
use cursio_protocol::{InputEvent, MouseButton};
use cursio_transport::pairing::Role;
use cursio_transport::TcpTransport;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// How long to browse the LAN before giving up on finding a peer.
///
/// Long enough for mDNS to answer on a busy network, short enough that a user
/// staring at a phone screen learns something went wrong rather than wondering
/// whether it is still trying.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(8);

/// What the UI is told about the link, so the screen can say something true
/// rather than pretending every tap landed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    Idle,
    Searching,
    Connecting { peer: String },
    Connected { peer: String },
    Failed { reason: String },
}

/// Shared state behind the Tauri commands.
pub struct Touchpad {
    /// Events on their way to the peer. Present only while a link is up: with
    /// no session there is nothing to send to, and quietly buffering strokes
    /// that will never arrive is worse than dropping them.
    events: Mutex<Option<UnboundedSender<InputEvent>>>,
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

    /// Hand an event to the live session, if there is one.
    ///
    /// Returns whether it went anywhere, which is what lets the UI stop
    /// pretending: a surface that swallows strokes into a dead link looks
    /// identical to one whose peer has frozen.
    fn send(&self, event: InputEvent) -> bool {
        let guard = self.events.lock().expect("touchpad channel");
        match guard.as_ref() {
            Some(tx) => tx.send(event).is_ok(),
            None => false,
        }
    }

    fn attach(&self, tx: UnboundedSender<InputEvent>) {
        *self.events.lock().expect("touchpad channel") = Some(tx);
    }

    fn detach(&self) {
        *self.events.lock().expect("touchpad channel") = None;
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

/// Find a peer, bring the link up, and run this device as its input source
/// until the session ends.
async fn run_link(
    identity: NodeIdentity,
    password: String,
    mut events: UnboundedReceiver<InputEvent>,
    report: impl Fn(Status),
) -> anyhow::Result<()> {
    report(Status::Searching);
    let peer = link::discover_one(identity.peer_id, DISCOVER_TIMEOUT)
        .await
        .ok_or_else(|| anyhow::anyhow!("no peer found on this network"))?;
    report(Status::Connecting {
        peer: peer.display_name.clone(),
    });

    let transport = TcpTransport::connect(peer.addr).await?;
    // A touchpad runs no modules: it has no disks to share, no audio to carry
    // and no windows to move. It advertises input and nothing else, so the peer
    // does not stand up half a session for a device that cannot use it.
    let registry = ModuleRegistry::new();
    let link = link::establish(
        transport,
        link::Handshake {
            identity: &identity,
            password: &password,
            // We dialled, so we lead both handshakes.
            role: Role::Initiator,
            registry: &registry,
            peer_edge: None,
            sends_input: true,
        },
    )
    .await?;

    if link.cursor.start_active {
        // Refusing rather than carrying on: an input source that is somehow
        // active would inject its own events locally instead of sending them,
        // and the user would be left tapping at a surface that moves nothing.
        anyhow::bail!(
            "this device was handed the shared cursor, which it cannot hold; \
             the peer is running an older build"
        );
    }

    report(Status::Connected {
        peer: link.peer.display_name.clone(),
    });
    match Link::run_as_input_source(link, &mut events).await {
        // The peer hanging up is how a session normally ends.
        Ok(()) | Err(SessionError::PeerLeft) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Tauri commands. Thin by design: everything they do is push an event or start
/// the task above.
pub mod commands {
    use super::*;
    use tauri::State;

    #[tauri::command]
    pub fn touchpad_status(pad: State<'_, Touchpad>) -> Status {
        pad.status()
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

    /// Start looking for a peer and drive it until the link ends.
    ///
    /// The password is only used on a first meeting; once the two machines are
    /// paired the trust store answers instead and it is ignored.
    #[tauri::command]
    pub async fn touchpad_connect(
        app: tauri::AppHandle,
        pad: State<'_, Touchpad>,
        password: String,
    ) -> Result<(), String> {
        let identity = NodeIdentity::load_or_create().map_err(|e| e.to_string())?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        pad.attach(tx);

        // The task outlives this command, so it reports through the app handle
        // rather than the borrowed state.
        let handle = app.clone();
        tokio::spawn(async move {
            let report = |status: Status| {
                use tauri::Manager;
                let pad = handle.state::<Touchpad>();
                pad.set_status(status);
            };
            let outcome = run_link(identity, password, rx, &report).await;
            use tauri::Manager;
            let pad = handle.state::<Touchpad>();
            match outcome {
                Ok(()) => pad.set_status(Status::Idle),
                Err(e) => pad.set_status(Status::Failed {
                    reason: e.to_string(),
                }),
            }
            // Nothing to send to any more; further taps are refused rather
            // than queued into a channel nobody is reading.
            pad.detach();
        });
        Ok(())
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

    /// With no link up, events must be refused rather than buffered.
    ///
    /// A queue that fills while nothing is connected replays a burst of stale
    /// motion the moment a session starts — the pointer jumps somewhere the
    /// user pointed at a minute ago.
    #[test]
    fn events_are_dropped_while_nothing_is_connected() {
        let pad = Touchpad::default();
        assert!(!pad.send(InputEvent::PointerMotion { dx: 5, dy: 5 }));
    }

    /// And accepted once there is somewhere to send them.
    #[test]
    fn events_reach_a_live_session() {
        let pad = Touchpad::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        pad.attach(tx);

        assert!(pad.send(InputEvent::PointerMotion { dx: 3, dy: -2 }));
        assert_eq!(
            rx.try_recv().expect("delivered"),
            InputEvent::PointerMotion { dx: 3, dy: -2 }
        );

        // A session that ends stops accepting, rather than filling a channel
        // whose reader is gone.
        pad.detach();
        assert!(!pad.send(InputEvent::PointerMotion { dx: 1, dy: 1 }));
    }
}
