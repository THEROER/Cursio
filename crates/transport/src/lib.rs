//! Cursio transport layer.
//!
//! Responsibilities:
//! - [`discovery`]: find/announce peers on the LAN (mDNS).
//! - [`pairing`]: password-authenticated key agreement (SPAKE2) + a confirmed
//!   shared secret, plus persistence of trusted peers.
//! - [`tcp`]: the concrete Wi-Fi/LAN transport (framed TCP over tokio).
//! - [`Transport`] / [`select_transport`]: the abstraction and auto-selection
//!   that let a future channel slot in without touching callers.
//!
//! Only one transport (TCP) exists today. The trait and selector exist now so
//! the session/core layer is written against the abstraction from day one.

use async_trait::async_trait;
use cursio_protocol::Message;

pub mod crypto;
pub mod discovery;
pub mod pairing;
pub mod priority;
pub mod tcp;
#[cfg(test)]
pub(crate) mod test_util;

pub use crypto::EncryptedTransport;
pub use priority::PrioritizedTx;
pub use tcp::TcpTransport;

/// Errors surfaced by transports.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] cursio_protocol::ProtocolError),
    #[error("framing error: {0}")]
    Frame(#[from] cursio_protocol::FrameError),
    #[error("connection closed by peer")]
    Closed,
    #[error("{0}")]
    Other(String),
}

/// A bidirectional, message-oriented link to a single peer.
///
/// Implementors handle framing internally: callers send and receive whole
/// [`Message`]s and never see bytes. This is deliberately channel-agnostic so
/// TCP today (and, hypothetically, another transport later) present the same
/// surface to the session layer.
#[async_trait]
pub trait Transport: Send {
    /// Send one message to the peer.
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError>;

    /// Receive the next message from the peer.
    ///
    /// Resolves with [`TransportError::Closed`] when the peer hangs up cleanly.
    async fn recv(&mut self) -> Result<Message, TransportError>;

    /// A short, human-readable label for logs/UI, e.g. `"tcp"`.
    fn kind(&self) -> &'static str;

    /// Split into independent send and receive halves.
    ///
    /// The session layer runs receive in its own task so it is **never
    /// cancelled** — cancelling a `recv` mid-await can corrupt stateful
    /// transports (e.g. desynchronise the Noise cipher), silently dropping
    /// later messages. Splitting removes that hazard by construction.
    fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>);

    /// Split with a **priority-multiplexed** send half, if this transport
    /// supports it.
    ///
    /// The encrypted transport overrides this to return a
    /// [`PrioritizedTx`](crate::priority::PrioritizedTx) so latency-critical
    /// traffic (input) is never stuck behind a bulk payload. The default just
    /// delegates to [`split`](Self::split): plain transports carry no scheduled
    /// application traffic of their own, so they need no prioritization.
    fn split_prioritized(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        self.split()
    }
}

/// The send half of a split [`Transport`].
#[async_trait]
pub trait TransportTx: Send {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError>;

    /// Flush any buffered outbound frames and wait until they have hit the
    /// wire.
    ///
    /// Synchronous senders write inside `send`, so the default is a no-op. A
    /// buffered sender (the priority multiplexer, whose `send` only *queues*)
    /// overrides this so a final message — e.g. the session's `Bye` — actually
    /// leaves before the session drops the tx. Best-effort: the link may already
    /// be gone.
    async fn flush(&mut self) {}
}

/// The receive half of a split [`Transport`].
#[async_trait]
pub trait TransportRx: Send {
    async fn recv(&mut self) -> Result<Message, TransportError>;
}

/// Describes a channel a peer is reachable over, ranked so we can pick the best.
///
/// Today the only variant is [`TransportCandidate::Tcp`]; the enum exists so
/// [`select_transport`] already has a shape to grow into.
#[derive(Debug, Clone)]
pub enum TransportCandidate {
    /// Wi-Fi/LAN over TCP at the given socket address.
    Tcp(std::net::SocketAddr),
}

impl TransportCandidate {
    /// Lower is better. Used by [`select_transport`] to order candidates.
    ///
    /// TCP/Wi-Fi is our only (and preferred) channel today. Future channels
    /// slot into this ranking rather than into caller code.
    fn preference_rank(&self) -> u8 {
        match self {
            TransportCandidate::Tcp(_) => 0,
        }
    }
}

/// Pick the best available transport candidate and connect to it.
///
/// "Auto-select the best channel" is a first-class concept even with one
/// candidate: callers hand us everything a peer advertises and we choose,
/// rather than hard-coding TCP at the call site. When more channels exist,
/// only [`TransportCandidate::preference_rank`] and this function change.
pub async fn select_transport(
    mut candidates: Vec<TransportCandidate>,
) -> Result<Box<dyn Transport>, TransportError> {
    candidates.sort_by_key(|c| c.preference_rank());

    let mut last_err: Option<TransportError> = None;
    for candidate in candidates {
        match connect_candidate(&candidate).await {
            Ok(t) => return Ok(t),
            Err(e) => {
                tracing::warn!(?candidate, error = %e, "candidate failed, trying next");
                last_err = Some(e);
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| TransportError::Other("no transport candidates available".into())))
}

async fn connect_candidate(
    candidate: &TransportCandidate,
) -> Result<Box<dyn Transport>, TransportError> {
    match candidate {
        TransportCandidate::Tcp(addr) => {
            let t = TcpTransport::connect(*addr).await?;
            Ok(Box::new(t))
        }
    }
}
