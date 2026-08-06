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
//! handed to a low-priority worker pool ([`cursio_files::Workers`]) and
//! collected on a later pump. Each pass is bounded by a time slice rather than
//! a message count, so a pump stays short whatever the size of the transfer.
//!
//! ## Nothing else slows down either
//!
//! A transfer is the heaviest thing cursio does, and it happens while the user
//! is still working on both machines. Three separate mechanisms keep it from
//! being felt elsewhere, because they answer three different questions:
//!
//! - *Who gets the link?* — [`FilesMsg::PRIORITY`] is `Bulk`, the lowest class
//!   the transport schedules, so a mouse event queued mid-transfer is written
//!   before the transfer's next frame.
//! - *Who gets the CPU and the disk?* — the worker pool runs below normal
//!   thread priority and is bounded well under the core count, so foreground
//!   applications are scheduled first.
//! - *How much may be in flight at once?* — [`Pacer`] sizes the window of
//!   outstanding requests from the delay it measures on the link, so file
//!   traffic keeps the pipe full without building a queue in front of
//!   everything sharing it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cursio_files::bridge::{Op, PeerLink};
use cursio_files::{FileServer, Workers};
use cursio_protocol::{Chunk, FileError, FileReqId, FilesMsg, Message, ModuleId, Volume};
use cursio_transport::{QueueDepth, TransportError, TransportTx};

use crate::module::SessionModule;

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
const PUMP_SLICE: Duration = Duration::from_millis(2);

/// Absolute cap on messages moved in one pump.
///
/// The time slice is the real budget; this only bounds the damage if the clock
/// misbehaves (a suspended laptop, a coarse timer) so a pass cannot run away
/// with the loop.
const MAX_MESSAGES_PER_PUMP: usize = 512;

/// How long file traffic may sit in the link's queue before the pacer eases
/// off.
///
/// Not zero, and not large. Some queue is what keeps the link busy: with none,
/// every round trip is a stall, and a transfer runs at one chunk per RTT. Too
/// much and the queue itself becomes the problem — bytes committed to the wire
/// minutes before they are wanted, a `Bye` that cannot get out, and a Wi-Fi
/// radio held busy on behalf of a background copy while the user's own traffic
/// waits behind it in the same buffers.
///
/// A tenth of a second is comfortably more than any LAN round trip (6 ms
/// measured here), so the pipe stays full, and short enough that the queue
/// never becomes the dominant delay.
const TARGET_QUEUE_DELAY: Duration = Duration::from_millis(100);

/// Never fewer than this many requests in flight.
///
/// The floor exists so that easing off cannot turn into a stall: the mount's
/// callers are blocked in a filesystem call with a deadline
/// ([`cursio_files::bridge::OP_TIMEOUT`]), so a window that shrank toward one
/// would turn congestion into `EIO`. Enough to cover a directory walk's
/// lookup-per-entry without the window having to grow first.
const MIN_IN_FLIGHT: usize = 8;

/// Never more than this many, however fast the link looks.
///
/// The window is the pipeline depth, and every request in it holds a buffer on
/// both machines — at 256 KiB a chunk this is the memory a transfer can commit
/// to. Well above the depth a LAN needs (a 1 Gbit/s link with a 6 ms round trip
/// is filled by roughly three 256 KiB chunks in flight), so the cap only ever
/// bites on a link fast enough not to need the help.
const MAX_IN_FLIGHT: usize = 128;

/// Shortest interval between two rate samples.
///
/// Below this the measurement is mostly quantisation: a pump every 5 ms sees a
/// handful of frames, and dividing by a near-zero interval turns one late frame
/// into a wildly wrong rate.
const RATE_SAMPLE_INTERVAL: Duration = Duration::from_millis(20);

