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
                .map(|p| (p.id, p.kind, p.writer.clone()));
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
                            let _ = path.writer.try_send(frame.clone());
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
                tokio::time::sleep(self.next_bulk.saturating_duration_since(Instant::now())).await;
                let rate = self.inner.congestion.lock().unwrap().bytes_per_second();
                self.next_bulk =
                    Instant::now() + Duration::from_secs_f64(frame.len() as f64 / rate);
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
