//! The link between FUSE's blocking callbacks and the async session.
//!
//! FUSE hands work to us on its own threads and expects an answer before the
//! call returns: `read()` in an application blocks until we reply. The session
//! that can actually fetch the bytes is async and lives on the tokio runtime.
//! Something has to join the two.
//!
//! ## Why there is no `block_on` here
//!
//! The obvious bridge — call `Handle::block_on` inside the callback — is a trap.
//! Tokio panics if a worker thread is blocked from inside the runtime, and even
//! where it does not panic, parking a worker for the length of a network round
//! trip starves every other task on it, including the cursor.
//!
//! So no part of this file touches the runtime. A callback sends its request
//! down a channel and waits on a plain [`std::sync::mpsc`] receiver; the async
//! side picks it up, does the round trip, and sends the answer back. The FUSE
//! threads block, which is exactly what they are for, and the runtime never
//! does.
//!
//! ## Timeouts are not optional
//!
//! A mounted filesystem that stops answering leaves every caller in
//! uninterruptible sleep — not even `kill -9` clears it, and the mount point
//! becomes an unremovable hole in the user's home directory. Since the peer is
//! on the other end of a network that can drop at any moment, every wait here
//! is bounded and a lost peer surfaces as `EIO`.

use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use cursio_protocol::{FileError, FileHandle, FileMeta, OpenMode, Volume};

/// How long a single filesystem operation may wait for the peer.
///
/// Deliberately short. A process blocked in a filesystem call is in
/// uninterruptible sleep: `Ctrl-C` does nothing, `kill -9` does nothing, and the
/// only cure is aborting the FUSE connection as root. Worse, the cost compounds
/// — listing a directory issues a `lookup` and a `getattr` per entry, so a
/// per-operation wait of 20 s meant a ten-entry `ls` against an unresponsive
/// peer hung the shell for minutes. That is exactly what happened here: the
/// agent's own `ls` of the mount point wedged in `d_alloc_parallel` and had to
/// be killed from outside.
///
/// Three seconds is above any plausible round trip on a LAN (measured RTT here
/// is 6 ms) while keeping a dead peer to a brief stall rather than a hang: a
/// caller is in uninterruptible sleep for this long. Callers see `EIO`, which
/// every application already knows how to report.
///
/// This deadline covers metadata only; bulk data waits [`BULK_OP_TIMEOUT`].
/// Raising it to survive a `stat` queued behind a download's chunks treats the
/// symptom — metadata now rides its own class above the bytes of a copy, so a
/// busy transfer no longer sits in front of it.
pub const OP_TIMEOUT: Duration = Duration::from_secs(3);

/// How long an operation carrying bulk data may wait.
///
/// Longer than [`OP_TIMEOUT`], because the two are bounded by different things.
/// A `stat` is tens of bytes and its answer is late only if the peer is in
/// trouble; a write is a megabyte that has to be compressed, queued behind
/// whatever else the link is carrying, and acknowledged. Judging both by the
/// same three seconds meant an ordinary busy link failed writes that were
/// merely waiting their turn — and one such failure starts an
/// [`UNRESPONSIVE_COOLOFF`] during which *every* operation is refused, so nine
/// late writes became seventeen device errors and Steam abandoned a download.
///
/// Fifteen seconds is comfortably above the worst queueing delay measured here
/// while staying below the peer-liveness deadline, so a genuinely dead peer is
/// still noticed by the session rather than waited on here.
pub const BULK_OP_TIMEOUT: Duration = Duration::from_secs(15);

