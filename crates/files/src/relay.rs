//! A mount presented by another process on this machine.
//!
//! On Windows a drive letter belongs to the logon session that made it, and UAC
//! gives an administrator two: the elevated one the node runs in (it elevates
//! so injected input reaches elevated windows) and the filtered one Explorer
//! runs in. A drive mounted by the node is real, readable — and absent from
//! Explorer, which sees only its own session's letters.
//!
//! So the node keeps the session and an unelevated helper keeps the drive. Only
//! the mount moves: the files engine — its in-flight window, read-ahead, and
//! the serving of this machine's disks — stays in the node, because it is
//! paced from the link's own queue figures, which no other process can read.
//! What crosses between the two is exactly what crosses between a mount and
//! the engine inside one process, [`Op`]: the helper's dispatcher threads ask
//! through a local [`PeerLink`], and the node answers each question by asking
//! its own `PeerLink` on a blocking thread. Nothing in the engine or the mount
//! knows the boundary is there.
//!
//! One stream is one mounted volume. The node opens it, names the volume, and
//! the helper answers with the drive it mounted; closing the stream from
//! either end unmounts. That makes the drive's lifetime the stream's, so a
//! node that dies or restarts cannot leave a drive behind in a helper that
//! outlives it.
//!
//! Timeouts are not duplicated on the node side: the node's `PeerLink` already
//! bounds every wait, and the helper's bounds the same wait again, so a peer
//! that stops answering fails both within the same deadline.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use cursio_protocol::{FileError, FileHandle, FileMeta, OpenMode, Volume};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::bridge::{Op, PeerLink};
use crate::FilesError;

/// Bumped whenever a message below changes. The node starts the helper from
/// its own executable, so the two normally agree; this catches the case where
/// they do not (a helper left running across an update) before any request is
/// misread.
pub const RELAY_VERSION: u32 = 1;

/// The largest frame either side accepts. A write carries at most one wire
/// chunk, a directory listing at most a few thousand entries; this is far
/// above both and still refuses a length that is garbage.
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Which of the peer's volumes to present, and how to name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRequest {
    pub peer_name: String,
    pub volume: String,
    pub label: String,
}

/// Node → helper.
#[derive(Debug, Serialize, Deserialize)]
enum NodeMsg {
    Mount { version: u32, request: MountRequest },
    Reply { call: u64, answer: Answer },
}

/// Helper → node.
#[derive(Debug, Serialize, Deserialize)]
enum HelperMsg {
    Mounted { mountpoint: String },
    Refused { reason: String },
    Call { call: u64, request: Request },
}

/// An [`Op`] without its reply channel, which stays with the caller.
#[derive(Debug, Serialize, Deserialize)]
enum Request {
    Volumes,
    ReadDir {
        volume: String,
        path: String,
    },
    Stat {
        volume: String,
        path: String,
    },
    ReadLink {
        volume: String,
        path: String,
    },
    Open {
        volume: String,
        path: String,
        mode: OpenMode,
    },
    Read {
        handle: FileHandle,
        offset: u64,
        len: u32,
    },
    Write {
        handle: FileHandle,
        offset: u64,
        bytes: Vec<u8>,
    },
    Truncate {
        handle: FileHandle,
        size: u64,
        reserve: bool,
    },
    Close {
        handle: FileHandle,
    },
    MkDir {
        volume: String,
        path: String,
    },
    Remove {
        volume: String,
        path: String,
    },
    Rename {
        volume: String,
        from: String,
        to: String,
    },
}

/// What a [`Request`] answered, one variant per reply type.
#[derive(Debug, Serialize, Deserialize)]
enum Answer {
    Volumes(Result<Vec<Volume>, FileError>),
    Metas(Result<Vec<FileMeta>, FileError>),
    Meta(Result<FileMeta, FileError>),
    Link(Result<String, FileError>),
    Opened(Result<(FileHandle, u64), FileError>),
    Bytes(Result<Vec<u8>, FileError>),
    Wrote(Result<u64, FileError>),
    Unit(Result<(), FileError>),
}

