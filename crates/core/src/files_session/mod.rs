//! The files [`SessionModule`]: the peer's disks here, ours there.
//!
//! Two halves sharing one message type:
//!
//! - **Server half** — answers the peer's requests against our own volumes.
//!   The work is real disk I/O, so it never runs inline: see [`FilesEngine::serve`].
//! - **Client half** — carries requests from the FUSE mount ([`cursio_files::mount`])
//!   to the peer and routes replies back to whichever callback is waiting.
//!
//! ## Nothing blocks the session loop
//!
//! A `pump` that takes longer than ~50 ms is logged as having blocked the
//! session, and the shared cursor rides the same channel — so a file read must
//! never happen inline. Disk work, and the compression that goes with it, is
//! handed to low-priority worker pools ([`cursio_files::Workers`]) — one
//! serving the peer, one for our own compression, so neither side's traffic
//! queues behind the other's — and collected on a later pump. Each pass is bounded by a time slice rather than
//! a message count, so a pump stays short whatever the size of the transfer.
//!
//! ## Nothing else slows down either
//!
//! A transfer is the heaviest thing cursio does, and it happens while the user
//! is still working on both machines. Three separate mechanisms keep it from
//! being felt elsewhere, because they answer three different questions:
//!
//! - *Who gets the link?* — a file's bytes ride `Bulk`, the lowest class the
//!   transport schedules, so a mouse event queued mid-transfer is written
//!   before the transfer's next frame; questions about paths ride `Metadata`,
//!   one class up, so an `ls` does not wait out a copy (see
//!   [`FilesMsg::priority`]).
//! - *Who gets the CPU and the disk?* — the worker pool runs below normal
//!   thread priority and is bounded well under the core count, so foreground
//!   applications are scheduled first.
//! - *How much may be in flight at once?* — [`Pacer`] sizes the window of
//!   outstanding requests from the delay it measures on the link, so file
//!   traffic keeps the pipe full without building a queue in front of
//!   everything sharing it.

use std::collections::HashMap;
// Every lock of the file server recovers from poisoning rather than
// propagating it. A panic in one request — the one kind the worker pool now
// survives — would otherwise poison the server for every request after it,
// and the peer's mount would stop answering for the rest of the session. The
// server's state is a table of open handles; the entry a panicking request
// was touching is the worst it can leave behind.
use std::time::Instant;

use cursio_files::bridge::{Op, PeerLink};
use cursio_files::{FileServer, Workers};
use cursio_protocol::{FileHandle, FileReqId, FilesMsg, ModuleId, Volume};

use crate::files_session::client::PendingMap;
use crate::files_session::health::report_walk;
use crate::files_session::health::Health;
use crate::files_session::mounts::excluded_paths;
#[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
use crate::files_session::mounts::Mount;
use crate::files_session::pacer::Pacer;
use crate::files_session::serve::CODEC_THREADS;
use crate::module::SessionModule;

mod client;
mod health;
mod module;
mod mounts;
mod pacer;
mod serve;
#[cfg(test)]
mod test_support;
mod wire;