/// How many threads the OS should use to call into a mount.
///
/// This is the mount's concurrency and nothing else. Every callback blocks its
/// thread for a full round trip ([`PeerLink::call`]), so the thread count *is*
/// the number of operations that can be outstanding — on a single-threaded
/// dispatcher a copy of many small files runs at one file per round trip no
/// matter how much bandwidth is free. Threads waiting on the network cost a
/// request buffer each and no CPU, which is why the safe error is to have too
/// many rather than too few.
///
/// Shared by both mount implementations because the reasoning is the same on
/// each: FUSE and WinFsp differ in how the OS asks, not in what it costs us to
/// wait. Scaled with the machine, with a floor that keeps a small laptop from
/// serialising a directory walk and a ceiling that keeps a large server from
/// reserving buffers nothing will use — above it the limit is the session's
/// in-flight window, which is sized from the link itself
/// (`cursio_core::files_session::Pacer`) rather than guessed at here.
pub fn mount_threads() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores * 4).clamp(16, 64)
}

/// One thing the mount wants the peer to do.
///
/// Each carries the channel its answer comes back on, so replies cannot be
/// mixed up between concurrent callers.
#[derive(Debug)]
pub enum Op {
    Volumes {
        reply: std_mpsc::Sender<Result<Vec<Volume>, FileError>>,
    },
    ReadDir {
        volume: String,
        path: String,
        reply: std_mpsc::Sender<Result<Vec<FileMeta>, FileError>>,
    },
    Stat {
        volume: String,
        path: String,
        reply: std_mpsc::Sender<Result<FileMeta, FileError>>,
    },
    ReadLink {
        volume: String,
        path: String,
        reply: std_mpsc::Sender<Result<String, FileError>>,
    },
    Open {
        volume: String,
        path: String,
        mode: OpenMode,
        reply: std_mpsc::Sender<Result<(FileHandle, u64), FileError>>,
    },
    Read {
        handle: FileHandle,
        offset: u64,
        len: u32,
        reply: std_mpsc::Sender<Result<Vec<u8>, FileError>>,
    },
    Write {
        handle: FileHandle,
        offset: u64,
        bytes: Vec<u8>,
        reply: std_mpsc::Sender<Result<u64, FileError>>,
    },
    Truncate {
        handle: FileHandle,
        size: u64,
        /// Commit the space rather than move the end of the file. See
        /// [`PeerLink::set_size`].
        reserve: bool,
        reply: std_mpsc::Sender<Result<(), FileError>>,
    },
    /// Fire-and-forget: nothing useful comes back, and waiting a round trip for
    /// an acknowledgement would stall every `close()`.
    Close { handle: FileHandle },
    MkDir {
        volume: String,
        path: String,
        reply: std_mpsc::Sender<Result<(), FileError>>,
    },
    Remove {
        volume: String,
        path: String,
        reply: std_mpsc::Sender<Result<(), FileError>>,
    },
    Rename {
        volume: String,
        from: String,
        to: String,
        reply: std_mpsc::Sender<Result<(), FileError>>,
    },
}

