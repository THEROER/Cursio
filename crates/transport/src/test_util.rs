//! Shared test scaffolding: an in-memory [`Transport`] pair.
//!
//! Two cross-wired [`MemTransport`]s form a bidirectional link with no sockets,
//! so tests can exercise the encrypted transport, chunking, and priority
//! scheduling end to end.

use async_trait::async_trait;
use cursio_protocol::Message;
use tokio::sync::mpsc;

use crate::{Transport, TransportError, TransportRx, TransportTx};

pub(crate) struct MemTransport {
    tx: mpsc::UnboundedSender<Message>,
    rx: mpsc::UnboundedReceiver<Message>,
}

impl MemTransport {
    pub(crate) fn pair() -> (Self, Self) {
        let (tx_a, rx_b) = mpsc::unbounded_channel();
        let (tx_b, rx_a) = mpsc::unbounded_channel();
        (
            MemTransport { tx: tx_a, rx: rx_a },
            MemTransport { tx: tx_b, rx: rx_b },
        )
    }
}

#[async_trait]
impl Transport for MemTransport {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        self.tx
            .send(msg.clone())
            .map_err(|_| TransportError::Closed)
    }
    async fn recv(&mut self) -> Result<Message, TransportError> {
        self.rx.recv().await.ok_or(TransportError::Closed)
    }
    fn kind(&self) -> &'static str {
        "mem"
    }
    fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        (
            Box::new(MemTx { tx: self.tx }),
            Box::new(MemRx { rx: self.rx }),
        )
    }
}

pub(crate) struct MemTx {
    tx: mpsc::UnboundedSender<Message>,
}
#[async_trait]
impl TransportTx for MemTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        self.tx
            .send(msg.clone())
            .map_err(|_| TransportError::Closed)
    }
}

pub(crate) struct MemRx {
    rx: mpsc::UnboundedReceiver<Message>,
}
#[async_trait]
impl TransportRx for MemRx {
    async fn recv(&mut self) -> Result<Message, TransportError> {
        self.rx.recv().await.ok_or(TransportError::Closed)
    }
}
