//! The link's shared core: its state behind one lock, and what a path's
//! arrival, failure or acknowledgement does to it.

use crate::seal::link_tag;
use crate::session::congestion::BackgroundRate;
use crate::session::frame::ack_frame;
use crate::session::frame::read_delivered;
use crate::session::frame::read_u64;
use crate::session::state::PathSlot;
use crate::session::state::State;
use crate::session::Path;
use crate::session::PathKind;
use crate::session::Paths;
use crate::{ChannelError, Classes, LaneId, SessionTag};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::time::Instant;

pub(super) type PathsObserver = Box<dyn Fn(Paths) + Send + Sync>;

pub(super) struct Inner {
    pub(super) classes: Arc<Classes>,
    pub(super) state: Mutex<State>,
    /// Woken when the path set changes or the link ends.
    pub(super) changed: Notify,
    /// Every path's reader feeds this, one queue per lane; the session's
    /// receiver drains it.
    pub(super) inbound: Vec<mpsc::Sender<(u64, Vec<u8>)>>,
    pub(super) tag: SessionTag,
    pub(super) observer: Mutex<Option<PathsObserver>>,
    pub(super) congestion: Mutex<BackgroundRate>,
}

impl Inner {
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn lanes(&self) -> usize {
        self.classes.lanes().len()
    }