/// The mount's handle on the session.
///
/// Cloneable and `Send`, because FUSE calls its filesystem from several threads
/// at once.
#[derive(Clone)]
pub struct PeerLink {
    ops: tokio::sync::mpsc::UnboundedSender<Op>,
    /// When the peer last failed to answer in time.
    ///
    /// A timeout is rarely an isolated event: if one operation went unanswered,
    /// the next almost certainly will too. Without this, every entry of a
    /// directory listing pays the full [`OP_TIMEOUT`] in turn, and the caller —
    /// stuck in uninterruptible sleep the whole while — waits entries × timeout.
    /// Remembering the failure collapses that to one wait, after which calls
    /// fail immediately until the cool-off expires and one is let through to
    /// test the water again.
    unresponsive_since: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Set when the session is tearing down; see [`PeerLink::shut_down`].
    shutting_down: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// How long to keep failing fast after the peer missed a deadline.
///
/// Long enough that a listing of any size costs one timeout rather than one per
/// entry; short enough that a peer which comes back is noticed within a couple
/// of operations.
const UNRESPONSIVE_COOLOFF: Duration = Duration::from_secs(5);

impl PeerLink {
    pub fn new(ops: tokio::sync::mpsc::UnboundedSender<Op>) -> Self {
        Self {
            ops,
            unresponsive_since: std::sync::Arc::new(std::sync::Mutex::new(None)),
            shutting_down: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Stop answering: the session is going away and the mounts are next.
    ///
    /// Unmounting waits for the filesystem's own dispatcher threads to return,
    /// and each of those may be sitting in [`PeerLink::call_within`] for an
    /// answer that is never coming. Dropping the session's end of the channel
    /// would release them — but the channel's sender lives in the engine, which
    /// is still alive at that moment, so `recv_timeout` waits out its full
    /// deadline instead of failing at once. With writes pipelined that is
    /// [`BULK_OP_TIMEOUT`] per outstanding write, and the process was seen
    /// stuck in `unmount` with its last log line naming it.
    ///
    /// Set before the mounts are dropped, so every waiting thread fails
    /// immediately and the unmount can proceed.
    pub fn shut_down(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
    }

    /// Whether the session has given up; see [`PeerLink::shut_down`].
    fn is_shut_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// Whether to refuse immediately instead of waiting out another timeout.
    fn in_cooloff(&self) -> bool {
        let guard = self.unresponsive_since.lock().expect("peer health");
        guard.is_some_and(|at| at.elapsed() < UNRESPONSIVE_COOLOFF)
    }

    fn mark_unresponsive(&self) {
        *self.unresponsive_since.lock().expect("peer health") = Some(std::time::Instant::now());
    }

    fn mark_responsive(&self) {
        *self.unresponsive_since.lock().expect("peer health") = None;
    }

    /// Submit an operation and wait for its answer.
    ///
    /// `build` is handed the reply channel so each variant can carry its own
    /// result type. Both failure modes — the session gone, or the peer too slow
    /// — come back as [`FileError::Io`], which the caller turns into `EIO`.
    fn call<T>(
        &self,
        build: impl FnOnce(std_mpsc::Sender<Result<T, FileError>>) -> Op,
    ) -> Result<T, FileError> {
        self.call_within(OP_TIMEOUT, build)
    }

    /// As [`PeerLink::call`], but waiting `limit` rather than the metadata
    /// deadline. See [`BULK_OP_TIMEOUT`].
    fn call_within<T>(
        &self,
        limit: Duration,
        build: impl FnOnce(std_mpsc::Sender<Result<T, FileError>>) -> Op,
    ) -> Result<T, FileError> {
        // Still within a cool-off from an earlier timeout: fail now rather than
        // making this caller wait out another one.
        //
        // The cool-off applies to metadata only.
        //
        // It exists so a directory walk against a peer that has stopped
        // answering costs one wait rather than one per entry — and a walk is
        // metadata. Applying it to data as well meant a single late `stat`
        // silenced every write for the next five seconds: measured here, three
        // timeouts produced 2226 refusals, and each one reached Windows as a
        // device error. A transfer that was working died because something
        // unrelated to it was slow once.
        //
        // A write is worth attempting even when the last `stat` was late: it
        // has its own, longer deadline, and if the peer really is gone it will
        // find that out for itself.
        if limit == OP_TIMEOUT && self.in_cooloff() {
            tracing::debug!("refusing a metadata operation: still in the cool-off from a timeout");
            return Err(FileError::Io("peer is not responding".into()));
        }
        if self.is_shut_down() {
            return Err(FileError::Io(
                "file sharing session is shutting down".into(),
            ));
        }
        let (tx, rx) = std_mpsc::channel();
        self.ops.send(build(tx)).map_err(|_| {
            tracing::warn!("a file operation could not be queued: the session is gone");
            FileError::Io("file sharing session is gone".into())
        })?;
        self.await_reply(rx, limit)
    }

    /// Wait for one already-queued reply, applying the same liveness rules as
    /// [`PeerLink::call_within`].
    ///
    /// Split out so a caller that queued several requests at once can await
    /// them without each having to re-implement the timeout, the cool-off rule
    /// and the disconnect case — see [`PeerLink::write`], which sends the
    /// pieces of one large write together rather than one after another.
    fn await_reply<T>(
        &self,
        rx: std_mpsc::Receiver<Result<T, FileError>>,
        limit: Duration,
    ) -> Result<T, FileError> {
        if self.is_shut_down() {
            // Whatever this was waiting for, nothing will answer it now, and
            // the unmount is behind this thread.
            return Err(FileError::Io(
                "file sharing session is shutting down".into(),
            ));
        }
        match rx.recv_timeout(limit) {
            Ok(result) => {
                self.mark_responsive();
                result
            }
            // The session dropped the reply channel: it is shutting down or the
            // peer disconnected mid-operation.
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("a file operation was dropped mid-flight; the peer disconnected");
                Err(FileError::Io("peer disconnected".into()))
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // Only a *metadata* timeout means the peer is unresponsive.
                //
                // The cool-off exists so a directory walk against a dead peer
                // costs one wait rather than one per entry, and metadata is
                // what a walk is made of. A late bulk transfer is a different
                // thing: the peer is answering, the link is merely full.
                // Starting a cool-off for one made every operation of the next
                // five seconds fail — nine late writes turned into seventeen
                // device errors, which is what Steam reported as a failing
                // disk while the link was working.
                if limit == OP_TIMEOUT {
                    self.mark_unresponsive();
                }
                tracing::warn!(
                    timeout_s = limit.as_secs(),
                    "peer did not answer a file operation; failing fast until it does"
                );
                Err(FileError::Io("peer did not answer in time".into()))
            }
        }
    }

    pub fn volumes(&self) -> Result<Vec<Volume>, FileError> {
        self.call(|reply| Op::Volumes { reply })
    }

    pub fn read_dir(&self, volume: &str, path: &str) -> Result<Vec<FileMeta>, FileError> {
        self.call(|reply| Op::ReadDir {
            volume: volume.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    pub fn stat(&self, volume: &str, path: &str) -> Result<FileMeta, FileError> {
        self.call(|reply| Op::Stat {
            volume: volume.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    pub fn read_link(&self, volume: &str, path: &str) -> Result<String, FileError> {
        self.call(|reply| Op::ReadLink {
            volume: volume.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    pub fn open(
        &self,
        volume: &str,
        path: &str,
        mode: OpenMode,
    ) -> Result<(FileHandle, u64), FileError> {
        self.call(|reply| Op::Open {
            volume: volume.to_string(),
            path: path.to_string(),
            mode,
            reply,
        })
    }

    pub fn read(&self, handle: FileHandle, offset: u64, len: u32) -> Result<Vec<u8>, FileError> {
        self.call_within(BULK_OP_TIMEOUT, |reply| Op::Read {
            handle,
            offset,
            len,
            reply,
        })
    }

    /// Send one write without waiting for its answer, returning the channel the
    /// answer will arrive on.
    ///
    /// For a mount keeping several writes in flight against one handle — see
    /// [`crate::writeback`] for why that is the difference between one round
    /// trip per write and a full link.
    pub fn write_later(
        &self,
        handle: FileHandle,
        offset: u64,
        bytes: Vec<u8>,
    ) -> Result<std_mpsc::Receiver<Result<u64, FileError>>, FileError> {
        let (tx, rx) = std_mpsc::channel();
        self.ops
            .send(Op::Write {
                handle,
                offset,
                bytes,
                reply: tx,
            })
            .map_err(|_| {
                tracing::warn!("a write could not be queued: the session is gone");
                FileError::Io("file sharing session is gone".into())
            })?;
        Ok(rx)
    }

    /// Wait for a reply from [`PeerLink::write_later`], with the bulk deadline.
    pub fn await_write(
        &self,
        rx: std_mpsc::Receiver<Result<u64, FileError>>,
        limit: Duration,
    ) -> Result<u64, FileError> {
        self.await_reply(rx, limit)
    }

    /// Write `bytes` at `offset`, in pieces no larger than the wire chunk.
    ///
    /// The caller's buffer is whatever the application chose, and Windows
    /// passes it through: Steam writes about 1.2 MB at a time, which left here
    /// as one logical message and reached the link as nineteen Noise frames
    /// back to back. `MAX_CHUNK` exists to bound exactly that — a transfer
    /// yields to input between chunks, and a message that spans nineteen frames
    /// does not yield for the length of all of them. Measured at the rate this
    /// link achieves, one such write occupies it for about 75 ms, which is felt
    /// as the mouse stalling.
    ///
    /// Split here rather than at the session: this is where the oversized
    /// buffer arrives, and each piece then travels as its own request with its
    /// own reply, so a short write still reports what was actually stored.
    pub fn write(&self, handle: FileHandle, offset: u64, bytes: Vec<u8>) -> Result<u64, FileError> {
        let chunk = cursio_protocol::FilesMsg::MAX_CHUNK;
        if bytes.len() <= chunk {
            return self.call_within(BULK_OP_TIMEOUT, |reply| Op::Write {
                handle,
                offset,
                bytes,
                reply,
            });
        }

        // Queued together, then awaited together. One at a time would trade a
        // single round trip for one per piece, and the thread count *is* the
        // concurrency here (see `mount_threads`): an application writing a
        // large buffer from one thread would go strictly slower for being
        // split. Sent this way the pieces share the one round trip they always
        // did, while each stays small enough for the link to interleave input
        // between them.
        let mut waiting = Vec::new();
        for (idx, piece) in bytes.chunks(chunk).enumerate() {
            let (tx, rx) = std_mpsc::channel();
            let at = offset + (idx * chunk) as u64;
            if self
                .ops
                .send(Op::Write {
                    handle,
                    offset: at,
                    bytes: piece.to_vec(),
                    reply: tx,
                })
                .is_err()
            {
                tracing::warn!("a write could not be queued: the session is gone");
                return Err(FileError::Io("file sharing session is gone".into()));
            }
            waiting.push((rx, piece.len() as u64));
        }

        // Counted in order: a short piece ends the write there, because what
        // follows it landed at an offset the caller must not be told was
        // reached. The later answers are collected regardless so their pieces
        // are not left pending.
        let mut written = 0u64;
        let mut stopped = false;
        for (rx, len) in waiting {
            let got = self.await_reply(rx, BULK_OP_TIMEOUT);
            if stopped {
                continue;
            }
            match got {
                Ok(n) => {
                    written += n;
                    stopped = n < len;
                }
                Err(e) => {
                    if written == 0 {
                        return Err(e);
                    }
                    stopped = true;
                }
            }
        }
        Ok(written)
    }

    pub fn truncate(&self, handle: FileHandle, size: u64) -> Result<(), FileError> {
        self.set_size(handle, size, false)
    }

    /// Set a file's length, or reserve the space it will need.
    ///
    /// `reserve` picks which: `false` moves the end of the file, `true` commits
    /// room for it without changing how long the file is. Windows asks for both
    /// through one call and distinguishes them by a flag; the FUSE mount only
    /// ever asks for the first, since `truncate(2)` has no notion of the
    /// second. See the `Truncate` message in the protocol for what conflating
    /// them cost.
    pub fn set_size(&self, handle: FileHandle, size: u64, reserve: bool) -> Result<(), FileError> {
        // Reserving space can mean committing gigabytes on the owner's disk,
        // which is bulk work however small the request that asks for it.
        self.call_within(BULK_OP_TIMEOUT, |reply| Op::Truncate {
            handle,
            size,
            reserve,
            reply,
        })
    }

    /// Release a remote handle. Errors are ignored: a close has nowhere to
    /// report to, and if the session is gone the peer has already dropped it.
    pub fn close(&self, handle: FileHandle) {
        let _ = self.ops.send(Op::Close { handle });
    }

    pub fn mkdir(&self, volume: &str, path: &str) -> Result<(), FileError> {
        self.call(|reply| Op::MkDir {
            volume: volume.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    pub fn remove(&self, volume: &str, path: &str) -> Result<(), FileError> {
        self.call(|reply| Op::Remove {
            volume: volume.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    pub fn rename(&self, volume: &str, from: &str, to: &str) -> Result<(), FileError> {
        self.call(|reply| Op::Rename {
            volume: volume.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            reply,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dead session must surface as an error, not a hang.
    ///
    /// This is the difference between a stale mount the user can unmount and a
    /// process wedged in uninterruptible sleep.
    #[test]
    fn a_dead_session_fails_instead_of_hanging() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);
        drop(rx); // the session went away

        let started = std::time::Instant::now();
        let got = link.stat("C:", "a.txt");
        assert!(matches!(got, Err(FileError::Io(_))), "got {got:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a dead session must fail at once, not wait out the timeout"
        );
    }

    /// An oversized write is split to the wire chunk, in order and in full.
    ///
    /// Windows hands through whatever buffer the application chose — Steam's is
    /// about 1.2 MB — and one logical message that large reaches the link as
    /// nineteen Noise frames back to back, which is 75 ms during which nothing
    /// higher-priority gets out.
    #[test]
    fn a_large_write_is_split_into_wire_chunks() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);
        let chunk = cursio_protocol::FilesMsg::MAX_CHUNK;
        let total = chunk * 2 + 1024;

        let writer = std::thread::spawn(move || link.write(7, 0, vec![0u8; total]));

        let mut seen = Vec::new();
        while seen.len() < 3 {
            let Some(op) = rx.blocking_recv() else { break };
            if let Op::Write {
                offset,
                bytes,
                reply,
                ..
            } = op
            {
                seen.push((offset, bytes.len()));
                let _ = reply.send(Ok(bytes.len() as u64));
            }
        }

        let written = writer.join().expect("writer").expect("write succeeds");
        assert_eq!(written, total as u64, "every byte was written");
        assert_eq!(
            seen,
            vec![(0, chunk), (chunk as u64, chunk), (chunk as u64 * 2, 1024)],
            "pieces are chunk-sized, in order, and cover the whole buffer"
        );
    }

    /// The pieces of one write are outstanding together, not one after another.
    ///
    /// The thread count is this mount's concurrency: every callback blocks its
    /// own thread for a round trip. So splitting a large write into pieces and
    /// awaiting each in turn would trade one round trip for one per piece, and
    /// an application writing from a single thread — which is what Steam does —
    /// would go strictly slower for the split.
    #[test]
    fn the_pieces_of_one_write_are_in_flight_together() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);
        let chunk = cursio_protocol::FilesMsg::MAX_CHUNK;

        let writer = std::thread::spawn(move || link.write(7, 0, vec![0u8; chunk * 3]));

        // Collect every request before answering any: if the writer awaited
        // each piece before sending the next, the third would never arrive.
        let mut held = Vec::new();
        while held.len() < 3 {
            let Some(op) = rx.blocking_recv() else {
                break;
            };
            if let Op::Write { bytes, reply, .. } = op {
                held.push((bytes.len(), reply));
            }
        }
        assert_eq!(
            held.len(),
            3,
            "all three pieces were queued before any answer"
        );

        for (len, reply) in held {
            let _ = reply.send(Ok(len as u64));
        }
        let written = writer.join().expect("writer").expect("write succeeds");
        assert_eq!(written, (chunk * 3) as u64, "every byte was written");
    }

    /// Once the session is shutting down, waiting callers fail at once.
    ///
    /// Unmounting waits for the filesystem's dispatcher threads to return, and
    /// those threads wait here. The session's sender lives in the engine, which
    /// is still alive at that point, so nothing closes the channel for them and
    /// each waits out its full deadline — `BULK_OP_TIMEOUT` per outstanding
    /// write once writes are pipelined. Live, the process was left stuck in
    /// `unmount` with a single thread in an uninterruptible kernel wait.
    #[test]
    fn a_shutting_down_link_fails_instead_of_waiting_out_the_deadline() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);
        // `_rx` is deliberately kept alive: the session's receiver outliving
        // the decision to stop is the situation this exists for.
        link.shut_down();

        let started = std::time::Instant::now();
        let got = link.write(7, 0, vec![0u8; 64]);
        assert!(matches!(got, Err(FileError::Io(_))), "got {got:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a stopping link fails at once rather than waiting out {BULK_OP_TIMEOUT:?}"
        );
    }

    /// A session that accepts the request but never answers must also fail,
    /// once the timeout expires — the peer can vanish mid-operation.
    #[test]
    fn a_silent_peer_times_out() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);

        // Take the request and drop its reply channel, as a session shutting
        // down mid-flight would.
        let taker = std::thread::spawn(move || {
            let op = rx.blocking_recv().expect("request arrives");
            drop(op);
        });

        let got = link.stat("C:", "a.txt");
        assert!(
            matches!(got, Err(FileError::Io(_))),
            "a request nobody answers must fail, got {got:?}"
        );
        taker.join().expect("taker finishes");
    }

    /// A directory listing against a dead peer must cost ONE timeout, not one
    /// per entry.
    ///
    /// This is the bug that wedged the agent twice: `ls` of the mount point
    /// issues a lookup per entry, each waited out the full timeout, and the
    /// process sat in uninterruptible sleep for minutes — unkillable, because a
    /// task blocked in a filesystem call ignores every signal.
    #[test]
    fn repeated_calls_to_a_dead_peer_fail_fast() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);
        // Nobody ever answers: hold the receiver so sends succeed but replies
        // never come, which is what an unresponsive peer looks like.
        let _held = rx;

        let first = std::time::Instant::now();
        assert!(link.stat("C:", "a.txt").is_err());
        let first_took = first.elapsed();
        assert!(
            first_took >= OP_TIMEOUT,
            "the first call must actually wait for the peer"
        );

        // Every call after it returns at once, from the cool-off.
        let rest = std::time::Instant::now();
        for i in 0..20 {
            assert!(link.stat("C:", &format!("entry-{i}.txt")).is_err());
        }
        assert!(
            rest.elapsed() < OP_TIMEOUT,
            "20 further calls took {:?}; they must not each wait out {:?}",
            rest.elapsed(),
            OP_TIMEOUT
        );
    }

    /// A slow bulk transfer must not silence the whole mount.
    ///
    /// The cool-off is for a peer that has stopped answering, and it refuses
    /// *every* operation while it lasts. A write that was merely queued behind
    /// other traffic is not that: the peer is answering, the link is full. One
    /// such write starting a cool-off is how nine late writes became seventeen
    /// device errors and Steam abandoned a download over a working link.
    #[test]
    fn a_slow_write_does_not_silence_every_other_operation() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);
        let _held = rx; // accepted, never answered

        // A bulk deadline, waited out with a short one rather than the real
        // fifteen seconds: what is under test is which deadline trips the
        // cool-off, not how long each is. Any limit other than `OP_TIMEOUT`
        // takes the bulk path.
        let got = link.call_within(Duration::from_millis(50), |reply| Op::Write {
            handle: 1,
            offset: 0,
            bytes: Vec::new(),
            reply,
        });
        assert!(got.is_err(), "an unanswered write must fail");

        // A `stat` right after must still be attempted rather than refused out
        // of hand: nothing has said the peer is unresponsive.
        assert!(
            !link.in_cooloff(),
            "a late bulk transfer must not mark the peer unresponsive"
        );
    }

    /// A metadata timeout still starts the cool-off, which is what it is for.
    #[test]
    fn a_metadata_timeout_still_trips_the_cool_off() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);
        let _held = rx;

        assert!(link.stat("C:", "a.txt").is_err());
        assert!(
            link.in_cooloff(),
            "an unanswered stat means the peer is not answering"
        );
    }

    /// Bulk work gets the longer deadline; metadata keeps the short one.
    #[test]
    fn bulk_waits_longer_than_metadata() {
        assert!(
            BULK_OP_TIMEOUT > OP_TIMEOUT,
            "a megabyte queued behind other traffic needs longer than a stat"
        );
        assert!(
            BULK_OP_TIMEOUT <= Duration::from_secs(20),
            "still short enough that a dead peer is a stall, not a hang"
        );
    }

    /// The timeout has to stay short enough that a stalled filesystem call is a
    /// stutter rather than a hang.
    #[test]
    fn the_timeout_is_short_enough_to_be_survivable() {
        assert!(
            OP_TIMEOUT <= Duration::from_secs(5),
            "a caller is in uninterruptible sleep for this long; {OP_TIMEOUT:?} is too long"
        );
        assert!(
            OP_TIMEOUT >= Duration::from_secs(1),
            "must not trip on an ordinary slow stat"
        );
    }

    /// A late `stat` must not silence writes.
    ///
    /// The cool-off is a metadata measure: it saves a directory walk from
    /// paying a timeout per entry. Applied to data as well, one slow `stat`
    /// refused every write for five seconds — three timeouts produced 2226
    /// device errors here, and a working download died of them.
    #[test]
    fn the_cool_off_does_not_reach_bulk_transfers() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);
        let _held = rx; // accepted, never answered

        // Put the link into cool-off the way a late `stat` would.
        link.mark_unresponsive();
        assert!(link.in_cooloff());

        // Metadata is refused at once, as intended.
        let started = std::time::Instant::now();
        assert!(link.stat("C:", "a.txt").is_err());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "metadata must fail fast during the cool-off"
        );

        // A write is still attempted: it waits for its own deadline rather than
        // being refused for something another operation did.
        let started = std::time::Instant::now();
        let _ = link.call_within(Duration::from_millis(200), |reply| Op::Write {
            handle: 1,
            offset: 0,
            bytes: Vec::new(),
            reply,
        });
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "a write must be tried during a metadata cool-off, not refused"
        );
    }

