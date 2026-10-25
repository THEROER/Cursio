//! The WinFsp filesystem itself.
//!
//! WinFsp calls these methods from any thread and hands out `&self` only, so
//! everything mutable sits behind a lock — the same constraint FUSE imposes on
//! the Linux side, for the same reason.

use std::sync::Mutex;

use cursio_protocol::{FileError, FileKind, FileMeta, OpenMode, Volume};
use widestring::U16CStr;
use winfsp::filesystem::{
    DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
};

use super::convert::{attributes_of, to_ntstatus};
use crate::bridge::PeerLink;
use crate::winmount::fs::info::fill_info;
use crate::winmount::fs::info::fill_info_at;
use crate::winmount::fs::info::set_normalized_name;
use crate::winmount::fs::info::synthetic_dir;

mod handle;
mod info;
mod listing;
mod pattern;
mod write;

pub use handle::{mount, MountHandle};

/// Sector and cluster geometry reported to Windows.
///
/// Windows insists on a geometry even for a filesystem that has none. 4 KiB is
/// what NTFS reports by default, so nothing downstream is surprised by it.
const SECTOR_SIZE: u16 = 4096;
const SECTORS_PER_CLUSTER: u16 = 1;

/// An open file, as WinFsp sees it.
pub struct FileCtx {
    /// The peer's handle, held only while this file's data is in use.
    ///
    /// Every open of the peer's file is a descriptor on the peer, and Windows
    /// opens far more than it reads: a `stat` is an open, a query and a close,
    /// so `git status`, Explorer, the indexer and Defender each opened every
    /// file they looked at. And the close that would release it is `Close`,
    /// which Windows sends when the last reference to the file object goes —
    /// arbitrarily later than the application's own `CloseHandle`, which
    /// arrives as `cleanup`. Holding a peer handle from `open` to `close` let
    /// one walk of a shared tree take the peer to its descriptor limit, after
    /// which every file request failed with EMFILE and the drive went away.
    ///
    /// So a handle is taken only when data is wanted — at `open` if the caller
    /// asked for data access, else at the first read or write — and given back
    /// at `cleanup`. Always `None` for a directory: there is nothing on the
    /// far side to hold open.
    remote: Mutex<Option<cursio_protocol::FileHandle>>,
    /// Set at `cleanup`: from then on nothing may hold a handle here.
    ///
    /// A file mapped into memory keeps being read after its last handle is
    /// closed — a page fault is a read — and `close` does not follow until the
    /// memory manager drops the mapping's section, which it keeps cached long
    /// after the process that mapped it has exited. Git maps every file it
    /// hashes, so re-taking a held handle for those reads left one `git status`
    /// holding 303 descriptors on the peer indefinitely. After cleanup each
    /// read or write opens the peer's file for itself and closes it again.
    cleaned: std::sync::atomic::AtomicBool,
    /// What a handle for this file is opened with, create flags removed: the
    /// file already exists by the time one is taken lazily.
    mode: OpenMode,
    volume: String,
    path: String,
    is_dir: bool,
    /// The file's length as this handle can vouch for it; see
    /// [`crate::filesize`] for why the peer's latest answer is not enough.
    size: crate::filesize::KnownSize,
    /// Bytes fetched ahead of what was asked for, while this handle is read
    /// sequentially. Per handle, so it dies with the file it belongs to.
    ahead: crate::readahead::ReadAhead,
    /// Writes sent against this handle and not yet accounted for.
    ///
    /// Windows hands cached writes to a filesystem from one thread, so without
    /// this a download runs at one write per round trip however much bandwidth
    /// is free. See [`crate::writeback`].
    writes: Mutex<crate::writeback::WriteBack>,
    /// The process that opened this handle, or 0 when WinFsp could not say.
    ///
    /// Captured at open because that is the only time WinFsp carries it: the
    /// caller's PID rides the access token of a create request, and a
    /// directory listing is a request with no token at all.
    opener: u32,
}

