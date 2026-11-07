//! Measuring the link between two machines: how fast it carries a file each
//! way, and how far a pointer's round trip stretches while it does.
//!
//! The figures a person asks for ("is 9 MB/s much or little?") cannot be read
//! off a transfer of Cursio's own: a mount's speed is the link's speed less
//! whatever the filesystem waits on. So the test is traffic of its own,
//! shaped as a file is — requested pieces on the bulk lane, paced by the same
//! congestion control — with no disk on either end.
//!
//! What a speed test on its own misses, and what Orb-style tools measure
//! besides, is *responsiveness under load*: a link that is fast and holds a
//! pointer up for half a second while it is fast is a bad link for this
//! program. Pings ride the input class throughout, idle first and then under
//! each direction's load, and the answer gives their round trips for each.
//!
//! ## On demand, and rarely by itself
//!
//! A test spends the link it measures, so it runs when asked (`cursio link
//! test`) and otherwise only at a quiet moment, rarely; see
//! `docs/transport-design.md` "Measuring a link".

use serde::{Deserialize, Serialize};

use crate::Priority;

/// Largest piece of filler one message carries: the files module's own
/// largest read, so the test moves data in the shape a mount does.
pub const MAX_PIECE: u32 = 256 * 1024;

/// The probe module's messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeMsg {
    /// Answer with [`ProbeMsg::Pong`] carrying the same number.
    Ping {
        seq: u32,
    },
    Pong {
        seq: u32,
    },
    /// Send `bytes` of filler back as one [`ProbeMsg::Fill`]; at most
    /// [`MAX_PIECE`].
    Pull {
        id: u32,
        bytes: u32,
    },
    Fill {
        id: u32,
        payload: Vec<u8>,
    },
    /// Filler sent to be counted and dropped; answered by
    /// [`ProbeMsg::Took`].
    Push {
        id: u32,
        payload: Vec<u8>,
    },
    Took {
        id: u32,
        bytes: u32,
    },
}

impl ProbeMsg {
    /// Pings ride where input does, since what they measure is what a pointer
    /// would feel; filler rides where a file does; the requests and receipts
    /// that pace it are small and wanted at once.
    pub fn priority(&self) -> Priority {
        match self {
            ProbeMsg::Ping { .. } | ProbeMsg::Pong { .. } => Priority::Input,
            ProbeMsg::Fill { .. } | ProbeMsg::Push { .. } => Priority::Bulk,
            ProbeMsg::Pull { .. } | ProbeMsg::Took { .. } => Priority::Metadata,
        }
    }
}

/// Round trips over one stretch of a test, in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundTrips {
    pub p50_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    /// Pings sent in the stretch.
    pub sent: u32,
    /// Pings never answered before the stretch ended.
    pub lost: u32,
}

/// One direction of a test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Direction {
    /// Bytes per second over the measured stretch, after the warm-up.
    pub bytes_per_sec: u64,
    /// The pointer's round trips while this direction was loaded.
    pub loaded: RoundTrips,
}

/// What one test found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    /// The asking machine's clock at the start, Unix seconds.
    pub at_unix: u64,
    /// What carried the session from the asking end: "network",
    /// "bluetooth".
    pub path: Option<String>,
    pub idle: RoundTrips,
    /// The peer sending to the asking machine.
    pub download: Direction,
    /// The asking machine sending to the peer.
    pub upload: Direction,
    /// Whether the test ran by itself, at a quiet moment.
    pub automatic: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filler_rides_as_a_file_and_pings_as_input() {
        assert_eq!(
            ProbeMsg::Fill {
                id: 1,
                payload: vec![0; 8]
            }
            .priority(),
            Priority::Bulk
        );
        assert_eq!(ProbeMsg::Ping { seq: 1 }.priority(), Priority::Input);
        assert_eq!(
            ProbeMsg::Pull { id: 1, bytes: 8 }.priority(),
            Priority::Metadata
        );
    }
}