    /// The mount must be able to hold enough operations open to keep the link
    /// busy, on any machine.
    ///
    /// This is the setting that decides whether a copy of many small files runs
    /// at the link's speed or at one file per round trip, and nothing fails
    /// when it is wrong — the filesystem behaves correctly either way, just far
    /// slower. So the sizing is asserted rather than left to be noticed.
    #[test]
    fn a_mount_can_keep_enough_operations_open_to_fill_the_link() {
        let threads = mount_threads();
        assert!(
            threads >= 16,
            "a 6 ms round trip and {threads} threads caps the mount at \
             {} operations a second",
            threads * 166,
        );
        assert!(
            threads <= 64,
            "each thread holds a request buffer; {threads} is memory for \
             concurrency the session's window will never use"
        );
    }

    /// The happy path: a request reaches the session and its answer comes back.
    #[test]
    fn an_answered_request_returns_its_value() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let link = PeerLink::new(tx);

        let responder = std::thread::spawn(move || {
            let Some(Op::Stat {
                volume,
                path,
                reply,
            }) = rx.blocking_recv()
            else {
                panic!("expected a Stat");
            };
            assert_eq!(volume, "C:");
            assert_eq!(path, "a.txt");
            reply
                .send(Ok(FileMeta {
                    name: "a.txt".into(),
                    kind: cursio_protocol::FileKind::File,
                    size: 42,
                    modified_unix: 0,
                    writable: true,
                    allocated: None,
                }))
                .expect("reply sends");
        });

        let meta = link.stat("C:", "a.txt").expect("answered");
        assert_eq!(meta.size, 42);
        responder.join().expect("responder finishes");
    }
}
