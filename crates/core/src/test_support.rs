//! What the crate's tests share: a transport that keeps what was sent.

use cursio_protocol::{Message, ModuleId, Priority};
use cursio_transport::{QueueDepth, TransportError, TransportTx};

/// Stands in for the link, so a test asserts on what a module or the session
/// put on the wire rather than on its internals.
#[derive(Default)]
pub(crate) struct RecordingTx {
    pub(crate) sent: Vec<Message>,
    /// The backlog every queue reports; a module asks only of its own.
    pub(crate) depth: QueueDepth,
}

impl RecordingTx {
    /// What was sent for `module`, out of its envelopes — the way the peer's
    /// module sees it.
    pub(crate) fn decoded<T: for<'de> serde::Deserialize<'de>>(&self, module: ModuleId) -> Vec<T> {
        self.sent
            .iter()
            .filter_map(|m| m.decode_module::<T>(module).unwrap())
            .collect()
    }
}

#[async_trait::async_trait]
impl TransportTx for RecordingTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        self.sent.push(msg.clone());
        Ok(())
    }

    fn queue_depth(&self, _priority: Priority) -> QueueDepth {
        self.depth
    }
}