/// The peer's volumes, presented as a Windows drive.
pub struct RemoteFs {
    peer: PeerLink,
    /// Writes in flight across every handle on this drive — see
    /// [`crate::writeback::InFlightBudget`] for why it is not per handle.
    write_budget: crate::writeback::InFlightBudget,
    volumes: crate::metacache::VolumeList,
    /// What the drive calls itself in Explorer; also answered to anything
    /// asking the volume for its label.
    label: String,
    /// The peer volume this drive letter *is*.
    ///
    /// One letter per volume, because on Windows a peer's disk has the same
    /// nature as a local one: its own root, its own capacity, its own letter.
    /// Presenting several as folders under one letter — which is what the Linux
    /// mount does, since it has no letters to give — meant the drive could only
    /// report one capacity for all of them, so it reported the sum: a peer with
    /// five disks showed as a single 2 TB drive that matched none of them.
    bound: String,
    /// What the peer last said about each path, briefly remembered.
    ///
    /// Windows asks about a path far more often than an application does: an
    /// open is preceded by a query, a write is followed by one, and a listing
    /// is followed by a query per entry. Each was a round trip. Measured
    /// against a peer 9 ms away, Steam's preallocation pass spent about 150 ms
    /// on each file it created — seventeen round trips for a file that did not
    /// exist yet — and 4896 files of a game took longer than Steam was willing
    /// to wait.
    meta: crate::metacache::MetaCache,
    /// Whose disk this is, for naming it when something walks it.
    peer_name: String,
}

impl RemoteFs {
    pub fn new(peer: PeerLink, label: impl Into<String>, bound: impl Into<String>) -> Self {
        Self {
            peer,
            volumes: crate::metacache::VolumeList::default(),
            label: label.into(),
            bound: bound.into(),
            meta: crate::metacache::MetaCache::new(),
            write_budget: crate::writeback::InFlightBudget::new(),
            peer_name: String::new(),
        }
    }

    /// Name the peer this volume belongs to.
    pub fn of_peer(mut self, peer_name: &str) -> Self {
        self.peer_name = peer_name.to_string();
        self
    }

    /// The process behind the create or open being served, or 0.
    fn caller(&self) -> u32 {
        // SAFETY: called only from `open` and `create`, synchronous operations
        // during which WinFsp has set this thread's operation context, and
        // whose request is a create request — so reading the union as one is
        // reading what is there. The PID is the high half of the token field,
        // as `FSP_FSCTL_TRANSACT_REQ_TOKEN_PID` defines it.
        unsafe {
            self.with_operation_request(|request| (request.Req.Create.AccessToken >> 32) as u32)
        }
        .unwrap_or(0)
    }

    /// The peer's volumes, refetched when the cached list has expired.
    fn volumes(&self) -> Result<Vec<Volume>, FileError> {
        self.volumes.get(|| self.peer.volumes())
    }

    /// Resolve a Windows path against the volume this drive is bound to.
    ///
    /// The path names nothing but a location *inside* that volume: the letter
    /// already said which one. Every path, `\` included, belongs to the bound
    /// volume, and `\` is that volume's own root.
    fn resolve(&self, file_name: &U16CStr) -> (String, String) {
        let win_path = file_name.to_string_lossy();
        let path = win_path.trim_start_matches('\\').replace('\\', "/");
        (self.bound.clone(), path)
    }

    /// Metadata for a path, answered from the cache when it was asked for
    /// recently. The volume's root is a synthetic directory: it exists here,
    /// not as an entry the peer could stat.
    ///
    /// Both outcomes are remembered. A `NotFound` is as much an answer as a
    /// `FileMeta`, and an installer asks about thousands of paths that do not
    /// exist yet — going back to the peer to be told the same thing again is a
    /// round trip spent on a fact already in hand.
    fn stat_path(&self, volume: &str, path: &str) -> Result<FileMeta, FileError> {
        if path.is_empty() {
            return Ok(synthetic_dir(volume));
        }
        self.meta
            .get_or_ask(volume, path, || self.peer.stat(volume, path))
    }