/// Sizes the window of outstanding requests from what the link is doing.
///
/// The mount can always ask for more than the link can carry: a copy issues
/// requests as fast as its threads are answered, and every one of them is a
/// blind write into a queue that never pushes back. Left alone, the queue
/// grows to whatever the mount's concurrency allows and every byte in it is a
/// byte of delay — for this transfer, for anything else sharing the link, and
/// for anything else sharing the network.
///
/// So the window is *measured* rather than configured. Two numbers come back
/// from the transport — bytes waiting, bytes gone — and their ratio is the
/// delay that queue represents. Above [`TARGET_QUEUE_DELAY`] the window halves;
/// comfortably below it, the window grows by one. That is TCP's own
/// additive-increase/multiplicative-decrease rule, for the same reason it uses
/// it: growth has to be cautious and retreat immediate, or the controller
/// oscillates around the very congestion it is trying to avoid.
///
/// The effect is that the window finds the link. On a gigabit LAN it settles
/// near the ceiling; on a saturated Wi-Fi link it settles low, and the transfer
/// slows down instead of everything else doing so.
struct Pacer {
    /// Requests allowed outstanding at once.
    in_flight_limit: usize,
    /// Measured drain rate of our own traffic class, in bytes per second.
    /// `None` until enough has moved to measure anything.
    rate: Option<f64>,
    /// The previous reading: when it was taken, and the sent-byte count then.
    last_sample: Option<(Instant, u64)>,
}

impl Pacer {
    fn new() -> Self {
        Pacer {
            // Start at the floor and grow. The opposite — start wide, retreat
            // on congestion — fills the link's queue before the first
            // measurement exists to say it should not have.
            in_flight_limit: MIN_IN_FLIGHT,
            rate: None,
            last_sample: None,
        }
    }

    /// Fold in a fresh reading of the link and re-size the window.
    fn observe(&mut self, depth: QueueDepth, now: Instant) {
        self.sample_rate(depth.sent_bytes, now);
        match self.queue_delay(depth.queued_bytes) {
            // Backed up: retreat at once, and keep the floor.
            Some(delay) if delay > TARGET_QUEUE_DELAY => {
                let was = self.in_flight_limit;
                self.in_flight_limit = (self.in_flight_limit / 2).max(MIN_IN_FLIGHT);
                if self.in_flight_limit != was {
                    // Worth saying out loud: this is the module deciding the
                    // link is congested and slowing itself down, which from
                    // outside looks like "the copy got slower for no reason".
                    tracing::debug!(
                        was,
                        now = self.in_flight_limit,
                        queued_kb = depth.queued_bytes / 1024,
                        delay_ms = delay.as_millis() as u64,
                        rate_kb_s = self.rate.unwrap_or(0.0) as u64 / 1024,
                        "link is backing up; easing off file traffic"
                    );
                }
            }
            // Comfortably clear of the target: room for one more. Deliberately
            // not "anything below target" — at the target itself, growing again
            // would just push it back over.
            Some(delay) if delay * 2 <= TARGET_QUEUE_DELAY => {
                self.in_flight_limit = (self.in_flight_limit + 1).min(MAX_IN_FLIGHT);
            }
            // Either between the two, or nothing has drained yet so the queue's
            // age is unknown. Hold: acting on a guess is how a controller ends
            // up amplifying what it cannot see.
            _ => {}
        }
    }

    /// Update the drain-rate estimate, if enough time has passed to mean
    /// anything.
    fn sample_rate(&mut self, sent_bytes: u64, now: Instant) {
        let Some((at, previous)) = self.last_sample else {
            self.last_sample = Some((now, sent_bytes));
            return;
        };
        let elapsed = now.saturating_duration_since(at);
        if elapsed < RATE_SAMPLE_INTERVAL {
            return;
        }
        let moved = sent_bytes.saturating_sub(previous);
        self.last_sample = Some((now, sent_bytes));
        if moved == 0 {
            // An idle link says nothing about how fast it is; keeping the last
            // estimate is what lets a transfer resume at speed rather than
            // re-learning the link from the floor after every pause.
            return;
        }
        let sample = moved as f64 / elapsed.as_secs_f64();
        self.rate = Some(match self.rate {
            // Smoothed heavily: the samples are noisy (a 5 ms pump against a
            // 20 ms window catches whole frames or none), and the window reacts
            // to this figure.
            Some(previous) => previous * 0.75 + sample * 0.25,
            None => sample,
        });
    }

    /// How long the queued bytes will take to leave, or `None` when there is no
    /// rate to judge them by.
    fn queue_delay(&self, queued_bytes: usize) -> Option<Duration> {
        if queued_bytes == 0 {
            return Some(Duration::ZERO);
        }
        match self.rate {
            Some(rate) if rate > 0.0 => Some(Duration::from_secs_f64(queued_bytes as f64 / rate)),
            _ => None,
        }
    }

