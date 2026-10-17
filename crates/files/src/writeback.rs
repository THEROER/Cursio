//! Keeping several writes in flight against one handle.
//!
//! A mount callback blocks its own thread for a full round trip, so the number
//! of threads Windows uses *is* the number of operations that can be
//! outstanding. That is fine when an application writes from many threads and
//! useless when it writes from one: Windows hands cached writes to a filesystem
//! from a single thread, in views the Memory Manager fixes at 256 KiB, so a
//! download arrived here as one 220 KiB write after another, each waiting out
//! the link before the next was sent. Measured against Steam over Wi-Fi that
//! was 56 writes a second — exactly one round trip's worth — while the link
//! itself was idle five times over.
//!
//! The same problem is why `rclone` outruns `sshfs` on a high-latency link by
//! two orders of magnitude: it keeps requests pipelined instead of waiting for
//! each answer.
//!
//! ## Why the acknowledgement is not given away
//!
//! The obvious pipeline returns success from `write` immediately and reports
//! failures later, at `flush`. That is what `rclone` does, and on Windows it
//! has a hole: WinFsp does not propagate an error returned from `flush`, so a
//! transfer that failed mid-way is reported to the application as having
//! succeeded and the file it wrote is quietly wrong.
//!
//! So nothing here is acknowledged before the peer has stored it. What moves is
//! *which* write is being waited on: a call sends its own bytes, then collects
//! the answer to a write [`WINDOW`] places earlier. The caller still blocks —
//! and still learns about every failure — but it blocks behind a request that
//! has already had the link's latency spent on it, so up to `WINDOW` writes are
//! travelling at once.
//!
//! A short write or an error stops the pipeline: everything still outstanding
//! is collected and discarded, and the failure is handed to the first caller to
//! ask. Bytes past a short write must never be counted, since the application
//! will resend from that offset.

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use cursio_protocol::FileError;

/// How many writes may be outstanding across the whole mount.
///
/// Counted per mount rather than per handle. Per handle was the first attempt
/// and it fell over live: an installer writes to many files at once, so eight
/// each became tens of megabytes queued on a link that carries about 19 MB/s,
/// and the queue *is* the latency. Measured against Steam, the round trip went
/// from 8 ms to 164 ms with a worst case of 6 seconds, the pacer cut its window
/// 107 times, 43 operations failed, and Steam stopped the download with a disk
/// write error — a transfer made slower and then broken by the very thing meant
/// to speed it up.
///
/// Sixteen 256 KiB writes is 4 MiB in flight at most, which is under a third of
/// a second on this link: enough to keep it busy through any round trip it has,
/// and short enough that a write's answer never waits behind a queue longer
/// than the timeout that judges it.
pub const WINDOW: usize = 16;

/// Writes outstanding across every handle on this mount.
///
/// The permit a handle takes before sending and returns once the answer is
/// collected. A counter rather than a semaphore: a handle that cannot get a
/// permit collects one of its own answers instead of blocking, which is the
/// work it would otherwise be waiting to do anyway.
#[derive(Clone, Default)]
pub struct InFlightBudget(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl InFlightBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a slot if the mount has one to spare.
    fn take(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < WINDOW).then_some(n + 1)
            })
            .is_ok()
    }

    /// Give a slot back once its write has been accounted for.
    fn give_back(&self) {
        use std::sync::atomic::Ordering;
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
}

/// One write that has been sent and not yet accounted for.
struct Sent {
    reply: std_mpsc::Receiver<Result<u64, FileError>>,
    /// What was asked of the peer, to tell a short write from a complete one.
    len: u64,
}

/// The writes outstanding against a single handle.
pub struct WriteBack {
    /// Shared with every other handle on this mount, so the queue on the link
    /// is bounded by what the link can carry rather than by the file count.
    budget: InFlightBudget,
    sent: VecDeque<Sent>,
    /// Set once a write came back short or failed. The pipeline is finished
    /// after that: the application will resend from the offset that failed, and
    /// anything already in flight past it describes a file that will not exist.
    failed: Option<FileError>,
}

impl WriteBack {
    pub fn new(budget: InFlightBudget) -> Self {
        WriteBack {
            budget,
            sent: VecDeque::new(),
            failed: None,
        }
    }

