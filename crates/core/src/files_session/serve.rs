//! Answering the peer from our own disks, off the session loop: the
//! workers, the request they run, and the compression of what they read.

use crate::files_session::health::SLOW_SERVE_WARN;
use crate::files_session::wire::request_kind;
use crate::files_session::wire::request_req;
use crate::files_session::FilesEngine;
use cursio_files::FileServer;
use cursio_protocol::{Chunk, FileError, FileReqId, FilesMsg};
use std::sync::PoisonError;
use std::time::Instant;

/// Threads for compressing our writes and unpacking our reads.
///
/// Two, because one zstd thread at [`ZSTD_LEVEL`] already outruns the link by
/// an order of magnitude; the second keeps a large chunk from holding up the
/// small ones queued behind it.
pub(super) const CODEC_THREADS: usize = 2;

/// Compression level for file chunks.
///
/// Level 1 measured ~284 MB/s on compressible data here — an order of magnitude
/// above the link — while still reaching ~38% on binaries and ~30% on text. A
/// higher level buys a few percent for a large slice of that headroom, which is
/// the wrong trade when the network is the bottleneck.
pub(super) const ZSTD_LEVEL: i32 = 1;

/// Compress a chunk, keeping the result only when it genuinely shrank.
pub(super) fn pack(bytes: Vec<u8>) -> Chunk {
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
pub(super) fn unpack(chunk: Chunk) -> Result<Vec<u8>, FileError> {
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

/// A request counted as in flight for as long as this lives.
///
/// The count decides whether the module reports pending work, so it has to fall
/// on every exit a worker has, unwinding included — a job that panicked while
/// holding the count raised would leave the session loop believing there was
/// work outstanding forever.
pub(super) struct InFlight(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl InFlight {
    pub(super) fn raise(count: &std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        InFlight(std::sync::Arc::clone(count))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Execute one peer request. Returns the reply to send, if any.
pub(super) fn run_request(
    server: &std::sync::Mutex<FileServer>,
    msg: FilesMsg,
) -> Option<FilesMsg> {
    /// Turn a `Result` into either its success message or a `Failed`.
    ///
    /// `what` names the operation and its target, so a refusal says which path
    /// was refused rather than only that something was. A bare error kind is
    /// nearly useless here: `NotFound` repeating twice a second says an
    /// application is hunting for something, and only the path says what.
    fn answer<T>(
        req: FileReqId,
        what: &str,
        r: Result<T, FileError>,
        ok: impl FnOnce(T) -> FilesMsg,
    ) -> Option<FilesMsg> {
        Some(match r {
            Ok(v) => ok(v),
            // Said out loud before it goes on the wire. A refusal is invisible
            // on this side otherwise — the peer reports "the drive failed" and
            // the machine that decided so has nothing in its log to say why.
            Err(error) => {
                tracing::debug!(?error, what, "refusing a file request from the peer");
                FilesMsg::Failed { req, error }
            }
        })
    }

    // Path operations never hold the server's lock across the disk: taken for
    // the length of an `Arc` clone, released before anything is read. See
    // [`cursio_files::Namespace`].
    let ns = || {
        server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .namespace()
    };

    match msg {
        FilesMsg::ReadDir { req, volume, path } => {
            let r = ns().read_dir(&volume, &path);
            answer(req, &format!("readdir {volume}/{path}"), r, |entries| {
                FilesMsg::DirEntries {
                    req,
                    entries,
                    // Sent whole: a listing is metadata, and splitting it would only
                    // matter for a directory with tens of thousands of entries.
                    last: true,
                }
            })
        }
        FilesMsg::Stat { req, volume, path } => {
            let r = ns().stat(&volume, &path);
            answer(req, &format!("stat {volume}/{path}"), r, |meta| {
                FilesMsg::StatResult { req, meta }
            })
        }
        FilesMsg::ReadLink { req, volume, path } => {
            let r = ns().read_link(&volume, &path);
            answer(req, &format!("readlink {volume}/{path}"), r, |target| {
                FilesMsg::LinkTarget { req, target }
            })
        }
        FilesMsg::Open {
            req,
            volume,
            path,
            mode,
        } => {
            // Opened outside the lock — on Windows an open can wait on another
            // process's sharing lock or an antivirus scan — and handed a handle
            // under it afterwards.
            let r = ns().open(&volume, &path, mode).and_then(|(file, size)| {
                let handle = server
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .adopt(file, mode.write)?;
                Ok((handle, size))
            });
            answer(
                req,
                &format!("open {volume}/{path}"),
                r,
                |(handle, size)| FilesMsg::Opened { req, handle, size },
            )
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
            // The handle is taken under the lock and the disk is touched after
            // it is released. A temporary guard lives to the end of its
            // expression, so `server.lock().read(..)` held the whole server for
            // the length of every read — every other request, for every other
            // file, queued behind it. Measured against a peer downloading a
            // game: 11 writes a second with 0.2 operations in flight, against a
            // window of 128.
            //
            // So the detach is a statement of its own. Chaining the read onto
            // it as `.and_then(..)` is the same expression, and kept the guard
            // alive across the disk after all: under I/O pressure a 2 s read
            // held the lock for 2 s, and everything else wanting it waited.
            let file = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .detach(handle);
            let r = file.and_then(|file| file.read(offset, len));
            answer(
                req,
                &format!("read handle {handle} at {offset}"),
                r,
                |bytes| FilesMsg::Data {
                    req,
                    offset,
                    bytes: pack(bytes),
                    last: true,
                },
            )
        }
        FilesMsg::Write {
            req,
            handle,
            offset,
            bytes,
            ..
        } => {
            // Detached in a statement of its own, for the reason given under
            // `Read`.
            let r = unpack(bytes).and_then(|bytes| {
                let file = server
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .detach_writable(handle);
                file.and_then(|file| file.write(offset, &bytes))
            });
            answer(
                req,
                &format!("write handle {handle} at {offset}"),
                r,
                |written| FilesMsg::Wrote { req, written },
            )
        }
        FilesMsg::Truncate {
            req,
            handle,
            size,
            reserve,
        } => {
            // Reserving space for a large file writes it out; see `Read` for
            // why the detach stands alone.
            let file = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .detach_writable(handle);
            let r = file.and_then(|file| file.set_size(size, reserve));
            answer(
                req,
                &format!("set_size handle {handle} to {size} reserve={reserve}"),
                r,
                |()| FilesMsg::Ok { req },
            )
        }
        FilesMsg::MkDir { req, volume, path } => {
            let r = ns().mkdir(&volume, &path);
            answer(req, &format!("mkdir {volume}/{path}"), r, |()| {
                FilesMsg::Ok { req }
            })
        }
        FilesMsg::Remove { req, volume, path } => {
            let r = ns().remove(&volume, &path);
            answer(req, &format!("remove {volume}/{path}"), r, |()| {
                FilesMsg::Ok { req }
            })
        }
        FilesMsg::Rename {
            req,
            volume,
            from,
            to,
        } => {
            let r = ns().rename(&volume, &from, &to);
            answer(req, &format!("rename {volume}/{from} -> {to}"), r, |()| {
                FilesMsg::Ok { req }
            })
        }
        FilesMsg::ListVolumes { .. } => {
            // Answered with the announcement itself, which carries no request
            // id — the list is idempotent, so the asker takes the next one.
            Some(FilesMsg::Volumes(
                server
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .volumes(),
            ))
        }
        FilesMsg::Close { handle } => {
            // Released after the lock, not under it: the last reference going
            // is the actual `close`, which can wait on the disk — an antivirus
            // scanning a file just written, a filesystem flushing on close.
            let closed = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .close(handle);
            drop(closed);
            None // deliberately unanswered; see the protocol docs
        }
        // Replies and announcements are not requests: they are handled inline
        // by `handle_message`, which never routes them here.
        _ => None,
    }
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
    pub(super) fn serve(&mut self, msg: FilesMsg) {
        let server = std::sync::Arc::clone(&self.server);
        let done = self.done_tx.clone();
        // Counted here, where the peer's requests arrive. The other counters
        // watch what *this* machine asks for, which is the wrong half when the
        // peer is the one writing: Steam downloading onto our disk produced a
        // "Device Error" that left nothing at all in our log, because every
        // request involved was inbound and nothing was watching this side.
        let kind = request_kind(&msg);
        *self.served.entry(kind).or_insert(0) += 1;
        // Sized from the request, before it goes to a worker: a write carries
        // its bytes with it, and a read states how many it wants. A read that
        // returns short overstates by the difference, which is the cheaper
        // error than reporting nothing at all — the alternative is threading a
        // count back from the worker for a number only a log line reads.
        match &msg {
            FilesMsg::Write { bytes, .. } => {
                self.health.served_written_bytes += bytes.wire_len() as u64;
            }
            FilesMsg::Read { len, .. } => {
                self.health.served_read_bytes += *len as u64;
            }
            _ => {}
        }
        let slow = self.slow_served.clone();
        // Counted before the worker starts and cleared when it finishes, so the
        // module reads as busy for exactly as long as it is. The guard clears
        // it on unwind too: `run_request` takes a lock it expects to hold, and
        // one panicking job that left the count raised would keep the session
        // loop at its fast pace for the rest of the link's life.
        let busy = InFlight::raise(&self.in_progress);
        self.workers.spawn(move || {
            let started = Instant::now();
            let req = request_req(&msg);
            // A request that panicked is still owed an answer. Without one the
            // peer's entry for it never clears, and those entries are what its
            // window is counted from: enough of them and it sends nothing more
            // until the link is torn down.
            let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_request(&server, msg)
            })) {
                Ok(reply) => reply,
                Err(panic) => {
                    let why = cursio_files::background::panic_message(&*panic).to_string();
                    tracing::warn!(kind, why, "a request from the peer panicked");
                    req.map(|req| FilesMsg::Failed {
                        req,
                        error: FileError::Io(format!("the request failed on the peer: {why}")),
                    })
                }
            };
            drop(busy);
            let took = started.elapsed();
            // A request the peer is blocked on, taking long enough that its
            // caller may already have given up: the mount's own deadline is
            // `OP_TIMEOUT`, and an application sees a device error rather than
            // a slow disk. Worth naming individually, not just counting.
            if took >= SLOW_SERVE_WARN {
                slow.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    kind,
                    took_ms = took.as_millis() as u64,
                    "a request from the peer took long enough that its caller may have given up"
                );
            }
            if let Some(reply) = reply {
                // The receiver only closes when the session is ending, in which
                // case the answer is moot.
                let _ = done.send(reply);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker that panics must not leave the count raised.
    ///
    /// `run_request` takes a lock it expects to hold, so a panic is reachable,
    /// and the pool's worker thread dies with it. A count stranded above zero
    /// would keep the session loop at its fast pace for the rest of the link.
    #[test]
    fn a_panicking_worker_still_clears_its_count() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let for_job = std::sync::Arc::clone(&count);
        let panicked = std::panic::catch_unwind(move || {
            let _busy = InFlight::raise(&for_job);
            panic!("the disk went away");
        });
        assert!(panicked.is_err(), "the job panicked");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "unwinding clears the count"
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
}
