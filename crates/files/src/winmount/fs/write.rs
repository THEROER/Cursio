//! Writing through the drive: each write sent at once and its answer
//! collected a window later, so the link carries several at a time.

use crate::winmount::convert::to_ntstatus;
use crate::winmount::fs::info::fill_info;
use crate::winmount::fs::FileCtx;
use crate::winmount::fs::RemoteFs;
use winfsp::filesystem::FileInfo;

impl RemoteFs {
    /// [`FileSystemContext::write`](winfsp::filesystem::FileSystemContext::write): the bytes sent, pipelined behind the
    /// writes already in flight on this handle.
    pub(super) fn write_pipelined(
        &self,
        context: &FileCtx,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let _call = crate::inflight::shared().enter("write");
        let lease = self.remote(context).map_err(|e| to_ntstatus(&e))?;
        let remote = lease.handle;
        // Pipelined: this write is sent now and its answer collected later,
        // while the answer being waited on here belongs to a write already a
        // window behind. Nothing is acknowledged before the peer has stored it
        // — what changes is only which write the caller waits out, so the
        // link carries several at once instead of one per round trip. See
        // `crate::writeback`.
        let mut pipeline = context.writes.lock().expect("handle writes");
        if let Some(e) = pipeline.take_failure() {
            // An earlier write in this pipeline failed. Report it here rather
            // than letting the application believe its file is complete.
            pipeline.drain(
                |rx, limit| self.peer.await_write(rx, limit),
                crate::bridge::BULK_OP_TIMEOUT,
            );
            tracing::warn!(
                path = %context.path,
                offset,
                error = ?e,
                "a write earlier in this file failed"
            );
            return Err(to_ntstatus(&e));
        }

        // Placed while the pipeline is held, so two appends on one handle
        // cannot both be told the same end of file.
        let (offset, len) = crate::filesize::place_write(
            context.size.get(),
            offset,
            buffer.len(),
            write_to_eof,
            constrained_io,
        );
        if len == 0 {
            drop(pipeline);
            let meta = self.described_after_write(context, context.size.get());
            fill_info(file_info, &meta);
            return Ok(0);
        }
        let buffer = &buffer[..len];

        // A slot on the link, or an answer collected to free one. Collecting is
        // the work this thread would be waiting to do in any case, and it keeps
        // the queue on the link bounded by what the link carries rather than by
        // how many files the application has open.
        if !pipeline.reserve() {
            match pipeline.collect_one(
                |rx, limit| self.peer.await_write(rx, limit),
                crate::bridge::BULK_OP_TIMEOUT,
            ) {
                Some(Err(e)) => {
                    tracing::warn!(
                        path = %context.path,
                        offset,
                        error = ?e,
                        "a write to the peer failed"
                    );
                    return Err(to_ntstatus(&e));
                }
                // The slot that answer held is free now. If another handle
                // took it first this write still goes out — one over the
                // budget is a rounding error next to stalling the thread.
                _ => drop(pipeline.reserve()),
            }
        }

        let rx = self
            .peer
            .write_later(remote, offset, buffer.to_vec())
            .map_err(|e| {
                // Named with the path and offset Steam quotes back — its
                // "writing chunk, offset N" names the same write, so the two
                // logs can be lined up against each other.
                tracing::warn!(
                    path = %context.path,
                    offset,
                    len = buffer.len(),
                    error = ?e,
                    "a write to the peer failed"
                );
                to_ntstatus(&e)
            })?;
        pipeline.push(rx, buffer.len() as u64);

        // Report this write's own length as accepted. It is not an
        // acknowledgement: the bytes are still outstanding, and any failure
        // reaches the application at a later write or at `flush`/`cleanup`,
        // before the file can be closed as complete — which is the guarantee
        // `rclone`'s pipeline does not give on Windows, where WinFsp drops an
        // error returned from `flush`.
        let written = buffer.len() as u64;
        let size = context.size.wrote(offset, written);
        drop(pipeline);
        // Windows expects the file's state after the write, and takes what it
        // is given: a `FileInfo` left untouched is whatever the caller's memory
        // held — a size and timestamps invented from nothing.
        //
        // Computed rather than fetched. The peer was just told to put `written`
        // bytes at `offset`, so the file is at least that long and its contents
        // changed now; asking for that back is a round trip spent learning what
        // this call already decided. It is not a small saving: Windows follows
        // almost every write with a `get_file_info`, and Steam's unpacking
        // measured 3.3 stats per write — the metadata traffic outnumbered the
        // data more than three to one, and a link doing 41 MB/s on a plain
        // sequential write served Steam at 5.6.
        //
        // The size is the handle's own figure, raised by this write and never
        // lowered by it. It was once invented from the write itself —
        // `offset + len` whenever the cache had expired — which told Windows a
        // preallocated file had shrunk to the end of whatever chunk was being
        // written. See `crate::filesize`.
        //
        // The cached entry is updated to match instead of being dropped, so the
        // `get_file_info` that follows is answered from memory rather than
        // starting the round trip again.
        let meta = self.described_after_write(context, size);
        context.ahead.invalidate();
        fill_info(file_info, &meta);
        Ok(written as u32)
    }
}
