//! What the session tests share: peer ids and motion events.

use cursio_protocol::PeerId;

pub(super) fn id(byte: u8) -> PeerId {
    PeerId::from_bytes([byte; 16])
}

pub(super) fn motion(dx: i32, dy: i32) -> cursio_protocol::InputEvent {
    cursio_protocol::InputEvent::PointerMotion { dx, dy }
}
