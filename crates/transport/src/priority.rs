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
    /// Bytes waiting in each class, kept alongside the queues so a producer can
    /// read its own backlog without walking a `VecDeque` of frames.
    queued_bytes: [usize; CLASS_COUNT],
    /// Bytes of each class actually written to the socket, ever. Paired with
    /// `queued_bytes` in [`QueueDepth`]; see there for what it is for.
    sent_bytes: [u64; CLASS_COUNT],
    /// Set once the sender side is gone *and* all queues are empty, so the
    /// drainer knows to exit cleanly rather than park forever.
    closed: bool,
    /// Fatal drainer error. Once set, the socket can never accept another
    /// frame; producers must see an error instead of filling dead queues.
    terminal_error: Option<String>,
}

impl Queues {
    /// Queue one frame in `class`, keeping the byte count with it.
    fn push(&mut self, class: usize, frame: Vec<u8>) {
        self.queued_bytes[class] += frame.len();
        self.classes[class].push_back(frame);
    }

    /// Pop the next frame to send: the front of the highest-priority non-empty
    /// class. `None` when everything is empty.
    ///
    /// Returns the class alongside the frame so the drainer can credit the
    /// bytes to it once they are on the wire.
    fn pop_highest(&mut self) -> Option<(usize, Vec<u8>)> {
        let (idx, frame) = self
            .classes
            .iter_mut()
            .enumerate()
            .find_map(|(idx, q)| q.pop_front().map(|f| (idx, f)))?;
        // Leaving the queue is what "no longer waiting" means; the bytes are
        // credited as sent only after the socket takes them.
        self.queued_bytes[idx] -= frame.len();
        Some((idx, frame))
    }

    /// True when every class is empty — used by `flush` to know the drainer has
    /// nothing left to write.
    fn pop_is_empty(&self) -> bool {
        self.classes.iter().all(VecDeque::is_empty)
    }

