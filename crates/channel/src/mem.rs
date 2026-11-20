//! An in-memory carrier pair: two cross-wired ends with no sockets, so the
//! handshake, the seal, the scheduler and the session can be exercised end
//! to end in a test.
//!
//! It has no backpressure — a send never waits — so it shows that frames
//! arrive intact and in order, not that urgent traffic overtakes a backlog.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{Carrier, CarrierRx, CarrierTx, ChannelError};

/// One end of an in-memory link.
pub struct MemCarrier {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl MemCarrier {
    pub fn pair() -> (Self, Self) {
        let (tx_a, rx_b) = mpsc::unbounded_channel();
        let (tx_b, rx_a) = mpsc::unbounded_channel();
        (
            MemCarrier { tx: tx_a, rx: rx_a },
            MemCarrier { tx: tx_b, rx: rx_b },
        )
    }
}

#[async_trait]
impl Carrier for MemCarrier {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        self.tx.send(frame).map_err(|_| ChannelError::Closed)
    }

    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        self.rx.recv().await.ok_or(ChannelError::Closed)
    }

    fn kind(&self) -> &'static str {
        "mem"
    }

    fn split(self: Box<Self>) -> (Box<dyn CarrierTx>, Box<dyn CarrierRx>) {
        (Box::new(MemTx(self.tx)), Box::new(MemRx(self.rx)))
    }
}

struct MemTx(mpsc::UnboundedSender<Vec<u8>>);

#[async_trait]
impl CarrierTx for MemTx {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        self.0.send(frame).map_err(|_| ChannelError::Closed)
    }
}

struct MemRx(mpsc::UnboundedReceiver<Vec<u8>>);

#[async_trait]
impl CarrierRx for MemRx {
    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        self.0.recv().await.ok_or(ChannelError::Closed)
    }
}
