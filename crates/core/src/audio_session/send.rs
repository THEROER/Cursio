//! Sending this machine's streams: how much leaves per turn, what is
//! dropped when the link falls behind, and the bitrate the peer's
//! acknowledgements earn.

use crate::audio_session::module::send_control;
use crate::audio_session::AudioEngine;
use cursio_protocol::AudioMsg;
use cursio_transport::{TransportError, TransportTx};
use std::collections::{HashMap, VecDeque};

/// How often we return an [`AudioFrameAck`] per inbound stream.
///
/// The design says acks are "sent sparsely (a few per second), not per chunk":
/// they only drive slow bitrate decisions, so acking every ~10 ms chunk would
/// flood the shared channel (felt as input lag) for no benefit. 250 ms = ~4/s.
pub(super) const ACK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// How often each outbound stream re-evaluates its bitrate.
///
/// Rate control must react slowly and hysteretically: reacting per chunk would
/// oscillate audibly. Half a second lets a real trend (a genuinely congested
/// link) show through transient jitter.
pub(super) const ADAPT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Sent-minus-acked gap (in chunks) above which a stream is "behind" and should
/// shed bitrate. At ~200 chunks/s a gap of 100 is ~0.5 s of unplayed audio — a
/// real backlog, not ordinary jitter-buffer slack.
pub(super) const BACKLOG_HIGH: u32 = 100;

/// Gap below which the link is considered caught up, so bitrate may climb back.
/// Kept well under [`BACKLOG_HIGH`] so the two thresholds don't chatter.
pub(super) const BACKLOG_LOW: u32 = 20;

/// Consecutive caught-up evaluations before the bitrate may climb one step.
///
/// Down stays immediate — a backlog is felt as mouse lag, since input shares
/// the link — but up waits for the link to hold for 1.5 s. Climbing on the
/// first calm sample let a single hiccup flap a stream between two rates, which
/// the logs showed many times a day, seconds apart.
pub(super) const BITRATE_RAISE_AFTER: u32 = 3;

/// Most unacknowledged chunks remembered per stream.
///
/// Only a bound on memory for a peer that stopped acking altogether: the
/// backlog is already far past [`BACKLOG_HIGH`] long before this is reached, so
/// forgetting the oldest changes no decision.
pub(super) const UNACKED_CAP: usize = 4096;

/// Note that a chunk of stream `id` went out, for rate control.
///
/// One chunk becomes several Opus packets sharing its seq; it is recorded once,
/// so the backlog stays in the chunks [`BACKLOG_HIGH`] is written in.
pub(super) fn record_sent(
    unacked: &mut HashMap<u32, VecDeque<u32>>,
    sent_since_adapt: &mut std::collections::HashSet<u32>,
    id: u32,
    seq: u32,
) {
    let queue = unacked.entry(id).or_default();
    if queue.back() != Some(&seq) {
        queue.push_back(seq);
        if queue.len() > UNACKED_CAP {
            queue.pop_front();
        }
    }
    sent_since_adapt.insert(id);
}

/// Bitrate floor and ceiling for adaptation (bits/s). The ceiling is the codec
/// default; the floor stays intelligible for speech/music while cutting the
/// link cost roughly in half at each step.
pub(super) const BITRATE_MIN: u32 = 24_000;
pub(super) const BITRATE_STEP: u32 = 24_000;

/// Bitrate a stream *starts* at, before any ack has proved what the link can
/// carry.
///
/// Deliberately below the codec default. Rate control is reactive: it needs an
/// ack ([`ACK_INTERVAL`], 250 ms) and then an adaptation tick
/// ([`ADAPT_INTERVAL`], 500 ms) before it can react at all, and it steps down
/// one [`BITRATE_STEP`] at a time — so starting at the ceiling means every
/// stream floods the link for the first seconds of a session, all at once,
/// while rate control slowly walks each one down. Input shares this channel, so
/// that opening burst is felt as mouse lag exactly when the user is reconnecting
/// and most likely to be moving the pointer.
///
/// Starting low inverts the risk: the first seconds are quiet-but-cheap, and the
/// climb back to [`DEFAULT_BITRATE`] happens within a few adaptation ticks once
/// acks show the link is keeping up. Under-shooting briefly costs some audio
/// quality; over-shooting costs pointer responsiveness, which is worse.
///
/// [`DEFAULT_BITRATE`]: cursio_audio::codec::DEFAULT_BITRATE
pub(super) const BITRATE_START: u32 = 48_000;