    /// What a file looks like once a write to it has been sent, remembered so
    /// the query Windows follows almost every write with needs no round trip.
    fn described_after_write(&self, context: &FileCtx, size: u64) -> FileMeta {
        let meta = FileMeta {
            name: context
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&context.path)
                .to_string(),
            kind: FileKind::File,
            size,
            modified_unix: crate::now_unix(),
            writable: true,
            allocated: None,
        };
        self.meta
            .put(&context.volume, &context.path, Ok(meta.clone()));
        meta
    }

    /// Wait out every write still in flight on this handle, returning the
    /// first failure among them.
    ///
    /// Anything that must see the file as the application left it — a flush,
    /// a resize, an overwrite — goes through here first, so a pipelined write
    /// can neither be reported as stored before it is nor land after a
    /// request that was meant to follow it.
    fn settle_writes(&self, context: &FileCtx, before: &'static str) -> Result<(), FileError> {
        let mut pipeline = context.writes.lock().unwrap_or_else(|p| p.into_inner());
        pipeline.drain(
            |rx, limit| self.peer.await_write(rx, limit),
            crate::bridge::BULK_OP_TIMEOUT,
        );
        match pipeline.take_failure() {
            Some(e) => {
                tracing::warn!(path = %context.path, error = ?e, before, "a write failed");
                Err(e)
            }
            None => Ok(()),
        }
    }

    /// The peer's handle for one operation on this file.
    ///
    /// Before `cleanup` it is the handle the context holds, taken now if it
    /// is not held yet. After, it is opened for this operation alone and
    /// closed when the lease drops — see [`FileCtx::cleaned`].
    fn remote(&self, context: &FileCtx) -> Result<Lease<'_>, FileError> {
        if context.is_dir {
            return Err(FileError::WrongKind);
        }
        // Read under the lock `release_remote` takes after setting it: a
        // handle stored here is then either taken by that release or never
        // stored at all.
        let mut held = context.remote.lock().unwrap_or_else(|p| p.into_inner());
        let transient = context.cleaned.load(std::sync::atomic::Ordering::SeqCst);
        if let (Some(handle), false) = (*held, transient) {
            return Ok(Lease {
                fs: self,
                handle,
                transient,
            });
        }
        let (handle, _) = self
            .peer
            .open(&context.volume, &context.path, context.mode)?;
        if !transient {
            *held = Some(handle);
        }
        Ok(Lease {
            fs: self,
            handle,
            transient,
        })
    }

    /// Run `op` against the peer's handle, taking a fresh one once if the one
    /// it had was released under it.
    ///
    /// A paging read can arrive while `cleanup` gives the handle back — it was
    /// read out of the context just before. Failing it is not an option: for a
    /// mapped executable that read is a page fault, and an error there kills
    /// the process that faulted.
    fn with_remote<T>(
        &self,
        context: &FileCtx,
        op: impl Fn(cursio_protocol::FileHandle) -> Result<T, FileError>,
    ) -> Result<T, FileError> {
        let lease = self.remote(context)?;
        match op(lease.handle) {
            Err(FileError::BadHandle) if !lease.transient => {
                {
                    let mut held = context.remote.lock().unwrap_or_else(|p| p.into_inner());
                    if *held == Some(lease.handle) {
                        *held = None;
                    }
                }
                drop(lease);
                op(self.remote(context)?.handle)
            }
            other => other,
        }
    }

    /// Give the peer's handle back, once every write sent through it has
    /// landed.
    fn release_remote(&self, context: &FileCtx) {
        if context.is_dir {
            return;
        }
        // A failure here has nowhere to go — neither `cleanup` nor `close`
        // returns anything — but `settle_writes` logs it, and `flush` and the
        // next `write` both report it while the application can still act.
        let _ = self.settle_writes(context, "release");
        let taken = context
            .remote
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(handle) = taken {
            self.peer.close(handle);
        }
    }
}

/// The peer's handle, lent for one operation.
///
/// A transient one was opened for that operation alone and is closed when the
/// lease drops. The close goes out after the operation's own request on the
/// same link, and the peer finishes the writes a close follows before closing
/// (`files_session`), so a pipelined write through it still lands.
struct Lease<'a> {
    fs: &'a RemoteFs,
    handle: cursio_protocol::FileHandle,
    transient: bool,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.transient {
            self.fs.peer.close(self.handle);
        }
    }
}

