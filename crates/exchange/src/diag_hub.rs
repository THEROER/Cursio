//! The diagnostics module's hub, and the sessions it reports the paths of.
//!
//! Apart from the module itself (`cursio_core::diag_session`) because the
//! audio and phone-screen modules ask it too: whether Bluetooth carries a
//! peer's session, and a phone's own answers. The module answers through
//! the rest of the node — its log, its applications — and stays there.
//!
//! The paths of a session belong to the transport, which the module does not
//! hold — it runs on its own thread and sees only messages. So the registry
//! records each session's [`ResumeHandle`] here as it holds it
//! ([`hold`]/[`release`]), and both the control socket and the answering
//! half find it by peer.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use cursio_protocol::diag::{DiagRequest, DiagResponse, PathChoice, PathReport};
use cursio_protocol::PeerId;
use cursio_transport::resume::ResumeHandle;

use crate::request_hub::{HubKind, RequestHub};

/// Longest a caller waits for a peer's answer. A log answer is up to half a
/// megabyte, which over Bluetooth is seconds.
pub const ANSWER_WAIT: Duration = Duration::from_secs(30);

pub type DiagHub = RequestHub<DiagRequest, DiagResponse>;

pub fn hub() -> &'static DiagHub {
    static HUB: OnceLock<DiagHub> = OnceLock::new();
    HUB.get_or_init(|| {
        RequestHub::new(HubKind {
            wait: ANSWER_WAIT,
            peer: "machine",
            shares: "diagnostics",
        })
    })
}

fn sessions() -> std::sync::MutexGuard<'static, HashMap<PeerId, ResumeHandle>> {
    static SESSIONS: OnceLock<Mutex<HashMap<PeerId, ResumeHandle>>> = OnceLock::new();
    SESSIONS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// `peer`'s session is now `handle`.
pub fn hold(peer: PeerId, handle: ResumeHandle) {
    sessions().insert(peer, handle);
}

/// `peer`'s session tagged `tag` ended, unless a newer one replaced it.
pub fn release(peer: PeerId, tag: cursio_protocol::SessionTag) {
    let mut sessions = sessions();
    if sessions.get(&peer).is_some_and(|h| h.tag() == tag) {
        sessions.remove(&peer);
    }
}

/// Choose `choice` for this end of the session with `peer`, when given, and
/// report its paths.
pub fn paths(peer: &PeerId, choice: Option<PathChoice>) -> Result<PathReport, String> {
    let handle = sessions()
        .get(peer)
        .filter(|h| !h.is_ended())
        .cloned()
        .ok_or_else(|| "no session to this machine is running".to_string())?;
    if let Some(choice) = choice {
        handle.choose(choice);
    }
    Ok(handle.report())
}

/// Whether Bluetooth carries the session with `peer` now: the one path a
/// module's traffic shares with every pointer move, at a fraction of the
/// network's speed.
pub fn carried_by_bluetooth(peer: &PeerId) -> bool {
    sessions()
        .get(peer)
        .filter(|h| !h.is_ended())
        .is_some_and(|h| h.report().carrying.as_deref() == Some("bluetooth"))
}
