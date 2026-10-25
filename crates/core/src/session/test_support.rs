//! What the session tests share: peer ids, motion events, and a transport
//! that keeps what was sent.

use cursio_protocol::{Message, PeerId};
use cursio_transport::{TransportError, TransportTx};

pub(super) fn id(byte: u8) -> PeerId {
    PeerId::from_bytes([byte; 16])
}

pub(super) fn motion(dx: i32, dy: i32) -> cursio_protocol::InputEvent {
    cursio_protocol::InputEvent::PointerMotion { dx, dy }
}

pub(super) struct RecordingSender(pub(super) Vec<Message>);

#[async_trait::async_trait]
impl TransportTx for RecordingSender {
    async fn send(&mut self, message: &Message) -> Result<(), TransportError> {
        self.0.push(message.clone());
        Ok(())
    }
}
