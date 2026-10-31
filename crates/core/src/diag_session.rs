//! Diagnostics on one link: the peer's log, and the session's paths chosen
//! by hand. See [`cursio_protocol::diag`] for why both exist.
//!
//! Symmetric, like the gallery: every node answers for its own log and
//! paths and may ask its peer for theirs. Requests come from the control
//! socket (`cursio logs`, `cursio path`) through [`hub`].
//!
//! The paths of a session belong to the transport, which the module does not
//! hold — it runs on its own thread and sees only messages. So the registry
//! records each session's [`ResumeHandle`] here as it holds it
//! ([`hold`]/[`release`]), and both the control socket and the answering
//! half find it by peer.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use cursio_protocol::diag::{DiagMsg, DiagRequest, DiagResponse, PathChoice, PathReport};
use cursio_protocol::{ModuleAdvert, ModuleId, ModuleRole, PeerId, Priority};
use cursio_transport::resume::ResumeHandle;

use crate::exchange_session::{Exchange, ExchangeEngine};
use crate::log_tail::Tail;
use crate::module::SessionModule;
use crate::request_hub::{HubKind, RequestHub};

/// Version of [`DiagMsg`]; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

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

/// This node's own log, as a peer or the control socket asks for it.
pub fn own_log(tail: Tail) -> Result<DiagResponse, String> {
    let tail = tail.bounded()?;
    let name = cursio_log::log_file_name().ok_or("this node keeps no log file")?;
    crate::log_tail::read(
        &cursio_log::log_dir(),
        name,
        &tail,
        std::time::SystemTime::now(),
    )
}

/// What answers the peer: which peer, so its session's paths are found.
pub struct DiagStore {
    peer: PeerId,
}

pub struct Diag;

impl Exchange for Diag {
    type Request = DiagRequest;
    type Response = DiagResponse;
    type Store = DiagStore;

    const MODULE: ModuleId = ModuleId::DIAG;
    const NAME: &'static str = "diag";
    const NOT_HERE: &'static str = "a peer asked for diagnostics this machine does not answer";
    // A log read is up to 64 MiB of disk; one at a time per link.
    const MAX_IN_FLIGHT: Option<usize> = Some(1);

    fn hub() -> &'static DiagHub {
        hub()
    }

    fn answer(store: &DiagStore, request: &DiagRequest) -> Result<DiagResponse, String> {
        match request {
            DiagRequest::Logs {
                after,
                within_secs,
                contains,
                max_lines,
            } => own_log(Tail {
                after: after.clone(),
                within: within_secs.map(Duration::from_secs),
                contains: contains.clone(),
                max_lines: *max_lines,
            }),
            DiagRequest::Paths { choice } => {
                if let Some(choice) = choice {
                    tracing::info!(?choice, "the peer chose this session's path by hand");
                }
                paths(&store.peer, *choice).map(DiagResponse::Paths)
            }
        }
    }

    fn check(request: DiagRequest) -> Result<DiagRequest, String> {
        match request {
            DiagRequest::Logs {
                after,
                within_secs,
                contains,
                max_lines,
            } => {
                let tail = Tail {
                    after,
                    within: within_secs.map(Duration::from_secs),
                    contains,
                    max_lines,
                }
                .bounded()?;
                Ok(DiagRequest::Logs {
                    after: tail.after,
                    within_secs: tail.within.map(|w| w.as_secs()),
                    contains: tail.contains,
                    max_lines: tail.max_lines,
                })
            }
            paths @ DiagRequest::Paths { .. } => Ok(paths),
        }
    }

    fn priority(msg: &DiagMsg) -> Priority {
        msg.priority()
    }
}

pub type DiagEngine = ExchangeEngine<Diag>;

/// Builds [`DiagEngine`] on every link; a node with the module answers and
/// asks both.
pub struct DiagFactory;

impl crate::module::ModuleFactory for DiagFactory {
    fn id(&self) -> ModuleId {
        ModuleId::DIAG
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        Some(ModuleAdvert::new(ModuleId::DIAG, VERSION, ModuleRole::Both))
    }

    fn supported(&self) -> bool {
        true
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        let peer = ctx.peer.peer_id;
        Ok(Box::new(DiagEngine::new(
            peer,
            ctx.peer_name().to_string(),
            Some(std::sync::Arc::new(DiagStore { peer })),
            true,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingTx;

    #[test]
    fn a_peer_cannot_ask_for_more_than_an_answer_carries() {
        let checked = Diag::check(DiagRequest::Logs {
            after: None,
            within_secs: None,
            contains: None,
            max_lines: u32::MAX,
        })
        .unwrap();
        assert!(matches!(
            checked,
            DiagRequest::Logs {
                max_lines: cursio_protocol::diag::MAX_LINES,
                ..
            }
        ));
        assert!(Diag::check(DiagRequest::Logs {
            after: None,
            within_secs: None,
            contains: Some("x".repeat(10_000)),
            max_lines: 10,
        })
        .is_err());
    }

    #[tokio::test]
    async fn asking_for_paths_with_no_session_says_so_across_the_link() {
        let other = PeerId::from_bytes([0x71; 16]);
        let mut here = DiagEngine::new(other, "Lightmoon".into(), None, true);
        let mut there = DiagEngine::new(
            PeerId::from_bytes([0x72; 16]),
            "Nocturne".into(),
            Some(std::sync::Arc::new(DiagStore {
                peer: PeerId::from_bytes([0x72; 16]),
            })),
            false,
        );
        let ask = tokio::spawn(async move {
            hub()
                .ask(&other.to_string(), DiagRequest::Paths { choice: None })
                .await
        });
        let mut to_there = RecordingTx::default();
        let mut to_here = RecordingTx::default();
        for _ in 0..200 {
            here.pump(&mut to_there).await.unwrap();
            for msg in to_there.sent.drain(..) {
                assert!(there.handle_message(&msg));
            }
            there.pump(&mut to_here).await.unwrap();
            for msg in to_here.sent.drain(..) {
                assert!(here.handle_message(&msg));
            }
            if ask.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            ask.await.unwrap(),
            Err("no session to this machine is running".to_string())
        );
    }
}