/// Most chunks to send in one [`SessionModule::pump`] call, across **all**
/// streams together.
///
/// This used to be a per-*stream* limit of 4, which stops limiting anything as
/// streams multiply: mirroring a Windows box with 14 audio endpoints meant up to
/// ~80 sends between two polls of the input channel. Since input shares this
/// one encrypted channel, that was felt directly as mouse lag — the session loop
/// cannot reach its `select!` while a module is pumping.
///
/// A whole-pump budget keeps the loop responsive no matter how many devices are
/// mirrored, so no functionality has to be given up to stay fast.
/// Anything beyond this waits for the next pump — it is *not* discarded (see
/// the pump body). Sized to carry a handful of genuinely active streams at
/// their natural rate while still returning to the session loop promptly.
pub(super) const MAX_CHUNKS_PER_PUMP: usize = 32;

/// How many queued chunks a stream may hold before the excess counts as stale.
///
/// Chosen to sit just above what one pump can send ([`MAX_CHUNKS_PER_PUMP`]), so
/// a stream running at its natural rate is never trimmed — only one that has
/// genuinely fallen behind. At ~256-frame chunks this is roughly 250 ms of
/// audio, the same order as the playback jitter ceiling.
pub(super) const MAX_CHUNK_BACKLOG: usize = 48;

/// Pop one queued packet only when the current pump can send it.
///
/// Checking the budget before `pop_front` is important: the capture channel
/// cannot put an item back, so taking first and discovering a zero budget later
/// silently lost audio at every boundary.
pub(super) fn take_with_budget<T>(queue: &mut VecDeque<T>, budget: &mut usize) -> Option<T> {
    if *budget == 0 {
        return None;
    }
    let item = queue.pop_front()?;
    *budget -= 1;
    Some(item)
}

/// Drop all but the newest `keep` chunks waiting in `rx`, returning how many
/// were discarded.
///
/// An unbounded capture channel turns a *rate* mismatch into unbounded *latency*
/// exactly like the playback queue did: whatever the pump cannot send simply
/// accumulates, and every later chunk is sent that much later. Trimming from the
/// front bounds the age of what we do send.
///
/// Note this drains and re-queues nothing — `try_recv` already removes from the
/// front, so discarding the front *is* keeping the tail.
pub(super) fn drain_stale(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::AudioChunk>,
    keep: usize,
) -> usize {
    let backlog = rx.len();
    if backlog <= keep {
        return 0;
    }
    let excess = backlog - keep;
    let mut dropped = 0;
    for _ in 0..excess {
        if rx.try_recv().is_err() {
            break;
        }
        dropped += 1;
    }
    dropped
}

/// Decide a stream's next Opus bitrate from its current rate and its sent-vs-
/// acked backlog (in chunks).
///
/// Hysteretic degrade-not-buffer: a backlog over [`BACKLOG_HIGH`] steps down
/// toward [`BITRATE_MIN`]; a link caught up under [`BACKLOG_LOW`] steps back up
/// toward the codec default; in between it holds. Pulled out of
/// [`AudioEngine::adapt_bitrate`] so the thresholds are unit-testable without a
/// live session.
pub(super) fn next_bitrate(current: u32, gap: u32) -> u32 {
    if gap > BACKLOG_HIGH {
        current.saturating_sub(BITRATE_STEP).max(BITRATE_MIN)
    } else if gap < BACKLOG_LOW {
        (current + BITRATE_STEP).min(cursio_audio::codec::DEFAULT_BITRATE)
    } else {
        current
    }
}

