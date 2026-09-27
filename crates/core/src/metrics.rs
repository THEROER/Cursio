//! Live readings the control API reports: what the session loop measured last.
//!
//! The session loop owns its measurements and logs them on its own schedule;
//! this is where it also *leaves* them, so a status window can read the most
//! recent figure without scraping a log. Process-wide rather than per link
//! because the reader — `cursio status`, the tray — asks about the machine,
//! and because the loop that measures is `!Send` and cannot be asked directly.
//!
//! Only the latest reading per path is kept. The loop already summarises a
//! window of samples into percentiles; storing the samples here too would
//! double the work for a figure a human glances at.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use cursio_protocol::{NodeStatus, PeerId};

/// One latency window, as the session loop summarised it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LatencyReading {
    /// Which stretch was measured: `received->injected` or `captured->sent`.
    pub path: String,
    pub p50_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    /// Events the window saw. Few events make a p99 that means little.
    pub events: u64,
    /// When the window closed. A reading from minutes ago describes a link
    /// that has since been idle, not one that is fast.
    pub at: SystemTime,
}

static LATENCY: Mutex<BTreeMap<String, LatencyReading>> = Mutex::new(BTreeMap::new());
static PEER_STATUS: Mutex<BTreeMap<PeerId, NodeStatus>> = Mutex::new(BTreeMap::new());

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Leave the latest percentiles of one input path for readers.
pub fn record_input_latency(path: &str, p50: Duration, p99: Duration, max: Duration, events: u64) {
    lock(&LATENCY).insert(
        path.to_owned(),
        LatencyReading {
            path: path.to_owned(),
            p50_us: p50.as_micros() as u64,
            p99_us: p99.as_micros() as u64,
            max_us: max.as_micros() as u64,
            events,
            at: SystemTime::now(),
        },
    );
}

pub fn input_latency() -> Vec<LatencyReading> {
    lock(&LATENCY).values().cloned().collect()
}

/// Remember what a peer last said about its power and network, whether it
/// arrived in the greeting or as a later [`cursio_protocol::Message::NodeStatus`].
pub fn note_peer_status(peer: PeerId, status: NodeStatus) {
    lock(&PEER_STATUS).insert(peer, status);
}

pub fn peer_status(peer: &PeerId) -> Option<NodeStatus> {
    lock(&PEER_STATUS).get(peer).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_window_replaces_the_earlier_one_for_the_same_path() {
        let path = "metrics-test-path";
        record_input_latency(
            path,
            Duration::from_micros(10),
            Duration::from_micros(90),
            Duration::from_micros(100),
            5,
        );
        record_input_latency(
            path,
            Duration::from_micros(20),
            Duration::from_micros(80),
            Duration::from_micros(200),
            7,
        );
        let mine: Vec<_> = input_latency()
            .into_iter()
            .filter(|r| r.path == path)
            .collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(
            (mine[0].p50_us, mine[0].p99_us, mine[0].events),
            (20, 80, 7)
        );
    }

    #[test]
    fn a_peer_status_is_read_back_as_last_reported() {
        let peer = PeerId::new_v4();
        assert_eq!(peer_status(&peer), None);
        let status = NodeStatus {
            battery_percent: Some(40),
            ..NodeStatus::unknown()
        };
        note_peer_status(peer, status);
        assert_eq!(peer_status(&peer), Some(status));
    }
}
