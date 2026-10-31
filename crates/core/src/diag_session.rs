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
//!
//! A node also reports on itself ([`own_report`], `cursio report`) and takes
//! a restart or an update from a peer ([`maintain`], `cursio restart`): a
//! machine on a hotspot, with no shell anyone can reach, was otherwise left
//! running an old build until a quiet moment came or a person walked over.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use cursio_protocol::diag::{
    DiagMsg, DiagRequest, DiagResponse, Maintenance, NodeReport, PathChoice, PathReport,
};
use cursio_protocol::{ModuleAdvert, ModuleId, ModuleRole, PeerId, Priority};
use cursio_transport::resume::ResumeHandle;

use crate::exchange_session::{Exchange, ExchangeEngine};
use crate::log_tail::Tail;
use crate::module::SessionModule;
use crate::request_hub::{HubKind, RequestHub};

/// Version of [`DiagMsg`]; peers run the module only on an exact match.
pub const VERSION: u16 = 3;

/// How many of the machine's largest processes a report lists.
const TOP_PROCESSES: usize = 8;

/// Longest a report reaches back.
const MAX_REPORT_SPAN: Duration = Duration::from_secs(14 * 24 * 3600);

/// How long a node waits before acting on a maintenance request, so its
/// answer leaves on the link the request is about to end.
const MAINTAIN_AFTER: Duration = Duration::from_millis(500);

/// What performs a [`Maintenance`] on this node: set by the node's control
/// state, which owns the restart signal and the update agent.
pub type Maintainer = Arc<dyn Fn(Maintenance) -> Result<(), String> + Send + Sync>;

fn maintainer() -> std::sync::MutexGuard<'static, Option<Maintainer>> {
    static MAINTAINER: OnceLock<Mutex<Option<Maintainer>>> = OnceLock::new();
    MAINTAINER
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Let peers restart or update this node through `act`.
pub fn set_maintainer(act: Maintainer) {
    *maintainer() = Some(act);
}

/// Do `what` to this node, shortly: the answer goes out first.
pub fn maintain(what: Maintenance) -> Result<(), String> {
    let act = maintainer()
        .clone()
        .ok_or("this node takes no maintenance from its peers")?;
    std::thread::Builder::new()
        .name("cursio-maintain".into())
        .spawn(move || {
            std::thread::sleep(MAINTAIN_AFTER);
            if let Err(error) = act(what) {
                tracing::warn!(?what, %error, "a peer's maintenance request could not be done");
            }
        })
        .map_err(|e| format!("cannot start the maintenance: {e}"))?;
    Ok(())
}

/// How this node has fared over the last `since`: counted from its log, with
/// its memory, the machine's largest processes and its problems now.
pub fn own_report(since: Duration) -> Result<NodeReport, String> {
    let name = cursio_log::log_file_name().ok_or("this node keeps no log file")?;
    let mut report = crate::log_report::build(
        &cursio_log::log_dir(),
        name,
        since.min(MAX_REPORT_SPAN),
        std::time::SystemTime::now(),
    )?;
    report.memory = crate::proc_memory::own();
    report.top = crate::proc_memory::top(TOP_PROCESSES);
    report.problems = crate::diagnostics::snapshot()
        .into_iter()
        .map(|p| p.summary)
        .collect();
    Ok(report)
}

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
    let name = match &tail.log {
        Some(log) => format!("{log}.log"),
        None => cursio_log::log_file_name()
            .ok_or("this node keeps no log file")?
            .to_string(),
    };
    crate::log_tail::read(
        &cursio_log::log_dir(),
        &name,
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
                log,
            } => own_log(Tail {
                after: after.clone(),
                within: within_secs.map(Duration::from_secs),
                contains: contains.clone(),
                max_lines: *max_lines,
                log: log.clone(),
            }),
            DiagRequest::Paths { choice } => {
                if let Some(choice) = choice {
                    tracing::info!(?choice, "the peer chose this session's path by hand");
                }
                paths(&store.peer, *choice).map(DiagResponse::Paths)
            }
            DiagRequest::Report { since_secs } => own_report(Duration::from_secs(*since_secs))
                .map(|r| DiagResponse::Report(Box::new(r))),
            DiagRequest::Maintain(what) => {
                tracing::info!(?what, "a peer asked this node for maintenance");
                maintain(*what).map(|()| DiagResponse::Maintaining(*what))
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
                log,
            } => {
                let tail = Tail {
                    after,
                    within: within_secs.map(Duration::from_secs),
                    contains,
                    max_lines,
                    log,
                }
                .bounded()?;
                Ok(DiagRequest::Logs {
                    after: tail.after,
                    within_secs: tail.within.map(|w| w.as_secs()),
                    contains: tail.contains,
                    max_lines: tail.max_lines,
                    log: tail.log,
                })
            }
            DiagRequest::Report { since_secs } => Ok(DiagRequest::Report {
                since_secs: since_secs.min(MAX_REPORT_SPAN.as_secs()),
            }),
            other @ (DiagRequest::Paths { .. } | DiagRequest::Maintain(_)) => Ok(other),
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
            log: None,
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
            log: None,
        })
        .is_err());
        assert!(Diag::check(DiagRequest::Logs {
            after: None,
            within_secs: None,
            contains: None,
            max_lines: 10,
            log: Some("../../.ssh/id_ed25519".into()),
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
