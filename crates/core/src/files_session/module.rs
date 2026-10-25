//! The engine as a session module: its pump, sliced so a transfer never
//! holds the loop, and what the peer sends it.

use crate::files_session::client::PENDING_EXPIRY;
use crate::files_session::wire::drain;
use crate::files_session::wire::is_request;
use crate::files_session::wire::send;
use crate::files_session::FilesEngine;
use crate::module::SessionModule;
use cursio_protocol::{FileError, FilesMsg, Message, ModuleId};
use cursio_transport::{TransportError, TransportTx};
use std::time::{Duration, Instant};

/// How long one pump may spend moving file traffic before handing the session
/// loop back.
///
/// A budget in time rather than in messages, because messages are not the cost:
/// a pass of thirty `Ok` replies is free, while a pass of thirty 256 KiB chunks
/// is thirty memory copies. A fixed count has to be tuned for the expensive
/// case, which then throttles the cheap one — the count that kept large reads
/// smooth left the mount's threads parked between passes on a directory walk.
///
/// Sized against the loop's own 5 ms pace: file traffic may take a share of
/// each turn and no more, so input and audio keep their pumps whatever the
/// transfer is doing.
pub(super) const PUMP_SLICE: Duration = Duration::from_millis(2);

/// How the pump's slice is divided between replies, our data and our requests.
///
/// Replies take two shares of four because a peer is blocked on them and they
/// are usually small; data and requests take one each. What matters is that
/// none of the three can take the whole slice — see the division in `pump`.
pub(super) const SLICE_SHARES: u32 = 4;

/// Shares of [`SLICE_SHARES`] reserved for replies to the peer.
pub(super) const REPLY_SHARES: u32 = 2;

/// Absolute cap on messages moved in one pump.
///
/// The time slice is the real budget; this only bounds the damage if the clock
/// misbehaves (a suspended laptop, a coarse timer) so a pass cannot run away
/// with the loop.
pub(super) const MAX_MESSAGES_PER_PUMP: usize = 512;

