//! The send side: numbering each frame, keeping what may need resending, and
//! choosing the path that carries it.

use crate::crypto::link_tag;
use crate::lane::Lane;
use crate::priority::{FrameSink, OutFrame};
use crate::resume::frame::ack_frame;
use crate::resume::frame::read_u64;
use crate::resume::frame::seq_frame;
use crate::resume::frame::ACK_ALL_PATHS;
use crate::resume::inner::Inner;
use crate::resume::state::Kept;
use crate::resume::PathKind;
use crate::resume::RESEND_CAP;
use crate::TransportError;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// The drainer's end of a resumable link: numbers each frame, keeps the
/// reliable ones, and writes on whichever path carries the session.
pub(super) struct ResumableSink {
    pub(super) inner: Arc<Inner>,
    pub(super) lane: Lane,
    pub(super) last_path: Option<u64>,
    pub(super) next_bulk: Instant,
}

#[async_trait]
impl FrameSink for ResumableSink {
    async fn send_frame(&mut self, out: OutFrame) -> Result<(), TransportError> {
        debug_assert_eq!(Lane::for_priority(out.priority), self.lane);
        let ack = out.tagged.first() == Some(&link_tag::ACK);
        if !ack && !out.lossy {
            loop {
                let changed = self.inner.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let state = self.inner.lock();
                    if let Some(why) = &state.ended {
                        return Err(TransportError::Other(why.clone()));
                    }
                    if state.lanes[self.lane as usize].unacked_bytes
                        + out.tagged.len()
                        + link_tag::SEQ_HEADER_LEN
                        <= RESEND_CAP
                    {
                        break;
                    }
                }
                changed.await;
            }
        }
        let now = Instant::now();
        let mut writes = Vec::new();
        {
            let mut state = self.inner.lock();
            if let Some(why) = &state.ended {
                return Err(TransportError::Other(why.clone()));
            }
            let carrying = state
                .carrying(self.lane, now)
                .map(|p| (p.id, p.kind, p.writer(self.lane).clone()));
            if let Some((id, _, writer)) = &carrying {
                if self.last_path != Some(*id) {
                    self.last_path = Some(*id);
                    for kept in &mut state.lanes[self.lane as usize].unacked {
                        kept.sent_at = None;
                        kept.sent_on = None;
                        writes.push((writer.clone(), *id, kept.frame.clone()));
                    }
                }
            } else {
                self.last_path = None;
            }
            if ack {
                if self.lane == Lane::Interactive {
                    let flags = out.tagged.get(1 + Lane::COUNT * 8).copied().unwrap_or(0);
                    let frame = ack_frame(state.delivered(), flags);
                    state.ack_due = false;
                    let bluetooth_active = Lane::ALL.into_iter().any(|lane| {
                        state
                            .carrying(lane, now)
                            .is_some_and(|p| p.kind == PathKind::Bluetooth)
                    });
                    // Cumulative snapshots may be retried, without waiting on a
                    // full data writer. A Bluetooth standby stays quiet.
                    for path in &state.paths {
                        if path.kind != PathKind::Bluetooth
                            || bluetooth_active
                            || flags & ACK_ALL_PATHS != 0
                        {
                            let _ = path.control().try_send(frame.clone());
                        }
                    }
                }
            } else {
                if self.lane == Lane::Bulk {
                    state.bulk_active = Some(now);
                }
                let delivery = &mut state.lanes[self.lane as usize];
                delivery.next_seq += 1;
                let frame = seq_frame(self.lane, delivery.next_seq, out.lossy, &out.tagged);
                if !out.lossy {
                    delivery.unacked_bytes += frame.len();
                    delivery.unacked.push_back(Kept {
                        seq: delivery.next_seq,
                        frame: frame.clone(),
                        sent_at: None,
                        sent_on: None,
                    });
                }
                match carrying {
                    Some((_, PathKind::Bluetooth, _)) if out.lossy && self.lane == Lane::Video => {}
                    Some((id, _, writer)) => writes.push((writer, id, frame)),
                    None => {}
                }
            }
        }
        for (writer, id, frame) in writes {
            if self.lane == Lane::Bulk {
                // Only when the slot is still ahead: a zero-length sleep is
                // not free. Tokio rounds a deadline up to its next millisecond
                // and fires it on a turn of its timer, which Windows takes on
                // its 15.6 ms tick, so sleeping before every frame "for no
                // time" held a Windows machine's bulk near one 64 KiB frame
                // per tick even with the pacer wide open (measured: 7.5 MB/s
                // out of a link carrying 25).
                if self.next_bulk > Instant::now() {
                    tokio::time::sleep_until(self.next_bulk).await;
                }
                let rate = self.inner.congestion.lock().unwrap().bytes_per_second();
                self.next_bulk = next_bulk_slot(self.next_bulk, Instant::now(), frame.len(), rate);
                self.inner.lock().bulk_active = Some(Instant::now());
            }
            if let Some(seq) = frame.get(2..).and_then(read_u64) {
                if let Some(kept) = self.inner.lock().lanes[self.lane as usize]
                    .unacked
                    .iter_mut()
                    .find(|kept| kept.seq == seq)
                {
                    kept.sent_at = Some(Instant::now());
                    kept.sent_on = Some(id);
                }
            }
            if writer.send(frame).await.is_err() {
                self.inner.path_failed(id, "its writer stopped");
            }
        }
        Ok(())
    }
}

/// How far a bulk frame's slot may lag the clock and still be caught up on.
///
/// Slots used to be counted from the moment the pacer woke, so whatever a
/// sleep overran was lost for good — and Windows wakes a sleeper on its
/// 15.6 ms system tick, so every 64 KiB frame cost at least one tick: bulk
/// from a Windows machine never passed 64 KiB / 15.6 ms ≈ 4 MB/s, whatever
/// the rate allowed and the link carried (measured: 3.4–4.0 MB/s against
/// 25 MB/s of plain TCP on the same Wi-Fi). Counting from the previous slot
/// carries the overrun into the next frames instead. Bounded, so a link that
/// sat idle does not bank an allowance and dump it at once: just over one
/// Windows tick.
const BULK_CATCH_UP: Duration = Duration::from_millis(20);

/// When the bulk frame after one of `len` bytes may go, at `rate` bytes a
/// second, the previous one having been due at `due` and sent at `now`.
fn next_bulk_slot(due: Instant, now: Instant, len: usize, rate: f64) -> Instant {
    let from = due.max(now.checked_sub(BULK_CATCH_UP).unwrap_or(now));
    from + Duration::from_secs_f64(len as f64 / rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sleep_that_overran_its_tick_does_not_cost_bulk_its_rate() {
        // 64 KiB frames at 20 MB/s are due every 3.3 ms; a Windows sleep wakes
        // 15.6 ms late each time. Over a second the frames sent must still
        // carry about the rate, not one frame per tick.
        let start = Instant::now();
        let rate = 20_000_000.0;
        let (mut due, mut now, mut sent) = (start, start, 0usize);
        while now.duration_since(start) < Duration::from_secs(1) {
            if due > now {
                now += Duration::from_micros(15_600);
            }
            sent += 65_536;
            due = next_bulk_slot(due, now, 65_536, rate);
        }
        assert!(sent as f64 > rate * 0.9, "{sent} bytes in a second");
    }

    #[test]
    fn an_idle_link_banks_no_more_than_a_tick_of_bulk() {
        let start = Instant::now();
        let later = start + Duration::from_secs(5);
        let due = next_bulk_slot(start, later, 65_536, 1_000_000.0);
        assert!(due >= later - BULK_CATCH_UP);
    }
}
