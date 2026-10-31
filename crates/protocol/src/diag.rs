//! Looking at another machine of the mesh from this one: its log, and which
//! path carries the session between the two.
//!
//! A fault in a mesh is seen from two ends at once — a path one side calls
//! silent is one the other side wrote into, a session one side ends is one
//! the other was about to join — and reading only one end's log guesses at
//! the other's. Fetching the peer's log over the link it is about is what
//! makes the two read as one story, and it reaches a machine that has no
//! shell to log into: a phone, or a laptop on a hotspot.
//!
//! ## Asked for, never pushed
//!
//! A day of log is tens of megabytes, so a request names how much: the last
//! lines, newer than a mark or within a window of the answering machine's own
//! clock, and holding a piece of text. "Newer than a mark" is how a follower
//! keeps up: it hands back the timestamp of the last line it showed. The
//! answer is bounded by [`MAX_LINES`] and [`MAX_LOG_BYTES`] whatever was
//! asked, and says when it left older lines out.
//!
//! ## Choosing the path
//!
//! [`PathChoice`] is a preference for testing and for diagnosis, not a
//! setting: it lasts as long as the session, and a choice of path that does
//! not exist leaves the session on the one that does. Each end chooses for
//! what it sends, so the asking machine applies it to its own end and asks the
//! peer to do the same to the other.
//!
//! The trusted mesh permits both: a paired machine can already drive this
//! one's input and read its disks.

use serde::{Deserialize, Serialize};

use crate::Priority;

/// Most lines one answer carries.
pub const MAX_LINES: u32 = 5000;

/// Most text one answer carries, so it stays within one control-socket line
/// on the asking machine (1 MiB) once escaped as JSON.
pub const MAX_LOG_BYTES: usize = 512 * 1024;

/// Longest piece of text a request may filter by.
pub const MAX_FILTER_BYTES: usize = 256;

/// Which of a session's paths should carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PathChoice {
    /// The network whenever it is there; Bluetooth when it is not.
    #[default]
    Auto,
    /// The network even while it is failing: no move to Bluetooth while a
    /// network path exists, which is how the network alone is tested.
    Network,
    /// Bluetooth whenever there is a Bluetooth path, the network standing by.
    Bluetooth,
}

/// What one machine asks another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagRequest {
    /// The newest `max_lines` lines of the node's log that are newer than
    /// `after` (a line's leading timestamp, compared as text), no older than
    /// `within_secs` by the answering machine's clock, and contain
    /// `contains`.
    Logs {
        after: Option<String>,
        within_secs: Option<u64>,
        contains: Option<String>,
        max_lines: u32,
    },
    /// Report the session's paths, first choosing `choice` for this end of
    /// it when given.
    Paths { choice: Option<PathChoice> },
}

/// The answer to one [`DiagRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagResponse {
    /// Oldest first. `truncated` when older lines matched but were left out.
    Logs {
        lines: Vec<String>,
        truncated: bool,
    },
    Paths(PathReport),
}

/// One end's view of the session between two machines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathReport {
    pub network: bool,
    pub bluetooth: bool,
    /// What carries input from this end now: "network", "bluetooth", or
    /// `None` while the session waits for a path.
    pub carrying: Option<String>,
    pub choice: PathChoice,
}

/// The diagnostics module's messages.
pub type DiagMsg = crate::exchange::ExchangeMsg<DiagRequest, DiagResponse>;

impl DiagMsg {
    /// A log is bulk, behind input and sound; a path report and every request
    /// are small and wanted now.
    pub fn priority(&self) -> Priority {
        match self {
            DiagMsg::Response {
                response: Ok(DiagResponse::Logs { .. }),
                ..
            } => Priority::Bulk,
            _ => Priority::Metadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_answer_round_trips_and_rides_as_bulk() {
        let msg = DiagMsg::Response {
            id: 4,
            response: Ok(DiagResponse::Logs {
                lines: vec!["2026-10-31T16:11:12.191382Z  INFO cursio::mesh: up".into()],
                truncated: true,
            }),
        };
        let bytes = bincode::serialize(&msg).unwrap();
        assert_eq!(bincode::deserialize::<DiagMsg>(&bytes).unwrap(), msg);
        assert_eq!(msg.priority(), Priority::Bulk);
    }

    #[test]
    fn a_path_choice_is_wanted_at_once() {
        let msg = DiagMsg::Request {
            id: 1,
            request: DiagRequest::Paths {
                choice: Some(PathChoice::Bluetooth),
            },
        };
        assert_eq!(msg.priority(), Priority::Metadata);
    }
}