impl AudioEngine {
    /// The peer acked the newest chunk it has played for one of our streams.
    ///
    /// Feedback for adaptive rate control: the sent-vs-acked gap tells us the
    /// link is falling behind. The rule is the design's — **degrade rather than
    /// buffer unboundedly** — so a sustained backlog drops the stream's Opus
    /// bitrate; a recovered link lets it climb back. (Applied on the encoder in
    /// `adapt_bitrate`; here we only retire what the ack covers.)
    pub(super) fn on_ack(&mut self, ack: &cursio_protocol::AudioFrameAck) {
        self.acked_seq.insert(ack.id, ack.acked_seq);
        if let Some(unacked) = self.unacked.get_mut(&ack.id) {
            while unacked.front().is_some_and(|&seq| seq <= ack.acked_seq) {
                unacked.pop_front();
            }
        }
    }

    /// Return a sparse [`AudioFrameAck`] for each inbound stream that has moved
    /// since we last acked it, at most once per [`ACK_INTERVAL`].
    ///
    /// This is the receiver half of rate control: the sender compares these acks
    /// against what it has sent to gauge how far behind the link is. Sent from
    /// `pump` (not `on_chunk`) because that is where `tx` lives and where the
    /// cadence is naturally throttled.
    pub(super) async fn send_acks(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        let now = std::time::Instant::now();
        // Collect first to avoid borrowing `recv_seq` across the await.
        let due: Vec<(u32, u32)> = self
            .recv_seq
            .iter()
            .filter(|(id, _)| {
                self.last_ack_sent
                    .get(id)
                    .is_none_or(|t| now.duration_since(*t) >= ACK_INTERVAL)
            })
            .map(|(&id, &seq)| (id, seq))
            .collect();
        for (id, acked_seq) in due {
            self.last_ack_sent.insert(id, now);
            send_control(
                tx,
                AudioMsg::FrameAck(cursio_protocol::AudioFrameAck { id, acked_seq }),
            )
            .await?;
        }
        Ok(())
    }