impl FileSystemContext for RemoteFs {
    type FileContext = FileCtx;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [std::ffi::c_void]>,
        _resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let _call = crate::inflight::shared().enter("get_security_by_name");
        let (volume, path) = self.resolve(file_name);
        let meta = self
            .stat_path(&volume, &path)
            .map_err(|e| to_ntstatus(&e))?;
        Ok(FileSecurity {
            reparse: false,
            // No descriptor written: the filesystem does not model ACLs, and
            // WinFsp falls back to the mount's own permissions. Inventing a
            // descriptor from the peer's Unix mode would claim an authority
            // that does not exist on this machine.
            sz_security_descriptor: 0,
            attributes: attributes_of(&meta),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let _call = crate::inflight::shared().enter("open");
        let (volume, path) = self.resolve(file_name);

        let meta = self
            .stat_path(&volume, &path)
            .map_err(|e| to_ntstatus(&e))?;
        let is_dir = matches!(meta.kind, FileKind::Dir);

        // Honour the caller's declaration of what it expects to find.
        //
        // Windows states its intent in `create_options`: `FILE_DIRECTORY_FILE`
        // means "this must be a directory", `FILE_NON_DIRECTORY_FILE` means "it
        // must not be". Ignoring the mismatch let a caller open a directory as
        // if it were a file, and the error surfaced later as
        // `EISDIR: illegal operation on a directory, stat 'Z:\etc'` — which is
        // what stopped VS Code and Cursor from opening any folder here while
        // Explorer, cmd and plain Node all worked.
        // Only the "must be a directory" half is enforced.
        //
        // The mirror check — refusing a directory when `FILE_NON_DIRECTORY_FILE`
        // is set — looks symmetrical but breaks `stat`: Node opens a path with
        // that flag purely to read its metadata, so answering
        // `STATUS_FILE_IS_A_DIRECTORY` turns every `statSync` on a folder into
        // `EISDIR`, and VS Code refuses to open any folder on the drive.
        // Windows tolerates the open and sorts the distinction out itself.
        const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
        if !is_dir && create_options & FILE_DIRECTORY_FILE != 0 {
            return Err(to_ntstatus(&FileError::WrongKind));
        }

        fill_info_at(file_info.as_mut(), &meta, &volume, &path);
        set_normalized_name(file_info, file_name);

        // A directory has nothing to open on the far side; only files do, and
        // only when their data is wanted — see [`FileCtx::remote`]. Taken here
        // when it is, so an open the peer refuses (no permission, a read-only
        // volume asked for writing) fails at `CreateFile`, where the
        // application looks for that answer.
        let mode = access_to_mode(granted_access);
        let (remote, size) = match !is_dir && wants_data(granted_access) {
            true => {
                let (handle, size) = self
                    .peer
                    .open(&volume, &path, mode)
                    .map_err(|e| to_ntstatus(&e))?;
                (Some(handle), size)
            }
            false => (None, meta.size),
        };

        Ok(FileCtx {
            remote: Mutex::new(remote),
            mode,
            volume,
            path,
            is_dir,
            size: crate::filesize::KnownSize::new(size),
            ahead: crate::readahead::ReadAhead::new(),
            writes: Mutex::new(crate::writeback::WriteBack::new(self.write_budget.clone())),
            opener: self.caller(),
            cleaned: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn close(&self, context: Self::FileContext) {
        let _call = crate::inflight::shared().enter("close");
        // Outstanding writes first: the close is what tells the peer the file
        // is done, and it must not overtake bytes still on their way to it.
        // A failure here has nowhere to be reported — `close` returns nothing —
        // but it is logged, and `flush` and the next `write` both report it
        // while the application can still act on it.
        if let Ok(mut pipeline) = context.writes.lock() {
            pipeline.drain(
                |rx, limit| self.peer.await_write(rx, limit),
                crate::bridge::BULK_OP_TIMEOUT,
            );
            if let Some(e) = pipeline.take_failure() {
                tracing::warn!(path = %context.path, error = ?e, "a write failed before close");
            }
        }
        self.release_remote(&context);
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        let _call = crate::inflight::shared().enter("read");
        // Through the read-ahead buffer: Windows asks in whatever size the
        // application chose, often 4 KiB, and each of those was a round trip.
        // One fetch now serves every read that lands inside it.
        let bytes = context
            .ahead
            .read(offset, buffer.len(), |at, len| {
                self.with_remote(context, |remote| self.peer.read(remote, at, len))
            })
            .map_err(|e| to_ntstatus(&e))?;
        let n = bytes.len().min(buffer.len());
        buffer[..n].copy_from_slice(&bytes[..n]);
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        self.write_pipelined(
            context,
            buffer,
            offset,
            write_to_eof,
            constrained_io,
            file_info,
        )
    }

    /// Report the volume's size and label.
    ///
    /// Without this the trait's default answers `STATUS_INVALID_DEVICE_REQUEST`,
    /// and that single refusal is what broke `stat` for every Node-based tool.
    /// Node asks for volume information to fill `st_dev` while stat-ing *any*
    /// path; libuv turns the refusal into `EISDIR: illegal operation on a
    /// directory`, so VS Code and Cursor could not open a folder here while
    /// Explorer, cmd and .NET — none of which make that call — worked fine.
    /// The same refusal is what `fsutil` reported as "Incorrect function".
    ///
    /// The figures are those of the one peer volume this letter is bound to.
    ///
    /// This used to sum every volume the peer offered, because the mount was a
    /// single letter standing for all of them — so a machine with five disks
    /// reported one drive of their combined size, a figure matching no disk the
    /// user has. One letter per volume removes the question: the drive reports
    /// what its own disk holds.
    ///
    /// The fallback is deliberately not zero. A volume answering zero total
    /// makes "percent used" a division by zero in callers that compute it, and
    /// zero free makes file managers refuse to write; see `FALLBACK_TOTAL` in
    /// [`crate::server`], which the FUSE half answers with for the same reason.
    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let _call = crate::inflight::shared().enter("get_volume_info");
        let (total, free) = match self.volumes() {
            Ok(volumes) => volumes
                .iter()
                .find(|v| v.id == self.bound)
                .map(|v| (v.total_bytes, v.free_bytes))
                // Bound to a volume the peer no longer lists: it was unplugged,
                // or the link has not answered yet. Either way a plausible
                // figure keeps Explorer usable until the mount goes away.
                .unwrap_or((crate::server::FALLBACK_TOTAL, crate::server::FALLBACK_FREE)),
            // The peer cannot be reached.
            _ => (crate::server::FALLBACK_TOTAL, crate::server::FALLBACK_FREE),
        };
        out_volume_info.total_size = total;
        out_volume_info.free_size = free;
        out_volume_info.set_volume_label(&self.label);
        Ok(())
    }

    /// Wait for every write still in flight against this handle.
    ///
    /// This is where a pipelined write stops being outstanding. An application
    /// that flushes is asking whether its bytes are safe, and the honest answer
    /// needs the peer's — so the window is emptied here and any failure it
    /// collected is returned. Past that the peer's own filesystem owns the
    /// durability guarantee.
    ///
    /// Implemented at all because the default refuses with
    /// `STATUS_INVALID_DEVICE_REQUEST`, which Windows surfaces as "function not
    /// supported" — and rustc calls `FlushFileBuffers` after writing every
    /// object file, so a cargo build on the drive failed with
    /// `could not write output to ...build_script_build-....o`. A null context
    /// is a volume-wide flush, which has no handle to wait on.
    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let _call = crate::inflight::shared().enter("flush");
        // A flush with no context is a volume-wide flush; there is no handle
        // whose writes could be waited on.
        let Some(context) = context else {
            return Ok(());
        };
        self.settle_writes(context, "flush")
            .map_err(|e| to_ntstatus(&e))
    }

    /// Truncate an existing file that is being opened for overwrite.
    ///
    /// Windows splits "create or replace" in two: `open` resolves the name,
    /// then `overwrite` empties the file. Without this the second half refused
    /// and rewriting any existing file failed, which is most of what a build
    /// does — every rebuilt artefact lands on top of the previous one.
    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let _call = crate::inflight::shared().enter("overwrite");
        let lease = self.remote(context).map_err(|e| to_ntstatus(&e))?;
        let remote = lease.handle;
        self.settle_writes(context, "overwrite")
            .map_err(|e| to_ntstatus(&e))?;
        self.peer.truncate(remote, 0).map_err(|e| to_ntstatus(&e))?;
        self.meta.forget(&context.volume, &context.path);
        context.ahead.invalidate();
        // Reported rather than passed over: see `write` for why an unfilled
        // `FileInfo` is worse than a refusal.
        let mut meta = self
            .stat_path(&context.volume, &context.path)
            .map_err(|e| to_ntstatus(&e))?;
        meta.size = context.size.observe(meta.size, false);
        fill_info(file_info, &meta);
        Ok(())
    }

    /// Accept metadata changes that this filesystem cannot carry.
    ///
    /// The wire has no call for setting timestamps or attributes, so nothing is
    /// forwarded. Refusing outright is worse than accepting: cargo stamps the
    /// files it extracts and fails the build when the stamp is rejected, and
    /// Explorer treats a refusal as a failed copy. The current metadata is
    /// reported back, so a caller that reads the values it just set sees what
    /// actually holds rather than what it asked for.
    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let _call = crate::inflight::shared().enter("set_basic_info");
        if let Ok(meta) = self.stat_path(&context.volume, &context.path) {
            fill_info(file_info, &meta);
        }
        Ok(())
    }

