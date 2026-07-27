//! Priority multiplexing for the shared encrypted channel.
//!
//! Everything — input, audio, window pixels, clipboard, files — rides one Noise
//! link. Without scheduling, a large payload (a clipboard image, a file) sent as
//! many back-to-back frames would sit in front of the next mouse event and the
//! pointer would visibly lag. [`PrioritizedTx`] fixes that: [`send`] does not
//! write to the socket, it *chunks and queues* the message by
//! [`Message::priority`] and returns immediately. A background **drainer** owns
//! the real socket and always emits a frame from the highest non-empty class,
//! re-checking between every frame — so an input frame queued while a bulk
//! transfer is draining jumps ahead of that transfer's remaining chunks.
//!
//! [`send`]: cursio_transport::TransportTx::send
//!
//! ## Why chunk-then-queue plaintext (not sealed frames)
//!
//! Noise nonces must advance in exactly the order ciphertext hits the wire. So
//! we queue *tagged plaintext* pieces (via [`chunk_plaintext`]) and let the
//! drainer seal each one the instant before it sends it — never sooner. Sealing
//! at `send` time would fix the nonce order at enqueue time and interleaving
//! would corrupt the stream.
//!
//! ## Latency vs. buffering
//!
//! Queues are unbounded but drained continuously, so steady traffic (input,
//! audio) never accumulates — latency stays minimal, which is the whole point.
//! Only a genuine bulk transfer builds depth, and only in its own low-priority
//! class where it can't delay anything above it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cursio_protocol::{Message, Priority};
use tokio::sync::Notify;

use crate::crypto::{chunk_plaintext, seal_frame, SharedNoise};
use crate::{TransportError, TransportTx};

/// Number of priority classes (see [`Priority`]). Kept as an array index space.
const CLASS_COUNT: usize = 5;

/// Map a [`Priority`] to a queue index, highest priority = lowest index so the
/// drainer can scan `0..` and stop at the first non-empty class.
fn class_index(p: Priority) -> usize {
    match p {
        Priority::Control => 0,
        Priority::Input => 1,
        Priority::Audio => 2,
        Priority::Video => 3,
        Priority::Bulk => 4,
    }
}

/// The per-class frame queues, shared between the [`PrioritizedTx`] handles and
/// the drainer task. Each entry is one tagged-plaintext frame awaiting sealing.
#[derive(Default)]
struct Queues {
    classes: [VecDeque<Vec<u8>>; CLASS_COUNT],
    /// Set once the sender side is gone *and* all queues are empty, so the
    /// drainer knows to exit cleanly rather than park forever.
    closed: bool,
}

impl Queues {
    /// Pop the next frame to send: the front of the highest-priority non-empty
    /// class. `None` when everything is empty.
    fn pop_highest(&mut self) -> Option<Vec<u8>> {
        self.classes.iter_mut().find_map(VecDeque::pop_front)
    }

    /// True when every class is empty — used by `flush` to know the drainer has
    /// nothing left to write.
    fn pop_is_empty(&self) -> bool {
        self.classes.iter().all(VecDeque::is_empty)
    }
}

/// Shared scheduler state: the queues plus notifiers for both directions —
/// `wake` rouses the drainer when work arrives or the sender closes; `drained`
/// lets `flush` wait until the drainer has emptied the queues.
struct Shared {
    queues: Mutex<Queues>,
    wake: Notify,
    drained: Notify,
}

/// A [`TransportTx`] that queues by priority instead of writing synchronously.
///
/// Cloneable is not needed — the session holds exactly one — but it is cheap to
/// hand out: all real work lives in the drainer task spawned by [`Self::spawn`].
pub struct PrioritizedTx {
    shared: Arc<Shared>,
}

impl PrioritizedTx {
    /// Wrap a raw inner tx + Noise state in a prioritized sender, spawning the
    /// drainer task that owns the socket. Returns the sender the session sends
    /// through; the drainer runs until the sender is dropped and the queues
    /// empty (or the socket errors).
    pub fn spawn(mut inner: Box<dyn TransportTx>, noise: SharedNoise) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            wake: Notify::new(),
            drained: Notify::new(),
        });
        let drain_shared = shared.clone();

        tokio::spawn(async move {
            loop {
                // Take the next frame under the lock, then release it before any
                // await (sealing is CPU-only; the socket write is the await).
                let (frame, closed) = {
                    let mut q = drain_shared.queues.lock().unwrap();
                    match q.pop_highest() {
                        Some(f) => (Some(f), false),
                        None => (None, q.closed),
                    }
                };

                match frame {
                    Some(tagged) => {
                        let sealed = match seal_frame(&noise, &tagged) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::warn!(error = %e, "sealing a queued frame failed");
                                continue;
                            }
                        };
                        if let Err(e) = inner.send(&sealed).await {
                            // The link is gone; nothing more can be sent. Stop.
                            tracing::debug!(error = %e, "prioritized drainer: link closed");
                            break;
                        }
                    }
                    None => {
                        // Queues are empty: wake anyone blocked in `flush`.
                        drain_shared.drained.notify_waiters();
                        if closed {
                            break; // sender dropped and nothing left to flush
                        }
                        // Nothing to do: park until woken by a send or close.
                        drain_shared.wake.notified().await;
                    }
                }
            }
        });

        PrioritizedTx { shared }
    }
}

