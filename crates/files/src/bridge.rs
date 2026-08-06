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
/// is 6 ms, and a cold read of a large chunk is still well under a second)
/// while keeping a dead peer to a brief stall rather than a hang. Callers see
/// `EIO`, which every application already knows how to report.
pub const OP_TIMEOUT: Duration = Duration::from_secs(3);

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
        }
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
        // Still within a cool-off from an earlier timeout: fail now rather than
        // making this caller wait out another one.
        if self.in_cooloff() {
            return Err(FileError::Io("peer is not responding".into()));
        }
        let (tx, rx) = std_mpsc::channel();
        self.ops
            .send(build(tx))
            .map_err(|_| FileError::Io("file sharing session is gone".into()))?;
        match rx.recv_timeout(OP_TIMEOUT) {
            Ok(result) => {
                self.mark_responsive();
                result
            }
            // The session dropped the reply channel: it is shutting down or the
            // peer disconnected mid-operation.
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                Err(FileError::Io("peer disconnected".into()))
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                self.mark_unresponsive();
                tracing::warn!(
                    timeout_s = OP_TIMEOUT.as_secs(),
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
        self.call(|reply| Op::Read {
            handle,
            offset,
            len,
            reply,
        })
    }

    pub fn write(&self, handle: FileHandle, offset: u64, bytes: Vec<u8>) -> Result<u64, FileError> {
        self.call(|reply| Op::Write {
            handle,
            offset,
            bytes,
            reply,
        })
    }

    pub fn truncate(&self, handle: FileHandle, size: u64) -> Result<(), FileError> {
        self.call(|reply| Op::Truncate {
            handle,
            size,
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
            "must not trip on an ordinary slow read"
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
                }))
                .expect("reply sends");
        });

        let meta = link.stat("C:", "a.txt").expect("answered");
        assert_eq!(meta.size, 42);
        responder.join().expect("responder finishes");
    }
}