/// Serves our volumes to the peer and mounts theirs here.
pub struct FilesEngine {
    peer_name: String,
    /// Our disks, as the peer sees them. Behind a mutex because the blocking
    /// tasks that touch the disk need it across await points.
    ///
    /// Never locked on the session loop. The workers take it for every request
    /// the peer makes, and a worker that ends up holding it across the disk —
    /// which a request did until recently, see [`run_request`](serve::run_request) — turns a slow
    /// disk into a frozen cursor. Seen live under heavy I/O pressure: the
    /// loop's once-a-second rescan hand-over waited out 0.3–1.9 s reads every
    /// two seconds. What the loop needs from the server is kept in
    /// [`FilesEngine::local_volumes`] instead.
    server: std::sync::Arc<std::sync::Mutex<FileServer>>,
    /// The volume set we last announced, owned by the loop so announcing it
    /// needs no lock. Replaced by a rescan worker's result.
    local_volumes: Vec<Volume>,
    /// Requests from the local FUSE mount, waiting to go out.
    ops_rx: tokio::sync::mpsc::UnboundedReceiver<Op>,
    /// Replies from disk work finished on a worker thread.
    done_tx: tokio::sync::mpsc::UnboundedSender<FilesMsg>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<FilesMsg>,
    /// Our own requests, once the workers have prepared them.
    ///
    /// A write has to be compressed before it can be sent, and compression of a
    /// 256 KiB chunk is milliseconds of CPU — done inline it was the session
    /// loop's own stall, one per chunk, for the whole length of a copy.
    outbox_tx: tokio::sync::mpsc::UnboundedSender<FilesMsg>,
    outbox_rx: tokio::sync::mpsc::UnboundedReceiver<FilesMsg>,
    /// Writes handed to a worker to compress and not yet queued on `outbox`,
    /// counted per handle.
    ///
    /// A close must not overtake them. Compression runs on the pool, so between
    /// accepting a `Write` and its message reaching `outbox` there is a window
    /// in which the handle has bytes outstanding that nothing on the link knows
    /// about yet. Closing inside that window let the peer drop the handle first
    /// and answer the writes behind it with `BadHandle`: the bytes were lost,
    /// and the application learned of it only at the end of a file it had
    /// finished writing. The count is what a deferred close waits on.
    ///
    /// Shared with the workers, which decrement their own entry immediately
    /// after queueing the compressed write — so the count falls in the same
    /// order the messages enter `outbox`.
    compressing: std::sync::Arc<std::sync::Mutex<HashMap<FileHandle, usize>>>,
    /// Closes held back until this handle's writes have been queued.
    ///
    /// Retried each pump by [`FilesEngine::flush_deferred_closes`].
    deferred_closes: Vec<FileHandle>,
    /// Threads that do the disk work the peer asks of us, kept below the
    /// priority of everything the user is actually looking at.
    workers: Workers,
    /// Threads that compress our own writes and unpack our own reads.
    ///
    /// Apart from `workers` because the two answer to different people. That
    /// pool serves the peer, and a peer walking a directory tree keeps it busy
    /// with hundreds of `readdir` and `stat` a second — each of which, on a
    /// Windows disk, can take milliseconds apiece. Sharing one queue put every
    /// chunk of a local download behind that walk: seen live as Steam writing
    /// to the peer's disk at 0 B/s while the peer's crawl was being served.
    /// Compression is pure CPU and needs no disk, so a small pool of its own
    /// keeps our transfer moving whatever the peer is asking.
    codec: Workers,
    /// Sizes the in-flight window from what the link is measured to be doing.
    pace: Pacer,
    /// Requests we have sent and not yet had answered.
    pending: PendingMap,
    next_req: FileReqId,
    /// Requests the peer asked of us, counted by kind.
    ///
    /// The other counters watch what this machine asks for. When the peer is
    /// the one doing the work — an installer downloading onto our disk — every
    /// request is inbound, and without this the log for a failed transfer holds
    /// nothing but the refusals.
    served: HashMap<&'static str, u64>,
    /// How many of those took long enough that their caller may have given up.
    ///
    /// Incremented from the worker that ran the request, hence the atomic.
    slow_served: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Requests from the peer currently on a worker thread.
    ///
    /// The gap this closes: between accepting a request and its answer reaching
    /// `done_rx`, the module holds nothing at all — `pending` is what *we*
    /// asked for, and a peer's request is neither sent by us nor yet answered.
    /// So a mount serving a download looked idle for exactly as long as it was
    /// busy, and the session loop slept 50 ms in the middle of every one.
    in_progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// What the link is actually doing, reported periodically.
    ///
    /// Kept because none of it can be inferred from outside: a transfer running
    /// at a fraction of the link looks exactly like a slow network, and the
    /// difference is only visible by watching the round trip and the pacer's
    /// own decisions side by side. Diagnosing that by hand meant measuring the
    /// link with `ssh` and guessing — which produced the wrong answer twice
    /// before the log line above gave it away.
    health: Health,
    /// One live mount per peer volume, keyed by volume id; unmounted when
    /// dropped.
    ///
    /// Per volume rather than per peer because a volume cannot be added to a
    /// mount already made — on Windows the drive letter *is* the volume, and on
    /// Linux a single mount could report only one capacity for all of them. So
    /// a peer plugging in a stick means a new mount, and unplugging it means
    /// dropping that one and no other.
    #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
    volume_mounts: HashMap<String, Mount>,
    /// Kept so a volume that appears mid-session can be mounted then, rather
    /// than only at startup.
    mount_link: PeerLink,
    /// When a volume that failed to mount may be tried again.
    ///
    /// A mount can fail for a reason that clears on its own: the commonest is a
    /// previous run's mount still occupying the path, which the user (or the
    /// next sweep) removes a moment later. Without a retry the volume stayed
    /// missing for the whole session, because the only thing that drove
    /// mounting was the peer's announcement — and a peer whose volume set has
    /// not changed never sends another. Seen live: every one of a peer's six
    /// drives failed against a stale mount point and none came back.
    #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
    mount_retry_at: Option<Instant>,
    /// Deferred until the first pump, because `new` is not async.
    announce_volumes: bool,
    /// The peer's volumes as last announced, answered from here instead of over
    /// the link. See where it is filled for why this matters.
    peer_volumes: Vec<Volume>,
    /// When the local volume set was last re-read, so a stick plugged in after
    /// the session started is noticed. See [`VOLUME_RESCAN`](mounts::VOLUME_RESCAN).
    last_rescan: Instant,
    /// Set while a rescan is running on a worker, so the once-a-second timer
    /// cannot stack a second one behind a scan that is still stuck.
    rescanning: bool,
    /// Finished scans, collected by a later pump: the new volume set when it
    /// changed, `None` when it did not.
    rescan_tx: tokio::sync::mpsc::UnboundedSender<Option<Vec<Volume>>>,
    rescan_rx: tokio::sync::mpsc::UnboundedReceiver<Option<Vec<Volume>>>,
    /// What this machine offers. Applied to every rescan too, so a stick
    /// plugged in later is offered only if the policy names it.
    share: cursio_files::SharePolicy,
}

