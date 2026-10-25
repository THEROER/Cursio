//! Telling the module's messages apart: a request or a reply, which
//! request it answers, and putting one on the link.

use crate::files_session::health::Health;
use crate::files_session::module::MAX_MESSAGES_PER_PUMP;
use cursio_protocol::{FileReqId, FilesMsg, Message, ModuleId};
use cursio_transport::{TransportError, TransportTx};
use std::time::Instant;

pub(super) async fn send(tx: &mut dyn TransportTx, msg: FilesMsg) -> Result<(), TransportError> {
    // Per message, not per module: a `stat` someone is blocked on must not
    // queue behind a copy's chunks. See [`FilesMsg::priority`].
    let framed = Message::module(ModuleId::FILES, msg.priority(), &msg)?;
    tx.send(&framed).await
}

/// Move messages the workers have prepared onto the link, until the queue runs
/// dry or this pump's slice is spent.
///
/// Nothing here is subject to the pacer's window: these are answers the peer is
/// blocked on, and requests already counted against the window when they were
/// accepted. Holding them back would not reduce what is in flight, only make
/// the peer wait out its timeout for something we had ready.
pub(super) async fn drain(
    queue: &mut tokio::sync::mpsc::UnboundedReceiver<FilesMsg>,
    tx: &mut dyn TransportTx,
    deadline: Instant,
    // Written to as each message leaves, so a write's time here can be told
    // apart from its time on the link. `None` for the reply queue, whose
    // messages answer the peer rather than waiting on it.
    mut timing: Option<&mut Health>,
    // Bytes this pump may hand to the link, or `None` for a queue that is not
    // paced. See [`Pacer::pump_budget`] for why a window alone is not enough.
    budget: Option<usize>,
) -> Result<(), TransportError> {
    let mut spent = 0usize;
    for _ in 0..MAX_MESSAGES_PER_PUMP {
        if Instant::now() >= deadline {
            break;
        }
        if budget.is_some_and(|b| spent >= b) {
            break;
        }
        let Ok(msg) = queue.try_recv() else { break };
        spent += msg.wire_cost();
        if let (Some(health), FilesMsg::Write { req, .. }) = (timing.as_deref_mut(), &msg) {
            health.reached_wire(*req, Instant::now());
        }
        send(tx, msg).await?;
    }
    Ok(())
}

/// Whether a message is something to act on, rather than an answer to something
/// we asked.
pub(super) fn is_request(msg: &FilesMsg) -> bool {
    matches!(
        msg,
        FilesMsg::ReadDir { .. }
            | FilesMsg::Stat { .. }
            | FilesMsg::ReadLink { .. }
            | FilesMsg::Open { .. }
            | FilesMsg::Read { .. }
            | FilesMsg::Write { .. }
            | FilesMsg::Truncate { .. }
            | FilesMsg::MkDir { .. }
            | FilesMsg::Remove { .. }
            | FilesMsg::Rename { .. }
            | FilesMsg::Close { .. }
            | FilesMsg::ListVolumes { .. }
    )
}

/// A one-word name for what a request asks, for counting by kind.
pub(super) fn request_kind(msg: &FilesMsg) -> &'static str {
    match msg {
        FilesMsg::ReadDir { .. } => "readdir",
        FilesMsg::Stat { .. } => "stat",
        FilesMsg::ReadLink { .. } => "readlink",
        FilesMsg::Open { .. } => "open",
        FilesMsg::Read { .. } => "read",
        FilesMsg::Write { .. } => "write",
        FilesMsg::Truncate { .. } => "set_size",
        FilesMsg::MkDir { .. } => "mkdir",
        FilesMsg::Remove { .. } => "remove",
        FilesMsg::Rename { .. } => "rename",
        FilesMsg::Close { .. } => "close",
        FilesMsg::ListVolumes { .. } => "volumes",
        _ => "other",
    }
}

/// The id a request from the peer expects its answer under.
///
/// `None` for the two that are never answered by id: a `Close` is deliberately
/// unanswered, and `ListVolumes` is answered with the announcement itself.
pub(super) fn request_req(msg: &FilesMsg) -> Option<FileReqId> {
    match msg {
        FilesMsg::ReadDir { req, .. }
        | FilesMsg::Stat { req, .. }
        | FilesMsg::ReadLink { req, .. }
        | FilesMsg::Open { req, .. }
        | FilesMsg::Read { req, .. }
        | FilesMsg::Write { req, .. }
        | FilesMsg::Truncate { req, .. }
        | FilesMsg::MkDir { req, .. }
        | FilesMsg::Remove { req, .. }
        | FilesMsg::Rename { req, .. } => Some(*req),
        _ => None,
    }
}

/// The request a reply answers, for timing its round trip.
///
/// `None` for the two messages that answer nothing in particular: `Volumes` is
/// announced unprompted as well as sent in reply, so it has no single request
/// to time, and anything that is not a reply at all is not this function's
/// business.
pub(super) fn reply_req(msg: &FilesMsg) -> Option<FileReqId> {
    match msg {
        FilesMsg::DirEntries { req, .. }
        | FilesMsg::StatResult { req, .. }
        | FilesMsg::LinkTarget { req, .. }
        | FilesMsg::Opened { req, .. }
        | FilesMsg::Data { req, .. }
        | FilesMsg::Wrote { req, .. }
        | FilesMsg::Ok { req }
        | FilesMsg::Failed { req, .. } => Some(*req),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::FileError;

    /// Requests are served; replies are routed. Confusing the two would either
    /// hang the mount or run a reply against our own disk.
    #[test]
    fn requests_and_replies_are_told_apart() {
        assert!(is_request(&FilesMsg::Stat {
            req: 1,
            volume: "C:".into(),
            path: "a".into()
        }));
        assert!(is_request(&FilesMsg::Close { handle: 1 }));
        assert!(!is_request(&FilesMsg::Ok { req: 1 }));
        assert!(!is_request(&FilesMsg::Volumes(Vec::new())));
        assert!(!is_request(&FilesMsg::Failed {
            req: 1,
            error: FileError::NotFound
        }));
    }

    /// Asking for the volume list must not look like announcing an empty one.
    ///
    /// These were the same message once: a request encoded as
    /// `Volumes(vec![])`. The peer's request then arrived here as "I have no
    /// volumes", blanked the list we had just been given, and the mount was torn
    /// down a moment later — with `count=1` and `count=0` logged back to back.
    #[test]
    fn asking_for_volumes_is_not_announcing_none() {
        assert!(
            is_request(&FilesMsg::ListVolumes { req: 1 }),
            "a request must be served, not routed as a reply"
        );
        assert!(
            !is_request(&FilesMsg::Volumes(Vec::new())),
            "an announcement is a reply, whatever it contains"
        );
    }
}
