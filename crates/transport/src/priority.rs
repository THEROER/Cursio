//! The channel's scheduler as Cursio's [`TransportTx`]: frames queued by
//! [`Message::priority`], sealed and written by a background drainer, so a
//! bulk transfer cannot stall the mouse. The scheduling itself — budgets,
//! chunking, the starvation guard, expiry, folding motion — is
//! `cursio_channel::scheduler`'s.

use std::time::Duration;

use async_trait::async_trait;
use cursio_channel::{Carrying, Classes, Window};
use cursio_protocol::{Message, Priority};

use crate::{QueueDelay, QueueDepth, TransportError, TransportTx};

/// The prioritized sender every encrypted link and session sends through.
pub type PrioritizedTx = cursio_channel::PrioritizedTx<Message>;

/// Input p99 at which a window is logged at `info` rather than `debug`.
///
/// Half a frame at 120–144 Hz: below it the pointer lands in the frame it was
/// moved in; above it the user starts to see it trail.
const INPUT_DELAY_NOTICE: Duration = Duration::from_millis(4);

/// Report a finished delay window, if input moved during it, and what was
/// dropped for age, if anything was.
///
/// A window with no input frames says nothing about the pointer and is not
/// logged: an idle link would otherwise write a line every ten seconds
/// forever. Video and bulk ride along because they are what input waits
/// behind, and seeing them deep while input stays shallow is the evidence
/// that priority is holding.
pub(crate) fn log_window(_: &Classes, window: &Window) {
    let expired: u32 = window.expired.iter().sum();
    if expired > 0 {
        tracing::info!(
            expired,
            audio = window.expired[Priority::Audio.class()],
            video = window.expired[Priority::Video.class()],
            "dropped messages too old to be worth sending; the link stalled"
        );
    }
    let window = &window.delay;
    let input = window[Priority::Input.class()];
    if input.frames == 0 {
        return;
    }
    let video = window[Priority::Video.class()];
    let bulk = window[Priority::Bulk.class()];
    if input.p99 >= INPUT_DELAY_NOTICE {
        tracing::info!(
            input_frames = input.frames,
            input_p50 = ?input.p50,
            input_p99 = ?input.p99,
            input_max = ?input.max,
            video_p99 = ?video.p99,
            bulk_p99 = ?bulk.p99,
            "input waited long enough to be felt before reaching the socket"
        );
    } else {
        tracing::debug!(
            input_frames = input.frames,
            input_p50 = ?input.p50,
            input_p99 = ?input.p99,
            input_max = ?input.max,
            video_p99 = ?video.p99,
            bulk_p99 = ?bulk.p99,
            "send queue delay"
        );
    }
}

#[async_trait]
impl TransportTx for PrioritizedTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        Ok(PrioritizedTx::send(self, msg).await?)
    }

    fn queue_age(&self, priority: Priority) -> Duration {
        PrioritizedTx::queue_age(self, priority.class())
    }

    fn queue_depth(&self, priority: Priority) -> QueueDepth {
        PrioritizedTx::queue_depth(self, priority.class())
    }

    fn queue_delay(&self, priority: Priority) -> QueueDelay {
        PrioritizedTx::queue_delay(self, priority.class())
    }

    fn clone_sender(&self) -> Option<Box<dyn TransportTx>> {
        Some(Box::new(self.clone()))
    }

    fn holding(&self) -> bool {
        PrioritizedTx::holding(self)
    }

    fn carrying_path(&self) -> Option<&'static str> {
        PrioritizedTx::carrying_path(self).map(|carrying| match carrying {
            Carrying::Standby => crate::stream::BLUETOOTH,
            Carrying::Network | Carrying::Direct => "network",
        })
    }

    fn note_input(&self) {
        PrioritizedTx::note_input(self);
    }

    async fn flush(&mut self) {
        PrioritizedTx::flush(self).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::EncryptedTransport;
    use crate::pairing::Role;
    use crate::test_util::MemTransport;
    use crate::{Transport, TransportRx};
    use cursio_protocol::InputEvent;

    async fn pair() -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        let (a, b) = MemTransport::pair();
        let (init, resp) = tokio::join!(
            EncryptedTransport::establish(a, b"k", Role::Initiator),
            EncryptedTransport::establish(b, b"k", Role::Responder)
        );
        let (tx, _) = Box::new(init.unwrap()).split_prioritized();
        let (_, rx) = Box::new(resp.unwrap()).split();
        (tx, rx)
    }

    /// Cursio's messages through the channel's scheduler arrive as sent,
    /// queued in their own priority's class.
    #[tokio::test]
    async fn messages_are_scheduled_by_their_priority_and_arrive_intact() {
        let (mut tx, mut rx) = pair().await;
        let bulk = Message::Opaque(vec![0u8; 800_000]);
        let input = Message::Input(InputEvent::PointerMotion { dx: 5, dy: -3 });
        tx.send(&bulk).await.unwrap();
        tx.send(&input).await.unwrap();
        tx.flush().await;
        assert!(tx.queue_depth(Priority::Bulk).sent_bytes >= 800_000);
        assert_eq!(tx.queue_depth(Priority::Audio), QueueDepth::default());
        let got = [rx.recv().await.unwrap(), rx.recv().await.unwrap()];
        assert!(got.contains(&input));
        assert!(got.contains(&bulk));
    }

    #[tokio::test]
    async fn a_cloned_sender_shares_the_queue_until_the_last_handle_drops() {
        let (tx, mut rx) = pair().await;
        let mut clone = tx.clone_sender().expect("a prioritized sender clones");
        drop(tx);
        clone.send(&Message::Heartbeat { nonce: 7 }).await.unwrap();
        clone.flush().await;
        assert_eq!(rx.recv().await.unwrap(), Message::Heartbeat { nonce: 7 });
        drop(clone);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("the link must close once the last handle is gone")
                .is_err()
        );
    }

    /// A link over one connection has no path to name.
    #[tokio::test]
    async fn a_link_over_one_connection_neither_holds_nor_names_a_path() {
        let (tx, _rx) = pair().await;
        assert!(!tx.holding());
        assert_eq!(tx.carrying_path(), None);
    }
}