#[async_trait]
impl TransportTx for PrioritizedTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        // Chunk to tagged-plaintext frames (may be one, or many for a big
        // payload), then drop them into this message's priority class. This is
        // the only work `send` does — no socket, no crypto — so the caller (the
        // session loop) returns at once and never blocks behind a bulk transfer.
        let idx = class_index(msg.priority());
        let frames = chunk_plaintext(msg)?;
        {
            let mut q = self.shared.queues.lock().unwrap();
            for f in frames {
                q.classes[idx].push_back(f);
            }
        }
        self.shared.wake.notify_one();
        Ok(())
    }

    async fn flush(&mut self) {
        loop {
            // Register interest *before* checking, so a drain that completes
            // between the check and the await can't slip past us.
            let drained = self.shared.drained.notified();
            let empty = {
                let q = self.shared.queues.lock().unwrap();
                q.pop_is_empty()
            };
            if empty {
                return;
            }
            // Make sure the drainer is running, then wait for it to empty the
            // queues (or for the link to die, which also ends draining).
            self.shared.wake.notify_one();
            drained.await;
        }
    }
}

impl Drop for PrioritizedTx {
    fn drop(&mut self) {
        // Mark closed so the drainer exits once it has flushed what's queued,
        // then wake it in case it's parked.
        if let Ok(mut q) = self.shared.queues.lock() {
            q.closed = true;
        }
        self.shared.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_order_is_control_first_bulk_last() {
        // The drainer scans indices ascending; assert the mapping puts the most
        // urgent class first and the bulk class last.
        assert_eq!(class_index(Priority::Control), 0);
        assert!(class_index(Priority::Input) < class_index(Priority::Audio));
        assert!(class_index(Priority::Audio) < class_index(Priority::Video));
        assert_eq!(class_index(Priority::Bulk), CLASS_COUNT - 1);
    }

    #[test]
    fn pop_highest_prefers_lower_index() {
        let mut q = Queues::default();
        const BULK: u8 = 0xBB;
        const INPUT: u8 = 0x11;
        const AUDIO: u8 = 0xAA;
        q.classes[class_index(Priority::Bulk)].push_back(vec![BULK]);
        q.classes[class_index(Priority::Input)].push_back(vec![INPUT]);
        q.classes[class_index(Priority::Audio)].push_back(vec![AUDIO]);
        // Input must come out before audio, audio before bulk.
        assert_eq!(q.pop_highest(), Some(vec![INPUT]));
        assert_eq!(q.pop_highest(), Some(vec![AUDIO]));
        assert_eq!(q.pop_highest(), Some(vec![BULK]));
        assert_eq!(q.pop_highest(), None);
    }

    // --- End-to-end scheduling + flush over the real encrypted transport. ---
    //
    // NOTE on what is *not* unit-tested: the headline property — an input frame
    // interleaving *between* the chunks of an in-flight bulk transfer — only
    // manifests when the socket applies backpressure (the drainer parks mid
    // transfer, letting a newly-queued input frame jump the remaining chunks).
    // The in-memory transport uses unbounded channels with no backpressure, so
    // the drainer would flush the whole bulk instantly and the interleave could
    // never be observed. Asserting it here would test the mock, not the
    // scheduler. The scheduler's ordering guarantee is covered by
    // `pop_highest_prefers_lower_index`; the interleave is verified live on a
    // real link. What we *can* pin down end-to-end is that priority scheduling
    // doesn't corrupt the stream and that flush works — below.

    use crate::crypto::EncryptedTransport;
    use crate::pairing::Role;
    use crate::test_util::MemTransport;
    use crate::Transport;
    use cursio_protocol::{InputEvent, Message};

    /// Priority-scheduled sends must still decode correctly on the far side: a
    /// bulk payload and an input event both round-trip intact through the
    /// prioritized tx + chunking + reassembly path.
    #[tokio::test]
    async fn prioritized_sends_roundtrip_intact() {
        let (a, b) = MemTransport::pair();
        let init = tokio::spawn(async move {
            EncryptedTransport::establish(a, b"k", Role::Initiator)
                .await
                .unwrap()
        });
        let resp = EncryptedTransport::establish(b, b"k", Role::Responder)
            .await
            .unwrap();
        let init = init.await.unwrap();

        let (mut tx, _u) = Box::new(init).split_prioritized();
        let (_u2, mut rx) = Box::new(resp).split();

        let bulk = Message::Opaque(vec![0u8; 800_000]); // ~13 frames
        let input = Message::Input(InputEvent::PointerMotion { dx: 5, dy: -3 });

        tx.send(&bulk).await.unwrap();
        tx.send(&input).await.unwrap();

        // Both arrive intact. (Order isn't asserted: without socket backpressure
        // the mock can't reproduce the interleave — see the note above.)
        let mut got = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
        got.sort_by_key(|m| format!("{m:?}").len());
        assert!(got.contains(&input), "input must round-trip");
        assert!(got.contains(&bulk), "bulk must round-trip");
    }

    /// `flush` must not return until queued frames have actually been sent, so a
    /// final `Bye` isn't lost when the session drops the tx.
    #[tokio::test]
    async fn flush_waits_for_queued_frames() {
        let (a, b) = MemTransport::pair();
        let init = tokio::spawn(async move {
            EncryptedTransport::establish(a, b"k", Role::Initiator)
                .await
                .unwrap()
        });
        let resp = EncryptedTransport::establish(b, b"k", Role::Responder)
            .await
            .unwrap();
        let init = init.await.unwrap();

        let (mut tx, _u) = Box::new(init).split_prioritized();
        let (_u2, mut rx) = Box::new(resp).split();

        tx.send(&Message::Bye).await.unwrap();
        tx.flush().await; // must have hit the wire by now

        // Receiver already has it available.
        assert_eq!(rx.recv().await.unwrap(), Message::Bye);
    }
}
