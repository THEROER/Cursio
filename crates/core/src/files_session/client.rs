//! Our requests to the peer: numbering them, holding the callers until
//! their answers come, failing the ones that never do, and the closes
//! that must wait for the writes they follow.

use crate::files_session::module::MAX_MESSAGES_PER_PUMP;
use crate::files_session::serve::pack;
use crate::files_session::serve::unpack;
use crate::files_session::wire::reply_req;
use crate::files_session::wire::request_kind;
use crate::files_session::wire::send;
use crate::files_session::FilesEngine;
use cursio_files::bridge::Op;
use cursio_protocol::{FileError, FileHandle, FileReqId, FilesMsg, Volume};
use cursio_transport::{TransportError, TransportTx};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One request we sent that has not been answered yet.
pub(super) enum Pending {
    Volumes(std::sync::mpsc::Sender<Result<Vec<Volume>, FileError>>),
    Dir {
        reply: std::sync::mpsc::Sender<Result<Vec<cursio_protocol::FileMeta>, FileError>>,
        /// Entries gathered so far: a large listing arrives in several messages.
        acc: Vec<cursio_protocol::FileMeta>,
    },
    Stat(std::sync::mpsc::Sender<Result<cursio_protocol::FileMeta, FileError>>),
    Link(std::sync::mpsc::Sender<Result<String, FileError>>),
    Open(std::sync::mpsc::Sender<Result<(cursio_protocol::FileHandle, u64), FileError>>),
    Read {
        reply: std::sync::mpsc::Sender<Result<Vec<u8>, FileError>>,
        acc: Vec<u8>,
    },
    Wrote(std::sync::mpsc::Sender<Result<u64, FileError>>),
    Unit(std::sync::mpsc::Sender<Result<(), FileError>>),
}

impl Pending {
    /// Hand this request's failure to whoever is waiting on it.
    pub(super) fn fail(self, e: FileError) {
        match self {
            Pending::Volumes(tx) => drop(tx.send(Err(e))),
            Pending::Dir { reply, .. } => drop(reply.send(Err(e))),
            Pending::Stat(tx) => drop(tx.send(Err(e))),
            Pending::Link(tx) => drop(tx.send(Err(e))),
            Pending::Open(tx) => drop(tx.send(Err(e))),
            Pending::Read { reply, .. } => drop(reply.send(Err(e))),
            Pending::Wrote(tx) => drop(tx.send(Err(e))),
            Pending::Unit(tx) => drop(tx.send(Err(e))),
        }
    }
}

/// How long a request of ours may go unanswered before it is given up on here.
///
/// The mount's own caller has stopped waiting by then — no operation waits
/// longer than `BULK_OP_TIMEOUT` — so the entry serves nobody. It still counts
/// against the window, though, and that is what made an unanswered request
/// expensive: the window is sized from the number of entries, so a handful the
/// peer never answered (a job that died on its side, a reply lost to a
/// reconnect of the transport under us) closed it for good, and every later
/// operation timed out without ever being sent. Seen as a mount that stopped
/// answering until the link was torn down and rebuilt.
pub(super) const PENDING_EXPIRY: Duration = cursio_files::bridge::BULK_OP_TIMEOUT;

/// Requests we have sent and not yet had answered, with when each went out.
#[derive(Default)]
pub(super) struct PendingMap(HashMap<FileReqId, (Pending, Instant)>);

impl PendingMap {
    fn insert(&mut self, req: FileReqId, pending: Pending) {
        self.0.insert(req, (pending, Instant::now()));
    }

    fn remove(&mut self, req: &FileReqId) -> Option<Pending> {
        self.0.remove(req).map(|(pending, _)| pending)
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every request still waiting for the peer's volume list.
    fn waiting_for_volumes(&self) -> Vec<FileReqId> {
        self.0
            .iter()
            .filter(|(_, (p, _))| matches!(p, Pending::Volumes(_)))
            .map(|(&req, _)| req)
            .collect()
    }

    pub(super) fn drain(&mut self) -> impl Iterator<Item = Pending> + '_ {
        self.0.drain().map(|(_, (pending, _))| pending)
    }

