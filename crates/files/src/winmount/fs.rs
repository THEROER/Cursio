//! The WinFsp filesystem itself.
//!
//! WinFsp calls these methods from any thread and hands out `&self` only, so
//! everything mutable sits behind a lock — the same constraint FUSE imposes on
//! the Linux side, for the same reason.

use std::sync::Mutex;

use cursio_protocol::{FileError, FileKind, FileMeta, OpenMode, Volume};
use widestring::U16CStr;
use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};

use super::convert::{attributes_of, dir_name_of, to_filetime, to_ntstatus};
use crate::bridge::PeerLink;

/// How long the volume list is trusted before being fetched again. Drives do
/// not come and go the way file contents do, and this is consulted on every
/// path resolution.
const VOLUMES_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Sector and cluster geometry reported to Windows.
///
/// Windows insists on a geometry even for a filesystem that has none. 4 KiB is
/// what NTFS reports by default, so nothing downstream is surprised by it.
const SECTOR_SIZE: u16 = 4096;
const SECTORS_PER_CLUSTER: u16 = 1;

/// An open file, as WinFsp sees it.
pub struct FileCtx {
    /// The peer's handle, absent for a directory — WinFsp opens those too, but
    /// there is nothing on the far side to hold open.
    remote: Option<cursio_protocol::FileHandle>,
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
}

/// The peer's volumes, presented as a Windows drive.
pub struct RemoteFs {
    peer: PeerLink,
    /// Writes in flight across every handle on this drive — see
    /// [`crate::writeback::InFlightBudget`] for why it is not per handle.
    write_budget: crate::writeback::InFlightBudget,
    volumes: Mutex<(Vec<Volume>, Option<std::time::Instant>)>,
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
}

impl RemoteFs {
    pub fn new(peer: PeerLink, label: impl Into<String>, bound: impl Into<String>) -> Self {
        Self {
            peer,
            volumes: Mutex::new((Vec::new(), None)),
            label: label.into(),
            bound: bound.into(),
            meta: crate::metacache::MetaCache::new(),
            write_budget: crate::writeback::InFlightBudget::new(),
        }
    }

    /// The peer's volumes, refetched when the cached list has expired.
    fn volumes(&self) -> Result<Vec<Volume>, FileError> {
        {
            let guard = self.volumes.lock().expect("volume cache");
            if let (list, Some(at)) = &*guard {
                if at.elapsed() < VOLUMES_TTL && !list.is_empty() {
                    return Ok(list.clone());
                }
            }
        }
        // Fetched without the lock: this is a network round trip, and holding
        // the lock across it would serialise every other call behind it.
        let fetched = self.peer.volumes()?;
        *self.volumes.lock().expect("volume cache") =
            (fetched.clone(), Some(std::time::Instant::now()));
        Ok(fetched)
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
        if let Some(hit) = self.meta.get(volume, path) {
            return hit;
        }
        let fresh = self.peer.stat(volume, path);
        self.meta.put(volume, path, fresh.clone());
        fresh
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
            modified_unix: now_unix(),
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
}

/// A directory that exists only in this presentation: the mount root, or a
/// volume's own folder.
fn synthetic_dir(name: &str) -> FileMeta {
    FileMeta {
        name: name.to_string(),
        kind: FileKind::Dir,
        size: 0,
        modified_unix: 0,
        writable: true,
        allocated: None,
    }
}

/// Write a directory entry's name, giving its length without the terminator.
///
/// `DirInfo::set_name` appends a NUL and then reports the length *including*
/// it, while WinFsp takes that length as the name itself — so every entry went
/// out two bytes long, `a.o` described as `a.o\0`. A plain listing tolerated the
/// stray character, but a filtered one did not: the kernel matched the padded
/// name against the caller's pattern, `<.O` failed to match `a.o\0`, and a
/// search for `*.o` came back empty in a directory that held exactly that file.
/// That empty answer is what cargo reported as `ERROR_INVALID_NAME` while
/// clearing its `.temp-archive` directories.
fn set_entry_name(info: &mut DirInfo, name: &str) -> winfsp::Result<()> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    info.set_name_raw(wide.as_slice())
        .map_err(|_| to_ntstatus(&FileError::Io("name too long".into())))
}

