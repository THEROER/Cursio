//! What the module's tests share: an engine on a recording link, and a
//! request to put through it.

use crate::files_session::client::PendingMap;
use crate::files_session::health::Health;
use crate::files_session::pacer::Pacer;
use crate::files_session::FilesEngine;
use cursio_files::bridge::{Op, PeerLink};
use cursio_files::{FileServer, Workers};
use cursio_protocol::{FilesMsg, Message, ModuleId};
use cursio_transport::{QueueDepth, TransportError, TransportTx};
use std::collections::HashMap;
use std::time::Instant;

/// Collects what the module sends, standing in for the transport, and
/// reports whatever backlog the test wants the link to have.
#[derive(Default)]
pub(super) struct RecordingTx {
    pub(super) sent: Vec<FilesMsg>,
    depth: QueueDepth,
}
#[async_trait::async_trait]
impl TransportTx for RecordingTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        if let Ok(Some(msg)) = msg.decode_module::<FilesMsg>(ModuleId::FILES) {
            self.sent.push(msg);
        }
        Ok(())
    }
    fn queue_depth(&self, _priority: cursio_protocol::Priority) -> QueueDepth {
        self.depth
    }
}

/// An engine with no mount attached: the client half is driven directly
/// through its operation channel, which is all these tests need and avoids
/// mounting a filesystem on the machine running them.
pub(super) fn engine() -> (FilesEngine, tokio::sync::mpsc::UnboundedSender<Op>) {
    let (ops_tx, ops_rx) = tokio::sync::mpsc::unbounded_channel();
    let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
    let (outbox_tx, outbox_rx) = tokio::sync::mpsc::unbounded_channel();
    let (rescan_tx, rescan_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = FileServer::detect();
    let engine = FilesEngine {
        peer_name: "peer".into(),
        local_volumes: server.volumes(),
        server: std::sync::Arc::new(std::sync::Mutex::new(server)),
        ops_rx,
        done_tx,
        done_rx,
        outbox_tx,
        outbox_rx,
        compressing: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        deferred_closes: Vec::new(),
        workers: Workers::with_threads("test-files", 2),
        codec: Workers::with_threads("test-files-codec", 1),
        pace: Pacer::new(),
        pending: PendingMap::default(),
        next_req: 1,
        health: Health::default(),
        served: HashMap::new(),
        slow_served: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        in_progress: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
        volume_mounts: HashMap::new(),
        mount_link: PeerLink::new(tokio::sync::mpsc::unbounded_channel().0),
        #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
        mount_retry_at: None,
        announce_volumes: false,
        peer_volumes: Vec::new(),
        last_rescan: Instant::now(),
        rescanning: false,
        rescan_tx,
        rescan_rx,
        share: cursio_files::SharePolicy::everything(),
    };
    (engine, ops_tx)
}

/// A `Stat` the test can queue without caring about the answer.
pub(super) fn a_request() -> Op {
    let (reply, _rx) = std::sync::mpsc::channel();
    Op::Stat {
        volume: "C:".into(),
        path: "a.txt".into(),
        reply,
    }
}