    /// Agree that a file may be deleted.
    ///
    /// This only asks whether the delete is allowed — the removal itself
    /// happens in [`cleanup`](Self::cleanup) once the last handle closes, which
    /// is the order Windows expects. Refusing here made `unlink` fail on the
    /// drive, and left cargo unable to clear its temporary directories.
    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        let _call = crate::inflight::shared().enter("set_delete");
        if context.path.is_empty() {
            // The mount root and the volume folders are ours, not the peer's.
            return Err(to_ntstatus(&FileError::Denied));
        }
        // Refusing a doomed delete belongs here, because `cleanup` — where the
        // removal itself happens — returns no status and can only log a failure.
        // A non-empty directory answered "yes" here and then quietly stayed:
        // `rmSync` reported success on a directory that was still there, and
        // Windows, having been promised a delete it did not get, failed the
        // enclosing operation with `ERROR_INVALID_PARAMETER`.
        if delete_file && context.is_dir {
            let listing = self
                .peer
                .read_dir(&context.volume, &context.path)
                .map_err(|e| to_ntstatus(&e))?;
            tracing::debug!(
                path = %context.path,
                remaining = listing.len(),
                names = ?listing.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
                "winmount set_delete(dir)"
            );
            if !listing.is_empty() {
                return Err(to_ntstatus(&FileError::NotEmpty));
            }
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let _call = crate::inflight::shared().enter("get_file_info");
        let mut meta = self
            .stat_path(&context.volume, &context.path)
            .map_err(|e| to_ntstatus(&e))?;
        if !context.is_dir {
            // The peer's answer may predate writes still on their way to it,
            // and reporting it as-is would shrink the file under them.
            let outstanding = context
                .writes
                .lock()
                .map(|w| w.has_outstanding())
                .unwrap_or(true);
            meta.size = context.size.observe(meta.size, outstanding);
        }
        fill_info_at(file_info, &meta, &context.volume, &context.path);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        self.list_into(context, pattern, marker, buffer)
    }