/// Match a name against a Windows enumeration pattern.
///
/// `*` stands for any run of characters, `?` for exactly one, and the match is
/// case-insensitive because the volume declares itself so. Backtracking is
/// iterative rather than recursive: a pattern of many `*` against a long name
/// would otherwise recurse once per position.
///
/// The pattern does not arrive as the user typed it. NT rewrites it into the
/// DOS wildcards its own matcher speaks, upper-cased — `*.o` reaches this
/// function as `<.O`. Those three characters are the reason a build on the
/// drive failed: `<` was compared literally, matched nothing, and cargo took
/// the empty listing for `ERROR_INVALID_NAME`.
///
/// - `<` (`DOS_STAR`) — like `*`, except it will not consume the final `.`
///   of the name; treating it as a plain `*` is close enough here, since the
///   difference only shows on names ending in a dot, which the peer does not
///   produce.
/// - `>` (`DOS_QM`) — like `?`, but also matches nothing at the end of a name.
/// - `"` (`DOS_DOT`) — matches a literal `.`, or nothing at the end of a name.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    let pattern: String = pattern
        .chars()
        .map(|c| match c {
            '<' => '*',
            '>' => '?',
            '"' => '.',
            other => other,
        })
        .collect();
    let pat: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let txt: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();

    let (mut p, mut t) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have consumed too little.
    let (mut star, mut retry) = (None, 0usize);

    while t < txt.len() {
        match pat.get(p) {
            Some('*') => {
                star = Some(p);
                retry = t;
                p += 1;
            }
            Some('?') => {
                p += 1;
                t += 1;
            }
            Some(&c) if c == txt[t] => {
                p += 1;
                t += 1;
            }
            // A mismatch is fatal only if no `*` can absorb one more character.
            _ => match star {
                Some(s) => {
                    p = s + 1;
                    retry += 1;
                    t = retry;
                }
                None => return false,
            },
        }
    }
    // Trailing `*`s may still match the empty remainder.
    pat[p..].iter().all(|&c| c == '*')
}

/// Tell Windows the spelling this filesystem actually uses for an opened path.
///
/// The volume declares itself case-insensitive but case-preserving, which is a
/// promise to answer *what the name really is* — not merely to tolerate any
/// casing on the way in. Leaving the normalized name unset broke that promise:
/// Windows then substituted its own upper-cased form, and it reached the peer
/// verbatim. A rename arrived as
/// `TMP/CURSIO-WRITE-TEST/DEPS/LIB.RMETA.TMP` → `.../lib.rmeta` — the source
/// spelled in capitals, the target as typed — and since the peer's filesystem
/// is case-*sensitive*, the source did not exist there and the rename failed
/// with `NotFound`. Cargo builds on the drive died this way: every `.rmeta` is
/// written to a temporary file and renamed into place.
///
/// Opening an existing file on a path already cached in the other casing failed
/// for the same reason.
///
/// The name handed back is the one WinFsp passed in, which is the caller's own
/// spelling and the one the peer stores.
fn set_normalized_name(file_info: &mut OpenFileInfo, file_name: &U16CStr) {
    // Without the terminator: the length is taken from the slice, and a
    // trailing NUL would be counted as part of the name.
    file_info.set_normalized_name(file_name.as_slice(), None);
}

/// A stable file id for a path, in place of the inode number this filesystem
/// does not have.
///
/// Windows enumeration classes that carry a file id — `FileIdBothDirectoryInfo`
/// and its `Restart` and `ExtdDirectory` variants — are refused outright by the
/// kernel when the volume reports id zero for every entry. Rust's
/// `remove_dir_all` walks a directory with exactly those classes, so it failed
/// with `ERROR_INVALID_PARAMETER` before it deleted anything, while Node, which
/// enumerates with `FindFirstFile`, was unaffected. Measured directly: classes
/// 13 and 20 returned error 87, classes 10 and 15 — which carry no id —
/// succeeded on the same directory.
///
/// The id is derived from the path rather than tracked in a table: it must be
/// stable for as long as the name exists, which hashing gives for free, and
/// nothing here addresses a file by id the way FUSE does by inode. A renamed
/// file changes id, which is the same thing that happens when a file moves
/// between volumes on NTFS.
fn file_id_of(volume: &str, path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    volume.hash(&mut h);
    path.hash(&mut h);
    // Zero means "no id", so it cannot be handed out as one.
    h.finish() | 1
}