    fn fail(&mut self, error: String) {
        for class in &mut self.classes {
            class.clear();
        }
        self.queued_bytes.fill(0);
        self.terminal_error = Some(error);
        self.closed = true;
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
                    Some((class, tagged)) => {
                        let len = tagged.len();
                        let sealed = match seal_frame(&noise, &tagged) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::warn!(error = %e, "sealing a queued frame failed");
                                drain_shared.queues.lock().unwrap().fail(e.to_string());
                                drain_shared.drained.notify_waiters();
                                break;
                            }
                        };
                        if let Err(e) = inner.send(&sealed).await {
                            // The link is gone; nothing more can be sent. Stop.
                            tracing::debug!(error = %e, "prioritized drainer: link closed");
                            drain_shared.queues.lock().unwrap().fail(e.to_string());
                            drain_shared.drained.notify_waiters();
                            break;
                        }
                        // Credited only now: a producer pacing itself against
                        // this rate must see bytes the socket has taken, not
                        // bytes we merely intended to write.
                        drain_shared.queues.lock().unwrap().sent_bytes[class] += len as u64;
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
            drain_shared.drained.notify_waiters();
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
            if let Some(error) = &q.terminal_error {
                return Err(TransportError::Other(error.clone()));
            }
            for f in frames {
                q.push(idx, f);
            }
        }
        self.shared.wake.notify_one();
        Ok(())
    }

    fn queue_depth(&self, priority: cursio_protocol::Priority) -> crate::QueueDepth {
        let idx = class_index(priority);
        let q = self.shared.queues.lock().unwrap();
        crate::QueueDepth {
            queued_bytes: q.queued_bytes[idx],
            sent_bytes: q.sent_bytes[idx],
        }
    }

    async fn flush(&mut self) {
        loop {
            // Register interest *before* checking, so a drain that completes
            // between the check and the await can't slip past us.
            let drained = self.shared.drained.notified();
            let done = {
                let q = self.shared.queues.lock().unwrap();
                q.pop_is_empty() || q.terminal_error.is_some()
            };
            if done {
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
        q.push(class_index(Priority::Bulk), vec![BULK]);
        q.push(class_index(Priority::Input), vec![INPUT]);
        q.push(class_index(Priority::Audio), vec![AUDIO]);
        // Input must come out before audio, audio before bulk.
        assert_eq!(q.pop_highest().map(|(_, f)| f), Some(vec![INPUT]));
        assert_eq!(q.pop_highest().map(|(_, f)| f), Some(vec![AUDIO]));
        assert_eq!(q.pop_highest().map(|(_, f)| f), Some(vec![BULK]));
        assert!(q.pop_highest().is_none());
    }

    /// The backlog a class reports must be its own, and must fall as the
    /// drainer takes frames.
    ///
    /// This is what the files module paces itself against: a count that never
    /// dropped would read as a permanently congested link and throttle a
    /// transfer to nothing, while one that counted every class would let a
    /// burst of audio look like a file backlog.
    #[test]
    fn a_class_reports_only_its_own_backlog() {
        let mut q = Queues::default();
        q.push(class_index(Priority::Bulk), vec![0u8; 100]);
        q.push(class_index(Priority::Bulk), vec![0u8; 50]);
        q.push(class_index(Priority::Input), vec![0u8; 7]);

        assert_eq!(q.queued_bytes[class_index(Priority::Bulk)], 150);
        assert_eq!(q.queued_bytes[class_index(Priority::Input)], 7);

        // Input drains first, leaving the bulk backlog untouched.
        let (class, frame) = q.pop_highest().expect("a frame");
        assert_eq!(class, class_index(Priority::Input));
        assert_eq!(frame.len(), 7);
        assert_eq!(q.queued_bytes[class_index(Priority::Bulk)], 150);
        assert_eq!(q.queued_bytes[class_index(Priority::Input)], 0);

        q.pop_highest().expect("a bulk frame");
        assert_eq!(q.queued_bytes[class_index(Priority::Bulk)], 50);
    }

    #[test]
    fn terminal_failure_discards_every_backlog() {
        let mut q = Queues::default();
        q.push(class_index(Priority::Video), vec![0; 100]);
        q.push(class_index(Priority::Bulk), vec![0; 200]);
        q.fail("socket closed".into());

        assert!(q.pop_is_empty());
        assert_eq!(q.queued_bytes, [0; CLASS_COUNT]);
        assert_eq!(q.terminal_error.as_deref(), Some("socket closed"));
        assert!(q.closed);
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

    #[tokio::test]
    async fn writer_failure_unblocks_flush_and_rejects_later_sends() {
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

        let (mut tx, _rx) = Box::new(init).split_prioritized();
        let (_peer_tx, peer_rx) = Box::new(resp).split();
        drop(peer_rx);

        let _ = tx.send(&Message::Bye).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), tx.flush())
            .await
            .expect("flush remained blocked after the writer failed");

        assert!(tx.send(&Message::Bye).await.is_err());
        assert_eq!(
            tx.queue_depth(Priority::Control).queued_bytes,
            0,
            "dead queues must not retain unsendable frames"
        );
    }

    /// A queued bulk payload must show up as a backlog, and be credited as sent
    /// once it has left — the two readings a producer paces itself with.
    #[tokio::test]
    async fn the_backlog_a_producer_reads_tracks_the_wire() {
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

        assert_eq!(tx.queue_depth(Priority::Bulk), crate::QueueDepth::default());

        tx.send(&Message::Opaque(vec![0u8; 800_000])).await.unwrap();
        tx.flush().await;
        let _ = rx.recv().await.unwrap();

        let depth = tx.queue_depth(Priority::Bulk);
        assert_eq!(depth.queued_bytes, 0, "nothing is left after a flush");
        assert!(
            depth.sent_bytes >= 800_000,
            "the payload must be credited to its own class, got {depth:?}"
        );
        // Another class stays at zero: a bulk transfer is not audio's backlog.
        assert_eq!(
            tx.queue_depth(Priority::Audio),
            crate::QueueDepth::default()
        );
    }
}