    /// How many further requests may be issued with `in_flight` outstanding.
    fn allowance(&self, in_flight: usize) -> usize {
        self.in_flight_limit.saturating_sub(in_flight)
    }
}

/// Compression level for file chunks.
///
/// Level 1 measured ~284 MB/s on compressible data here — an order of magnitude
/// above the link — while still reaching ~38% on binaries and ~30% on text. A
/// higher level buys a few percent for a large slice of that headroom, which is
/// the wrong trade when the network is the bottleneck.
const ZSTD_LEVEL: i32 = 1;

/// Where a peer's volumes are mounted on Linux.
///
/// Under the user's runtime directory rather than `/mnt`: no privileges needed,
/// and it disappears on logout instead of leaving a stale mount point behind.
#[cfg(target_os = "linux")]
fn mount_root(peer_name: &str) -> std::path::PathBuf {
    // Under the home directory, not `$XDG_RUNTIME_DIR`.
    //
    // The runtime directory is the tidier choice — it needs no privileges and
    // is cleared at logout — but file managers deliberately hide it, so the
    // peer's disks were mounted, readable, and invisible: not in Nautilus's
    // sidebar, not reachable by browsing, only by typing the path. A share
    // nobody can find is not shared. `~/Cursio/<peer>` shows up like any other
    // folder, and the stale-mount sweep at startup covers the cleanup the
    // runtime directory used to give for free.
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Cursio").join(peer_name)
}

/// A live mount of the peer's volumes, whichever OS is presenting them.
#[cfg(target_os = "linux")]
type Mount = cursio_files::mount::MountHandle;
#[cfg(all(target_os = "windows", target_env = "msvc"))]
type Mount = cursio_files::winmount::MountHandle;

/// Uninhabited where no mount implementation exists, so `Option<Mount>` is
/// always `None` and every path that reads it stays compiled and honest.
#[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
enum Mount {}