    /// How logs name a path of `kind`.
    pub(super) fn kind_name(&self, kind: PathKind) -> &'static str {
        match kind {
            PathKind::Lane(lane) => self.classes.lane(lane).path_name,
            PathKind::Standby => self.classes.standby_name(),
        }
    }

    /// The session in logs: the first bytes of its tag, the same on both
    /// ends. A node holds a session per peer and the transport knows no
    /// peer, so without it two sessions' path numbers read as one's.
    pub(super) fn label(&self) -> String {
        self.tag[..4].iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(super) fn kick(&self, flags: u8) {
        let kick = self.lock().kick.clone();
        if let Some(kick) = kick {
            for lane in self.classes.lane_ids() {
                kick.push_lane_control(ack_frame(&vec![0; self.lanes()], flags), lane);
            }
        }
    }

    /// Report the path set if it changed since last reported.
    fn report(&self) {
        let changed = {
            let mut state = self.lock();
            let now = state.summary();
            (now != state.reported).then(|| {
                state.reported = now;
                now
            })
        };
        if let Some(paths) = changed {
            tracing::info!(
                session = %self.label(),
                network = paths.network,
                bluetooth = paths.bluetooth,
                "the session's paths changed"
            );
            if let Some(observer) = self
                .observer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
            {
                observer(paths);
            }
        }
    }

    pub(super) fn path_failed(&self, id: u64, why: &str) {
        let removed = {
            let mut state = self.lock();
            let before = state.paths.len();
            state.paths.retain(|p| p.id != id);
            let removed = state.paths.len() != before;
            if removed
                && state
                    .carrying(LaneId::INTERACTIVE, Instant::now())
                    .is_none()
                && state.ended.is_none()
                && state.pathless_since.is_none()
            {
                state.pathless_since = Some(Instant::now());
            }
            removed
        };
        if removed {
            tracing::info!(session = %self.label(), path = id, %why, "a path of the session ended");
            self.changed.notify_waiters();
            // The carrying path may have changed: resend on the new one now,
            // not when the session next has something to say.
            self.kick(0);
            self.report();
        }
    }

    pub(super) fn end(&self, why: String) {
        {
            let mut state = self.lock();
            if state.ended.is_some() {
                return;
            }
            tracing::info!(session = %self.label(), %why, "the session can no longer be resumed; ending it");
            state.ended = Some(why);
            state.paths.clear();
        }
        self.changed.notify_waiters();
        self.report();
    }

    fn probe(&self, id: u64, tagged: &[u8]) {
        let Some(nonce) = tagged
            .get(1..)
            .filter(|bytes| bytes.len() == 8)
            .and_then(read_u64)
        else {
            return;
        };
        let mut state = self.lock();
        let active = state
            .bulk_active
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1));
        let Some(path) = state.paths.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if tagged[0] == link_tag::PING {
            let mut pong = vec![link_tag::PONG];
            pong.extend_from_slice(&nonce.to_be_bytes());
            if let Err(e) = path.control().try_send(pong) {
                tracing::debug!(session = %self.label(), path = id, error = %e, "a probe's answer was dropped: the path's writer is full");
            }
        } else if let Some((expected, at)) = path.probe {
            if expected == nonce {
                path.probe = None;
                drop(state);
                self.round_trip(id, at.elapsed(), active, false);
            } else {
                tracing::debug!(session = %self.label(), path = id, nonce, expected, "an answer to an earlier probe arrived");
            }
        } else {
            tracing::debug!(session = %self.label(), path = id, nonce, "an answer arrived with no probe outstanding");
        }
    }

    /// Hand one interactive round trip to the bulk pacer, saying why when it
    /// backs off: `overdue` when the reading is a probe still unanswered.
    pub(super) fn round_trip(&self, path: u64, rtt: Duration, active: bool, overdue: bool) {
        let cut = self
            .congestion
            .lock()
            .unwrap()
            .observe(rtt, Instant::now(), active);
        if let Some((standing, baseline)) = cut {
            let rate = self.congestion.lock().unwrap().bytes_per_second();
            tracing::debug!(
                session = %self.label(),
                path,
                overdue,
                standing_ms = standing.as_secs_f64() * 1000.0,
                baseline_ms = baseline.as_secs_f64() * 1000.0,
                rate_kb_s = (rate / 1024.0) as u64,
                "bulk eased off: the interactive round trip is standing above its baseline"
            );
        }
    }

    pub(super) fn acknowledge(&self, bytes: &[u8]) -> Result<(), ChannelError> {
        let delivered = read_delivered(bytes, self.lanes())?;
        {
            let mut state = self.lock();
            for lane in self.classes.lane_ids() {
                state.acknowledged(lane, delivered[lane.index()]);
            }
        }
        self.changed.notify_waiters();
        Ok(())
    }

    /// Start a path's reader and writer and add it to the session.
    pub(super) fn install(self: &Arc<Self>, path: Path) {
        let Path {
            mut tx,
            mut rx,
            seal,
            kind,
            direct,
        } = path;
        let (writers, mut frames): (Vec<_>, Vec<_>) = self
            .classes
            .lanes()
            .iter()
            .map(|lane| mpsc::channel::<Vec<u8>>(lane.writer_depth))
            .unzip();
        let id = {
            let mut state = self.lock();
            state.next_path_id += 1;
            state.next_path_id
        };

        let weak = Arc::downgrade(self);
        let write_seal = seal.clone();
        let lane_name = kind.lane().map(|lane| self.classes.lane(lane).name);
        let write = tokio::spawn(async move {
            let mut retransmits = crate::tcp::info::RetransmitLog::default();
            while let Some(frame) = next_by_lane(&mut frames).await {
                if let Some(lane) = lane_name {
                    retransmits.sample(tx.as_ref(), lane);
                }
                // Whatever else is already waiting leaves in the same write;
                // see `CarrierTx::send_batch` for what that saves.
                let mut batch = vec![frame];
                let mut bytes = batch[0].len();
                while bytes < BATCH_BYTES {
                    let Some(more) = waiting_by_lane(&mut frames) else {
                        break;
                    };
                    bytes += more.len();
                    batch.push(more);
                }
                let sent = match batch
                    .iter()
                    .map(|frame| write_seal.seal(frame))
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(sealed) => tx.send_batch(sealed).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = sent {
                    if let Some(inner) = weak.upgrade() {
                        inner.path_failed(id, &format!("could not write: {e}"));
                    }
                    return;
                }
            }
        });

        let weak = Arc::downgrade(self);
        let inbound = self.inbound.clone();
        let read = tokio::spawn(async move {
            loop {
                let result = match rx.recv().await {
                    Ok(sealed) => seal.open(&sealed),
                    Err(e) => Err(e),
                };
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(tagged) => {
                        let recovered = {
                            let mut state = inner.lock();
                            match state.paths.iter_mut().find(|p| p.id == id) {
                                Some(slot) => {
                                    slot.last_heard = Instant::now();
                                    let was_suspect = slot.suspect_since.take().is_some();
                                    if was_suspect {
                                        slot.degraded_until = None;
                                    }
                                    was_suspect
                                }
                                None => false,
                            }
                        };
                        if recovered {
                            tracing::info!(
                                session = %inner.label(),
                                path = id,
                                "a quiet path came back by itself"
                            );
                            inner.changed.notify_waiters();
                            inner.kick(0);
                        }
                        if matches!(
                            tagged.first(),
                            Some(&link_tag::PING) | Some(&link_tag::PONG)
                        ) {
                            inner.probe(id, &tagged);
                            continue;
                        }
                        if tagged.first() == Some(&link_tag::ACK) {
                            if let Err(error) = inner.acknowledge(&tagged[1..]) {
                                inner.path_failed(id, &error.to_string());
                                return;
                            }
                            continue;
                        }
                        let Some(lane) = tagged.get(1).and_then(|v| inner.classes.decode_lane(*v))
                        else {
                            inner.path_failed(id, "invalid delivery lane");
                            return;
                        };
                        if kind.lane().is_some_and(|expected| expected != lane) {
                            inner.path_failed(id, "frame arrived on the wrong lane");
                            return;
                        }
                        if inbound[lane.index()].send((id, tagged)).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        inner.path_failed(id, &format!("could not read: {e}"));
                        return;
                    }
                }
            }
        });

        let now = Instant::now();
        let replaced: Vec<PathSlot> = {
            let mut state = self.lock();
            let (old, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut state.paths)
                .into_iter()
                .partition(|p| p.kind == kind && p.direct == direct);
            state.paths = keep;
            state.paths.push(PathSlot {
                id,
                kind,
                direct,
                writers,
                last_heard: now,
                degraded_until: None,
                suspect_since: None,
                attached_at: now,
                probe: None,
                last_probe: now,
                tasks: vec![write.abort_handle(), read.abort_handle()],
            });
            if state.carrying(LaneId::INTERACTIVE, now).is_some() {
                state.pathless_since = None;
            }
            old
        };
        let kind_name = self.kind_name(kind);
        for old in &replaced {
            tracing::info!(
                session = %self.label(),
                path = old.id,
                kind = kind_name,
                "a newer path of the same kind replaced this one"
            );
        }
        drop(replaced);
        // The round trip the pacer reads is the interactive lane's, so a new
        // path that may carry it starts the estimate over.
        if matches!(kind, PathKind::INTERACTIVE | PathKind::Standby) {
            *self.congestion.lock().unwrap() = BackgroundRate::default();
        }
        tracing::info!(session = %self.label(), path = id, kind = kind_name, direct, "a path joined the session");
        self.changed.notify_waiters();
        self.kick(0);
        self.report();
    }
}

