//! The files [`SessionModule`]: the peer's disks here, ours there.
//!
//! Two halves sharing one message type:
//!
//! - **Server half** — answers the peer's requests against our own volumes.
//!   The work is real disk I/O, so it never runs inline: see `pump_serving`.
//! - **Client half** — carries requests from the FUSE mount ([`cursio_files::mount`])
//!   to the peer and routes replies back to whichever callback is waiting.
//!
//! ## Nothing blocks the session loop
//!
//! A `pump` that takes longer than ~50 ms is logged as having blocked the
//! session, and the shared cursor rides the same channel — so a file read must
//! never happen inline. Disk work is handed to `spawn_blocking` and collected on
//! a later pump, which keeps each pass short no matter how large the transfer.

use std::collections::HashMap;

use cursio_files::bridge::{Op, PeerLink};
use cursio_files::FileServer;
use cursio_protocol::{Chunk, FileError, FileReqId, FilesMsg, Message, ModuleId, Volume};
use cursio_transport::{TransportError, TransportTx};

use crate::module::SessionModule;

/// Most replies to send in one pump.
///
/// The same discipline as the audio module's budget: bounded work per pass so
/// the loop always gets back to its `select!`. Anything left waits for the next
/// pump rather than being dropped — file data, unlike audio, is worthless if it
/// arrives incomplete.
const MAX_REPLIES_PER_PUMP: usize = 8;

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

/// One request we sent that has not been answered yet.
enum Pending {
    Volumes(std::sync::mpsc::Sender<Result<Vec<Volume>, FileError>>),
    Dir {
        reply: std::sync::mpsc::Sender<Result<Vec<cursio_protocol::FileMeta>, FileError>>,
        /// Entries gathered so far: a large listing arrives in several messages.
        acc: Vec<cursio_protocol::FileMeta>,
    },
    Stat(std::sync::mpsc::Sender<Result<cursio_protocol::FileMeta, FileError>>),
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
    /// Replies from disk work finished on a blocking thread.
    done_tx: tokio::sync::mpsc::UnboundedSender<FilesMsg>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<FilesMsg>,
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

impl FilesEngine {
    /// Run one request from the peer against our disks, off the session thread.
    ///
    /// Every arm here touches a real filesystem, which can block for as long as
    /// the disk feels like — so the whole thing goes to `spawn_blocking` and its
    /// reply is picked up by a later pump. Doing this inline is what would show
    /// up in the log as the audio module's "pump blocked the session loop", only
    /// for reads that can be far slower.
    fn serve(&self, msg: FilesMsg) {
        let server = std::sync::Arc::clone(&self.server);
        let done = self.done_tx.clone();
        tokio::task::spawn_blocking(move || {
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
fn run_request(
    server: &std::sync::Mutex<FileServer>,
    msg: FilesMsg,
) -> Option<FilesMsg> {
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
            answer(req, r, |(handle, size)| FilesMsg::Opened { req, handle, size })
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
            let r = server.lock().expect("file server").read(handle, offset, len);
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
    /// Bounded per pass, like every other module: the FUSE threads can queue
    /// faster than the link drains, and draining them dry here would keep the
    /// session from its `select!`.
    async fn pump_requests(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        for _ in 0..MAX_REPLIES_PER_PUMP {
            let Ok(op) = self.ops_rx.try_recv() else { break };
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
                Op::Write {
                    handle,
                    offset,
                    bytes,
                    reply,
                } => (
                    Pending::Wrote(reply),
                    FilesMsg::Write {
                        req,
                        handle,
                        offset,
                        bytes: pack(bytes),
                        last: true,
                    },
                ),
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
            send(tx, msg).await?;
        }
        Ok(())
    }

    /// Flush replies produced by finished disk work.
    async fn pump_serving(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        for _ in 0..MAX_REPLIES_PER_PUMP {
            let Ok(reply) = self.done_rx.try_recv() else {
                break;
            };
            send(tx, reply).await?;
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
        // Replies first: the peer is blocked on them, and a request of ours can
        // wait a pass.
        self.pump_serving(tx).await?;
        self.pump_requests(tx).await?;
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
pub const VERSION: u16 = 1;

/// Whether a message is something to act on, rather than an answer to something
/// we asked.
fn is_request(msg: &FilesMsg) -> bool {
    matches!(
        msg,
        FilesMsg::ReadDir { .. }
            | FilesMsg::Stat { .. }
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
        assert!(packed.wire_len() < text.len(), "and must actually be smaller");
        assert_eq!(unpack(packed).expect("unpacks"), text);

        // Incompressible: kept raw rather than declared compressed for nothing.
        let noise: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 16) as u8).collect();
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