    /// Take a slot from the mount's budget, if one is free.
    ///
    /// A caller that cannot get one must collect an answer before sending, so
    /// the number of writes travelling at once stays bounded whatever the
    /// application is doing.
    pub fn reserve(&self) -> bool {
        self.budget.take()
    }

    /// Record a write that has just been sent under a reserved slot.
    pub fn push(&mut self, reply: std_mpsc::Receiver<Result<u64, FileError>>, len: u64) {
        self.sent.push_back(Sent { reply, len });
    }

    /// Whether this handle has anything outstanding to collect.
    pub fn has_outstanding(&self) -> bool {
        !self.sent.is_empty()
    }

    /// Whether a failure has already been seen and not yet reported.
    pub fn poisoned(&self) -> bool {
        self.failed.is_some()
    }

    /// Take the recorded failure, if any.
    pub fn take_failure(&mut self) -> Option<FileError> {
        self.failed.take()
    }

    /// Collect the answer to the oldest outstanding write.
    ///
    /// `wait` is how each answer is awaited, so the caller keeps its own
    /// timeout and liveness rules rather than this module inventing them.
    ///
    /// Returns what that write stored, or `None` when nothing was outstanding.
    pub fn collect_one(
        &mut self,
        wait: impl FnOnce(
            std_mpsc::Receiver<Result<u64, FileError>>,
            Duration,
        ) -> Result<u64, FileError>,
        limit: Duration,
    ) -> Option<Result<u64, FileError>> {
        let Sent { reply, len } = self.sent.pop_front()?;
        self.budget.give_back();
        let got = wait(reply, limit);
        match &got {
            Ok(n) if *n < len => {
                // Short: the far side stopped early. Nothing behind this can be
                // counted, because the application resends from here.
                self.failed.get_or_insert(FileError::Io(
                    "the peer stored less than was written".into(),
                ));
            }
            Err(e) => {
                self.failed.get_or_insert_with(|| e.clone());
            }
            _ => {}
        }
        Some(got)
    }

    /// Collect everything still outstanding, keeping the first failure.
    ///
    /// Used by `flush`, `close` and the failure path: a request whose answer is
    /// never taken leaves the session holding a reply channel for a caller that
    /// has gone.
    pub fn drain(
        &mut self,
        mut wait: impl FnMut(
            std_mpsc::Receiver<Result<u64, FileError>>,
            Duration,
        ) -> Result<u64, FileError>,
        limit: Duration,
    ) {
        while let Some(Sent { reply, len }) = self.sent.pop_front() {
            self.budget.give_back();
            match wait(reply, limit) {
                Ok(n) if n < len => {
                    self.failed.get_or_insert(FileError::Io(
                        "the peer stored less than was written".into(),
                    ));
                }
                Err(e) => {
                    self.failed.get_or_insert(e);
                }
                _ => {}
            }
        }
    }
}

