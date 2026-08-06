use std::time::{Duration, Instant};

use cursio_protocol::{WindowFrameAck, WindowId};

const ACK_INTERVAL: Duration = Duration::from_millis(250);

/// Coalesces presentation notifications and rate-limits feedback to the host.
///
/// Platform renderers call [`note_presented`](Self::note_presented) only after
/// their UI backend has accepted the updated framebuffer for painting.
pub(crate) struct PresentationAcks {
    id: WindowId,
    pending: Option<u32>,
    last_ack: Instant,
}

impl PresentationAcks {
    pub(crate) fn new(id: WindowId) -> Self {
        Self {
            id,
            pending: None,
            last_ack: Instant::now(),
        }
    }

    pub(crate) fn note_presented(&mut self, seq: u32) {
        self.pending = Some(match self.pending {
            Some(previous) => previous.max(seq),
            None => seq,
        });
    }

    pub(crate) fn take(&mut self) -> Option<WindowFrameAck> {
        self.take_at(Instant::now())
    }

    fn take_at(&mut self, now: Instant) -> Option<WindowFrameAck> {
        let seq = self.pending?;
        if now.duration_since(self.last_ack) < ACK_INTERVAL {
            return None;
        }

        self.last_ack = now;
        self.pending = None;
        Some(WindowFrameAck {
            id: self.id,
            acked_seq: seq,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_frames_are_not_acknowledged_before_presentation() {
        let started = Instant::now();
        let mut acks = PresentationAcks::new(7);
        acks.last_ack = started;

        assert!(acks.take_at(started + ACK_INTERVAL).is_none());
    }

    #[test]
    fn presented_frames_are_coalesced_and_rate_limited() {
        let started = Instant::now();
        let mut acks = PresentationAcks::new(7);
        acks.last_ack = started;

        acks.note_presented(3);
        acks.note_presented(5);
        assert!(acks
            .take_at(started + ACK_INTERVAL - Duration::from_millis(1))
            .is_none());

        assert_eq!(
            acks.take_at(started + ACK_INTERVAL),
            Some(WindowFrameAck {
                id: 7,
                acked_seq: 5,
            })
        );
        assert!(acks.take_at(started + ACK_INTERVAL * 2).is_none());
    }

    #[test]
    fn take_uses_the_current_clock() {
        let mut acks = PresentationAcks::new(9);
        acks.last_ack = Instant::now() - ACK_INTERVAL;
        acks.note_presented(11);

        assert_eq!(
            acks.take(),
            Some(WindowFrameAck {
                id: 9,
                acked_seq: 11,
            })
        );
    }
}