    /// Fail every request older than `limit`, returning how many there were.
    ///
    /// Failed rather than dropped, so a caller still waiting — there should be
    /// none, but a clock can jump — hears why instead of waiting out its own
    /// deadline.
    pub(super) fn expire(&mut self, now: Instant, limit: Duration) -> usize {
        let stale: Vec<FileReqId> = self
            .0
            .iter()
            .filter(|(_, (_, since))| now.saturating_duration_since(*since) >= limit)
            .map(|(&req, _)| req)
            .collect();
        for req in &stale {
            if let Some(pending) = self.remove(req) {
                pending.fail(FileError::Io("the peer never answered".into()));
            }
        }
        stale.len()
    }
}

impl FilesEngine {
    /// Queue a close, or hold it back while this handle still has writes being
    /// compressed.
    ///
    /// See [`FilesEngine::compressing`] for what closing too early cost.
    pub(super) fn close_handle(&mut self, handle: FileHandle) {
        if self
            .compressing
            .lock()
            .expect("files compressing")
            .contains_key(&handle)
        {
            self.deferred_closes.push(handle);
            return;
        }
        let _ = self.outbox_tx.send(FilesMsg::Close { handle });
    }

    /// Send the closes whose writes have since reached the queue.
    ///
    /// Ordering within `outbox` does the rest: the writes are already in it, so
    /// a close appended now is behind them.
    pub(super) fn flush_deferred_closes(&mut self) {
        if self.deferred_closes.is_empty() {
            return;
        }
        let guard = self.compressing.lock().expect("files compressing");
        let (ready, waiting): (Vec<_>, Vec<_>) = self
            .deferred_closes
            .drain(..)
            .partition(|h| !guard.contains_key(h));
        drop(guard);
        self.deferred_closes = waiting;
        for handle in ready {
            let _ = self.outbox_tx.send(FilesMsg::Close { handle });
        }
    }

