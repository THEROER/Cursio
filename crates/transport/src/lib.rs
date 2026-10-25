//! Cursio transport layer.
//!
//! Responsibilities:
//! - [`discovery`]: find/announce peers on the LAN (mDNS).
//! - [`pairing`]: password-authenticated key agreement (SPAKE2) + a confirmed
//!   shared secret, plus persistence of trusted peers.
//! - [`tcp`]: the concrete Wi-Fi/LAN transport (framed TCP over tokio).
//! - [`stream`]: the same framing over any byte stream — the Bluetooth link.
//! - [`Transport`] / [`select_transport`]: the abstraction and auto-selection
//!   that let a future channel slot in without touching callers.
//!
//! TCP and a Bluetooth byte stream carry the same messages: everything above
//! [`Transport`] — the handshake, Noise, modules, priorities — is written
//! against the trait, so a link is the same link whichever one carries it.

use async_trait::async_trait;
use cursio_protocol::Message;

mod congestion;
pub mod crypto;
pub mod dead_peer;
pub mod discovery;
pub mod lane;
pub mod pairing;
pub mod priority;
pub mod resume;
mod send_backlog;
pub mod stream;
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

/// What one priority class has waiting, and what it has already got out.
///
/// A bulk producer (the files module) has no other way to tell whether the link
/// is keeping up with it: [`TransportTx::send`] on the priority multiplexer only
/// *queues*, so it succeeds just as quickly whether the socket is idle or an
/// hour behind. Reading the depth back turns that blind write into a control
/// loop — see `cursio_core::files_session`, which sizes its in-flight window
/// from the delay these two numbers imply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueDepth {
    /// Bytes queued in this class and not yet written to the socket.
    pub queued_bytes: usize,
    /// Bytes of this class written to the socket since the link came up.
    ///
    /// Monotonic, so the difference between two readings over a known interval
    /// is the rate this class is actually draining at.
    pub sent_bytes: u64,
}

/// How long one priority class's frames waited to reach the socket, over the
/// most recent complete measurement window.
///
/// Measured from `send` queuing a frame to the socket accepting its last byte,
/// so it covers both waiting behind other classes in our queue and waiting for
/// the kernel to take the frame. For input this is the lag the scheduler adds
/// to the pointer — the number to read when a mouse feels slow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueDelay {
    /// Frames of this class sent in the window; zero means the other fields
    /// say nothing.
    pub frames: u32,
    /// Median wait, to within a quarter octave.
    pub p50: std::time::Duration,
    /// 99th-percentile wait, to within a quarter octave.
    pub p99: std::time::Duration,
    /// Longest single wait, exact.
    pub max: std::time::Duration,
}

/// The send half of a split [`Transport`].
#[async_trait]
pub trait TransportTx: Send {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError>;

    /// How long `priority`'s frames have recently waited to reach the socket.
    ///
    /// Zero for a sender that writes inside `send`, for the same reason as
    /// [`Self::queue_depth`]: nothing waits in a queue it does not have.
    fn queue_delay(&self, _priority: cursio_protocol::Priority) -> QueueDelay {
        QueueDelay::default()
    }

    /// Age of the oldest queued or currently sending frame, for live control.
    fn queue_age(&self, _priority: cursio_protocol::Priority) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    /// How much of `priority` is queued here and how much has left.
    ///
    /// Zero for a sender that writes inside `send`: there is no queue to be
    /// behind on, so a caller reading this sees a link that is never backed up
    /// — which is the truth for a synchronous sender. Only the priority
    /// multiplexer holds frames, and only it reports real numbers.
    fn queue_depth(&self, _priority: cursio_protocol::Priority) -> QueueDepth {
        QueueDepth::default()
    }

    /// Flush any buffered outbound frames and wait until they have hit the
    /// wire.
    ///
    /// Synchronous senders write inside `send`, so the default is a no-op. A
    /// buffered sender (the priority multiplexer, whose `send` only *queues*)
    /// overrides this so a final message — e.g. the session's `Bye` — actually
    /// leaves before the session drops the tx. Best-effort: the link may already
    /// be gone.
    async fn flush(&mut self) {}

    /// Another handle onto this sender's queue, for a sender that has one.
    ///
    /// Session modules run on their own threads and send without going
    /// through the session. `None` — the default, for a sender that writes
    /// inside `send` — means the caller must share this one handle instead.
    fn clone_sender(&self) -> Option<Box<dyn TransportTx>> {
        None
    }

    /// Whether the link has lost its connection and is holding the session
    /// open for a new one to resume it. A session hears nothing from its peer
    /// meanwhile, which is not the peer being gone: it must not end itself
    /// for that silence while this is true. Always `false` for a link over a
    /// single connection, which cannot be resumed.
    fn holding(&self) -> bool {
        false
    }
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
    /// TCP/Wi-Fi is our only channel today, but not every address reaches a
    /// peer the same way: a VPN address is routed off the machine, through a
    /// relay and back, which costs tens of milliseconds that a shared cursor
    /// and a 60 fps screen both feel. Discovery already refuses to *announce*
    /// over such an interface; this is the other half, because an address can
    /// also arrive from the trust store, where it was persisted by a session
    /// that ran somewhere else.
    ///
    /// Ranked rather than rejected: away from the LAN a VPN is the only way
    /// through, and a peer that is merely slow beats a peer that is
    /// unreachable. Future channels slot into this ranking rather than into
    /// caller code.
    fn preference_rank(&self) -> u8 {
        match self {
            TransportCandidate::Tcp(addr) if discovery::is_virtual_addr(&addr.ip()) => 1,
            TransportCandidate::Tcp(_) => 0,
        }
    }

    /// Whether this candidate goes through a VPN or other virtual interface.
    fn is_virtual(&self) -> bool {
        self.preference_rank() > 0
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
        // Said before connecting, not after: once it works nobody looks at the
        // log, and "everything is oddly laggy today" is exactly the symptom
        // this explains.
        if candidate.is_virtual() {
            tracing::warn!(
                ?candidate,
                "no LAN address for this peer; going through a VPN or virtual interface, which \
                 adds latency to input and video"
            );
        }
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

#[cfg(test)]
mod candidate_tests {
    use super::*;
    use std::net::SocketAddr;

    fn tcp(addr: &str) -> TransportCandidate {
        TransportCandidate::Tcp(addr.parse::<SocketAddr>().unwrap())
    }

    /// The whole point: with both addresses known, the LAN one is dialled and
    /// the peer is not reached through a relay it does not need.
    #[test]
    fn a_lan_address_is_dialled_before_a_vpn_one() {
        let mut candidates = [tcp("100.101.102.103:7777"), tcp("192.168.1.35:7777")];
        candidates.sort_by_key(|c| c.preference_rank());
        assert!(!candidates[0].is_virtual());
        assert!(candidates[1].is_virtual());
    }

    /// Away from the LAN a VPN is the only way through, so it must still be
    /// tried rather than filtered out.
    #[test]
    fn a_vpn_address_is_still_a_candidate_on_its_own() {
        let candidate = tcp("100.101.102.103:7777");
        assert!(candidate.is_virtual());
        assert_eq!(candidate.preference_rank(), 1);
    }

    /// A home LAN commonly lives in 192.168/16 and 10/8; treating either as
    /// virtual would demote the address we actually want.
    #[test]
    fn ordinary_home_ranges_are_not_treated_as_virtual() {
        for addr in ["192.168.0.5:7777", "10.0.0.5:7777", "172.15.0.5:7777"] {
            assert!(!tcp(addr).is_virtual(), "{addr} should rank as LAN");
        }
    }
}