#[async_trait::async_trait]
impl SessionModule for FilesEngine {
    fn name(&self) -> &'static str {
        "files"
    }

    fn id(&self) -> ModuleId {
        ModuleId::FILES
    }

    /// Busy whenever anything is owed an answer, in either direction.
    ///
    /// A mount serving a download barely sends: it receives requests and
    /// answers them, so the loop's send-based idle test judged it quiet and
    /// slept 50 ms between answers. Both directions count here — requests this
    /// machine is waiting on (`pending`), replies the workers have prepared
    /// (`done_rx`), and writes still being compressed (`outbox_rx`).
    fn has_pending_work(&self) -> bool {
        !self.pending.is_empty()
            || !self.done_rx.is_empty()
            || !self.outbox_rx.is_empty()
            || self.in_progress.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let volumes = self.local_volumes.clone();
        tracing::info!(
            peer = %self.peer_name,
            volumes = volumes.len(),
            // The peer's own volumes are mounted when it announces them, so
            // there is nothing to report here yet — see `sync_peer_mounts`.
            "file sharing up"
        );
        #[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
        tracing::warn!(
            peer = %self.peer_name,
            "no mount backend on this platform, so the peer's volumes will not appear here; \
             sharing ours still works"
        );
        send(tx, FilesMsg::Volumes(volumes)).await?;
        self.announce_volumes = false;
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.rescan_volumes();
        // A walker that stopped makes no further call into the mount, so its
        // end is noticed here rather than there.
        cursio_files::walk::sweep();

        if self.announce_volumes {
            send(tx, FilesMsg::Volumes(self.local_volumes.clone())).await?;
            self.announce_volumes = false;
        }

        // Read the link before deciding how much to put on it. One reading per
        // pump — every 5 ms — is frequent enough to catch a link going from
        // idle to congested within a couple of chunks, and cheap: two integers
        // from behind the transport's lock.
        let now = Instant::now();
        // The data class, which is where a transfer's bytes queue. Metadata
        // rides a class above and is small enough that its depth says nothing
        // about whether the link is keeping up.
        self.pace.observe(tx.queue_depth(FilesMsg::PRIORITY), now);
        self.log_health(now);
        let deadline = now + PUMP_SLICE;

        // Retry any volume that could not be mounted. Driven from here rather
        // than only from the peer's announcement, because a peer whose volume
        // set has not changed never sends another one — so a mount that failed
        // once would otherwise stay failed for the life of the session.
        // `mount_retry_at` keeps this to one attempt every few seconds.
        self.sync_peer_mounts();

        // Before the window is sized from the entries: see [`PENDING_EXPIRY`].
        let expired = self.pending.expire(now, PENDING_EXPIRY);
        if expired > 0 {
            self.health.failed += expired as u64;
            tracing::warn!(
                peer = %self.peer_name,
                expired,
                "requests the peer never answered were given up on"
            );
        }

        // Before draining, so a close whose writes finished compressing since
        // the last pump goes out behind them in this same pass.
        self.flush_deferred_closes();

        // Replies first: the peer is blocked on them, and a request of ours can
        // wait a pass. First is not *all*, though — each of the three gets its
        // own share of the slice rather than the whole of what the one before
        // it left. Handing all three the same deadline reads as a priority
        // order and behaves as starvation: serving a peer's download fills
        // `done_rx` with hundreds of answers per chunk we send, and they spent
        // the 2 ms every time. Measured against Steam writing to our disk, our
        // own data reached the link in 6% of pumps — 12.6 writes a second where
        // the loop's 5 ms pace allows 200, which is below even the idle pace's
        // ceiling.
        //
        // Replies still get the largest share, and an empty queue costs its
        // holder nothing: `drain` returns on the first `try_recv` miss, so a
        // quiet direction hands its share straight to the next.
        let share = PUMP_SLICE / SLICE_SHARES;
        // Replies are not paced: the peer is blocked on them, they are small,
        // and holding one back to smooth our own sending would add latency to
        // the very machine waiting on us.
        drain(
            &mut self.done_rx,
            tx,
            now + share * REPLY_SHARES,
            None,
            None,
        )
        .await?;
        let budget = self.pace.pump_budget();
        drain(
            &mut self.outbox_rx,
            tx,
            deadline.min(now + share * REPLY_SHARES + share),
            Some(&mut self.health),
            budget,
        )
        .await?;
        self.pump_requests(tx, deadline).await?;
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<FilesMsg>(ModuleId::FILES) {
            Ok(Some(msg)) => {
                crate::update::quiet::note_file_activity();
                if is_request(&msg) {
                    // Disk work: off to a blocking thread, answered later.
                    self.serve(msg);
                } else {
                    self.on_reply(msg);
                }
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable files message from peer");
                true
            }
        }
    }

    async fn shutdown(&mut self, _tx: &mut dyn TransportTx) {
        // Fail everything still in flight before the mount goes away, or its
        // FUSE threads sit out the full timeout for answers that will never
        // come — and the unmount blocks behind them.
        for pending in self.pending.drain() {
            pending.fail(FileError::Io("session ended".into()));
        }
        // Release the mount's own threads before taking its mounts away.
        //
        // Failing the pending map covers requests this engine has already sent;
        // it does nothing for a dispatcher thread still waiting on a reply
        // channel, and dropping the session's receiver would not either — the
        // sender lives here, in `mount_link`, so those threads wait out their
        // full deadline rather than failing at once. Unmount waits for exactly
        // those threads, so the process hung in `unmount` with this function's
        // own log line as its last.
        self.mount_link.shut_down();
        // Dropping the handles unmounts.
        #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
        if !self.volume_mounts.is_empty() {
            tracing::info!(
                peer = %self.peer_name,
                volumes = self.volume_mounts.len(),
                "unmounting the peer's volumes"
            );
            self.volume_mounts.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files_session::client::Pending;
    use crate::files_session::serve::InFlight;
    use crate::files_session::test_support::a_request;
    use crate::files_session::test_support::engine;
    use crate::files_session::test_support::RecordingTx;
    use cursio_files::bridge::Op;
    use cursio_files::Workers;
    use cursio_protocol::Chunk;
    use cursio_protocol::Volume;

    /// Our own writes must not queue behind the work the peer asks of us.
    ///
    /// Sharing one pool put every chunk of a local download behind a peer's
    /// directory walk, and Steam's download sat at 0 B/s while it was served.
    #[tokio::test]
    async fn our_writes_are_prepared_while_the_peers_work_occupies_its_pool() {
        let (mut engine, ops) = engine();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        engine.workers = Workers::with_threads("test-files-busy", 1);
        engine.workers.spawn(move || {
            let _ = gate.recv();
        });

        let (reply, _rx) = std::sync::mpsc::channel();
        ops.send(Op::Write {
            handle: 7,
            offset: 0,
            bytes: vec![1u8; 4096],
            reply,
        })
        .expect("queued");
        let mut tx = RecordingTx::default();
        engine
            .pump_requests(&mut tx, Instant::now() + Duration::from_secs(30))
            .await
            .expect("pumps");

        let prepared = tokio::time::timeout(Duration::from_secs(10), engine.outbox_rx.recv())
            .await
            .expect("the write is compressed while the serving pool is stuck");
        assert!(matches!(prepared, Some(FilesMsg::Write { handle: 7, .. })));
        drop(release);
    }

    /// The pump must stop when its slice is up, whatever is left waiting.
    ///
    /// Input, audio and the shared cursor are pumped from the same loop, so a
    /// pass that ran until the queue emptied would hand a large copy the whole
    /// session — the stall this module logs as "pump blocked the session loop".
    #[tokio::test]
    async fn a_pump_stops_when_its_slice_is_spent() {
        let (mut engine, ops) = engine();
        for _ in 0..10 {
            ops.send(a_request()).expect("queued");
        }

        let mut tx = RecordingTx::default();
        // A deadline already past: the pass must decline to start rather than
        // run "just one more".
        engine
            .pump_requests(&mut tx, Instant::now())
            .await
            .expect("pumps");
        assert!(tx.sent.is_empty(), "sent {:?}", tx.sent);
    }

    /// Replies the peer is waiting on go out even while our own requests are
    /// being held back, and a write is not compressed on the session loop.
    #[tokio::test]
    async fn answers_are_never_held_back_and_writes_are_prepared_elsewhere() {
        let (mut engine, ops) = engine();
        let (reply, _rx) = std::sync::mpsc::channel();
        ops.send(Op::Write {
            handle: 7,
            offset: 0,
            bytes: b"the same words over and over ".repeat(64),
            reply,
        })
        .expect("queued");

        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");

        // The write left the loop without being sent: it is on a worker being
        // compressed, and is already counted as outstanding.
        assert!(tx.sent.is_empty(), "compression must not happen inline");
        assert_eq!(engine.pending.len(), 1);

        // It arrives on the outbox and goes out on a later pump, compressed.
        let prepared = tokio::time::timeout(Duration::from_secs(10), engine.outbox_rx.recv())
            .await
            .expect("the worker answers")
            .expect("a prepared message");
        let FilesMsg::Write { bytes, .. } = &prepared else {
            panic!("expected a write, got {prepared:?}");
        };
        assert!(
            matches!(bytes, Chunk::Zstd { .. }),
            "repetitive data must have been compressed by the worker"
        );
    }

    /// A worker stuck on the disk while holding the server's lock must not
    /// hold up the session loop.
    ///
    /// The loop carries the cursor. Under heavy I/O pressure a peer's read
    /// took 2 s with the lock held, and the pump that collected a finished
    /// volume scan waited for that lock every time — 0.3–1.9 s frozen, every
    /// two seconds. The lock is held here for far longer than the pump may
    /// take, with a scan result waiting to be announced.
    #[tokio::test]
    async fn a_pump_does_not_wait_on_a_server_lock_held_across_the_disk() {
        let (mut engine, _ops) = engine();
        let stick = Volume {
            id: "E:".into(),
            label: "Stick".into(),
            writable: true,
            total_bytes: 1 << 30,
            free_bytes: 1 << 29,
            removable: true,
        };
        engine.rescanning = true;
        engine
            .rescan_tx
            .send(Some(vec![stick.clone()]))
            .expect("delivers");

        let server = std::sync::Arc::clone(&engine.server);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let slow_disk = std::thread::spawn(move || {
            let _held = server.lock().expect("server");
            locked_tx.send(()).expect("reports");
            std::thread::sleep(Duration::from_secs(2));
        });
        locked_rx.recv().expect("the lock is taken");

        let mut tx = RecordingTx::default();
        let started = Instant::now();
        engine.pump(&mut tx).await.expect("pumps");
        let took = started.elapsed();
        slow_disk.join().expect("joins");

        assert!(
            took < Duration::from_millis(500),
            "the pump waited {took:?} on a lock held by a slow worker"
        );
        assert!(
            tx.sent
                .iter()
                .any(|m| matches!(m, FilesMsg::Volumes(v) if v == &vec![stick.clone()])),
            "the new volume set is still announced"
        );
        assert!(!engine.rescanning, "the finished scan was collected");
    }

    /// A module waiting on an answer must not be counted as idle.
    ///
    /// The session loop follows *sent* bytes, and a mount serving a download
    /// sends almost nothing: it receives requests and answers them. Judged by
    /// that alone the loop dropped to its 50 ms idle tick between a request and
    /// its reply — 391 times during one transfer, which is where 111 ms per
    /// write on a 10 ms link came from.
    #[tokio::test]
    async fn a_module_awaiting_an_answer_is_not_idle() {
        let (mut engine, ops) = engine();
        assert!(
            !engine.has_pending_work(),
            "a module with nothing outstanding is idle"
        );

        ops.send(a_request()).expect("queued");
        let mut tx = RecordingTx::default();
        engine
            .pump_requests(&mut tx, Instant::now() + Duration::from_secs(5))
            .await
            .expect("pumps");

        assert!(
            engine.has_pending_work(),
            "a request sent and unanswered is work outstanding"
        );
    }

    /// A request from the peer counts while a worker is still running it.
    ///
    /// This is the half the send-based idle test could never see, and the half
    /// a download actually consists of: between accepting the peer's request
    /// and its answer reaching `done_rx`, the module holds nothing — `pending`
    /// tracks what *we* asked for. So a mount serving a download looked idle
    /// for exactly as long as it was busy.
    #[test]
    fn a_request_being_served_for_the_peer_is_work_outstanding() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let raised = InFlight::raise(&count);
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a request handed to a worker is in flight"
        );
        drop(raised);
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a finished request is not"
        );
    }

    /// Replies to the peer must not spend the whole pump on their own.
    ///
    /// The three drains once shared a single deadline, which reads as a
    /// priority order and behaves as starvation: serving a download fills
    /// `done_rx` with hundreds of answers per chunk of ours, and they took the
    /// entire 2 ms every pass. Live against Steam that left our own data
    /// reaching the link in 6% of pumps — 12.6 writes a second where the loop's
    /// pace allows 200.
    #[test]
    fn replies_cannot_spend_the_whole_pump_slice() {
        let share = PUMP_SLICE / SLICE_SHARES;
        let replies = share * REPLY_SHARES;
        assert!(
            replies < PUMP_SLICE,
            "replies take a share of the slice, not all of it"
        );
        assert!(
            PUMP_SLICE - replies >= share,
            "at least one share is left for our own data and requests"
        );
    }

    /// Every in-flight request must be failed when the session ends, or the
    /// mount's threads wait out the full timeout for answers that cannot come.
    #[test]
    fn shutdown_releases_everyone_waiting() {
        let (tx, rx) = std::sync::mpsc::channel();
        let pending = Pending::Stat(tx);
        pending.fail(FileError::Io("session ended".into()));
        assert!(
            matches!(rx.recv(), Ok(Err(FileError::Io(_)))),
            "a waiting caller must be told, not left hanging"
        );
    }
}