    pub(super) fn next_req(&mut self) -> FileReqId {
        let id = self.next_req;
        self.next_req += 1;
        id
    }
    /// Turn queued mount operations into requests on the wire.
    ///
    /// Bounded three ways, each for its own reason: by the pacer's window, so
    /// the link is kept full but not backed up; by `deadline`, so the session
    /// loop gets its turn back; and by a message cap that only matters if the
    /// clock lies. Whatever is left waits for the next pump — a request is
    /// worth nothing early and everything eventually, unlike an audio frame.
    pub(super) async fn pump_requests(
        &mut self,
        tx: &mut dyn TransportTx,
        deadline: Instant,
    ) -> Result<(), TransportError> {
        // The window counts requests *outstanding*, which is what `pending`
        // holds: everything sent and not yet answered, plus the writes the
        // workers are still compressing.
        let mut allowance = self.pace.allowance(self.pending.len());
        let mut seen = 0;
        while allowance > 0 && seen < MAX_MESSAGES_PER_PUMP && Instant::now() < deadline {
            seen += 1;
            let Ok(op) = self.ops_rx.try_recv() else {
                break;
            };
            let req = self.next_req();
            let (pending, msg) = match op {
                Op::Volumes { reply } => {
                    // Answer from what the peer already told us. The list is
                    // pushed on connect and on any change, so asking again
                    // would only add a round trip — and, before the mount has
                    // heard the first announcement, a timeout.
                    if !self.peer_volumes.is_empty() {
                        let _ = reply.send(Ok(self.peer_volumes.clone()));
                        continue;
                    }
                    (Pending::Volumes(reply), FilesMsg::ListVolumes { req })
                }
                Op::ReadDir {
                    volume,
                    path,
                    reply,
                } => (
                    Pending::Dir {
                        reply,
                        acc: Vec::new(),
                    },
                    FilesMsg::ReadDir { req, volume, path },
                ),
                Op::Stat {
                    volume,
                    path,
                    reply,
                } => (Pending::Stat(reply), FilesMsg::Stat { req, volume, path }),
                Op::ReadLink {
                    volume,
                    path,
                    reply,
                } => (
                    Pending::Link(reply),
                    FilesMsg::ReadLink { req, volume, path },
                ),
                Op::Open {
                    volume,
                    path,
                    mode,
                    reply,
                } => (
                    Pending::Open(reply),
                    FilesMsg::Open {
                        req,
                        volume,
                        path,
                        mode,
                    },
                ),
                Op::Read {
                    handle,
                    offset,
                    len,
                    reply,
                } => (
                    Pending::Read {
                        reply,
                        acc: Vec::new(),
                    },
                    FilesMsg::Read {
                        req,
                        handle,
                        offset,
                        len,
                    },
                ),
                // Compressed on a worker, not here.
                //
                // `pack` on a 256 KiB chunk is milliseconds of CPU, and a copy
                // produces one per chunk without pause — inline, that is the
                // session loop spending most of a transfer inside zstd while
                // input and audio wait their turn behind it. The prepared
                // message comes back through `outbox` and goes out on a later
                // pump; the request counts against the window from now, because
                // it is work the peer will owe us an answer for.
                Op::Write {
                    handle,
                    offset,
                    bytes,
                    reply,
                } => {
                    self.pending.insert(req, Pending::Wrote(reply));
                    // Timed from here rather than from the send: the round trip
                    // the caller waits out includes the compression, so timing
                    // only the wire would understate what a write costs.
                    self.health.sent(req, "write", Instant::now());
                    self.health.written_bytes += bytes.len() as u64;
                    let outbox = self.outbox_tx.clone();
                    // Counted before the worker starts, cleared only once the
                    // message is on the queue, so a close arriving in between
                    // can see that this handle still owes bytes to the link.
                    let compressing = std::sync::Arc::clone(&self.compressing);
                    *compressing
                        .lock()
                        .expect("files compressing")
                        .entry(handle)
                        .or_insert(0) += 1;
                    self.codec.spawn(move || {
                        let packed = pack(bytes);
                        let _ = outbox.send(FilesMsg::Write {
                            req,
                            handle,
                            offset,
                            bytes: packed,
                            last: true,
                        });
                        let mut guard = compressing.lock().expect("files compressing");
                        if let Some(n) = guard.get_mut(&handle) {
                            *n -= 1;
                            if *n == 0 {
                                guard.remove(&handle);
                            }
                        }
                    });
                    allowance -= 1;
                    continue;
                }
                Op::Truncate {
                    handle,
                    size,
                    reserve,
                    reply,
                } => (
                    Pending::Unit(reply),
                    FilesMsg::Truncate {
                        req,
                        handle,
                        size,
                        reserve,
                    },
                ),
                Op::MkDir {
                    volume,
                    path,
                    reply,
                } => (Pending::Unit(reply), FilesMsg::MkDir { req, volume, path }),
                Op::Remove {
                    volume,
                    path,
                    reply,
                } => (Pending::Unit(reply), FilesMsg::Remove { req, volume, path }),
                Op::Rename {
                    volume,
                    from,
                    to,
                    reply,
                } => (
                    Pending::Unit(reply),
                    FilesMsg::Rename {
                        req,
                        volume,
                        from,
                        to,
                    },
                ),
                // Nothing to wait for, so nothing to track — but it must not
                // overtake this handle's writes.
                //
                // A `Write` is compressed on a worker and reaches the link
                // through `outbox` on a later pump; sending the close straight
                // down the socket here jumped that queue, so the peer closed
                // the handle and answered the writes that arrived after it with
                // `BadHandle`. The bytes were never written and the application
                // saw the failure at the end of a file it had finished writing
                // — which is what Steam reports as "Disk Write Error".
                //
                // Queueing it behind them restores the ordering the caller
                // expects: writes still compressing are waited for, everything
                // already queued goes out first, and the close lands last.
                Op::Close { handle } => {
                    self.close_handle(handle);
                    continue;
                }
            };

            // The peer answers `Volumes` with its own list; the request carries
            // no payload of ours, so an empty list is the ask.
            self.pending.insert(req, pending);
            self.health.sent(req, request_kind(&msg), Instant::now());
            allowance -= 1;
            send(tx, msg).await?;
        }
        Ok(())
    }

