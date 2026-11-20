//! What a resumable link knows about its paths and what each lane has
//! delivered.

use crate::scheduler::QueueKick;
use crate::session::PathKind;
use crate::session::Paths;
use crate::session::INPUT_STALL;
use crate::{LaneId, PathChoice};
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
    /// Over a shorter route that may go away, rather than through the router.
    pub(super) direct: bool,
    /// One inbox per lane, drained in lane order by the path's writer. A lane
    /// path only ever carries its own lane; a standby carries all of them, and
    /// without this they reached its socket in arrival order, so a file chunk
    /// or a run of sound written first held the pointer motion behind it for
    /// as long as the radio took to send it.
    pub(super) writers: Vec<mpsc::Sender<Vec<u8>>>,
    pub(super) last_heard: Instant,
    pub(super) degraded_until: Option<Instant>,
    /// A network path that went quiet while carrying, kept while another is
    /// placed; see `supervise`.
    pub(super) suspect_since: Option<Instant>,
    pub(super) attached_at: Instant,
    pub(super) probe: Option<(u64, Instant)>,
    pub(super) last_probe: Instant,
    pub(super) tasks: Vec<tokio::task::AbortHandle>,
}

impl PathSlot {
    pub(super) fn writer(&self, lane: LaneId) -> &mpsc::Sender<Vec<u8>> {
        &self.writers[lane.index()]
    }