impl Drop for WriteBack {
    /// Return the slots of writes nobody collected.
    ///
    /// The budget is the mount's, so a handle dropped with writes outstanding —
    /// a close that could not take the lock, a context torn down by an unmount —
    /// would otherwise keep its slots forever. Every such slot is one fewer
    /// write the whole drive may have in flight, and once all sixteen leak each
    /// write waits out a full round trip again, on every file.
    fn drop(&mut self) {
        for _ in self.sent.drain(..) {
            self.budget.give_back();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle dropped with writes outstanding must not take the mount's
    /// budget with it.
    #[test]
    fn dropping_a_handle_returns_its_slots_to_the_mount() {
        let budget = InFlightBudget::new();
        {
            let mut w = WriteBack::new(budget.clone());
            for _ in 0..WINDOW {
                assert!(w.reserve());
                w.push(answered(Ok(10)), 10);
            }
        }
        let fresh = WriteBack::new(budget);
        assert!(fresh.reserve(), "the dropped handle's slots are free again");
    }

    fn answered(v: Result<u64, FileError>) -> std_mpsc::Receiver<Result<u64, FileError>> {
        let (tx, rx) = std_mpsc::channel();
        tx.send(v).expect("queued");
        rx
    }

    fn take(rx: std_mpsc::Receiver<Result<u64, FileError>>, _: Duration) -> Result<u64, FileError> {
        rx.recv().expect("answered")
    }

    /// The window is what lets writes overlap; without it a single-threaded
    /// writer spends one round trip per write.
    #[test]
    fn the_window_bounds_what_is_outstanding() {
        let mut w = WriteBack::new(InFlightBudget::new());
        for _ in 0..WINDOW {
            assert!(w.reserve(), "room while under the window");
            w.push(answered(Ok(10)), 10);
        }
        assert!(!w.reserve(), "full at the window");
    }

    /// The budget is the mount's, not the handle's.
    ///
    /// Per handle was the first attempt and it broke the transfer it was meant
    /// to speed up: an installer writing to many files at once queued tens of
    /// megabytes on a link carrying about 19 MB/s, and the queue is the
    /// latency. The round trip went from 8 ms to 164 ms, worst case 6 seconds,
    /// and Steam stopped with a disk write error.
    #[test]
    fn handles_share_one_budget() {
        let budget = InFlightBudget::new();
        let mut a = WriteBack::new(budget.clone());
        let b = WriteBack::new(budget.clone());

        // One handle fills the whole window.
        for _ in 0..WINDOW {
            assert!(a.reserve(), "the first handle takes the budget");
            a.push(answered(Ok(10)), 10);
        }
        assert!(
            !b.reserve(),
            "a second handle cannot add to a link already full"
        );

        // Collecting on the first frees a slot for the second.
        a.collect_one(take, Duration::from_secs(1));
        assert!(
            b.reserve(),
            "a collected answer returns its slot to the mount"
        );
    }

    /// A short write ends the pipeline: bytes past it describe a file the
    /// application is about to write differently.
    #[test]
    fn a_short_write_poisons_the_pipeline() {
        let mut w = WriteBack::new(InFlightBudget::new());
        w.push(answered(Ok(4)), 10);
        let got = w.collect_one(take, Duration::from_secs(1));
        assert_eq!(got.expect("collected").expect("no error"), 4);
        assert!(w.poisoned(), "a short write is remembered");
        assert!(w.take_failure().is_some(), "and is reported once");
        assert!(!w.poisoned(), "having been reported, it is not repeated");
    }

    /// A failure must survive until someone asks, rather than being lost with
    /// the call that happened to collect it.
    #[test]
    fn a_failure_is_kept_for_the_next_caller() {
        let mut w = WriteBack::new(InFlightBudget::new());
        w.push(answered(Err(FileError::Io("disk full".into()))), 10);
        let got = w.collect_one(take, Duration::from_secs(1));
        assert!(got.expect("collected").is_err(), "the error is returned");
        assert!(w.poisoned(), "and remembered for whoever asks next");
    }

    /// A failure reaches the caller even though acknowledgement lags behind.
    ///
    /// This is the guarantee that makes the pipeline safe to use where
    /// `rclone`'s is not: WinFsp drops an error returned from `flush`, so a
    /// scheme that only reports there tells the application its file is
    /// complete when it is not. Here the failure is still pending after the
    /// write that hit it, so the next write reports it too — and that happens
    /// long before the file can be closed.
    #[test]
    fn a_failure_behind_the_window_still_reaches_the_caller() {
        let mut w = WriteBack::new(InFlightBudget::new());
        // A window's worth of writes, the first of which failed.
        assert!(w.reserve());
        w.push(answered(Err(FileError::Io("peer disk full".into()))), 10);
        for _ in 1..WINDOW {
            assert!(w.reserve());
            w.push(answered(Ok(10)), 10);
        }

        // The caller collecting the oldest answer sees the failure.
        let got = w
            .collect_one(take, Duration::from_secs(1))
            .expect("collected");
        assert!(got.is_err(), "the failure is returned to this caller");

        // And it is still pending for the next one, so a caller that ignored
        // the return value cannot proceed as though the file were whole.
        assert!(w.poisoned(), "the failure outlives the call that saw it");
    }

    /// Draining must not lose a failure that happened partway through.
    #[test]
    fn draining_keeps_the_first_failure() {
        let mut w = WriteBack::new(InFlightBudget::new());
        w.push(answered(Ok(10)), 10);
        w.push(answered(Err(FileError::Io("gone".into()))), 10);
        w.push(answered(Ok(10)), 10);
        w.drain(take, Duration::from_secs(1));
        assert!(w.take_failure().is_some(), "the failure survived the drain");
    }
}