    /// Route one reply back to the callback waiting for it.
    pub(super) fn on_reply(&mut self, msg: FilesMsg) {
        // Timed here, before the match, so every reply is measured in one place
        // rather than each arm having to remember to. A `DirEntries` that is not
        // the last batch is timed at its first message, which is the round trip
        // the caller actually waited for.
        let now = Instant::now();
        if let Some(req) = reply_req(&msg) {
            self.health.answered(req, now);
        }
        if let FilesMsg::Data { bytes, .. } = &msg {
            // Deliberately not fed to the pacer. These bytes crossed the link
            // in the other direction, and the window being sized is on ours:
            // counting them made a mount that was reading report 303 Mbit/s of
            // outbound capacity against the 144 the socket was managing, so the
            // window grew on evidence from a direction it does not control.
            self.health.read_bytes += bytes.plain_len() as u64;
        }
        if let FilesMsg::Failed { error, .. } = &msg {
            if super::health::is_fault(error) {
                self.health.failed += 1;
            } else {
                self.health.refused += 1;
            }
            // Logged, not just counted. A failure rate says something is wrong;
            // only the reason says what, and an operation the peer refuses
            // leaves no trace on this side otherwise — the count alone had me
            // guessing at which operation was failing.
            tracing::debug!(peer = %self.peer_name, ?error, "peer refused a file operation");
        }

        match msg {
            FilesMsg::Volumes(volumes) => {
                // Remember them: the peer announces its volumes unprompted a few
                // seconds after the session starts, which is *after* the mount
                // has already asked and timed out. Without keeping the answer,
                // that first listing fails and every later one pays a round trip
                // to learn what we were told for free.
                //
                // An empty list never replaces a known one. A machine with no
                // shareable disks is not a case worth modelling, whereas an
                // empty announcement arriving for any other reason would blank
                // a working mount — which is exactly what happened when the
                // request and the announcement were the same message.
                if !volumes.is_empty() {
                    self.peer_volumes = volumes.clone();
                    // Follow the set: on Windows each volume is its own drive
                    // letter, so this is where a peer's disks appear and
                    // disappear.
                    self.sync_peer_mounts();
                }
                // The peer announces its volumes unprompted at start, and also
                // answers our request with the same message — so satisfy every
                // waiting request rather than assuming there is exactly one.
                for req in self.pending.waiting_for_volumes() {
                    if let Some(Pending::Volumes(tx)) = self.pending.remove(&req) {
                        let _ = tx.send(Ok(volumes.clone()));
                    }
                }
                tracing::info!(
                    peer = %self.peer_name,
                    count = volumes.len(),
                    "peer offered its volumes"
                );
            }
            FilesMsg::DirEntries { req, entries, last } => {
                let Some(Pending::Dir { reply, mut acc }) = self.pending.remove(&req) else {
                    return;
                };
                acc.extend(entries);
                if last {
                    let _ = reply.send(Ok(acc));
                } else {
                    // More batches to come: keep the request open.
                    self.pending.insert(req, Pending::Dir { reply, acc });
                }
            }
            FilesMsg::StatResult { req, meta } => {
                if let Some(Pending::Stat(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok(meta));
                }
            }
            FilesMsg::LinkTarget { req, target } => {
                if let Some(Pending::Link(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok(target));
                }
            }
            FilesMsg::Opened { req, handle, size } => {
                if let Some(Pending::Open(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok((handle, size)));
                }
            }
            FilesMsg::Data {
                req, bytes, last, ..
            } => {
                let Some(Pending::Read { reply, mut acc }) = self.pending.remove(&req) else {
                    return;
                };
                // The whole read in one message — every read the server sends
                // today — is decompressed on a worker, which then answers the
                // waiting mount thread directly. Inline it is a quarter of a
                // millisecond per chunk on the session loop, several hundred
                // times a second while a file is being read: not a stall, but a
                // steady tax on input latency for the length of every copy.
                if last && acc.is_empty() {
                    self.codec.spawn(move || {
                        let _ = reply.send(unpack(bytes));
                    });
                    return;
                }
                // Split across messages: the pieces have to be joined in order,
                // so this path stays here. Rare enough not to matter.
                match unpack(bytes) {
                    Ok(b) => acc.extend(b),
                    Err(e) => return drop(reply.send(Err(e))),
                }
                if last {
                    let _ = reply.send(Ok(acc));
                } else {
                    self.pending.insert(req, Pending::Read { reply, acc });
                }
            }
            FilesMsg::Wrote { req, written } => {
                // The peer has stored these bytes, which makes this the one
                // measurement of the link that cannot outrun it.
                self.pace.sample_acked(written, Instant::now());
                if let Some(Pending::Wrote(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok(written));
                }
            }
            FilesMsg::Ok { req } => {
                if let Some(Pending::Unit(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok(()));
                }
            }
            FilesMsg::Failed { req, error } => {
                if let Some(pending) = self.pending.remove(&req) {
                    pending.fail(error);
                }
            }
            // A request from the peer, not a reply — served elsewhere.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files_session::health::Health;
    use crate::files_session::pacer::MIN_IN_FLIGHT;
    use crate::files_session::test_support::a_request;
    use crate::files_session::test_support::engine;
    use crate::files_session::test_support::RecordingTx;
    use cursio_files::Workers;

    /// The mount can queue far more work than the link can carry; only the
    /// window's worth may go out.
    ///
    /// Without this the queue depth is whatever the mount's thread count
    /// happens to be — dozens of chunks committed to a link that has not
    /// finished the first, which is delay for everything else riding it and
    /// memory held on both machines for work nobody is waiting on yet.
    #[tokio::test]
    async fn only_the_window_goes_out_however_much_is_waiting() {
        let (mut engine, ops) = engine();
        for _ in 0..100 {
            ops.send(a_request()).expect("queued");
        }

        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");

        assert_eq!(
            tx.sent.len(),
            MIN_IN_FLIGHT,
            "a fresh pacer opens at its floor and grows from there"
        );
        assert_eq!(engine.pending.len(), MIN_IN_FLIGHT);

        // Nothing further goes out until answers come back: the window counts
        // what is outstanding, not what was sent this pass.
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");
        assert_eq!(tx.sent.len(), MIN_IN_FLIGHT, "the window is still full");
    }

    /// Requests the peer never answers must not keep the window shut.
    ///
    /// The window is sized from the outstanding entries, so entries that are
    /// never cleared close it for good: every later operation times out in
    /// the mount without having been sent.
    #[tokio::test]
    async fn requests_the_peer_never_answers_stop_holding_the_window() {
        let (mut engine, ops) = engine();
        for _ in 0..10 {
            ops.send(a_request()).expect("queued");
        }
        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");
        assert_eq!(tx.sent.len(), MIN_IN_FLIGHT, "the window is full");

        let later = Instant::now() + PENDING_EXPIRY + Duration::from_millis(1);
        assert_eq!(engine.pending.expire(later, PENDING_EXPIRY), MIN_IN_FLIGHT);
        assert!(engine.pending.is_empty());

        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");
        assert_eq!(
            tx.sent.len(),
            MIN_IN_FLIGHT * 2,
            "with the dead entries gone the next requests go out"
        );
    }

    /// An expired request is failed back to whoever asked, not dropped.
    #[test]
    fn an_expired_request_is_failed_back_to_its_caller() {
        let mut pending = PendingMap::default();
        let (reply, answer) = std::sync::mpsc::channel();
        pending.insert(1, Pending::Stat(reply));

        assert_eq!(pending.expire(Instant::now(), PENDING_EXPIRY), 0, "not yet");
        let later = Instant::now() + PENDING_EXPIRY;
        assert_eq!(pending.expire(later, PENDING_EXPIRY), 1);
        assert!(matches!(answer.try_recv(), Ok(Err(FileError::Io(_)))));
    }

    /// A close must not reach the peer before the writes it follows.
    ///
    /// This is the bug behind "Disk Write Error": a write goes to a worker to
    /// be compressed, and the close that came after it used to be written
    /// straight to the socket from the pump. The peer dropped the handle first
    /// and answered the write that arrived behind it with `BadHandle`, so the
    /// last bytes of a file were silently lost and the application failed at
    /// the very end of a copy that had otherwise worked.
    #[tokio::test]
    async fn a_close_waits_for_the_writes_it_follows() {
        let (mut engine, ops) = engine();
        // The pool is held busy until the close has been seen, so the write
        // cannot finish compressing first and make the deferral moot — which
        // is a timing the test would otherwise lose now and then.
        let (release, gate) = std::sync::mpsc::channel::<()>();
        engine.codec = Workers::with_threads("test-files-gated", 1);
        engine.codec.spawn(move || {
            let _ = gate.recv();
        });
        let (reply, _rx) = std::sync::mpsc::channel();
        ops.send(Op::Write {
            handle: 7,
            offset: 0,
            bytes: b"payload that takes a moment to compress ".repeat(64),
            reply,
        })
        .expect("queued");
        ops.send(Op::Close { handle: 7 }).expect("queued");

        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");

        // Neither has gone out: the write is still compressing, and the close
        // is being held back precisely because of it.
        assert!(
            tx.sent.is_empty(),
            "nothing may go straight to the socket here"
        );
        assert_eq!(
            engine.deferred_closes,
            vec![7],
            "the close must be waiting on the write"
        );

        // Once the worker has queued the write, the close follows it — in that
        // order, which is the whole point.
        release.send(()).expect("the gate job is waiting");
        let write = tokio::time::timeout(Duration::from_secs(10), engine.outbox_rx.recv())
            .await
            .expect("the worker answers")
            .expect("a prepared message");
        assert!(matches!(write, FilesMsg::Write { handle: 7, .. }));

        // The worker clears its count just after queueing the write, so the
        // close may need another pump's look — exactly as it would live.
        let close = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                engine.flush_deferred_closes();
                if let Ok(msg) = engine.outbox_rx.try_recv() {
                    return msg;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the close follows");
        assert!(
            matches!(close, FilesMsg::Close { handle: 7 }),
            "expected the close, got {close:?}"
        );
        assert!(engine.deferred_closes.is_empty());
    }

    /// A close on a handle with nothing in flight goes out without waiting.
    ///
    /// The deferral exists for writes still being compressed; making every
    /// close wait a pump would slow down closing the files — most of them —
    /// that were only ever read.
    #[tokio::test]
    async fn a_close_with_no_writes_behind_it_is_not_delayed() {
        let (mut engine, ops) = engine();
        ops.send(Op::Close { handle: 4 }).expect("queued");

        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");

        assert!(engine.deferred_closes.is_empty(), "nothing to wait for");
        let close = engine.outbox_rx.recv().await.expect("queued at once");
        assert!(matches!(close, FilesMsg::Close { handle: 4 }));
    }

    /// An answer that never comes must not hold its entry forever.
    ///
    /// The mount gives up after `BULK_OP_TIMEOUT` at the latest, so a request
    /// older than that will never be timed — and keeping it would leak an entry
    /// per lost request for the life of the session.
    #[test]
    fn a_request_that_is_never_answered_is_eventually_forgotten() {
        let mut health = Health::default();
        let t0 = Instant::now();
        health.sent(1, "stat", t0);

        health.roll(t0 + cursio_files::bridge::BULK_OP_TIMEOUT + Duration::from_millis(1));
        assert!(
            health.sent_at.is_empty(),
            "a request past the mount's own deadline is never going to be timed"
        );
    }
}
