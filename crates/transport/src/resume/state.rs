//! What a resumable link knows about its paths and what each lane has
//! delivered.

use crate::lane::Lane;
use crate::priority::QueueKick;
use crate::resume::PathKind;
use crate::resume::Paths;
use crate::resume::INPUT_STALL;
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
    /// One inbox per lane, drained in lane order by the path's writer. A
    /// network path only ever carries its own lane; a Bluetooth path carries
    /// all of them, and without this they reached its socket in arrival order,
    /// so a file chunk or a run of sound written first held the pointer
    /// motion behind it for as long as the radio took to send it.
    pub(super) writers: [mpsc::Sender<Vec<u8>>; Lane::COUNT],
    pub(super) last_heard: Instant,
    pub(super) degraded_until: Option<Instant>,
    pub(super) attached_at: Instant,
    pub(super) probe: Option<(u64, Instant)>,
    pub(super) last_probe: Instant,
    pub(super) tasks: Vec<tokio::task::AbortHandle>,
}

impl PathSlot {
    pub(super) fn writer(&self, lane: Lane) -> &mpsc::Sender<Vec<u8>> {
        &self.writers[lane as usize]
    }

    /// Where `ACK`, `PING` and `PONG` go: a network path's own lane, and the
    /// interactive one on Bluetooth, so they keep pace with input.
    pub(super) fn control(&self) -> &mpsc::Sender<Vec<u8>> {
        self.writer(self.kind.lane().unwrap_or(Lane::Interactive))
    }
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

    /// Whether input has stopped getting through on `id`, a network path
    /// carrying the interactive lane, while a Bluetooth path could take it:
    /// a key or click unacknowledged, or a probe unanswered, for
    /// [`INPUT_STALL`]. Pointer motion is never kept for an acknowledgement,
    /// so the probe is what notices a stall while only the pointer moves.
    ///
    /// Never true of a Bluetooth path, whose round trip itself spikes past a
    /// second: moving input off it on that evidence would send it back to a
    /// network path that may be the dead one.
    pub(super) fn input_stalled_on(&self, id: u64, now: Instant) -> bool {
        let Some(path) = self.paths.iter().find(|p| p.id == id) else {
            return false;
        };
        if path.kind != PathKind::Network
            || !self.paths.iter().any(|p| p.kind == PathKind::Bluetooth)
        {
            return false;
        }
        let late = |at: Instant| now.duration_since(at) > INPUT_STALL;
        let unacked = self.lanes[Lane::Interactive as usize]
            .unacked
            .iter()
            .any(|k| k.sent_on == Some(id) && k.sent_at.is_some_and(late));
        unacked || path.probe.is_some_and(|(_, at)| late(at))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn path(id: u64, kind: PathKind, now: Instant) -> PathSlot {
        PathSlot {
            id,
            kind,
            writers: std::array::from_fn(|_| mpsc::channel(1).0),
            last_heard: now,
            degraded_until: None,
            attached_at: now,
            probe: None,
            last_probe: now,
            tasks: Vec::new(),
        }
    }

    #[test]
    fn an_unanswered_probe_moves_input_to_a_bluetooth_standby_within_a_second() {
        let now = Instant::now();
        let mut state = State::default();
        state.paths.push(path(1, PathKind::Network, now));
        state.paths[0].probe = Some((7, now));
        assert!(!state.input_stalled_on(1, now + INPUT_STALL * 2));

        state.paths.push(path(2, PathKind::Bluetooth, now));
        assert!(!state.input_stalled_on(1, now + INPUT_STALL / 2));
        assert!(state.input_stalled_on(1, now + INPUT_STALL * 2));
    }

    #[test]
    fn a_slow_bluetooth_path_is_never_called_stalled_for_input() {
        let now = Instant::now();
        let mut state = State::default();
        state.paths.push(path(1, PathKind::Network, now));
        state.paths.push(path(2, PathKind::Bluetooth, now));
        state.paths[1].probe = Some((7, now));
        assert!(!state.input_stalled_on(2, now + INPUT_STALL * 5));
    }

    #[test]
    fn an_unacknowledged_key_counts_as_stalled_input() {
        let now = Instant::now();
        let mut state = State::default();
        state.paths.push(path(1, PathKind::Network, now));
        state.paths.push(path(2, PathKind::Bluetooth, now));
        state.lanes[Lane::Interactive as usize]
            .unacked
            .push_back(Kept {
                seq: 1,
                frame: Vec::new(),
                sent_at: Some(now),
                sent_on: Some(1),
            });
        assert!(state.input_stalled_on(1, now + INPUT_STALL * 2));
    }
}