async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> Result<(), FilesError> {
    let bytes = bincode::serialize(msg).map_err(|e| FilesError::Relay(e.to_string()))?;
    w.write_u32(bytes.len() as u32).await.map_err(relay_io)?;
    w.write_all(&bytes).await.map_err(relay_io)?;
    w.flush().await.map_err(relay_io)
}

/// `None` at a clean end of stream.
async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> Result<Option<T>, FilesError> {
    let len = match r.read_u32().await {
        Ok(len) => len as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(relay_io(e)),
    };
    if len > MAX_FRAME {
        return Err(FilesError::Relay(format!(
            "a frame of {len} bytes is past the {MAX_FRAME}-byte limit"
        )));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await.map_err(relay_io)?;
    bincode::deserialize(&buf)
        .map(Some)
        .map_err(|e| FilesError::Relay(e.to_string()))
}

fn relay_io(e: std::io::Error) -> FilesError {
    FilesError::Relay(e.to_string())
}

/// Node side: ask the helper on `stream` to present `request`, then answer its
/// calls from `link` until either end closes.
///
/// `mounted` hears the drive the helper made, or why it could not. Returns when
/// the stream ends; dropping the future (or the task running it) closes the
/// stream, which is how the node unmounts.
pub async fn serve_mount<S>(
    stream: S,
    request: MountRequest,
    link: PeerLink,
    mounted: tokio::sync::oneshot::Sender<Result<String, String>>,
) -> Result<(), FilesError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    write_frame(
        &mut wr,
        &NodeMsg::Mount {
            version: RELAY_VERSION,
            request,
        },
    )
    .await?;

    // Answers are produced on blocking threads in whatever order they finish,
    // so they meet at one writer rather than each taking the stream.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<NodeMsg>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_frame(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    let mut mounted = Some(mounted);
    let result = loop {
        let msg = match read_frame::<_, HelperMsg>(&mut rd).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        match msg {
            HelperMsg::Mounted { mountpoint } => {
                if let Some(tx) = mounted.take() {
                    let _ = tx.send(Ok(mountpoint));
                }
            }
            HelperMsg::Refused { reason } => {
                if let Some(tx) = mounted.take() {
                    let _ = tx.send(Err(reason));
                }
                break Ok(());
            }
            HelperMsg::Call { call, request } => {
                let link = link.clone();
                let out = out_tx.clone();
                // Each call blocks for a round trip to the peer, as it would
                // have on the mount's own dispatcher thread in this process.
                tokio::task::spawn_blocking(move || {
                    if let Some(answer) = execute(&link, request) {
                        let _ = out.send(NodeMsg::Reply { call, answer });
                    }
                });
            }
        }
    };
    if let Some(tx) = mounted.take() {
        let _ = tx.send(Err("the mount helper closed before mounting".into()));
    }
    drop(out_tx);
    writer.abort();
    result
}

/// Ask the session what the helper's mount asked. `None` for a close, which
/// is answered by nobody.
fn execute(link: &PeerLink, request: Request) -> Option<Answer> {
    Some(match request {
        Request::Volumes => Answer::Volumes(link.volumes()),
        Request::ReadDir { volume, path } => Answer::Metas(link.read_dir(&volume, &path)),
        Request::Stat { volume, path } => Answer::Meta(link.stat(&volume, &path)),
        Request::ReadLink { volume, path } => Answer::Link(link.read_link(&volume, &path)),
        Request::Open { volume, path, mode } => Answer::Opened(link.open(&volume, &path, mode)),
        Request::Read {
            handle,
            offset,
            len,
        } => Answer::Bytes(link.read(handle, offset, len)),
        Request::Write {
            handle,
            offset,
            bytes,
        } => Answer::Wrote(link.write(handle, offset, bytes)),
        Request::Truncate {
            handle,
            size,
            reserve,
        } => Answer::Unit(link.set_size(handle, size, reserve)),
        Request::Close { handle } => {
            link.close(handle);
            return None;
        }
        Request::MkDir { volume, path } => Answer::Unit(link.mkdir(&volume, &path)),
        Request::Remove { volume, path } => Answer::Unit(link.remove(&volume, &path)),
        Request::Rename { volume, from, to } => Answer::Unit(link.rename(&volume, &from, &to)),
    })
}

