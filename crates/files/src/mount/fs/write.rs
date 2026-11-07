//! Writes to a mounted file: gathered, sent without waiting, collected later.
//!
//! The kernel's write-back cache used to gather an application's small writes
//! into large ones. It cannot be used here (see `init`): it makes the kernel
//! keep a file's size and time as its own, and the owner changes them. Without
//! it every `write(2)` arrived on its own and waited out the owner's answer —
//! 16 MiB took 4.6 s in 128 KiB writes and more than two minutes in 4 KiB ones.
//!
//! So this does here what the kernel no longer does for us:
//!
//! - **Gathered.** A write that continues the one before it joins it, up to
//!   [`GATHER`], and only then goes to the owner as one message.
//! - **Pipelined.** Each message is sent without waiting and its answer taken
//!   a window later ([`crate::writeback`]), so a single-threaded copy keeps the
//!   link busy instead of idling a round trip per message.
//!
//! A write is therefore answered before the owner has stored it, and a failure
//! reaches the application at its next write on that handle, at `fsync`, or at
//! `close` — Linux reports a failed `flush` as the result of `close(2)`, which
//! is where an application that cares looks.
//!
//! ## Settled before anything else looks at the file
//!
//! The owner's view of a file being written lags this handle's. Without the
//! write-back cache the kernel takes a file's size from the owner, and a size
//! read while the last writes were still on their way is smaller than what the
//! application wrote; the kernel then cuts its cached pages to it. A read that
//! reaches the owner would likewise miss bytes still here. So `getattr`,
//! `lookup`, a truncate and a read of the file first send what is gathered and
//! wait for every answer ([`RemoteFs::settle_path`]).

use std::sync::{Arc, Mutex};

use cursio_protocol::{FileError, FileHandle, FilesMsg};

use crate::bridge::BULK_OP_TIMEOUT;
use crate::writeback::WriteBack;

use super::RemoteFs;

/// The most bytes gathered into one message: one wire chunk.
pub(super) const GATHER: usize = FilesMsg::MAX_CHUNK;

/// One handle's writes that the owner has not confirmed yet.
pub(super) struct HandleWrites {
    pub(super) volume: String,
    pub(super) path: String,
    remote: FileHandle,
    pipeline: WriteBack,
    /// Bytes gathered and not yet sent, starting at `gathered_at`.
    gathered: Vec<u8>,
    gathered_at: u64,
}

impl HandleWrites {
    pub(super) fn new(
        volume: String,
        path: String,
        remote: FileHandle,
        budget: crate::writeback::InFlightBudget,
    ) -> Self {
        Self {
            volume,
            path,
            remote,
            pipeline: WriteBack::new(budget),
            gathered: Vec::new(),
            gathered_at: 0,
        }
    }

    /// Whether anything of this handle has not been confirmed by the owner.
    pub(super) fn unsettled(&self) -> bool {
        !self.gathered.is_empty() || self.pipeline.has_outstanding() || self.pipeline.poisoned()
    }

    /// Whether `offset` continues what is gathered and `len` more still fits.
    fn continues(&self, offset: u64, len: usize) -> bool {
        !self.gathered.is_empty()
            && offset == self.gathered_at + self.gathered.len() as u64
            && self.gathered.len() + len <= GATHER
    }
}

impl RemoteFs {
    /// Take a write for `fh`, gathering it with the one before when it
    /// continues it, and send what is complete.
    pub(super) fn gather_write(
        &self,
        writes: &Arc<Mutex<HandleWrites>>,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, FileError> {
        let mut writes = writes.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = writes.pipeline.take_failure() {
            // An earlier write failed: say so here, where the application is
            // still writing, rather than letting it believe the file whole.
            self.collect_all(&mut writes);
            tracing::warn!(path = %writes.path, offset, error = ?e, "a write earlier in this file failed");
            return Err(e);
        }
        if writes.continues(offset, data.len()) {
            writes.gathered.extend_from_slice(data);
        } else {
            self.send_gathered(&mut writes)?;
            if data.len() >= GATHER {
                self.send(&mut writes, offset, data.to_vec())?;
            } else {
                writes.gathered_at = offset;
                writes.gathered.extend_from_slice(data);
            }
        }
        if writes.gathered.len() >= GATHER {
            self.send_gathered(&mut writes)?;
        }
        Ok(data.len() as u32)
    }

    /// Send what is gathered and wait for every answer; the first failure, if
    /// any, is returned.
    pub(super) fn settle(&self, writes: &Mutex<HandleWrites>) -> Result<(), FileError> {
        let mut writes = writes.lock().unwrap_or_else(|p| p.into_inner());
        let sent = self.send_gathered(&mut writes);
        self.collect_all(&mut writes);
        match writes.pipeline.take_failure() {
            Some(e) => {
                tracing::warn!(path = %writes.path, error = ?e, "a write to the peer failed");
                Err(e)
            }
            None => sent,
        }
    }

    /// Settle every handle writing `path`, before something reads its size or
    /// its bytes from the owner. A failure is left for the handle's own writer
    /// to be told; the reader only needs the owner caught up.
    pub(super) fn settle_path(&self, volume: &str, path: &str) {
        let pending: Vec<Arc<Mutex<HandleWrites>>> = {
            let state = self.state.lock().expect("inode state");
            if state.writes.is_empty() {
                return;
            }
            state.writes.values().cloned().collect()
        };
        for writes in pending {
            let mut held = writes.lock().unwrap_or_else(|p| p.into_inner());
            if held.volume != volume || held.path != path || !held.unsettled() {
                continue;
            }
            if let Err(e) = self.send_gathered(&mut held) {
                held.pipeline.fail(e);
            }
            // Any failure stays in the pipeline: `settle` and the handle's
            // next write report it to the one who wrote.
            self.collect_all(&mut held);
        }
    }

    fn send_gathered(&self, writes: &mut HandleWrites) -> Result<(), FileError> {
        if writes.gathered.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut writes.gathered);
        let at = writes.gathered_at;
        self.send(writes, at, bytes)
    }

