//! Nonblocking, bounded process queues. Overflow is an explicit terminal error.
//!
//! The producers here cannot wait: a Windows hook or a libei callback that
//! blocks freezes the user's own pointer, and the session reader also carries
//! the heartbeat. So admission never awaits capacity, and the queues used to be
//! unbounded — which is how a stalled consumer once grew one to tens of
//! gigabytes. Dropping the newest item instead would be worse for input: a lost
//! key release leaves a key held on the peer, and the stream after it is no
//! longer the one the user typed. A full queue therefore fails the whole stream
//! — cleared, marked [`Stats::failed`], its receiver woken to see the end — and
//! the owner recovers by restarting that stream rather than continuing a
//! partially valid one.
//!
//! The one lossless way to stay under the bound is the caller's `merge`:
//! adjacent relative motion adds up to the same displacement, so a burst of it
//! costs one slot. Anything the caller does not declare equivalent keeps its
//! own place and order.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc::error::TryRecvError, Notify};

type Merge<T> = fn(&mut T, &T) -> bool;

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub items: usize,
    pub bytes: usize,
    pub peak_items: usize,
    pub peak_bytes: usize,
    pub merged: u64,
    pub oldest: Duration,
    pub max_age: Duration,
    pub failed: bool,
}

struct Entry<T> {
    item: T,
    bytes: usize,
    at: Instant,
}
struct State<T> {
    items: VecDeque<Entry<T>>,
    stats: Stats,
    senders: usize,
    closed: bool,
    receiver: bool,
    reported: Instant,
}
struct Shared<T> {
    name: &'static str,
    max_items: usize,
    max_bytes: usize,
    size: fn(&T) -> usize,
    merge: Merge<T>,
    state: Mutex<State<T>>,
    ready: Notify,
}

pub struct Sender<T>(Arc<Shared<T>>);
pub struct Receiver<T>(Arc<Shared<T>>);

/// The queue no longer accepts work: its receiver is gone, it was closed, or
/// this or an earlier send overflowed it ([`Stats::failed`] tells which).
#[derive(Debug, thiserror::Error)]
#[error("queue closed or overloaded")]
pub struct SendError;

pub fn channel<T>(
    name: &'static str,
    max_items: usize,
    max_bytes: usize,
    size: fn(&T) -> usize,
    merge: Merge<T>,
) -> (Sender<T>, Receiver<T>) {
    assert!(max_items > 0 && max_bytes > 0);
    let shared = Arc::new(Shared {
        name,
        max_items,
        max_bytes,
        size,
        merge,
        state: Mutex::new(State {
            items: VecDeque::new(),
            stats: Stats::default(),
            senders: 1,
            closed: false,
            receiver: true,
            reported: Instant::now(),
        }),
        ready: Notify::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl<T> Shared<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn clear(state: &mut State<T>) {
        state.items.clear();
        state.stats.items = 0;
        state.stats.bytes = 0;
    }
    fn snapshot(state: &State<T>) -> Stats {
        Stats {
            oldest: state
                .items
                .front()
                .map_or(Duration::ZERO, |item| item.at.elapsed()),
            ..state.stats
        }
    }
    fn report(&self, state: &mut State<T>) {
        if state.reported.elapsed() < Duration::from_secs(10) {
            return;
        }
        let stats = Self::snapshot(state);
        tracing::debug!(
            queue = self.name,
            items = stats.items,
            bytes = stats.bytes,
            peak_items = stats.peak_items,
            peak_bytes = stats.peak_bytes,
            merged = stats.merged,
            oldest_us = stats.oldest.as_micros() as u64,
            max_age_us = stats.max_age.as_micros() as u64,
            "process queue health"
        );
        state.reported = Instant::now();
        state.stats.peak_items = state.stats.items;
        state.stats.peak_bytes = state.stats.bytes;
        state.stats.max_age = Duration::ZERO;
        state.stats.merged = 0;
    }
}

impl<T> Sender<T> {
    pub fn send(&self, item: T) -> Result<(), SendError> {
        let bytes = (self.0.size)(&item);
        let mut state = self.0.lock();
        if state.closed || !state.receiver {
            return Err(SendError);
        }
        // Merge only where the caller explicitly guarantees equivalence.
        if let Some(last) = state.items.back_mut() {
            if (self.0.merge)(&mut last.item, &item) {
                state.stats.merged += 1;
                self.0.report(&mut state);
                return Ok(());
            }
        }
        if state.items.len() >= self.0.max_items
            || state.stats.bytes.saturating_add(bytes) > self.0.max_bytes
        {
            tracing::error!(
                queue = self.0.name,
                items = state.stats.items,
                bytes = state.stats.bytes,
                incoming_bytes = bytes,
                "process queue overloaded; terminating its stream"
            );
            state.stats.failed = true;
            state.closed = true;
            Shared::clear(&mut state);
            drop(state);
            self.0.ready.notify_one();
            return Err(SendError);
        }
        state.items.push_back(Entry {
            item,
            bytes,
            at: Instant::now(),
        });
        state.stats.items += 1;
        state.stats.bytes += bytes;
        state.stats.peak_items = state.stats.peak_items.max(state.stats.items);
        state.stats.peak_bytes = state.stats.peak_bytes.max(state.stats.bytes);
        self.0.report(&mut state);
        drop(state);
        self.0.ready.notify_one();
        Ok(())
    }
    pub fn is_closed(&self) -> bool {
        let state = self.0.lock();
        state.closed || !state.receiver
    }
    /// Finish accepted work, refusing further admissions.
    pub fn close(&self) {
        self.0.lock().closed = true;
        self.0.ready.notify_one();
    }
    /// Abort immediately; no stale input is processed after a failed stream.
    pub fn abort(&self) {
        let mut state = self.0.lock();
        state.closed = true;
        state.stats.failed = true;
        Shared::clear(&mut state);
        drop(state);
        self.0.ready.notify_one();
    }
    pub fn stats(&self) -> Stats {
        Shared::snapshot(&self.0.lock())
    }
}
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.lock().senders += 1;
        Self(self.0.clone())
    }
}
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.senders -= 1;
        let last = state.senders == 0;
        drop(state);
        if last {
            self.0.ready.notify_one();
        }
    }
}
impl<T> Receiver<T> {
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut state = self.0.lock();
        if let Some(entry) = state.items.pop_front() {
            state.stats.items -= 1;
            state.stats.bytes -= entry.bytes;
            state.stats.max_age = state.stats.max_age.max(entry.at.elapsed());
            self.0.report(&mut state);
            return Ok(entry.item);
        }
        if state.closed || state.senders == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            let shared = self.0.clone();
            let ready = shared.ready.notified();
            match self.try_recv() {
                Ok(item) => return Some(item),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => ready.await,
            }
        }
    }
    pub fn len(&self) -> usize {
        self.0.lock().stats.items
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn stats(&self) -> Stats {
        Shared::snapshot(&self.0.lock())
    }
}
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.receiver = false;
        Shared::clear(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn overload_is_explicit_and_wakes_the_receiver() {
        let (tx, mut rx) = channel("test", 2, 8, |_| 4, |_, _| false);
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert!(tx.send(3).is_err());
        assert!(rx.stats().failed);
        assert_eq!(rx.recv().await, None);
        assert_eq!(tx.stats().bytes, 0);
    }
}