/// The reply channel an [`Op`] carried, kept here while its request is away.
enum Waiting {
    Volumes(std_mpsc::Sender<Result<Vec<Volume>, FileError>>),
    Metas(std_mpsc::Sender<Result<Vec<FileMeta>, FileError>>),
    Meta(std_mpsc::Sender<Result<FileMeta, FileError>>),
    Link(std_mpsc::Sender<Result<String, FileError>>),
    Opened(std_mpsc::Sender<Result<(FileHandle, u64), FileError>>),
    Bytes(std_mpsc::Sender<Result<Vec<u8>, FileError>>),
    Wrote(std_mpsc::Sender<Result<u64, FileError>>),
    Unit(std_mpsc::Sender<Result<(), FileError>>),
}

impl Waiting {
    fn deliver(self, answer: Answer) {
        match (self, answer) {
            (Waiting::Volumes(tx), Answer::Volumes(r)) => drop(tx.send(r)),
            (Waiting::Metas(tx), Answer::Metas(r)) => drop(tx.send(r)),
            (Waiting::Meta(tx), Answer::Meta(r)) => drop(tx.send(r)),
            (Waiting::Link(tx), Answer::Link(r)) => drop(tx.send(r)),
            (Waiting::Opened(tx), Answer::Opened(r)) => drop(tx.send(r)),
            (Waiting::Bytes(tx), Answer::Bytes(r)) => drop(tx.send(r)),
            (Waiting::Wrote(tx), Answer::Wrote(r)) => drop(tx.send(r)),
            (Waiting::Unit(tx), Answer::Unit(r)) => drop(tx.send(r)),
            (waiting, answer) => {
                tracing::warn!(
                    ?answer,
                    "the node answered a call with the wrong kind of reply"
                );
                waiting.fail(FileError::Io("the node answered the wrong question".into()));
            }
        }
    }

    fn fail(self, e: FileError) {
        match self {
            Waiting::Volumes(tx) => drop(tx.send(Err(e))),
            Waiting::Metas(tx) => drop(tx.send(Err(e))),
            Waiting::Meta(tx) => drop(tx.send(Err(e))),
            Waiting::Link(tx) => drop(tx.send(Err(e))),
            Waiting::Opened(tx) => drop(tx.send(Err(e))),
            Waiting::Bytes(tx) => drop(tx.send(Err(e))),
            Waiting::Wrote(tx) => drop(tx.send(Err(e))),
            Waiting::Unit(tx) => drop(tx.send(Err(e))),
        }
    }
}

/// Separate what crosses to the node from the channel the answer goes back on.
fn split(op: Op) -> (Request, Option<Waiting>) {
    match op {
        Op::Volumes { reply } => (Request::Volumes, Some(Waiting::Volumes(reply))),
        Op::ReadDir {
            volume,
            path,
            reply,
        } => (
            Request::ReadDir { volume, path },
            Some(Waiting::Metas(reply)),
        ),
        Op::Stat {
            volume,
            path,
            reply,
        } => (Request::Stat { volume, path }, Some(Waiting::Meta(reply))),
        Op::ReadLink {
            volume,
            path,
            reply,
        } => (
            Request::ReadLink { volume, path },
            Some(Waiting::Link(reply)),
        ),
        Op::Open {
            volume,
            path,
            mode,
            reply,
        } => (
            Request::Open { volume, path, mode },
            Some(Waiting::Opened(reply)),
        ),
        Op::Read {
            handle,
            offset,
            len,
            reply,
        } => (
            Request::Read {
                handle,
                offset,
                len,
            },
            Some(Waiting::Bytes(reply)),
        ),
        Op::Write {
            handle,
            offset,
            bytes,
            reply,
        } => (
            Request::Write {
                handle,
                offset,
                bytes,
            },
            Some(Waiting::Wrote(reply)),
        ),
        Op::Truncate {
            handle,
            size,
            reserve,
            reply,
        } => (
            Request::Truncate {
                handle,
                size,
                reserve,
            },
            Some(Waiting::Unit(reply)),
        ),
        Op::Close { handle } => (Request::Close { handle }, None),
        Op::MkDir {
            volume,
            path,
            reply,
        } => (Request::MkDir { volume, path }, Some(Waiting::Unit(reply))),
        Op::Remove {
            volume,
            path,
            reply,
        } => (Request::Remove { volume, path }, Some(Waiting::Unit(reply))),
        Op::Rename {
            volume,
            from,
            to,
            reply,
        } => (
            Request::Rename { volume, from, to },
            Some(Waiting::Unit(reply)),
        ),
    }
}