impl FilesEngine {
    pub fn new(peer_name: String, share: cursio_files::SharePolicy) -> anyhow::Result<Self> {
        let (ops_tx, ops_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outbox_tx, outbox_rx) = tokio::sync::mpsc::unbounded_channel();
        let (rescan_tx, rescan_rx) = tokio::sync::mpsc::unbounded_channel();

        // Nothing is mounted here: a mount is per volume, and the peer has not
        // said what it has yet. `sync_peer_mounts` makes them when its `Volumes`
        // announcement arrives, and follows the set from then on.
        let link = PeerLink::new(ops_tx);

        // Clear what a previous run left mounted here before mounting anything.
        //
        // Each volume's directory is named after the disk, so a peer relabelling
        // a drive — or a Cursio that named them differently — leaves a mount
        // whose name nothing now looks up. Those stay in a file manager's
        // sidebar as entries that answer nothing, and multiply with every
        // restart.
        #[cfg(target_os = "linux")]
        cursio_files::mount::release_stale_peer_mounts(&mounts::mount_root(&peer_name));

        let mut server = FileServer::detect_shared(&share);
        server.exclude(excluded_paths(&peer_name));
        let local_volumes = server.volumes();
        cursio_files::walk::set_reporter(report_walk);

        Ok(Self {
            peer_name,
            server: std::sync::Arc::new(std::sync::Mutex::new(server)),
            local_volumes,
            ops_rx,
            done_tx,
            done_rx,
            outbox_tx,
            outbox_rx,
            compressing: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            deferred_closes: Vec::new(),
            workers: Workers::new("cursio-files"),
            codec: Workers::with_threads("cursio-files-codec", CODEC_THREADS),
            pace: Pacer::new(),
            pending: PendingMap::default(),
            next_req: 1,
            health: Health::default(),
            served: HashMap::new(),
            slow_served: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            in_progress: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
            volume_mounts: HashMap::new(),
            mount_link: link,
            #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
            mount_retry_at: None,
            announce_volumes: true,
            peer_volumes: Vec::new(),
            last_rescan: Instant::now(),
            rescanning: false,
            rescan_tx,
            rescan_rx,
            share,
        })
    }
}

impl FilesEngine {}

impl FilesEngine {}

/// Builds [`FilesEngine`] when file sharing is enabled.
///
/// Linux-only for now: sharing our own volumes works anywhere, but a peer that
/// could not mount ours would advertise a capability it cannot honour, and the
/// mount half needs WinFsp on Windows.
pub struct FilesFactory {
    pub enabled: bool,
    /// What this machine offers its peers. [`SharePolicy::everything`] is what
    /// sharing meant before there was a policy.
    ///
    /// Shared so a changed policy reaches the next engine built; the node
    /// restarts the module on every link to make that now.
    ///
    /// [`SharePolicy::everything`]: cursio_files::SharePolicy::everything
    pub share: crate::live::Shared<cursio_files::SharePolicy>,
    /// `Both` on a computer, which serves its disks and mounts a peer's.
    /// `Source` on a phone, which only serves its storage: it has nowhere to
    /// present a mount, and two phones have nothing to exchange, so the module
    /// does not start between them.
    pub role: cursio_protocol::ModuleRole,
}

impl crate::module::ModuleFactory for FilesFactory {
    fn id(&self) -> ModuleId {
        ModuleId::FILES
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        self.enabled
            .then(|| cursio_protocol::ModuleAdvert::new(ModuleId::FILES, VERSION, self.role))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(FilesEngine::new(
            ctx.peer_name().to_string(),
            self.share.get(),
        )?))
    }
}

/// Version of this module's wire vocabulary ([`FilesMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    fn files_advert(role: cursio_protocol::ModuleRole) -> cursio_protocol::ModuleAdvert {
        use crate::module::ModuleFactory;
        FilesFactory {
            enabled: true,
            share: crate::live::Shared::new(cursio_files::SharePolicy::everything()),
            role,
        }
        .advert()
        .expect("an enabled factory advertises")
    }

    /// A phone's storage is mounted by a computer; two phones serve and
    /// neither can mount, so the module does not start between them.
    #[test]
    fn a_phone_shares_its_storage_with_a_computer_but_not_with_another_phone() {
        use cursio_protocol::ModuleRole;
        let phone = [files_advert(ModuleRole::Source)];
        let computer = [files_advert(ModuleRole::Both)];
        assert!(cursio_protocol::negotiate(&phone, &computer).is_agreed(ModuleId::FILES));
        assert!(cursio_protocol::negotiate(&computer, &computer).is_agreed(ModuleId::FILES));
        assert!(!cursio_protocol::negotiate(&phone, &phone).is_agreed(ModuleId::FILES));
    }
}
