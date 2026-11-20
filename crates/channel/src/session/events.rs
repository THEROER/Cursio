//! What a session tells the application besides its messages: that it ended
//! and why, and what of its own sending never got through.
//!
//! The link's core holds encoded bytes and knows no envelope type, so events
//! cross from it as bytes and are decoded here, on the application's side,
//! with the envelope's own [`Envelope::decode`].

use std::marker::PhantomData;

use tokio::sync::mpsc;

use crate::Envelope;

pub(super) enum RawEvent {
    Ended {
        reason: String,
        unacknowledged: Vec<Vec<u8>>,
    },
}

/// One thing a session reports to the application.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent<E> {
    /// The session is over and cannot be resumed.
    Ended {
        reason: String,
        /// Every reliable message this end sent that the peer never
        /// acknowledged, in the order it was sent, for the application to
        /// keep and send again in a session of its own making. Empty unless
        /// the session was asked to hand them back.
        unacknowledged: Vec<E>,
    },
}

/// The application's end of a session's reports; see [`SessionEvent`].
pub struct SessionEvents<E> {
    pub(super) rx: mpsc::UnboundedReceiver<RawEvent>,
    pub(super) envelope: PhantomData<fn() -> E>,
}

impl<E: Envelope> SessionEvents<E> {
    /// The next report; `None` once the session is gone and every report
    /// has been read.
    pub async fn recv(&mut self) -> Option<SessionEvent<E>> {
        let raw = self.rx.recv().await?;
        Some(match raw {
            RawEvent::Ended {
                reason,
                unacknowledged,
            } => SessionEvent::Ended {
                reason,
                unacknowledged: decode_all(unacknowledged),
            },
        })
    }
}

fn decode_all<E: Envelope>(bodies: Vec<Vec<u8>>) -> Vec<E> {
    bodies
        .iter()
        .filter_map(|body| match E::decode(body) {
            Ok(message) => Some(message),
            Err(error) => {
                tracing::warn!(%error, "a message the session handed back could not be decoded; it is lost");
                None
            }
        })
        .collect()
}