    /// Re-evaluate each outbound stream's Opus bitrate from its sent-vs-acked gap.
    ///
    /// The design's rule is **degrade rather than buffer unboundedly**: a
    /// sustained backlog (chunks sent but unacknowledged, over [`BACKLOG_HIGH`])
    /// steps the bitrate down; a link that has stayed caught up (under
    /// [`BACKLOG_LOW`]) for [`BITRATE_RAISE_AFTER`] evaluations steps it back up
    /// toward the default. A change is applied to the running encoder, never by
    /// replacing it: the peer's decoder carries the old encoder's state, and a
    /// fresh encoder's first packets decode against that state as a loud
    /// burst. Runs at most once per [`ADAPT_INTERVAL`] per stream, and only for
    /// a stream that sent something since the last look.
    pub(super) fn adapt_bitrate(&mut self) {
        let now = std::time::Instant::now();
        // Only streams we're actively sending have an `unacked` entry.
        let ids: Vec<u32> = self.unacked.keys().copied().collect();
        for id in ids {
            let due = self
                .last_adapt
                .get(&id)
                .is_none_or(|t| now.duration_since(*t) >= ADAPT_INTERVAL);
            if !due || !self.sent_since_adapt.contains(&id) {
                continue;
            }
            // No ack yet ⇒ assume the link is fine and leave the rate alone; a
            // missing ack is not evidence of congestion.
            if !self.acked_seq.contains_key(&id) {
                continue;
            }
            self.last_adapt.insert(id, now);
            self.sent_since_adapt.remove(&id);
            let gap = self.unacked.get(&id).map_or(0, |q| q.len()) as u32;

            let current = self
                .stream_bitrate
                .get(&id)
                .copied()
                .unwrap_or(BITRATE_START);
            let streak = self.caught_up_streak.entry(id).or_insert(0);
            *streak = if gap < BACKLOG_LOW { *streak + 1 } else { 0 };
            let mut target = next_bitrate(current, gap);
            if target > current {
                if *streak < BITRATE_RAISE_AFTER {
                    target = current;
                } else {
                    *streak = 0;
                }
            }

            if target != current {
                // No encoder yet means nothing to change: `pump` builds the
                // first one at whatever `stream_bitrate` holds.
                if let Some(encoder) = self.encoders.get_mut(&id) {
                    if let Err(e) = encoder.set_bitrate(target) {
                        tracing::warn!(error = %e, id, from = current, to = target,
                            "cannot change the audio bitrate; keeping the current one");
                        continue;
                    }
                }
                tracing::info!(
                    id,
                    gap,
                    from = current,
                    to = target,
                    "adapting audio bitrate"
                );
                self.stream_bitrate.insert(id, target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_session::test_support::ack;
    use crate::audio_session::test_support::engine;
    use crate::audio_session::test_support::queued;
    use crate::audio_session::test_support::send_and_adapt;

    /// Streams must not open at the codec ceiling: rate control cannot react for
    /// the first ~750 ms, so every stream starting at maximum is what floods the
    /// shared channel — felt as mouse lag — right after connecting.
    #[test]
    fn streams_start_below_the_ceiling() {
        const {
            assert!(
                BITRATE_START < cursio_audio::codec::DEFAULT_BITRATE,
                "opening bitrate must leave headroom for input"
            );
            assert!(
                BITRATE_START >= BITRATE_MIN,
                "opening bitrate should not start below the adaptation floor"
            );
        }
    }

    /// A healthy link must still reach full quality — starting low is a ramp, not
    /// a permanent cap.
    #[test]
    fn a_quiet_link_climbs_back_to_full_quality() {
        let mut rate = BITRATE_START;
        for _ in 0..10 {
            rate = next_bitrate(rate, 0);
        }
        assert_eq!(
            rate,
            cursio_audio::codec::DEFAULT_BITRATE,
            "a caught-up link should recover the codec default"
        );
    }

    /// A stream keeping up must never be trimmed — trimming healthy audio would
    /// be heard as dropouts.
    #[test]
    fn backlog_within_budget_is_left_alone() {
        let mut rx = queued(MAX_CHUNK_BACKLOG as u32);
        assert_eq!(drain_stale(&mut rx, MAX_CHUNK_BACKLOG), 0);
        assert_eq!(
            rx.len(),
            MAX_CHUNK_BACKLOG,
            "nothing should have been dropped"
        );
    }

    /// A stream that fell behind keeps only the NEWEST chunks: the stale head is
    /// what costs link capacity (and therefore mouse latency) for audio too old
    /// to be wanted.
    #[test]
    fn stale_backlog_drops_oldest_and_keeps_newest() {
        let total = MAX_CHUNK_BACKLOG as u32 + 20;
        let mut rx = queued(total);

        let dropped = drain_stale(&mut rx, MAX_CHUNK_BACKLOG);
        assert_eq!(dropped, 20, "should discard exactly the excess");
        assert_eq!(rx.len(), MAX_CHUNK_BACKLOG);

        // The survivor at the front must be the 21st chunk — everything older is
        // gone, and the newest is still queued.
        let front = rx.try_recv().expect("a chunk should remain");
        assert_eq!(front.seq, 20, "kept the wrong end of the queue");
    }

    #[test]
    fn exhausted_send_budget_keeps_encoded_audio_queued() {
        let mut packets = VecDeque::from([1, 2, 3]);
        let mut budget = 0;

        assert_eq!(take_with_budget(&mut packets, &mut budget), None);
        assert_eq!(packets, VecDeque::from([1, 2, 3]));

        budget = 2;
        assert_eq!(take_with_budget(&mut packets, &mut budget), Some(1));
        assert_eq!(take_with_budget(&mut packets, &mut budget), Some(2));
        assert_eq!(take_with_budget(&mut packets, &mut budget), None);
        assert_eq!(packets, VecDeque::from([3]));
    }

    /// A sustained backlog must lower the bitrate; a caught-up link must raise it;
    /// a mid-range gap must hold. This is the degrade-not-buffer rule in numbers.
    #[test]
    fn bitrate_adapts_with_backlog() {
        let def = cursio_audio::codec::DEFAULT_BITRATE;

        // Congested: step down, but never below the floor.
        let lower = next_bitrate(def, BACKLOG_HIGH + 1);
        assert!(lower < def, "a big backlog must reduce the bitrate");
        assert!(lower >= BITRATE_MIN, "must not drop below the floor");

        // Caught up: climb back, but never above the default ceiling.
        let higher = next_bitrate(lower, 0);
        assert!(higher > lower, "a cleared backlog must raise the bitrate");
        assert!(higher <= def, "must not exceed the default ceiling");

        // In the hysteresis band: hold, so it doesn't chatter on jitter.
        let held = next_bitrate(def, (BACKLOG_LOW + BACKLOG_HIGH) / 2);
        assert_eq!(
            held, def,
            "a mid-range gap must leave the bitrate unchanged"
        );
    }

    /// The floor and ceiling must be hard stops even after repeated steps.
    #[test]
    fn bitrate_saturates_at_floor_and_ceiling() {
        let def = cursio_audio::codec::DEFAULT_BITRATE;
        let mut b = def;
        for _ in 0..20 {
            b = next_bitrate(b, BACKLOG_HIGH + 1);
        }
        assert_eq!(
            b, BITRATE_MIN,
            "repeated congestion must settle exactly at the floor"
        );
        for _ in 0..20 {
            b = next_bitrate(b, 0);
        }
        assert_eq!(
            b, def,
            "repeated recovery must settle exactly at the default"
        );
    }

    /// Silence between two sounds is not a backlog.
    ///
    /// The capture's seq keeps counting through silence, whose chunks are
    /// dropped rather than sent, so the first chunk after a pause carries a seq
    /// far past the last one acknowledged. Read as `last_sent - acked`, 10 000
    /// chunks of silence between two videos looked like 50 s of unplayed audio,
    /// and the stream was stepped to its floor the moment sound came back.
    #[test]
    fn a_long_silence_followed_by_audio_does_not_lower_the_bitrate() {
        let mut e = engine();
        let def = cursio_audio::codec::DEFAULT_BITRATE;
        e.stream_bitrate.insert(7, def);

        send_and_adapt(&mut e, 7, 0..50);
        ack(&mut e, 7, 49);

        // Resume after 10 000 silent chunks; nothing new acknowledged yet.
        for tick in 0..6 {
            send_and_adapt(&mut e, 7, 10_050 + tick * 10..10_060 + tick * 10);
            assert_eq!(
                e.stream_bitrate.get(&7),
                Some(&def),
                "the first seconds after a pause must not read as a backlog"
            );
        }
    }

    /// A real backlog still steps the rate down, once per new piece of
    /// evidence rather than once per tick.
    #[test]
    fn a_real_backlog_lowers_the_bitrate_once_per_evaluation_with_new_sends() {
        let mut e = engine();
        let def = cursio_audio::codec::DEFAULT_BITRATE;
        e.stream_bitrate.insert(7, def);
        ack(&mut e, 7, 0);

        send_and_adapt(&mut e, 7, 1..BACKLOG_HIGH + 50);
        let lowered = *e.stream_bitrate.get(&7).unwrap();
        assert!(
            lowered < def,
            "a backlog over the threshold must shed bitrate"
        );

        // Nothing sent since: the same evidence must not be judged again.
        e.last_adapt.clear();
        e.adapt_bitrate();
        assert_eq!(
            e.stream_bitrate.get(&7),
            Some(&lowered),
            "an unchanged backlog must not step the rate down a second time"
        );
    }

    /// The rate climbs back only after the link has stayed caught up.
    #[test]
    fn bitrate_climbs_only_after_the_link_stays_caught_up() {
        let mut e = engine();
        e.stream_bitrate.insert(7, BITRATE_START);
        ack(&mut e, 7, 0);

        let mut seq = 1;
        for tick in 1..=BITRATE_RAISE_AFTER {
            send_and_adapt(&mut e, 7, seq..seq + 5);
            ack(&mut e, 7, seq + 4);
            seq += 5;
            let expected = if tick < BITRATE_RAISE_AFTER {
                BITRATE_START
            } else {
                BITRATE_START + BITRATE_STEP
            };
            assert_eq!(
                e.stream_bitrate.get(&7),
                Some(&expected),
                "tick {tick}: one calm sample must not raise the rate"
            );
        }
    }
}