    /// Send `bytes` at `at` in wire-sized pieces, each under a slot of the
    /// mount's write budget, collecting an earlier answer when none is free.
    fn send(&self, writes: &mut HandleWrites, at: u64, bytes: Vec<u8>) -> Result<(), FileError> {
        for (i, piece) in bytes.chunks(GATHER).enumerate() {
            if !writes.pipeline.reserve() {
                if let Some(Err(e)) = writes.pipeline.collect_one(
                    |rx, limit| self.peer.await_write(rx, limit),
                    BULK_OP_TIMEOUT,
                ) {
                    return Err(e);
                }
                // The slot that answer held is free now; one over the budget
                // if another handle took it first is better than stalling.
                let _ = writes.pipeline.reserve();
            }
            let offset = at + (i * GATHER) as u64;
            let reply = self
                .peer
                .write_later(writes.remote, offset, piece.to_vec())?;
            writes.pipeline.push(reply, piece.len() as u64);
        }
        Ok(())
    }

    fn collect_all(&self, writes: &mut HandleWrites) {
        writes.pipeline.drain(
            |rx, limit| self.peer.await_write(rx, limit),
            BULK_OP_TIMEOUT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Op, PeerLink};

    /// A peer that records each write it receives as `(offset, length)` and
    /// stores it, or refuses every write when `refuse` is set.
    fn writing_peer(refuse: bool) -> (PeerLink, std::sync::mpsc::Receiver<(u64, usize)>) {
        let (ops, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (seen_tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Some(op) = rx.blocking_recv() {
                if let Op::Write {
                    offset,
                    bytes,
                    reply,
                    ..
                } = op
                {
                    let _ = seen_tx.send((offset, bytes.len()));
                    let _ = reply.send(if refuse {
                        Err(FileError::Denied)
                    } else {
                        Ok(bytes.len() as u64)
                    });
                }
            }
        });
        (PeerLink::new(ops), seen)
    }

    fn handle(fs: &RemoteFs) -> Arc<Mutex<HandleWrites>> {
        Arc::new(Mutex::new(HandleWrites::new(
            "C:".into(),
            "out.bin".into(),
            7,
            fs.write_budget.clone(),
        )))
    }

    fn sent(seen: &std::sync::mpsc::Receiver<(u64, usize)>) -> Vec<(u64, usize)> {
        seen.try_iter().collect()
    }

    /// Sixty-four 4 KiB writes in a row reach the owner as one 256 KiB message,
    /// sent once it is full.
    #[test]
    fn contiguous_small_writes_leave_as_one_message() {
        let (peer, seen) = writing_peer(false);
        let fs = RemoteFs::new(peer, "C:");
        let writes = handle(&fs);
        for i in 0..64u64 {
            fs.gather_write(&writes, i * 4096, &[1; 4096]).unwrap();
        }
        fs.settle(&writes).unwrap();
        assert_eq!(sent(&seen), [(0, GATHER)]);
    }

    /// A write that does not continue the gathered bytes sends them first, so
    /// nothing is reordered or merged across a gap.
    #[test]
    fn a_write_elsewhere_sends_what_was_gathered_first() {
        let (peer, seen) = writing_peer(false);
        let fs = RemoteFs::new(peer, "C:");
        let writes = handle(&fs);
        fs.gather_write(&writes, 0, &[1; 100]).unwrap();
        fs.gather_write(&writes, 100, &[1; 100]).unwrap();
        fs.gather_write(&writes, 5000, &[1; 10]).unwrap();
        fs.settle(&writes).unwrap();
        assert_eq!(sent(&seen), [(0, 200), (5000, 10)]);
    }

    /// The owner refusing a write is reported when the application next asks
    /// — its `close` — even though the write itself was answered at once.
    #[test]
    fn a_refused_write_is_reported_at_settle() {
        let (peer, _seen) = writing_peer(true);
        let fs = RemoteFs::new(peer, "C:");
        let writes = handle(&fs);
        assert!(fs.gather_write(&writes, 0, &[1; 10]).is_ok());
        assert!(matches!(fs.settle(&writes), Err(FileError::Denied)));
        assert!(fs.settle(&writes).is_ok(), "reported once, not forever");
    }

    /// Settling for a reader leaves a failure for the writer to be told.
    #[test]
    fn a_reader_settling_the_file_keeps_the_failure_for_the_writer() {
        let (peer, _seen) = writing_peer(true);
        let fs = RemoteFs::new(peer, "C:");
        let writes = handle(&fs);
        fs.state.lock().unwrap().writes.insert(1, writes.clone());
        fs.gather_write(&writes, 0, &[1; 10]).unwrap();

        fs.settle_path("C:", "out.bin");
        assert!(matches!(fs.settle(&writes), Err(FileError::Denied)));
    }
}
