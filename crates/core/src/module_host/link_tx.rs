//! A module's handle onto the link: its own sender into the shared prioritized
//! queue, closed when the session is done with it.

use async_trait::async_trait;
use cursio_protocol::{Message, Priority};
use cursio_transport::{QueueDepth, TransportError, TransportTx};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub(super) type Slot = Arc<tokio::sync::Mutex<Option<Box<dyn TransportTx>>>>;

/// One holder's way onto the link: the session's own, or one module's.
///
/// Closable from outside, which is what lets teardown finish while a module is
/// wedged: its handle onto the queue is taken away, so the link closes when
/// the session is done with it rather than when the module lets go, and
/// anything the module sends afterwards fails instead of reaching a peer that
/// has moved on.
pub(crate) struct LinkTx {
    slot: Slot,
    open: Arc<AtomicBool>,
    closed: Arc<tokio::sync::Notify>,
    /// Whether `slot` belongs to this holder alone. A shared one must stay
    /// filled when one holder closes, because the others still send through
    /// it.
    own: bool,
    /// Messages this handle has sent — what a module's pace follows.
    sent: u64,
}

impl LinkTx {
    pub(crate) fn new(tx: Box<dyn TransportTx>) -> Self {
        Self {
            slot: Arc::new(tokio::sync::Mutex::new(Some(tx))),
            open: Arc::new(AtomicBool::new(true)),
            closed: Arc::new(tokio::sync::Notify::new()),
            own: true,
            sent: 0,
        }
    }

    /// A handle for a module: a clone of the queue where the sender has one,
    /// or this very sender, shared, where it does not.
    pub(crate) fn fork(&self) -> Self {
        let cloned = self
            .slot
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|tx| tx.clone_sender()));
        match cloned {
            Some(tx) => Self::new(tx),
            None => Self {
                slot: self.slot.clone(),
                open: Arc::new(AtomicBool::new(true)),
                closed: Arc::new(tokio::sync::Notify::new()),
                own: false,
                sent: 0,
            },
        }
    }

    /// Another reference to the same handle, for closing it from outside.
    pub(crate) fn closer(&self) -> Self {
        Self {
            slot: self.slot.clone(),
            open: self.open.clone(),
            closed: self.closed.clone(),
            own: self.own,
            sent: 0,
        }
    }

    /// Refuse every later send, and let go of the queue if it is ours alone.
    ///
    /// Closing cancels a send awaiting queue capacity before taking its slot.
    /// Atomic scheduler admission makes that cancellation safe.
    pub(crate) async fn close(&self) {
        self.open.store(false, Ordering::Release);
        self.closed.notify_waiters();
        if !self.own {
            return;
        }
        match tokio::time::timeout(Duration::from_millis(250), self.slot.lock()).await {
            Ok(mut slot) => drop(slot.take()),
            Err(_) => tracing::warn!("a module held its sender through teardown; leaving it"),
        }
    }

    pub(crate) fn sent(&self) -> u64 {
        self.sent
    }
}

#[async_trait]
impl TransportTx for LinkTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        let closed = self.closed.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if !self.open.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        tokio::select! {
            biased;
            () = &mut closed => return Err(TransportError::Closed),
            result = async {
                let mut slot = self.slot.lock().await;
                let tx = slot.as_mut().ok_or(TransportError::Closed)?;
                tx.send(msg).await
            } => result?,
        }
        self.sent = self.sent.wrapping_add(1);
        Ok(())
    }

    fn queue_age(&self, priority: Priority) -> Duration {
        self.slot
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|tx| tx.queue_age(priority)))
            .unwrap_or_default()
    }

    fn queue_delay(&self, priority: Priority) -> cursio_transport::QueueDelay {
        self.slot
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|tx| tx.queue_delay(priority)))
            .unwrap_or_default()
    }

    fn queue_depth(&self, priority: Priority) -> QueueDepth {
        // Only this holder and teardown ever take the lock, so a miss means
        // teardown has it and the figure no longer matters.
        self.slot
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|tx| tx.queue_depth(priority)))
            .unwrap_or_default()
    }

    async fn flush(&mut self) {
        let mut slot = self.slot.lock().await;
        if let Some(tx) = slot.as_mut() {
            tx.flush().await;
        }
    }

    fn holding(&self) -> bool {
        self.slot
            .try_lock()
            .ok()
            .is_some_and(|slot| slot.as_ref().is_some_and(|tx| tx.holding()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WaitingSender;
    #[async_trait]
    impl TransportTx for WaitingSender {
        async fn send(&mut self, _: &Message) -> Result<(), TransportError> {
            std::future::pending().await
        }
        fn queue_age(&self, _: Priority) -> Duration {
            Duration::from_millis(123)
        }
    }

    #[tokio::test]
    async fn closing_a_holder_cancels_a_send_waiting_for_queue_capacity() {
        let mut sender = LinkTx::new(Box::new(WaitingSender));
        assert_eq!(
            sender.queue_age(Priority::Video),
            Duration::from_millis(123)
        );
        let closer = sender.closer();
        let pending = tokio::spawn(async move { sender.send(&Message::Bye).await });
        tokio::task::yield_now().await;
        tokio::time::timeout(Duration::from_millis(100), closer.close())
            .await
            .unwrap();
        assert!(matches!(
            pending.await.unwrap(),
            Err(TransportError::Closed)
        ));
    }
}
