//! What a resumable link knows about its paths and what each lane has
//! delivered.

use crate::lane::Lane;
use crate::priority::QueueKick;
use crate::resume::PathKind;
use crate::resume::Paths;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// A frame kept until the peer acknowledges it.
pub(super) struct Kept {
    pub(super) seq: u64,
    /// The whole `SEQ` frame, ready to seal again for whichever path resends it.
    pub(super) frame: Vec<u8>,
    pub(super) sent_at: Option<Instant>,
    pub(super) sent_on: Option<u64>,
}

pub(super) struct PathSlot {
    pub(super) id: u64,
    pub(super) kind: PathKind,
    pub(super) writer: mpsc::Sender<Vec<u8>>,
    pub(super) last_heard: Instant,
    pub(super) degraded_until: Option<Instant>,
    pub(super) attached_at: Instant,
    pub(super) probe: Option<(u64, Instant)>,
    pub(super) last_probe: Instant,
    pub(super) tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for PathSlot {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Default)]
pub(super) struct Delivery {
    pub(super) next_seq: u64,
    pub(super) unacked: VecDeque<Kept>,
    pub(super) unacked_bytes: usize,
    peer_delivered: u64,
    pub(super) delivered: u64,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) lanes: [Delivery; Lane::COUNT],
    pub(super) ack_due: bool,
    pub(super) paths: Vec<PathSlot>,
    pub(super) next_path_id: u64,
    pub(super) pathless_since: Option<Instant>,
    pub(super) ended: Option<String>,
    pub(super) kick: Option<QueueKick>,
    pub(super) reported: Paths,
    pub(super) bulk_active: Option<Instant>,
    pub(super) next_probe: u64,
}

impl State {
    pub(super) fn carrying(&self, lane: Lane, now: Instant) -> Option<&PathSlot> {
        self.paths
            .iter()
            .filter(|p| p.kind.lane() == Some(lane) || p.kind == PathKind::Bluetooth)
            .min_by_key(|p| {
                let degraded = p.degraded_until.is_some_and(|until| until > now);
                (
                    degraded,
                    p.kind == PathKind::Bluetooth,
                    std::cmp::Reverse(p.attached_at),
                )
            })
    }

    pub(super) fn summary(&self) -> Paths {
        Paths {
            network: self.paths.iter().any(|p| p.kind == PathKind::Network),
            bluetooth: self.paths.iter().any(|p| p.kind == PathKind::Bluetooth),
        }
    }

    pub(super) fn acknowledged(&mut self, lane: Lane, delivered: u64) -> Option<Duration> {
        let delivery = &mut self.lanes[lane as usize];
        // An ACK must never acknowledge data this end has not numbered.
        if delivered <= delivery.peer_delivered || delivered > delivery.next_seq {
            return None;
        }
        delivery.peer_delivered = delivered;
        let mut rtt = None;
        while delivery.unacked.front().is_some_and(|k| k.seq <= delivered) {
            if let Some(kept) = delivery.unacked.pop_front() {
                delivery.unacked_bytes -= kept.frame.len();
                if let Some(at) = kept.sent_at {
                    rtt = Some(at.elapsed());
                }
            }
        }
        rtt
    }

    pub(super) fn delivered(&self) -> [u64; Lane::COUNT] {
        std::array::from_fn(|i| self.lanes[i].delivered)
    }

    pub(super) fn missing_lane(&self) -> Option<Lane> {
        Lane::ALL.into_iter().find(|lane| {
            !self.paths.iter().any(|path| {
                path.kind.lane() == Some(*lane)
                    && path
                        .degraded_until
                        .is_none_or(|until| until <= Instant::now())
            })
        })
    }
}