/// Helper side: read which volume the node wants on `stream`, present it with
/// `mount`, and carry the mount's operations to the node until the stream
/// ends — then unmount.
///
/// `mount` is handed a [`PeerLink`] exactly like the one a mount gets inside
/// the node, and answers with the mounted handle (dropping it unmounts) and
/// where it is. It runs on a blocking thread: mounting waits on the OS.
pub async fn present_mount<S, M, F>(stream: S, mount: F) -> Result<(), FilesError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    M: Send + 'static,
    F: FnOnce(PeerLink, MountRequest) -> Result<(M, String), String> + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let request = match read_frame::<_, NodeMsg>(&mut rd).await? {
        Some(NodeMsg::Mount { version, request }) if version == RELAY_VERSION => request,
        Some(NodeMsg::Mount { version, .. }) => {
            let reason = format!(
                "the node speaks mount relay {version}, this helper {RELAY_VERSION}; \
                 restart the node so it starts a matching helper"
            );
            write_frame(
                &mut wr,
                &HelperMsg::Refused {
                    reason: reason.clone(),
                },
            )
            .await?;
            return Err(FilesError::Relay(reason));
        }
        Some(NodeMsg::Reply { .. }) => {
            return Err(FilesError::Relay("a reply arrived before any mount".into()))
        }
        None => return Ok(()),
    };

    let (ops_tx, mut ops_rx) = mpsc::unbounded_channel::<Op>();
    let link = PeerLink::new(ops_tx);
    let mounted = {
        let link = link.clone();
        tokio::task::spawn_blocking(move || mount(link, request))
            .await
            .map_err(|e| FilesError::Relay(format!("the mount panicked: {e}")))?
    };
    let handle = match mounted {
        Ok((handle, mountpoint)) => {
            write_frame(&mut wr, &HelperMsg::Mounted { mountpoint }).await?;
            handle
        }
        Err(reason) => {
            write_frame(&mut wr, &HelperMsg::Refused { reason }).await?;
            return Ok(());
        }
    };

    let waiting: Arc<Mutex<HashMap<u64, Waiting>>> = Arc::default();
    let forward = {
        let waiting = waiting.clone();
        tokio::spawn(async move {
            let mut next_call = 0u64;
            while let Some(op) = ops_rx.recv().await {
                next_call += 1;
                let (request, reply) = split(op);
                if let Some(reply) = reply {
                    waiting
                        .lock()
                        .expect("relay calls")
                        .insert(next_call, reply);
                }
                let msg = HelperMsg::Call {
                    call: next_call,
                    request,
                };
                if write_frame(&mut wr, &msg).await.is_err() {
                    break;
                }
            }
        })
    };

    let result = loop {
        match read_frame::<_, NodeMsg>(&mut rd).await {
            Ok(Some(NodeMsg::Reply { call, answer })) => {
                let reply = waiting.lock().expect("relay calls").remove(&call);
                match reply {
                    Some(reply) => reply.deliver(answer),
                    None => tracing::debug!(call, "an answer for a call nobody is waiting on"),
                }
            }
            Ok(Some(NodeMsg::Mount { .. })) => {
                break Err(FilesError::Relay("a second mount on one stream".into()))
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };

    // The node is gone: release every dispatcher thread before the unmount
    // waits for them (see `PeerLink::shut_down`).
    link.shut_down();
    forward.abort();
    for (_, reply) in waiting.lock().expect("relay calls").drain() {
        reply.fail(FileError::Io("the node closed the mount".into()));
    }
    let _ = tokio::task::spawn_blocking(move || drop(handle)).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::FileKind;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn meta(name: &str) -> FileMeta {
        FileMeta {
            name: name.into(),
            kind: FileKind::File,
            size: 42,
            modified_unix: 0,
            writable: true,
            allocated: None,
        }
    }

    /// The node's session, reduced to answering what its mount link asks.
    fn fake_session() -> (PeerLink, std_mpsc::Receiver<FileHandle>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Op>();
        let (closed_tx, closed_rx) = std_mpsc::channel();
        tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                match op {
                    Op::Stat { path, reply, .. } => drop(reply.send(Ok(meta(&path)))),
                    Op::Write { bytes, reply, .. } => drop(reply.send(Ok(bytes.len() as u64))),
                    Op::Close { handle } => drop(closed_tx.send(handle)),
                    other => panic!("unexpected {other:?}"),
                }
            }
        });
        (PeerLink::new(tx), closed_rx)
    }

    struct Unmounted(Arc<AtomicBool>);
    impl Drop for Unmounted {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn request() -> MountRequest {
        MountRequest {
            peer_name: "Lightmoon".into(),
            volume: "C:".into(),
            label: "Windows".into(),
        }
    }

    /// Starts both ends; answers the helper's link, the unmount flag, the
    /// node's task and the helper's task.
    async fn connect() -> (
        PeerLink,
        std_mpsc::Receiver<FileHandle>,
        Arc<AtomicBool>,
        tokio::task::JoinHandle<Result<(), FilesError>>,
        tokio::task::JoinHandle<Result<(), FilesError>>,
    ) {
        let (node_end, helper_end) = tokio::io::duplex(1 << 20);
        let (session, closed) = fake_session();
        let (mounted_tx, mounted_rx) = tokio::sync::oneshot::channel();
        let node = tokio::spawn(serve_mount(node_end, request(), session, mounted_tx));

        let unmounted = Arc::new(AtomicBool::new(false));
        let (link_tx, link_rx) = std_mpsc::channel();
        let flag = unmounted.clone();
        let helper = tokio::spawn(present_mount(helper_end, move |link, req| {
            assert_eq!(req, request());
            link_tx.send(link).unwrap();
            Ok((Unmounted(flag), "Z:".to_string()))
        }));

        assert_eq!(mounted_rx.await.unwrap(), Ok("Z:".to_string()));
        (link_rx.recv().unwrap(), closed, unmounted, node, helper)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_helpers_mount_is_answered_by_the_nodes_session() {
        let (link, _closed, _unmounted, _node, _helper) = connect().await;
        let got = tokio::task::spawn_blocking(move || {
            (
                link.stat("C:", "Users/notes.txt"),
                link.write(3, 0, vec![1; 1000]),
            )
        })
        .await
        .unwrap();
        assert_eq!(got.0, Ok(meta("Users/notes.txt")));
        assert_eq!(got.1, Ok(1000));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_close_crosses_without_waiting_for_an_answer() {
        let (link, closed, _unmounted, _node, _helper) = connect().await;
        link.close(7);
        let handle = tokio::task::spawn_blocking(move || closed.recv().unwrap())
            .await
            .unwrap();
        assert_eq!(handle, 7);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_node_going_away_unmounts_the_helpers_drive() {
        let (link, _closed, unmounted, node, helper) = connect().await;
        node.abort();
        helper.await.unwrap().unwrap();
        assert!(unmounted.load(Ordering::SeqCst));
        let after = tokio::task::spawn_blocking(move || link.stat("C:", "x"))
            .await
            .unwrap();
        assert!(after.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_mount_reaches_the_node_with_its_reason() {
        let (node_end, helper_end) = tokio::io::duplex(1 << 16);
        let (session, _closed) = fake_session();
        let (mounted_tx, mounted_rx) = tokio::sync::oneshot::channel();
        let node = tokio::spawn(serve_mount(node_end, request(), session, mounted_tx));
        tokio::spawn(present_mount(helper_end, |_, _| {
            Err::<((), String), _>("no free drive letter".into())
        }));
        assert_eq!(
            mounted_rx.await.unwrap(),
            Err("no free drive letter".to_string())
        );
        node.await.unwrap().unwrap();
    }
}