    /// Where `ACK`, `PING` and `PONG` go: a lane path's own lane, and the
    /// interactive one on a standby, so they keep pace with input.
    pub(super) fn control(&self) -> &mpsc::Sender<Vec<u8>> {
        self.writer(self.kind.lane().unwrap_or(LaneId::INTERACTIVE))
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

pub(super) struct State {
    pub(super) lanes: Vec<Delivery>,
    pub(super) ack_due: bool,
    pub(super) paths: Vec<PathSlot>,
    pub(super) next_path_id: u64,
    pub(super) pathless_since: Option<Instant>,
    pub(super) ended: Option<String>,
    pub(super) kick: Option<QueueKick>,
    pub(super) reported: Paths,
    /// When a paced lane last sent: background traffic is flowing.
    pub(super) bulk_active: Option<Instant>,
    pub(super) next_probe: u64,
    pub(super) choice: PathChoice,
}

impl State {
    pub(super) fn new(lanes: usize) -> Self {
        Self {
            lanes: (0..lanes).map(|_| Delivery::default()).collect(),
            ack_due: false,
            paths: Vec::new(),
            next_path_id: 0,
            pathless_since: None,
            ended: None,
            kick: None,
            reported: Paths::default(),
            bulk_active: None,
            next_probe: 0,
            choice: PathChoice::default(),
        }
    }

    pub(super) fn carrying(&self, lane: LaneId, now: Instant) -> Option<&PathSlot> {
        self.paths
            .iter()
            .filter(|p| p.kind.lane() == Some(lane) || p.kind == PathKind::Standby)
            .min_by_key(|p| {
                let bluetooth = p.kind == PathKind::Standby;
                let degraded = p.degraded_until.is_some_and(|until| until > now);
                // A direct path is a step shorter than the router's, so it
                // carries whenever it is healthy; the router's stays beside
                // it as the standby that takes over when the machines part.
                let routed = !p.direct;
                match self.choice {
                    PathChoice::Auto => (
                        false,
                        degraded,
                        bluetooth,
                        routed,
                        std::cmp::Reverse(p.attached_at),
                    ),
                    PathChoice::Network => (
                        bluetooth,
                        degraded,
                        false,
                        routed,
                        std::cmp::Reverse(p.attached_at),
                    ),
                    PathChoice::Bluetooth => (
                        !bluetooth,
                        degraded,
                        false,
                        routed,
                        std::cmp::Reverse(p.attached_at),
                    ),
                }
            })
    }

    /// The path input is written on as well as `carrying`: the router's,
    /// healthy, beside a direct path that carries the interactive lane.
    ///
    /// Both ride one radio, so doubling a file would halve it; input is tens
    /// of bytes a frame, and with a copy on each path the pointer moves by
    /// whichever arrives first — a stall on one, like the three seconds the
    /// direct path once froze for while a game loaded, is not felt at all.
    pub(super) fn mirror(&self, lane: LaneId, carrying: u64, now: Instant) -> Option<&PathSlot> {
        if lane != LaneId::INTERACTIVE
            || !self
                .paths
                .iter()
                .any(|p| p.id == carrying && p.direct && p.kind == PathKind::INTERACTIVE)
        {
            return None;
        }
        self.paths.iter().find(|p| {
            p.kind == PathKind::INTERACTIVE
                && !p.direct
                && p.suspect_since.is_none()
                && p.degraded_until.is_none_or(|until| until <= now)
        })
    }

    /// Whether input has stopped getting through on `id`, a network path
    /// carrying the interactive lane, while another path could take it — a
    /// Bluetooth one, or the router's beside a direct one: a key or click
    /// unacknowledged, or a probe unanswered, for [`INPUT_STALL`]. Pointer
    /// motion is never kept for an acknowledgement, so the probe is what
    /// notices a stall while only the pointer moves.
    ///
    /// A direct path stalled for three seconds once while a game loaded
    /// through it, with the router's standing by; at the general [`STALL`]
    /// that was three seconds of a pointer going nowhere. Its loaded round
    /// trip stays near a tenth of a second, so a second unanswered is not
    /// load but a stall.
    ///
    /// Never true of a Bluetooth path, whose round trip itself spikes past a
    /// second: moving input off it on that evidence would send it back to a
    /// network path that may be the dead one.
    pub(super) fn input_stalled_on(&self, id: u64, now: Instant) -> bool {
        let Some(path) = self.paths.iter().find(|p| p.id == id) else {
            return false;
        };
        let bluetooth = self.paths.iter().any(|p| p.kind == PathKind::Standby);
        let routed = path.direct
            && self
                .paths
                .iter()
                .any(|p| p.kind == PathKind::INTERACTIVE && !p.direct);
        if path.kind != PathKind::INTERACTIVE || !(bluetooth || routed) {
            return false;
        }
        let late = |at: Instant| now.duration_since(at) > INPUT_STALL;
        let unacked = self.lanes[LaneId::INTERACTIVE.index()]
            .unacked
            .iter()
            .any(|k| k.sent_on == Some(id) && k.sent_at.is_some_and(late));
        unacked || path.probe.is_some_and(|(_, at)| late(at))
    }

    pub(super) fn summary(&self) -> Paths {
        Paths {
            network: self.paths.iter().any(|p| p.kind == PathKind::INTERACTIVE),
            bluetooth: self.paths.iter().any(|p| p.kind == PathKind::Standby),
            direct: self
                .paths
                .iter()
                .any(|p| p.kind == PathKind::INTERACTIVE && p.direct),
        }
    }

    pub(super) fn acknowledged(&mut self, lane: LaneId, delivered: u64) -> Option<Duration> {
        let delivery = &mut self.lanes[lane.index()];
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

    pub(super) fn delivered(&self) -> Vec<u64> {
        self.lanes.iter().map(|lane| lane.delivered).collect()
    }

    /// The first lane with no healthy lane path: of any kind with `None`,
    /// over the direct route or through the router with `Some(direct)`.
    pub(super) fn missing_lane(&self, direct: Option<bool>) -> Option<LaneId> {
        (0..self.lanes.len() as u8).map(LaneId::new).find(|lane| {
            !self.paths.iter().any(|path| {
                path.kind.lane() == Some(*lane)
                    && direct.is_none_or(|d| path.direct == d)
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
            direct: false,
            writers: (0..4).map(|_| mpsc::channel(1).0).collect(),
            last_heard: now,
            degraded_until: None,
            suspect_since: None,
            attached_at: now,
            probe: None,
            last_probe: now,
            tasks: Vec::new(),
        }
    }

    #[test]
    fn an_unanswered_probe_moves_input_to_a_bluetooth_standby_within_a_second() {
        let now = Instant::now();
        let mut state = State::new(4);
        state.paths.push(path(1, PathKind::INTERACTIVE, now));
        state.paths[0].probe = Some((7, now));
        assert!(!state.input_stalled_on(1, now + INPUT_STALL * 2));

        state.paths.push(path(2, PathKind::Standby, now));
        assert!(!state.input_stalled_on(1, now + INPUT_STALL / 2));
        assert!(state.input_stalled_on(1, now + INPUT_STALL * 2));
    }

    #[test]
    fn an_unanswered_probe_moves_input_off_a_direct_path_to_the_routers_within_a_second() {
        let now = Instant::now();
        let mut state = State::new(4);
        state.paths.push(path(1, PathKind::INTERACTIVE, now));
        state.paths[0].direct = true;
        state.paths[0].probe = Some((7, now));
        assert!(
            !state.input_stalled_on(1, now + INPUT_STALL * 2),
            "nowhere to move it"
        );

        state.paths.push(path(2, PathKind::INTERACTIVE, now));
        assert!(!state.input_stalled_on(1, now + INPUT_STALL / 2));
        assert!(state.input_stalled_on(1, now + INPUT_STALL * 2));
        assert!(
            !state.input_stalled_on(2, now + INPUT_STALL * 2),
            "the router's own stall waits for the general rule"
        );
    }

    #[test]
    fn only_input_on_a_direct_path_is_copied_and_only_onto_a_healthy_routers_path() {
        let now = Instant::now();
        let mut state = State::new(4);
        state.paths.push(path(1, PathKind::INTERACTIVE, now));
        state.paths[0].direct = true;
        assert!(
            state.mirror(LaneId::INTERACTIVE, 1, now).is_none(),
            "no router path"
        );
        state.paths.push(path(2, PathKind::INTERACTIVE, now));
        assert_eq!(
            state.mirror(LaneId::INTERACTIVE, 1, now).map(|p| p.id),
            Some(2)
        );
        assert!(
            state.mirror(LaneId::new(3), 1, now).is_none(),
            "files are never doubled"
        );
        assert!(
            state.mirror(LaneId::INTERACTIVE, 2, now).is_none(),
            "the router carrying needs no copy"
        );
        state.paths[1].degraded_until = Some(now + Duration::from_secs(10));
        assert!(state.mirror(LaneId::INTERACTIVE, 1, now).is_none());
    }

    #[test]
    fn a_slow_bluetooth_path_is_never_called_stalled_for_input() {
        let now = Instant::now();
        let mut state = State::new(4);
        state.paths.push(path(1, PathKind::INTERACTIVE, now));
        state.paths.push(path(2, PathKind::Standby, now));
        state.paths[1].probe = Some((7, now));
        assert!(!state.input_stalled_on(2, now + INPUT_STALL * 5));
    }

    #[test]
    fn an_unacknowledged_key_counts_as_stalled_input() {
        let now = Instant::now();
        let mut state = State::new(4);
        state.paths.push(path(1, PathKind::INTERACTIVE, now));
        state.paths.push(path(2, PathKind::Standby, now));
        state.lanes[LaneId::INTERACTIVE.index()]
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