/// How many bytes of already-waiting frames one write may carry.
///
/// Enough for a burst of pointer motion and its acknowledgements in a few
/// RFCOMM frames; small enough that a frame arriving just after the batch is
/// built waits behind at most this much, which over Bluetooth is a few
/// milliseconds.
const BATCH_BYTES: usize = 16 * 1024;

/// A frame already waiting, the most urgent lane first, without waiting for
/// one.
fn waiting_by_lane(frames: &mut [mpsc::Receiver<Vec<u8>>]) -> Option<Vec<u8>> {
    frames.iter_mut().find_map(|inbox| inbox.try_recv().ok())
}

/// The next frame for a path's socket: the most urgent lane's first, so a
/// path shared by every lane sends in lane order. `None` once every inbox has
/// closed.
async fn next_by_lane(frames: &mut [mpsc::Receiver<Vec<u8>>]) -> Option<Vec<u8>> {
    std::future::poll_fn(|cx| {
        let mut open = false;
        for inbox in frames.iter_mut() {
            match inbox.poll_recv(cx) {
                std::task::Poll::Ready(Some(frame)) => return std::task::Poll::Ready(Some(frame)),
                std::task::Poll::Ready(None) => {}
                std::task::Poll::Pending => open = true,
            }
        }
        if open {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(None)
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{AUDIO_LANE, BULK_LANE, INTERACTIVE_LANE};

    /// What a batch adds after its first frame comes most urgent lane first,
    /// and the wait ends at an empty inbox rather than blocking.
    #[tokio::test]
    async fn a_batch_takes_waiting_frames_most_urgent_first_and_does_not_wait() {
        let (senders, mut inboxes): (Vec<_>, Vec<_>) = crate::test_util::classes()
            .lanes()
            .iter()
            .map(|lane| mpsc::channel::<Vec<u8>>(lane.writer_depth))
            .unzip();
        senders[BULK_LANE.index()]
            .send(b"bulk".to_vec())
            .await
            .unwrap();
        for n in 0..20u8 {
            senders[INTERACTIVE_LANE.index()]
                .send(vec![n])
                .await
                .unwrap();
        }

        let mut taken = Vec::new();
        while let Some(frame) = waiting_by_lane(&mut inboxes) {
            taken.push(frame);
        }
        assert_eq!(taken.len(), 21, "twenty moves fit beside each other");
        assert_eq!(
            taken[..20],
            (0..20u8).map(|n| vec![n]).collect::<Vec<_>>()[..]
        );
        assert_eq!(taken[20], b"bulk");
        assert!(waiting_by_lane(&mut inboxes).is_none());
    }

    #[tokio::test]
    async fn a_shared_path_sends_input_ahead_of_frames_queued_before_it() {
        let (senders, mut inboxes): (Vec<_>, Vec<_>) =
            (0..4).map(|_| mpsc::channel::<Vec<u8>>(2)).unzip();
        senders[BULK_LANE.index()]
            .send(b"bulk".to_vec())
            .await
            .unwrap();
        senders[AUDIO_LANE.index()]
            .send(b"audio".to_vec())
            .await
            .unwrap();
        senders[INTERACTIVE_LANE.index()]
            .send(b"input".to_vec())
            .await
            .unwrap();

        let mut order = Vec::new();
        for _ in 0..3 {
            order.push(next_by_lane(&mut inboxes).await.unwrap());
        }
        assert_eq!(
            order,
            [b"input".to_vec(), b"audio".to_vec(), b"bulk".to_vec()]
        );
    }

    #[tokio::test]
    async fn a_path_writer_ends_only_when_every_lane_has_closed() {
        let (senders, mut inboxes): (Vec<_>, Vec<_>) =
            (0..4).map(|_| mpsc::channel::<Vec<u8>>(2)).unzip();
        let mut senders = senders.into_iter();
        let interactive = senders.next().unwrap();
        drop(senders);
        interactive.send(b"last".to_vec()).await.unwrap();
        drop(interactive);
        assert_eq!(next_by_lane(&mut inboxes).await, Some(b"last".to_vec()));
        assert_eq!(next_by_lane(&mut inboxes).await, None);
    }
}
