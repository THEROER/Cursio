//! One channel, many lanes: several kinds of traffic over one link between
//! two parties, the urgent kind never waiting behind the bulky kind, while the
//! link itself drops, moves and comes back.
//!
//! From the application down, each layer knowing only the one beneath it:
//!
//! - [`Envelope`]: what a message is — its class, its [`Delivery`], its bytes.
//! - [`Classes`]: the classes a channel is built from, highest priority first,
//!   each with a byte budget and the lane it rides.
//! - [`scheduler`]: which waiting frame goes next.
//! - [`session`]: that every reliable frame arrives once, across connections.
//! - [`seal`]: that nobody else read or wrote it (Noise transport mode).
//! - [`carrier`]: moving framed bytes — [`tcp`], [`mem`], or the caller's own.
//! - [`trust`]: who the other end is, as a verifier proved it, and whether it
//!   still is; [`noise::NoisePsk`] keys Noise with a shared secret,
//!   [`CarrierAuthenticated`] takes it from verified TLS and a token.
//!
//! This crate was extracted from Cursio's transport, which is its first user
//! and declares its own classes on top of it; `docs/channel-design.md` in the
//! Cursio repository argues the split. It names no peers and finds none:
//! discovery and pairing are the caller's, and so is what makes a peer
//! trusted — the crate only insists that it was verified.

pub mod authenticated;
pub mod carrier;
mod classes;
mod envelope;
pub mod mem;
pub mod noise;
pub mod scheduler;
pub mod seal;
pub mod session;
pub mod tcp;
#[cfg(test)]
pub(crate) mod test_util;
#[cfg(feature = "tls")]
pub mod tls;
pub mod trust;
#[cfg(feature = "websocket")]
pub mod ws;

pub use authenticated::{CarrierAuthenticated, PeerContext, Verifier};
pub use carrier::{Carrier, CarrierRx, CarrierTx, TlsInfo};
pub use classes::{Class, ClassId, Classes, Lane, LaneId, Window};
pub use envelope::{Delivery, Envelope};
pub use noise::NoisePsk;
pub use scheduler::{Carrying, PrioritizedTx};
pub use seal::{Seal, SealedRx, SealedTx};
pub use session::{
    Authorization, Path, PathKind, Paths, ResumableLink, ResumableRx, ResumeHandle, SessionEvent,
    SessionEvents, SessionOptions, Unauthorized,
};
pub use trust::{Opened, PeerIdentity, Revoked, Role, Trust, Verified};

use serde::{Deserialize, Serialize};

/// Names one live session between two parties, the same on both ends:
/// derived from the handshake that opened it. Not a secret — a connection
/// that offers it must still complete the handshake with the pair's key
/// before it can carry anything.
pub type SessionTag = [u8; 16];

/// Errors surfaced by the channel.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection closed by peer")]
    Closed,
    #[error("noise error: {0}")]
    Noise(String),
    #[error("plaintext message too large for a single Noise frame: {0} bytes")]
    MessageTooLarge(usize),
    #[error("frame length {0} exceeds maximum {max}", max = carrier::MAX_FRAME_LEN)]
    FrameTooLarge(usize),
    /// An [`Envelope`] could not be encoded or decoded.
    #[error("{0}")]
    Envelope(String),
    #[error("{0}")]
    Other(String),
}

/// What one class has waiting, and what it has already got out.
///
/// A bulk producer has no other way to tell whether the link is keeping up
/// with it: [`PrioritizedTx::send`] only *queues*, so it succeeds just as
/// quickly whether the carrier is idle or an hour behind. Reading the depth
/// back turns that blind write into a control loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueDepth {
    /// Bytes queued in this class and not yet written to the carrier.
    pub queued_bytes: usize,
    /// Bytes of this class written to the carrier since the link came up.
    ///
    /// Monotonic, so the difference between two readings over a known interval
    /// is the rate this class is actually draining at.
    pub sent_bytes: u64,
}

/// How long one class's frames waited to reach the carrier, over the most
/// recent complete measurement window.
///
/// Measured from `send` queuing a frame to the carrier accepting its last
/// byte, so it covers both waiting behind other classes in our queue and
/// waiting for the kernel to take the frame. For the most urgent class this
/// is the lag the scheduler adds — the number to read when input feels slow.
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

/// Which of a session's paths should carry it.
///
/// `Network` means the lanes' own carriers and `Bluetooth` the standby
/// carrier that can take every lane, after Cursio's, whose reports first
/// carried these names on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PathChoice {
    /// The lanes' own carriers whenever they are there; the standby when not.
    #[default]
    Auto,
    /// The lanes' own carriers even while they are failing: no move to the
    /// standby while one exists, which is how they alone are tested.
    Network,
    /// The standby whenever there is one, the lanes' carriers standing by.
    Bluetooth,
}

/// One end's view of a session's paths, for a person diagnosing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathReport {
    /// A lane carrier is among the paths.
    pub network: bool,
    /// A standby carrier is among the paths.
    pub bluetooth: bool,
    /// What carries the interactive lane from this end now: "network",
    /// "direct", or the standby's name in lower case; `None` while the
    /// session waits for a path.
    pub carrying: Option<String>,
    pub choice: PathChoice,
}