/// Fill a `FileInfo` from wire metadata.
fn fill_info_at(info: &mut FileInfo, meta: &FileMeta, volume: &str, path: &str) {
    fill_info(info, meta);
    info.index_number = file_id_of(volume, path);
}

/// Fill a `FileInfo` from wire metadata, leaving the file id unset.
fn fill_info(info: &mut FileInfo, meta: &FileMeta) {
    let ft = to_filetime(meta.modified_unix);
    info.file_attributes = attributes_of(meta);
    info.reparse_tag = 0;
    info.file_size = meta.size;
    // Rounded up to a cluster, which is what Windows expects an allocation to
    // be; reporting the exact size makes some tools compute negative slack.
    info.allocation_size = meta.size.div_ceil(SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    info.creation_time = ft;
    info.last_access_time = ft;
    info.last_write_time = ft;
    info.change_time = ft;
    info.index_number = 0;
    info.hard_links = 0;
    info.ea_size = 0;
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

        // A directory has nothing to open on the far side; only files do.
        let (remote, size) = if is_dir {
            (None, 0)
        } else {
            let mode = access_to_mode(granted_access);
            let (handle, size) = self
                .peer
                .open(&volume, &path, mode)
                .map_err(|e| to_ntstatus(&e))?;
            (Some(handle), size)
        };

        Ok(FileCtx {
            remote,
            volume,
            path,
            is_dir,
            size: crate::filesize::KnownSize::new(size),
            ahead: crate::readahead::ReadAhead::new(),
            writes: Mutex::new(crate::writeback::WriteBack::new(self.write_budget.clone())),
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
        if let Some(remote) = context.remote {
            self.peer.close(remote);
        }
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        let _call = crate::inflight::shared().enter("read");
        let Some(remote) = context.remote else {
            return Err(to_ntstatus(&FileError::WrongKind));
        };
        // Through the read-ahead buffer: Windows asks in whatever size the
        // application chose, often 4 KiB, and each of those was a round trip.
        // One fetch now serves every read that lands inside it.
        let bytes = context
            .ahead
            .read(offset, buffer.len(), |at, len| {
                self.peer.read(remote, at, len)
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
        let _call = crate::inflight::shared().enter("write");
        let Some(remote) = context.remote else {
            return Err(to_ntstatus(&FileError::WrongKind));
        };
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
        let Some(remote) = context.remote else {
            return Err(to_ntstatus(&FileError::WrongKind));
        };
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
        let _call = crate::inflight::shared().enter("read_directory");
        if !context.is_dir {
            return Err(to_ntstatus(&FileError::WrongKind));
        }

        // Fill the caller's buffer directly, the way WinFsp's own MEMFS sample
        // does, rather than going through a `DirBuffer`.
        //
        // The buffered route ended enumeration by *not* finding the marker: its
        // continuation is a binary search over records WinFsp sorted itself, and
        // once the marker reached the last name the search kept resolving to
        // that same record. Asked for what follows `var`, it answered `var`,
        // forever — 2.5 million entries in a directory of 24.
        //
        // Writing straight into `buffer` removes the search entirely: we skip
        // past the marker ourselves and append until the buffer is full, which
        // is exactly what the sample implementation does.
        let mut cursor = 0u32;

        // `.` and `..` come first, and only at the start of an enumeration —
        // the sample emits `.` when there is no marker and `..` when the marker
        // is `.`, then clears the marker so the real entries follow.
        let pattern = pattern.map(|p| p.to_string_lossy());

        let mut skip_until = marker.inner_as_cstr().map(|m| m.to_string_lossy());
        if skip_until.is_none() || skip_until.as_deref() == Some(".") {
            let starting = skip_until.is_none();
            for dot in [".", ".."] {
                if starting || dot == ".." {
                    let mut info: DirInfo = DirInfo::default();
                    let dot_path = if dot == "." {
                        context.path.clone()
                    } else {
                        // The parent of the volume root is the mount root.
                        context
                            .path
                            .rsplit_once('/')
                            .map(|(p, _)| p.to_string())
                            .unwrap_or_default()
                    };
                    fill_info_at(
                        info.file_info_mut(),
                        &synthetic_dir(dot),
                        &context.volume,
                        &dot_path,
                    );
                    set_entry_name(&mut info, dot)?;
                    if !info.append_to_buffer(buffer, &mut cursor) {
                        return Ok(cursor);
                    }
                }
            }
            // Past the dot entries: everything after this is a real name.
            skip_until = None;
        }

        // Windows may ask for a subset. Answering with the whole directory
        // regardless is not merely wasteful — the caller takes the mismatch as a
        // malformed reply, which is how `cargo` clearing a `.temp-archive`
        // directory got `ERROR_INVALID_NAME` on a name that was perfectly valid.
        let entries = self.entries_of(context).map_err(|e| to_ntstatus(&e))?;
        let mut past_marker = skip_until.is_none();
        for meta in entries {
            if let Some(pat) = pattern.as_deref() {
                if !matches_pattern(pat, &meta.name) {
                    continue;
                }
            }
            // Resume *after* the marker, never at it — returning the marker
            // again is what made the listing repeat.
            if !past_marker {
                if Some(meta.name.as_str()) == skip_until.as_deref() {
                    past_marker = true;
                }
                continue;
            }
            let mut info: DirInfo = DirInfo::default();
            let child = if context.path.is_empty() {
                meta.name.clone()
            } else {
                format!("{}/{}", context.path, meta.name)
            };
            fill_info_at(info.file_info_mut(), &meta, &context.volume, &child);
            set_entry_name(&mut info, &meta.name)?;
            if !info.append_to_buffer(buffer, &mut cursor) {
                // Buffer full: what is left comes on the next call.
                return Ok(cursor);
            }
        }

        // Everything fit, so this is the end of the directory. The terminating
        // call is what tells the caller to stop asking.
        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
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
        let Some(remote) = context.remote else {
            return Err(to_ntstatus(&FileError::WrongKind));
        };
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
                remote: None,
                volume,
                path,
                is_dir: true,
                size: crate::filesize::KnownSize::new(0),
                ahead: crate::readahead::ReadAhead::new(),
                writes: Mutex::new(crate::writeback::WriteBack::new(self.write_budget.clone())),
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
            modified_unix: now_unix(),
            writable: true,
            allocated: None,
        };
        fill_info(file_info.as_mut(), &meta);
        set_normalized_name(file_info, file_name);
        Ok(FileCtx {
            remote: Some(remote),
            volume,
            path,
            is_dir: false,
            size: crate::filesize::KnownSize::new(size),
            ahead: crate::readahead::ReadAhead::new(),
            writes: Mutex::new(crate::writeback::WriteBack::new(self.write_budget.clone())),
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

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A drive letter nothing is using, counting down from `Z:`.
///
/// Downwards because the low letters belong to real hardware and to whatever the
/// user maps by habit; a share from another machine belongs at the far end,
/// which is also where Windows puts mapped network drives.
///
/// `GetLogicalDrives` returns a bitmask of the letters in use — one call, no
/// filesystem access, so a wedged mount elsewhere cannot make this hang.
fn free_drive_letter() -> Option<char> {
    // Safe: no arguments, no out-params; returns a bitmask.
    let used = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    if used == 0 {
        // The call failed. `Z:` is the conventional choice and mounting fails
        // loudly if it is taken, which beats giving up here.
        return Some('Z');
    }
    ('D'..='Z')
        .rev()
        .find(|c| used & (1 << (*c as u8 - b'A')) == 0)
}

/// Load `winfsp-x64.dll` from the directory the installer recorded.
///
/// WinFsp deliberately does **not** put its DLL in `System32`: installs are
/// side-by-side, several versions may coexist, and each lives under
/// `…\WinFsp\SxS\sxs.<timestamp>\bin\`. An application is expected to read the
/// location out of the registry and load it itself.
///
/// The crate can do this — but only with its `system` feature, which builds
/// bindings against WinFsp's headers *on the build machine*. These binaries are
/// cross-compiled from Linux, where those headers do not exist, so that route is
/// closed and the lookup is done here instead.
///
/// Delay-loading (see `crates/core/build.rs`) is what makes this work: the
/// import is not resolved until the first call, so loading the DLL by full path
/// first means the loader finds the module already present and binds to it.
/// Without this the process starts fine and then fails at mount time with
/// `WIN32(1285) ERROR_DELAY_LOAD_FAILED`.
fn load_winfsp_dll() -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let dir = winfsp_dll_dir().ok_or_else(|| {
        anyhow::anyhow!("WinFsp does not appear to be installed (no registry entry)")
    })?;
    let dll = dir.join("bin").join("winfsp-x64.dll");
    if !dll.exists() {
        anyhow::bail!("WinFsp is installed but {} is missing", dll.display());
    }

    let wide: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
    // Safe: `wide` is a valid NUL-terminated path that outlives the call. The
    // returned handle is intentionally leaked — the library must stay loaded for
    // the life of the process, since every later WinFsp call binds through it.
    let handle = unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
    if handle.is_null() {
        anyhow::bail!("cannot load {}", dll.display());
    }
    tracing::debug!(dll = %dll.display(), "loaded WinFsp");
    Ok(())
}

/// Where WinFsp says it lives.
///
/// `SxsDir` points at the versioned directory of the current install and is what
/// a modern install sets; `InstallDir` is the older layout. Both are under the
/// 32-bit view of the registry, which is where the (32-bit) installer writes
/// them regardless of the machine's architecture.
fn winfsp_dll_dir() -> Option<std::path::PathBuf> {
    for key in [r"SOFTWARE\WOW6432Node\WinFsp", r"SOFTWARE\WinFsp"] {
        for value in ["SxsDir", "InstallDir"] {
            if let Some(dir) = read_registry_string(key, value) {
                let path = std::path::PathBuf::from(dir);
                if path.join("bin").join("winfsp-x64.dll").exists() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Read one `HKEY_LOCAL_MACHINE` string value.
fn read_registry_string(subkey: &str, value: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    };
    let (subkey_w, value_w) = (wide(subkey), wide(value));

    // Ask for the size first: the value is a path of unknown length.
    let mut len: u32 = 0;
    // Safe: both strings are NUL-terminated and outlive the call; a null data
    // pointer with a valid length pointer is the documented way to size it.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }

    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    // Safe: `buf` is sized from the query above and `len` describes it in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    // Trim the trailing NUL the API includes in the length.
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// A live mount. Dropping it unmounts.
pub struct MountHandle {
    host: FileSystemHost<RemoteFs>,
    mountpoint: String,
}

impl MountHandle {
    pub fn mountpoint(&self) -> &str {
        &self.mountpoint
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        // The drive letter belongs to WinFsp, which drops it with the mount —
        // nothing to release separately.
        self.host.stop();
        self.host.unmount();
    }
}

/// Mount ONE of the peer's volumes on the next free drive letter.
///
/// Called once per volume the peer offers: `volume` is the wire id (`"C:"`,
/// `"/"`, a removable medium's mount path) and `volume_label` is what the drive
/// calls itself in Explorer.
pub fn mount(
    peer: PeerLink,
    peer_name: &str,
    volume: &str,
    volume_label: &str,
) -> anyhow::Result<MountHandle> {
    // Load WinFsp's DLL from where the installer actually put it, before
    // anything asks for a symbol from it.
    load_winfsp_dll()?;

    // WinFsp must be initialised once per process before any filesystem is
    // built; doing it here keeps the requirement with the code that needs it.
    winfsp::winfsp_init()
        .map_err(|e| anyhow::anyhow!("WinFsp is not available (is it installed?): {e:?}"))?;

    // The share this filesystem is published as. `prefix` is what WinFsp wants
    // (one leading backslash); `unc` is the same thing as a path a user or
    // `WNetAddConnection2` would name.
    // One share per volume, so the UNC path stays a working way in when the
    // drive letter is invisible (see the elevation note below). The volume's
    // directory name rather than its raw id: a share name cannot contain a
    // colon, which every Windows volume id ends with.
    let share = dir_name_of(volume);
    let prefix = format!(r"\cursio\{peer_name}-{share}");
    let unc = format!(r"\{prefix}");

    let mut params = VolumeParams::new();
    params
        .sector_size(SECTOR_SIZE)
        .sectors_per_allocation_unit(SECTORS_PER_CLUSTER)
        .filesystem_name(format!("cursio-{peer_name}"))
        // Declare this a NETWORK filesystem, by giving it a UNC prefix.
        //
        // The prefix is what makes WinFsp create the volume through
        // `WinFsp.Net` rather than `WinFsp.Disk`, and that distinction is not
        // cosmetic: as a "disk" the volume was created but no drive letter
        // could ever attach to it — `fsptool lsvol` showed it with a `-` where
        // the drive should be, invisible to Explorer and unreachable by path.
        // Windows expects a fixed disk to be a real device with a partition,
        // which this is not.
        //
        // It is also the honest description. Windows optimises fixed disks on
        // the assumption that they are fast and reliable, and network drives on
        // the assumption that they are neither — which is exactly right for a
        // filesystem whose every request crosses a Wi-Fi link to another
        // machine. sshfs-win and rclone mount this way for the same reason.
        .prefix(prefix.as_str())
        // No kernel cache: `FileInfoTimeout` is left at zero, which keeps the
        // volume off the NT cache manager.
        //
        // `u32::MAX` would put it on, and it was tried: the cache manager's lazy
        // writer coalesces an application's writes, which is what it is for.
        // But it accepts them at memory speed while this link drains at ~11
        // MB/s, so a download piled gigabytes of dirty pages into RAM, and
        // every `CloseHandle` — including the ones a dying process makes on
        // its way out — then had to flush its share through the link
        // synchronously. With the link stalled, Steam's download stopped at 0
        // B/s and the process could not be killed short of a reboot. Nothing a
        // user-mode filesystem does can interrupt that wait.
        //
        // The round-trip-per-write problem the cache was meant to solve is
        // solved without it, and without the unbounded buffer: `writeback`
        // keeps a bounded window of writes in flight and still reports every
        // failure to the application that caused it.
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        // Do the wildcard filtering here rather than leaving it to WinFsp.
        //
        // Left unset, the pattern never reaches `read_directory` and WinFsp
        // matches on its own — which on this volume found nothing: `*.o` came
        // back empty in a directory holding `a.o`, and cargo, which enumerates
        // with a pattern while clearing its `.temp-archive` directories, failed
        // with `ERROR_INVALID_NAME` on names that were perfectly valid.
        .pass_query_directory_pattern(true)
        // Accept POSIX-style unlink and rename.
        //
        // Rust's `remove_dir_all` deletes through `FileDispositionInformationEx`
        // with POSIX semantics, and without this declaration WinFsp refuses the
        // request outright: it never reached `set_delete` or `cleanup` — the
        // trace showed the directory opened and enumerated (empty), and then no
        // delete call at all — and Windows reported `ERROR_INVALID_PARAMETER`,
        // which cargo prints as "failed to remove temporary directory: the
        // parameter is incorrect (os error 87)" — localised, so it reads in
        // whatever language Windows runs in. Node deletes through the older
        // disposition class, which is why it never hit this.
        .supports_posix_unlink_rename(true);

    // Labelled with the peer and the disk, because several of these now sit
    // side by side in Explorer and "cursio" alone would not say which is which.
    let label = format!("{volume_label} ({peer_name})");
    let mut host: FileSystemHost<RemoteFs> =
        FileSystemHost::new(params, RemoteFs::new(peer, label, volume))
            .map_err(|e| anyhow::anyhow!("cannot create the filesystem host: {e:?}"))?;

    // Name the drive letter explicitly.
    //
    // A standalone probe against this WinFsp install (scratchpad/winfsp-probe)
    // tried all four combinations: a named letter produces a working drive for
    // both a disk and a network filesystem, while `*:` silently picks some
    // *other* free letter that the caller has no way to learn — WinFsp does not
    // report the choice back. Passing the UNC path instead is refused outright,
    // as mount points with three or more backslashes come back as
    // `STATUS_NETWORK_ACCESS_DENIED`.
    // KNOWN LIMITATION — the letter is invisible while cursio runs elevated.
    //
    // Cursio elevates itself so injected input can reach administrator-owned
    // windows. UAC gives such a user two linked logon sessions, and the symbolic
    // links that make up drive letters belong to one session, never both. The
    // drive mounted here is genuine and works — an elevated process listing
    // drives sees `C: Y: Z:` — but Explorer runs unelevated and sees only `C:`.
    //
    // Reaching it by its UNC path (`\\cursio\<peer>`) works from either session
    // and is unaffected, which is why the mount is still worth making.
    //
    // Two ways out, neither free: Microsoft's `EnableLinkedConnections` policy
    // mirrors the links into both sessions but is machine-wide and weakens the
    // boundary UAC draws; or the mount moves into an unelevated helper process,
    // which means forwarding every filesystem operation across a process
    // boundary. See `docs/files-design.md`.
    // Refused rather than folded in with another volume: there are 26 letters
    // for every disk on the machine and every peer's, and a volume quietly
    // sharing a letter with another is the ambiguity this change removes.
    let letter = free_drive_letter().ok_or_else(|| {
        anyhow::anyhow!("no free drive letter left for {peer_name}'s volume {volume}")
    })?;
    let mountpoint = format!("{letter}:");
    match host.mount(mountpoint.as_str()) {
        Ok(()) => tracing::debug!(drive = %mountpoint, "mounted"),
        Err(e) => return Err(anyhow::anyhow!("cannot mount on {mountpoint}: {e:?}")),
    }
    // Start the dispatcher with enough threads to keep requests in flight.
    //
    // `start()` asks WinFsp for its default, which is sized for a filesystem
    // whose work is local: a couple of threads per core, on the assumption that
    // a callback returns as soon as the disk does. Every callback here instead
    // blocks for a network round trip, so that default caps the mount at a
    // handful of concurrent operations and a copy of many small files spends
    // its time waiting rather than transferring — the same serialisation the
    // FUSE side had on its single event loop.
    //
    // WinFsp clamps whatever it is given to its own supported range, so an
    // unreasonable figure here degrades rather than failing to mount.
    let threads = crate::bridge::mount_threads();
    match host.start_with_threads(threads as u32) {
        Ok(()) => tracing::debug!(threads, "dispatcher started"),
        Err(e) => return Err(anyhow::anyhow!("cannot start the filesystem: {e:?}")),
    }

    tracing::info!(
        drive = %mountpoint,
        // Also reachable by path, which is worth recording: if the letter ever
        // fails to appear, this is the way in.
        unc = %unc,
        peer = peer_name,
        volume,
        "peer volume mounted"
    );
    Ok(MountHandle { host, mountpoint })
}

#[cfg(test)]
mod tests {
    use super::matches_pattern;

    #[test]
    fn a_bare_star_matches_everything() {
        assert!(matches_pattern("*", "anything.o"));
        assert!(matches_pattern("*", ""));
    }

    #[test]
    fn an_extension_pattern_selects_only_that_extension() {
        assert!(matches_pattern("*.o", "build.o"));
        assert!(!matches_pattern("*.o", "build.rmeta"));
        // The one that mattered: cargo's temporary directories.
        assert!(matches_pattern("*.temp-archive", ".tmpd6Z2gF.temp-archive"));
    }

    #[test]
    fn matching_ignores_case_as_the_volume_promises() {
        assert!(matches_pattern("*.O", "build.o"));
        assert!(matches_pattern("LIB*", "libcore.rlib"));
    }

    #[test]
    fn a_question_mark_stands_for_exactly_one_character() {
        assert!(matches_pattern("a?c", "abc"));
        assert!(!matches_pattern("a?c", "ac"));
        assert!(!matches_pattern("a?c", "abbc"));
    }

    #[test]
    fn several_stars_backtrack_without_running_away() {
        assert!(matches_pattern("*b*d", "abcd"));
        assert!(!matches_pattern("*b*e", "abcd"));
        // Pathological for a naive recursive matcher; must simply return.
        assert!(!matches_pattern("*a*a*a*a*b", &"a".repeat(64)));
    }

    #[test]
    fn dos_wildcards_are_understood_as_nt_sends_them() {
        // Exactly what the log showed arriving for `*.o` and `*.temp-archive`.
        assert!(matches_pattern("<.O", "a.o"));
        assert!(matches_pattern("<.TEMP-ARCHIVE", ".tmpXX.temp-archive"));
        assert!(!matches_pattern("<.O", "a.rmeta"));
        // `>` for a single character, `"` for the dot.
        assert!(matches_pattern("a>c", "abc"));
        assert!(matches_pattern("a\"o", "a.o"));
    }

    #[test]
    fn a_literal_name_matches_only_itself() {
        assert!(matches_pattern("Cargo.toml", "cargo.toml"));
        assert!(!matches_pattern("Cargo.toml", "Cargo.lock"));
    }
}