/// Mount the peer's volumes with whatever this platform provides.
///
/// The two implementations differ in everything the OS cares about — a
/// directory versus a drive letter, FUSE versus WinFsp — and in nothing this
/// module does: both take the same [`PeerLink`] and answer the same protocol.
#[cfg(target_os = "linux")]
fn mount_peer(peer: PeerLink, peer_name: &str) -> anyhow::Result<Mount> {
    Ok(cursio_files::mount::mount(
        peer,
        mount_root(peer_name),
        peer_name,
    )?)
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
fn mount_peer(peer: PeerLink, peer_name: &str) -> anyhow::Result<Mount> {
    cursio_files::winmount::mount(peer, peer_name)
}

/// Serving is portable; mounting is not.
///
/// macOS has no mount backend here (FUSE needs a kernel extension the user must
/// install and approve), and a MinGW build has none either — `winfsp-sys`
/// refuses that target. Failing only *this* half is what lets the module still
/// run: the engine already treats an absent mount as "still sharing ours", so a
/// Mac serves its volumes to a Linux peer that can mount them, and says plainly
/// why the reverse direction is missing.
#[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
fn mount_peer(_peer: PeerLink, _peer_name: &str) -> anyhow::Result<Mount> {
    anyhow::bail!("no mount backend on this platform; install FUSE (Linux) or WinFsp (Windows)")
}

/// One request we sent that has not been answered yet.
enum Pending {
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
    fn fail(self, e: FileError) {
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

/// Serves our volumes to the peer and mounts theirs here.
pub struct FilesEngine {
    peer_name: String,
    /// Our disks, as the peer sees them. Behind a mutex because the blocking
    /// tasks that touch the disk need it across await points.
    server: std::sync::Arc<std::sync::Mutex<FileServer>>,
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
    /// Threads that do the compressing and the disk work, kept below the
    /// priority of everything the user is actually looking at.
    workers: Workers,
    /// Sizes the in-flight window from what the link is measured to be doing.
    pace: Pacer,
    /// Requests we have sent and not yet had answered.
    pending: HashMap<FileReqId, Pending>,
    next_req: FileReqId,
    /// Live mount, unmounted when dropped.
    mount: Option<Mount>,
    /// Deferred until the first pump, because `new` is not async.
    announce_volumes: bool,
    /// The peer's volumes as last announced, answered from here instead of over
    /// the link. See where it is filled for why this matters.
    peer_volumes: Vec<Volume>,
}

impl FilesEngine {
    pub fn new(peer_name: String) -> anyhow::Result<Self> {
        let (ops_tx, ops_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outbox_tx, outbox_rx) = tokio::sync::mpsc::unbounded_channel();

        let mount = match mount_peer(PeerLink::new(ops_tx), &peer_name) {
            Ok(m) => Some(m),
            // A failed mount costs us the peer's disks but not our own sharing,
            // and certainly not the session — rule 6 in platform-design.
            Err(e) => {
                tracing::warn!(error = %e, peer = %peer_name, "cannot mount the peer's volumes here; still sharing ours");
                None
            }
        };

        Ok(Self {
            peer_name,
            server: std::sync::Arc::new(std::sync::Mutex::new(FileServer::detect())),
            ops_rx,
            done_tx,
            done_rx,
            outbox_tx,
            outbox_rx,
            workers: Workers::new("cursio-files"),
            pace: Pacer::new(),
            pending: HashMap::new(),
            next_req: 1,
            mount,
            announce_volumes: true,
            peer_volumes: Vec::new(),
        })
    }

    fn next_req(&mut self) -> FileReqId {
        let id = self.next_req;
        self.next_req += 1;
        id
    }
}

/// Compress a chunk, keeping the result only when it genuinely shrank.
fn pack(bytes: Vec<u8>) -> Chunk {
    let plain_len = bytes.len();
    match zstd::encode_all(bytes.as_slice(), ZSTD_LEVEL) {
        Ok(data) if Chunk::worth_compressing(plain_len, data.len()) => Chunk::Zstd {
            data,
            plain_len: plain_len as u32,
        },
        // Either it did not compress usefully or zstd failed; the raw bytes are
        // always correct, so a compression failure is not an error.
        _ => Chunk::Raw(bytes),
    }
}

/// Undo [`pack`].
fn unpack(chunk: Chunk) -> Result<Vec<u8>, FileError> {
    match chunk {
        Chunk::Raw(b) => Ok(b),
        Chunk::Zstd { data, plain_len } => {
            let out = zstd::decode_all(data.as_slice())
                .map_err(|e| FileError::Io(format!("decompress: {e}")))?;
            if out.len() != plain_len as usize {
                // A payload that decompresses to the wrong length would be
                // written into a file as if it were correct.
                return Err(FileError::Io(format!(
                    "chunk decompressed to {} bytes, expected {plain_len}",
                    out.len()
                )));
            }
            Ok(out)
        }
    }
}

async fn send(tx: &mut dyn TransportTx, msg: FilesMsg) -> Result<(), TransportError> {
    let framed = Message::module(ModuleId::FILES, FilesMsg::PRIORITY, &msg)?;
    tx.send(&framed).await
}

/// Move messages the workers have prepared onto the link, until the queue runs
/// dry or this pump's slice is spent.
///
/// Nothing here is subject to the pacer's window: these are answers the peer is
/// blocked on, and requests already counted against the window when they were
/// accepted. Holding them back would not reduce what is in flight, only make
/// the peer wait out its timeout for something we had ready.
async fn drain(
    queue: &mut tokio::sync::mpsc::UnboundedReceiver<FilesMsg>,
    tx: &mut dyn TransportTx,
    deadline: Instant,
) -> Result<(), TransportError> {
    for _ in 0..MAX_MESSAGES_PER_PUMP {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(msg) = queue.try_recv() else { break };
        send(tx, msg).await?;
    }
    Ok(())
}

impl FilesEngine {
    /// Run one request from the peer against our disks, off the session thread.
    ///
    /// Every arm here touches a real filesystem, which can block for as long as
    /// the disk feels like — so the whole thing goes to the worker pool and its
    /// reply is picked up by a later pump. Doing this inline is what would show
    /// up in the log as the audio module's "pump blocked the session loop", only
    /// for reads that can be far slower.
    ///
    /// The pool rather than `spawn_blocking`: this is the peer's work, done on
    /// our machine while its owner is using it for something else, so it runs
    /// behind everything local — see [`cursio_files::background`].
    fn serve(&self, msg: FilesMsg) {
        let server = std::sync::Arc::clone(&self.server);
        let done = self.done_tx.clone();
        self.workers.spawn(move || {
            let reply = run_request(&server, msg);
            if let Some(reply) = reply {
                // The receiver only closes when the session is ending, in which
                // case the answer is moot.
                let _ = done.send(reply);
            }
        });
    }
}

/// Execute one peer request. Returns the reply to send, if any.
fn run_request(server: &std::sync::Mutex<FileServer>, msg: FilesMsg) -> Option<FilesMsg> {
    /// Turn a `Result` into either its success message or a `Failed`.
    fn answer<T>(
        req: FileReqId,
        r: Result<T, FileError>,
        ok: impl FnOnce(T) -> FilesMsg,
    ) -> Option<FilesMsg> {
        Some(match r {
            Ok(v) => ok(v),
            Err(error) => FilesMsg::Failed { req, error },
        })
    }

    match msg {
        FilesMsg::ReadDir { req, volume, path } => {
            let r = server.lock().expect("file server").read_dir(&volume, &path);
            answer(req, r, |entries| FilesMsg::DirEntries {
                req,
                entries,
                // Sent whole: a listing is metadata, and splitting it would only
                // matter for a directory with tens of thousands of entries.
                last: true,
            })
        }
        FilesMsg::Stat { req, volume, path } => {
            let r = server.lock().expect("file server").stat(&volume, &path);
            answer(req, r, |meta| FilesMsg::StatResult { req, meta })
        }
        FilesMsg::ReadLink { req, volume, path } => {
            let r = server
                .lock()
                .expect("file server")
                .read_link(&volume, &path);
            answer(req, r, |target| FilesMsg::LinkTarget { req, target })
        }
        FilesMsg::Open {
            req,
            volume,
            path,
            mode,
        } => {
            let r = server
                .lock()
                .expect("file server")
                .open(&volume, &path, mode);
            answer(req, r, |(handle, size)| FilesMsg::Opened {
                req,
                handle,
                size,
            })
        }
        FilesMsg::Read {
            req,
            handle,
            offset,
            len,
        } => {
            // Clamped to the wire chunk size: a caller asking for more than one
            // frame can hold gets what fits, and asks again.
            let len = len.min(FilesMsg::MAX_CHUNK as u32);
            let r = server
                .lock()
                .expect("file server")
                .read(handle, offset, len);
            answer(req, r, |bytes| FilesMsg::Data {
                req,
                offset,
                bytes: pack(bytes),
                last: true,
            })
        }
        FilesMsg::Write {
            req,
            handle,
            offset,
            bytes,
            ..
        } => {
            let r = unpack(bytes).and_then(|bytes| {
                server
                    .lock()
                    .expect("file server")
                    .write(handle, offset, &bytes)
            });
            answer(req, r, |written| FilesMsg::Wrote { req, written })
        }
        FilesMsg::Truncate { req, handle, size } => {
            let r = server.lock().expect("file server").truncate(handle, size);
            answer(req, r, |()| FilesMsg::Ok { req })
        }
        FilesMsg::MkDir { req, volume, path } => {
            let r = server.lock().expect("file server").mkdir(&volume, &path);
            answer(req, r, |()| FilesMsg::Ok { req })
        }
        FilesMsg::Remove { req, volume, path } => {
            let r = server.lock().expect("file server").remove(&volume, &path);
            answer(req, r, |()| FilesMsg::Ok { req })
        }
        FilesMsg::Rename {
            req,
            volume,
            from,
            to,
        } => {
            let r = server
                .lock()
                .expect("file server")
                .rename(&volume, &from, &to);
            answer(req, r, |()| FilesMsg::Ok { req })
        }
        FilesMsg::ListVolumes { .. } => {
            // Answered with the announcement itself, which carries no request
            // id — the list is idempotent, so the asker takes the next one.
            Some(FilesMsg::Volumes(
                server.lock().expect("file server").volumes(),
            ))
        }
        FilesMsg::Close { handle } => {
            server.lock().expect("file server").close(handle);
            None // deliberately unanswered; see the protocol docs
        }
        // Replies and announcements are not requests: they are handled inline
        // by `handle_message`, which never routes them here.
        _ => None,
    }
}

impl FilesEngine {
    /// Turn queued mount operations into requests on the wire.
    ///
    /// Bounded three ways, each for its own reason: by the pacer's window, so
    /// the link is kept full but not backed up; by `deadline`, so the session
    /// loop gets its turn back; and by a message cap that only matters if the
    /// clock lies. Whatever is left waits for the next pump — a request is
    /// worth nothing early and everything eventually, unlike an audio frame.
    async fn pump_requests(
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
                    let outbox = self.outbox_tx.clone();
                    self.workers.spawn(move || {
                        let _ = outbox.send(FilesMsg::Write {
                            req,
                            handle,
                            offset,
                            bytes: pack(bytes),
                            last: true,
                        });
                    });
                    allowance -= 1;
                    continue;
                }
                Op::Truncate {
                    handle,
                    size,
                    reply,
                } => (
                    Pending::Unit(reply),
                    FilesMsg::Truncate { req, handle, size },
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
                // Nothing to wait for, so nothing to track.
                Op::Close { handle } => {
                    send(tx, FilesMsg::Close { handle }).await?;
                    continue;
                }
            };

            // The peer answers `Volumes` with its own list; the request carries
            // no payload of ours, so an empty list is the ask.
            self.pending.insert(req, pending);
            allowance -= 1;
            send(tx, msg).await?;
        }
        Ok(())
    }

    /// Route one reply back to the callback waiting for it.
    fn on_reply(&mut self, msg: FilesMsg) {
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
                }
                // The peer announces its volumes unprompted at start, and also
                // answers our request with the same message — so satisfy every
                // waiting request rather than assuming there is exactly one.
                let waiting: Vec<FileReqId> = self
                    .pending
                    .iter()
                    .filter(|(_, p)| matches!(p, Pending::Volumes(_)))
                    .map(|(&r, _)| r)
                    .collect();
                for req in waiting {
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
                    self.workers.spawn(move || {
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

#[async_trait::async_trait]
impl SessionModule for FilesEngine {
    fn name(&self) -> &'static str {
        "files"
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let volumes = self.server.lock().expect("file server").volumes();
        tracing::info!(
            peer = %self.peer_name,
            volumes = volumes.len(),
            mounted = self.mount.is_some(),
            "file sharing up"
        );
        send(tx, FilesMsg::Volumes(volumes)).await?;
        self.announce_volumes = false;
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if self.announce_volumes {
            let volumes = self.server.lock().expect("file server").volumes();
            send(tx, FilesMsg::Volumes(volumes)).await?;
            self.announce_volumes = false;
        }

        // Read the link before deciding how much to put on it. One reading per
        // pump — every 5 ms — is frequent enough to catch a link going from
        // idle to congested within a couple of chunks, and cheap: two integers
        // from behind the transport's lock.
        let now = Instant::now();
        self.pace.observe(tx.queue_depth(FilesMsg::PRIORITY), now);
        let deadline = now + PUMP_SLICE;

        // Replies first: the peer is blocked on them, and a request of ours can
        // wait a pass.
        drain(&mut self.done_rx, tx, deadline).await?;
        drain(&mut self.outbox_rx, tx, deadline).await?;
        self.pump_requests(tx, deadline).await?;
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<FilesMsg>(ModuleId::FILES) {
            Ok(Some(msg)) => {
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
        for (_, pending) in self.pending.drain() {
            pending.fail(FileError::Io("session ended".into()));
        }
        // Dropping the handle unmounts.
        if self.mount.take().is_some() {
            tracing::info!(peer = %self.peer_name, "unmounting the peer's volumes");
        }
    }
}

/// Builds [`FilesEngine`] when file sharing is enabled.
///
/// Linux-only for now: sharing our own volumes works anywhere, but a peer that
/// could not mount ours would advertise a capability it cannot honour, and the
/// mount half needs WinFsp on Windows.
pub struct FilesFactory {
    pub enabled: bool,
}

impl crate::module::ModuleFactory for FilesFactory {
    fn id(&self) -> ModuleId {
        ModuleId::FILES
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        self.enabled
            .then(|| cursio_protocol::ModuleAdvert::symmetric(ModuleId::FILES, VERSION))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(FilesEngine::new(ctx.peer_name().to_string())?))
    }
}

/// Version of this module's wire vocabulary ([`FilesMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 2;

/// Whether a message is something to act on, rather than an answer to something
/// we asked.
fn is_request(msg: &FilesMsg) -> bool {
    matches!(
        msg,
        FilesMsg::ReadDir { .. }
            | FilesMsg::Stat { .. }
            | FilesMsg::ReadLink { .. }
            | FilesMsg::Open { .. }
            | FilesMsg::Read { .. }
            | FilesMsg::Write { .. }
            | FilesMsg::Truncate { .. }
            | FilesMsg::MkDir { .. }
            | FilesMsg::Remove { .. }
            | FilesMsg::Rename { .. }
            | FilesMsg::Close { .. }
            | FilesMsg::ListVolumes { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects what the module sends, standing in for the transport, and
    /// reports whatever backlog the test wants the link to have.
    #[derive(Default)]
    struct RecordingTx {
        sent: Vec<FilesMsg>,
        depth: QueueDepth,
    }

    #[async_trait::async_trait]
    impl TransportTx for RecordingTx {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            if let Ok(Some(msg)) = msg.decode_module::<FilesMsg>(ModuleId::FILES) {
                self.sent.push(msg);
            }
            Ok(())
        }

        fn queue_depth(&self, _priority: cursio_protocol::Priority) -> QueueDepth {
            self.depth
        }
    }

    /// An engine with no mount attached: the client half is driven directly
    /// through its operation channel, which is all these tests need and avoids
    /// mounting a filesystem on the machine running them.
    fn engine() -> (FilesEngine, tokio::sync::mpsc::UnboundedSender<Op>) {
        let (ops_tx, ops_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outbox_tx, outbox_rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = FilesEngine {
            peer_name: "peer".into(),
            server: std::sync::Arc::new(std::sync::Mutex::new(FileServer::detect())),
            ops_rx,
            done_tx,
            done_rx,
            outbox_tx,
            outbox_rx,
            workers: Workers::with_threads("test-files", 2),
            pace: Pacer::new(),
            pending: HashMap::new(),
            next_req: 1,
            mount: None,
            announce_volumes: false,
            peer_volumes: Vec::new(),
        };
        (engine, ops_tx)
    }

    /// A `Stat` the test can queue without caring about the answer.
    fn a_request() -> Op {
        let (reply, _rx) = std::sync::mpsc::channel();
        Op::Stat {
            volume: "C:".into(),
            path: "a.txt".into(),
            reply,
        }
    }

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

    /// A link that is keeping up must let the window grow, or a fast LAN runs
    /// at the floor forever.
    #[test]
    fn the_window_opens_up_while_the_link_keeps_up() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        // A link draining 10 MB/s with nothing queued.
        pace.observe(QueueDepth::default(), start);
        for i in 1..500u64 {
            let at = start + RATE_SAMPLE_INTERVAL * i as u32;
            pace.observe(
                QueueDepth {
                    queued_bytes: 0,
                    sent_bytes: 200_000 * i,
                },
                at,
            );
        }
        assert_eq!(pace.in_flight_limit, MAX_IN_FLIGHT);
    }

    /// A link falling behind must be eased off at once — and never below the
    /// floor, or the mount's callers time out waiting for a slot.
    #[test]
    fn a_backed_up_link_closes_the_window_but_not_past_the_floor() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);
        // 10 MB/s measured, then 8 MB sitting in the queue: most of a second of
        // delay, against a target of a tenth.
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 1_000_000,
            },
            start + Duration::from_millis(100),
        );
        pace.in_flight_limit = 64;

        pace.observe(
            QueueDepth {
                queued_bytes: 8 * 1024 * 1024,
                sent_bytes: 1_000_000,
            },
            start + Duration::from_millis(200),
        );
        assert_eq!(pace.in_flight_limit, 32, "congestion halves the window");

        for i in 3..12u32 {
            pace.observe(
                QueueDepth {
                    queued_bytes: 8 * 1024 * 1024,
                    sent_bytes: 1_000_000,
                },
                start + Duration::from_millis(100) * i,
            );
        }
        assert_eq!(
            pace.in_flight_limit, MIN_IN_FLIGHT,
            "easing off must not become a stall"
        );
    }

    /// Until something has actually drained there is no rate, and a queue of
    /// unknown age is no reason to act in either direction.
    #[test]
    fn an_unmeasured_link_is_left_alone() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(
            QueueDepth {
                queued_bytes: 4 * 1024 * 1024,
                sent_bytes: 0,
            },
            start,
        );
        let opening = pace.in_flight_limit;
        pace.observe(
            QueueDepth {
                queued_bytes: 4 * 1024 * 1024,
                sent_bytes: 0,
            },
            start + Duration::from_millis(500),
        );
        assert_eq!(
            pace.in_flight_limit, opening,
            "a guess about an unmeasured link is worse than holding still"
        );
        assert!(pace.queue_delay(4 * 1024 * 1024).is_none());
    }

    /// The rate estimate must follow what actually left the machine, since the
    /// whole window is derived from it.
    #[test]
    fn the_rate_estimate_follows_what_left() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 1_000_000,
            },
            start + Duration::from_millis(100),
        );
        let rate = pace.rate.expect("a measured rate");
        assert!(
            (9_000_000.0..11_000_000.0).contains(&rate),
            "1 MB in 100 ms is ~10 MB/s, got {rate}"
        );
        // And a queue is judged against it: 1 MB at 10 MB/s is 100 ms.
        let delay = pace.queue_delay(1_000_000).expect("a delay");
        assert!(
            (Duration::from_millis(90)..Duration::from_millis(110)).contains(&delay),
            "got {delay:?}"
        );
    }

    /// Samples taken too close together are noise, not measurement.
    #[test]
    fn a_rate_is_not_read_from_too_short_an_interval() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 300_000,
            },
            start + Duration::from_millis(1),
        );
        assert!(
            pace.rate.is_none(),
            "300 KB in a millisecond is a scheduling artefact, not 300 MB/s"
        );
    }

    /// Requests are served; replies are routed. Confusing the two would either
    /// hang the mount or run a reply against our own disk.
    #[test]
    fn requests_and_replies_are_told_apart() {
        assert!(is_request(&FilesMsg::Stat {
            req: 1,
            volume: "C:".into(),
            path: "a".into()
        }));
        assert!(is_request(&FilesMsg::Close { handle: 1 }));
        assert!(!is_request(&FilesMsg::Ok { req: 1 }));
        assert!(!is_request(&FilesMsg::Volumes(Vec::new())));
        assert!(!is_request(&FilesMsg::Failed {
            req: 1,
            error: FileError::NotFound
        }));
    }

    /// Asking for the volume list must not look like announcing an empty one.
    ///
    /// These were the same message once: a request encoded as
    /// `Volumes(vec![])`. The peer's request then arrived here as "I have no
    /// volumes", blanked the list we had just been given, and the mount was torn
    /// down a moment later — with `count=1` and `count=0` logged back to back.
    #[test]
    fn asking_for_volumes_is_not_announcing_none() {
        assert!(
            is_request(&FilesMsg::ListVolumes { req: 1 }),
            "a request must be served, not routed as a reply"
        );
        assert!(
            !is_request(&FilesMsg::Volumes(Vec::new())),
            "an announcement is a reply, whatever it contains"
        );
    }

    /// Compressible data must survive the round trip, and so must data that
    /// does not compress at all.
    #[test]
    fn chunks_round_trip_whether_or_not_they_compress() {
        let text = b"the same words over and over ".repeat(64);
        let packed = pack(text.clone());
        assert!(
            matches!(packed, Chunk::Zstd { .. }),
            "repetitive text must compress"
        );
        assert!(
            packed.wire_len() < text.len(),
            "and must actually be smaller"
        );
        assert_eq!(unpack(packed).expect("unpacks"), text);

        // Incompressible: kept raw rather than declared compressed for nothing.
        let noise: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        let packed = pack(noise.clone());
        assert_eq!(unpack(packed).expect("unpacks"), noise);
    }

    /// A payload whose length does not match what was promised must be refused,
    /// not written into a file as if it were correct.
    #[test]
    fn a_mismatched_chunk_is_rejected() {
        let bad = Chunk::Zstd {
            data: zstd::encode_all(&b"four"[..], 1).expect("compresses"),
            plain_len: 999,
        };
        assert!(matches!(unpack(bad), Err(FileError::Io(_))));
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