    /// Set a file's length, or reserve the space it will need.
    ///
    /// `set_allocation_size` picks which. The two are genuinely different
    /// requests, and treating both as a truncate is what stalled Steam's
    /// preallocation pass partway through Forza's 4896 files.
    ///
    /// Windows keeps two numbers per file: `FileSize`, the bytes the file
    /// contains, and `AllocationSize`, the room committed for it on the medium
    /// — and requires `FileSize <= AllocationSize` always. An installer
    /// reserves the whole download up front by raising `AllocationSize` alone,
    /// so that a disk filling later cannot leave a half-written game. Passing
    /// that reservation through as a truncate does the opposite of what was
    /// asked: it moves the end of the file to the reserved figure, which is the
    /// very shortening NTFS performs when it sees an allocation smaller than
    /// the file.
    ///
    /// A reservation is forwarded as one, so the owner's filesystem can commit
    /// the space for real (`fallocate` on Linux, which `set_len` — a hole —
    /// does not). The peer's answer is what tells us the space exists.
    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let _call = crate::inflight::shared().enter("set_file_size");
        let lease = self.remote(context).map_err(|e| to_ntstatus(&e))?;
        let remote = lease.handle;
        // Writes still in flight must land before the resize, not after it: a
        // `Truncate` goes to the wire directly while a write is compressed on
        // a worker first, so an earlier write could otherwise arrive later and
        // extend the file again past the length just set.
        self.settle_writes(context, "set_file_size")
            .map_err(|e| to_ntstatus(&e))?;
        self.peer
            .set_size(remote, new_size, set_allocation_size)
            .map_err(|e| to_ntstatus(&e))?;
        self.meta.forget(&context.volume, &context.path);
        context.ahead.invalidate();
        // Nothing is outstanding any more, so the peer's answer is the length —
        // the one place this handle's figure may go down.
        let mut meta = self
            .stat_path(&context.volume, &context.path)
            .map_err(|e| to_ntstatus(&e))?;
        meta.size = context.size.observe(meta.size, false);
        fill_info(file_info, &meta);
        Ok(())
    }

    fn create(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_attributes: u32,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let _call = crate::inflight::shared().enter("create");
        let (volume, path) = self.resolve(file_name);

        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        let want_dir = file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0;

        if want_dir {
            self.peer
                .mkdir(&volume, &path)
                .map_err(|e| to_ntstatus(&e))?;
            // It exists now; a cached `NotFound` from moments ago does not.
            self.meta.forget(&volume, &path);
            let meta = self
                .stat_path(&volume, &path)
                .map_err(|e| to_ntstatus(&e))?;
            fill_info(file_info.as_mut(), &meta);
            set_normalized_name(file_info, file_name);
            return Ok(FileCtx {
                remote: Mutex::new(None),
                mode: access_to_mode(0),
                volume,
                path,
                is_dir: true,
                size: crate::filesize::KnownSize::new(0),
                ahead: crate::readahead::ReadAhead::new(),
                writes: Mutex::new(crate::writeback::WriteBack::new(self.write_budget.clone())),
                opener: self.caller(),
                cleaned: std::sync::atomic::AtomicBool::new(false),
            });
        }

        let mode = OpenMode {
            read: true,
            write: true,
            create: true,
            truncate: false,
            exclusive: false,
        };
        let (remote, size) = self
            .peer
            .open(&volume, &path, mode)
            .map_err(|e| to_ntstatus(&e))?;
        // `create` reaches here, so the path may have just come into existence.
        self.meta.forget(&volume, &path);
        let meta = FileMeta {
            name: path.rsplit('/').next().unwrap_or(&path).to_string(),
            kind: FileKind::File,
            size,
            modified_unix: crate::now_unix(),
            writable: true,
            allocated: None,
        };
        fill_info(file_info.as_mut(), &meta);
        set_normalized_name(file_info, file_name);
        Ok(FileCtx {
            remote: Mutex::new(Some(remote)),
            mode: OpenMode {
                create: false,
                ..mode
            },
            volume,
            path,
            is_dir: false,
            size: crate::filesize::KnownSize::new(size),
            ahead: crate::readahead::ReadAhead::new(),
            writes: Mutex::new(crate::writeback::WriteBack::new(self.write_budget.clone())),
            opener: self.caller(),
            cleaned: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let _call = crate::inflight::shared().enter("rename");
        let (volume, from) = self.resolve(file_name);
        let (new_volume, to) = self.resolve(new_file_name);
        // A move between volumes is a copy plus a delete, which Windows does
        // itself when told the rename cannot span them.
        if volume != new_volume {
            const STATUS_NOT_SAME_DEVICE: i32 = 0xC00000D4u32 as i32;
            return Err(winfsp::FspError::NTSTATUS(STATUS_NOT_SAME_DEVICE));
        }
        let renamed = self
            .peer
            .rename(&volume, &from, &to)
            .map_err(|e| to_ntstatus(&e));
        // Both ends move, and a directory takes everything under it.
        self.meta.forget_dir(&volume, &from);
        self.meta.forget_dir(&volume, &to);
        renamed
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let _call = crate::inflight::shared().enter("cleanup");
        // The application's last handle is gone. Give the peer's back now, not
        // at `close`: see [`FileCtx::remote`]. Marked first, so a read racing
        // this release cannot store a handle after it has been taken.
        context
            .cleaned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.release_remote(context);
        // WinFsp signals a delete through cleanup rather than a separate call.
        const FSP_CLEANUP_DELETE: u32 = 0x01;
        if flags & FSP_CLEANUP_DELETE != 0 {
            let r = self.peer.remove(&context.volume, &context.path);
            tracing::debug!(
                path = %context.path,
                is_dir = context.is_dir,
                result = ?r,
                "winmount cleanup(delete)"
            );
            if let Err(e) = r {
                tracing::warn!(path = %context.path, error = ?e, "cannot delete on the peer");
            }
            // Gone, along with anything that was inside it. A cached answer here
            // would report a deleted file as still present for another second.
            self.meta.forget_dir(&context.volume, &context.path);
        }
    }
}

impl RemoteFs {
    /// Entries of a directory, as the peer lists them.
    fn entries_of(&self, context: &FileCtx) -> Result<Vec<FileMeta>, FileError> {
        // Windows enumerates a directory in batches and calls back for each
        // one, so one pass over a folder asks for the same listing several
        // times. Measured against Steam: 1980 listings in 30 seconds, of
        // directories holding up to 1815 entries — around 11 MB/s of metadata,
        // which is the whole link. The writes sharing it then missed the
        // mount's deadline and Windows reported the drive as failing.
        if let Some(held) = self.meta.listing(&context.volume, &context.path) {
            return Ok(held);
        }
        let entries = self.peer.read_dir(&context.volume, &context.path)?;
        self.meta
            .put_listing(&context.volume, &context.path, entries.clone());
        // A listing already carries every entry's metadata, and what follows a
        // listing is almost always a query per entry — Explorer does it to fill
        // its columns, and a directory walk does it to decide where to descend.
        // Remembering them here turns that per-entry round trip into none: the
        // batch is free, since the answer has already crossed the wire.
        //
        // This is what a batched `stat` would buy, without a message for it: the
        // owner's `read_dir` stats every entry anyway to build the reply.
        for meta in &entries {
            let child = match context.path.is_empty() {
                true => meta.name.clone(),
                false => format!("{}/{}", context.path, meta.name),
            };
            self.meta.put(&context.volume, &child, Ok(meta.clone()));
        }
        Ok(entries)
    }
}

/// Whether an open was granted access to the file's contents, rather than only
/// to its name, attributes or security.
///
/// A `stat` — `GetFileAttributesEx`, Node's `fs.stat`, git's index refresh —
/// opens with `FILE_READ_ATTRIBUTES` alone, and that is most of what arrives
/// here; none of it needs the peer's file open.
fn wants_data(access: u32) -> bool {
    const FILE_READ_DATA: u32 = 0x0001;
    const FILE_WRITE_DATA: u32 = 0x0002;
    const FILE_APPEND_DATA: u32 = 0x0004;
    const FILE_EXECUTE: u32 = 0x0020;
    access & (FILE_READ_DATA | FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_EXECUTE) != 0
}

/// Translate Windows' granted access into the peer's open mode.
///
/// A read that arrives without write access must not open the file writable: on
/// a read-only volume that would fail an open which should have succeeded.
fn access_to_mode(access: u32) -> OpenMode {
    const FILE_WRITE_DATA: u32 = 0x0002;
    const FILE_APPEND_DATA: u32 = 0x0004;
    OpenMode {
        read: true,
        write: access & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0,
        create: false,
        truncate: false,
        exclusive: false,
    }
}

#[cfg(test)]
mod tests {
    use super::wants_data;

    #[test]
    fn a_stat_open_does_not_take_a_handle_on_the_peer() {
        const FILE_READ_ATTRIBUTES: u32 = 0x0080;
        const READ_CONTROL: u32 = 0x0002_0000;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const DELETE: u32 = 0x0001_0000;
        assert!(!wants_data(FILE_READ_ATTRIBUTES | SYNCHRONIZE));
        assert!(!wants_data(READ_CONTROL | FILE_READ_ATTRIBUTES));
        assert!(!wants_data(DELETE | SYNCHRONIZE));
    }

    #[test]
    fn reading_writing_or_running_a_file_takes_a_handle_at_open() {
        const FILE_GENERIC_READ: u32 = 0x0012_0089;
        const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
        const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;
        assert!(wants_data(FILE_GENERIC_READ));
        assert!(wants_data(FILE_GENERIC_WRITE));
        assert!(wants_data(FILE_GENERIC_EXECUTE));
    }
}
