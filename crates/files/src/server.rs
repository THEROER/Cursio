//! Serving this machine's volumes to the peer.
//!
//! Turns [`FilesMsg`] requests into local filesystem calls. Everything here runs
//! on behalf of *the other machine*, so the guiding rule is that a request is
//! untrusted input: a path is a string that arrived over a socket, not something
//! this process constructed.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use cursio_protocol::{FileError, FileHandle, FileKind, FileMeta, OpenMode, Volume};

/// A volume this machine offers, and where it actually lives.
#[derive(Clone)]
pub struct SharedVolume {
    pub info: Volume,
    /// Native path the volume's root maps to: `/` or `C:\`.
    pub root: PathBuf,
}

/// Serves the local filesystem to one peer.
///
/// Owns the open-file table: handles are indices into it, so a handle the peer
/// invents addresses nothing.
pub struct FileServer {
    ns: std::sync::Arc<Namespace>,
    open: HashMap<FileHandle, OpenFile>,
    next_handle: FileHandle,
}

/// What the peer may address, and everything that works on a path alone.
///
/// Split from [`FileServer`] so path operations run without its lock. The
/// server sits behind one mutex for the sake of its open-file table, and a
/// `readdir` or `stat` done under it held every other request — for every
/// other file — for as long as the disk took. On a Windows host one slow
/// directory (a `C:\Windows` subtree, a file Defender was scanning) stalled six
/// unrelated requests for 3.45 s, all answered in the same millisecond, past
/// the peer's 3 s deadline; a download writing through the same server stalls
/// the same way. The caller takes an `Arc` of this under the lock and does the
/// disk work after releasing it, as [`FileHandleRef`] already does for reads
/// and writes.
pub struct Namespace {
    volumes: Vec<SharedVolume>,
    /// Native paths that must never be served, however the peer addresses them.
    ///
    /// This is where the peer's own disks are mounted on this machine. Serving
    /// them sends the peer its own files back: on Linux the offered volume is
    /// `/`, and the peer's drives hang off it at `~/Cursio/<peer>/…`, so a
    /// machine with two real disks was offering six — its own two plus four of
    /// the peer's, arriving home under new letters. Following one of those is
    /// a round trip across the network for a file the asking machine already
    /// has, and a directory walk into it recurses until something gives up.
    ///
    /// Held as native paths and matched by prefix, because the loop has to be
    /// refused wherever it is reached from — a volume of its own, a symlink, or
    /// a path that walks into it from the volume above.
    excluded: Vec<PathBuf>,
}

/// An open file, shared so its I/O can happen outside the server's lock.
///
/// `Arc` rather than ownership because the whole `FileServer` sits behind one
/// mutex, and a `read`/`write` that borrowed from it would hold that mutex for
/// the length of a disk operation. Every other request — including requests for
/// other files entirely — then queued behind it, and measured against a peer
/// downloading a game that came to 11 writes a second with 0.2 operations
/// actually in flight, against a window of 128. Cloning the handle costs an
/// atomic increment and lets the lock be released before the disk is touched.
///
/// Positioned reads and writes (`read_at`/`write_at`) rather than
/// `seek` + `read`/`write`: a shared file has one cursor, so two concurrent
/// operations on the same handle would move it under each other. The positioned
/// calls carry the offset with them and leave no cursor to race over — which is
/// also exactly what the wire protocol supplies.
#[derive(Clone)]
struct OpenFile {
    file: std::sync::Arc<File>,
    writable: bool,
}

impl FileServer {
    /// Offer the volumes this machine has.
    pub fn new(volumes: Vec<SharedVolume>) -> Self {
        Self {
            ns: std::sync::Arc::new(Namespace {
                volumes,
                excluded: Vec::new(),
            }),
            open: HashMap::new(),
            next_handle: 1,
        }
    }

    /// Refuse to serve anything under these native paths.
    ///
    /// Set to where the peer's own volumes are mounted here; see
    /// [`Namespace::excluded`] for what serving them back costs.
    pub fn exclude(&mut self, paths: Vec<PathBuf>) {
        self.ns = std::sync::Arc::new(Namespace {
            volumes: self.ns.volumes.clone(),
            excluded: paths,
        });
    }

    /// The path half of the server, to use after releasing its lock.
    pub fn namespace(&self) -> std::sync::Arc<Namespace> {
        self.ns.clone()
    }

    /// Detect what this machine can share.
    ///
    /// Linux gets one volume — `/` — because everything, including other mounts
    /// and removable media, already hangs off it. Windows has no such single
    /// root, so each fixed drive letter is its own volume.
    pub fn detect() -> Self {
        Self::new(detect_volumes())
    }

    /// What to announce to the peer.
    pub fn volumes(&self) -> Vec<Volume> {
        self.ns.volumes.iter().map(|v| v.info.clone()).collect()
    }

    /// Take the volume set from a fresh scan, keeping the handles this one has
    /// open.
    ///
    /// A rescan used to replace the whole server, which threw away `open` and
    /// reset `next_handle` to 1. Both halves were wrong, and neither announced
    /// itself: every file the peer had open — on *every* volume, not just the
    /// one that changed — became `BadHandle` on its next write, and the handles
    /// handed out afterwards repeated numbers the peer was still using, so a
    /// write meant for one file could land in another. Plugging in a stick or
    /// unplugging a disk is enough to trigger it, since it is the volume *set*
    /// that changes.
    ///
    /// A handle whose volume genuinely went away keeps working here: it holds
    /// an open `File`, and the OS decides what a write to a vanished device
    /// does. That is the same answer a local program gets, and a truthful error
    /// from the kernel beats a fabricated one from us.
    pub fn adopt_volumes(&mut self, scanned: FileServer) {
        self.ns = std::sync::Arc::new(Namespace {
            volumes: scanned.ns.volumes.clone(),
            excluded: self.ns.excluded.clone(),
        });
    }

    pub fn resolve(&self, volume: &str, path: &str) -> Result<PathBuf, FileError> {
        self.ns.resolve(volume, path)
    }

    pub fn read_dir(&self, volume: &str, path: &str) -> Result<Vec<FileMeta>, FileError> {
        self.ns.read_dir(volume, path)
    }

    pub fn stat(&self, volume: &str, path: &str) -> Result<FileMeta, FileError> {
        self.ns.stat(volume, path)
    }

    pub fn read_link(&self, volume: &str, path: &str) -> Result<String, FileError> {
        self.ns.read_link(volume, path)
    }

    /// Open a file, returning a handle and its current size.
    pub fn open(
        &mut self,
        volume: &str,
        path: &str,
        mode: OpenMode,
    ) -> Result<(FileHandle, u64), FileError> {
        let (file, size) = self.ns.open(volume, path, mode)?;
        Ok((self.adopt(file, mode.write), size))
    }

    /// Give a file opened through [`Namespace::open`] a handle.
    pub fn adopt(&mut self, file: File, writable: bool) -> FileHandle {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.open.insert(
            handle,
            OpenFile {
                file: std::sync::Arc::new(file),
                writable,
            },
        );
        handle
    }

    pub fn mkdir(&self, volume: &str, path: &str) -> Result<(), FileError> {
        self.ns.mkdir(volume, path)
    }

    pub fn remove(&self, volume: &str, path: &str) -> Result<(), FileError> {
        self.ns.remove(volume, path)
    }

    pub fn rename(&self, volume: &str, from: &str, to: &str) -> Result<(), FileError> {
        self.ns.rename(volume, from, to)
    }
}

impl Namespace {
    /// Whether this resolved path lies in a place that must not be served.
    fn is_excluded(&self, path: &Path) -> bool {
        self.excluded.iter().any(|e| path.starts_with(e))
    }

    /// Resolve a wire path (volume id + `/`-separated relative path) to a native
    /// path inside that volume.
    ///
    /// This is the security boundary. The peer's path is rejected unless it is
    /// strictly relative and free of `..`: a request for `../../etc/shadow`, an
    /// absolute path, or a Windows drive prefix must not be able to address
    /// anything outside the volume it names. `..` is refused outright rather
    /// than normalised away, because normalising is only equivalent when no
    /// component is a symlink — and the peer chooses the components.
    pub fn resolve(&self, volume: &str, path: &str) -> Result<PathBuf, FileError> {
        let vol = self
            .volumes
            .iter()
            .find(|v| v.info.id == volume)
            .ok_or(FileError::BadHandle)?;

        let mut out = vol.root.clone();
        for part in path.split('/') {
            // Empty parts come from leading, trailing and doubled separators and
            // mean "no component"; `.` is a no-op.
            if part.is_empty() || part == "." {
                continue;
            }
            if part == ".." {
                return Err(FileError::Denied);
            }
            // A component must be exactly one plain name. Anything the host OS
            // would read as a root, a drive or a separator is refused — on
            // Windows `C:` inside a component would otherwise re-root the path.
            let candidate = Path::new(part);
            let mut comps = candidate.components();
            match (comps.next(), comps.next()) {
                (Some(Component::Normal(name)), None) => out.push(name),
                _ => return Err(FileError::Denied),
            }
        }
        // Where the peer's own disks are mounted here. Refused rather than
        // served, or the peer follows the path back to its own files across the
        // network — see [`Namespace::excluded`].
        if self.is_excluded(&out) {
            return Err(FileError::Denied);
        }
        Ok(out)
    }

    /// Whether a volume accepts writes at all.
    fn writable(&self, volume: &str) -> bool {
        self.volumes
            .iter()
            .find(|v| v.info.id == volume)
            .is_some_and(|v| v.info.writable)
    }

    /// List a directory.
    pub fn read_dir(&self, volume: &str, path: &str) -> Result<Vec<FileMeta>, FileError> {
        let dir = self.resolve(volume, path)?;
        let writable = self.writable(volume);
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(to_file_error)? {
            let entry = entry.map_err(to_file_error)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hidden, not merely refused on entry: a directory the peer can see
            // but never open is worse than one that was never offered — a file
            // manager shows it, a copy walks into it, and both fail. This is
            // where the peer's own disks are mounted here.
            if self.is_excluded(&entry.path()) {
                continue;
            }
            // `symlink_metadata`, not `metadata`: a listing must describe the
            // link itself, or a link pointing outside the volume would be
            // reported as whatever it targets.
            let md = match entry.path().symlink_metadata() {
                Ok(md) => md,
                // A file can vanish between reading the directory and stat'ing
                // it. Skipping it is right — it is genuinely no longer there.
                Err(_) => continue,
            };
            out.push(meta_from(name, &md, writable));
        }
        Ok(out)
    }

    /// Metadata for one path.
    pub fn stat(&self, volume: &str, path: &str) -> Result<FileMeta, FileError> {
        let target = self.resolve(volume, path)?;
        let md = target.symlink_metadata().map_err(to_file_error)?;
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());
        Ok(meta_from(name, &md, self.writable(volume)))
    }

    /// Read a symlink's target, exactly as stored.
    ///
    /// Deliberately not resolved: following it here would let a link inside the
    /// volume address anything on this disk, which is the whole reason a listing
    /// reports links rather than what they point at. The target crosses the wire
    /// as written and the mount side interprets it against the mounted tree,
    /// where a link leading out of the volume simply dangles.
    ///
    /// Windows targets keep their backslashes. Rewriting them would be a guess
    /// about a path this side never resolves, and a dangling link that reads
    /// back what the owner stored is easier to explain than one silently
    /// rewritten into a different dangling path.
    pub fn read_link(&self, volume: &str, path: &str) -> Result<String, FileError> {
        let target = self.resolve(volume, path)?;
        // `symlink_metadata` first: `read_link` on a regular file reports
        // `InvalidInput`, which `to_file_error` cannot tell from a malformed
        // request. The caller asked about the wrong kind of thing, and
        // `WrongKind` says so.
        let md = target.symlink_metadata().map_err(to_file_error)?;
        if !md.file_type().is_symlink() {
            return Err(FileError::WrongKind);
        }
        let dest = std::fs::read_link(&target).map_err(to_file_error)?;
        dest.to_str()
            .map(str::to_owned)
            // Wire paths are UTF-8. A target that is not says so plainly rather
            // than arriving mangled by a lossy conversion.
            .ok_or(FileError::Io("link target is not valid utf-8".into()))
    }

    /// Open a file, returning it and its current size. It has no handle until
    /// [`FileServer::adopt`] gives it one.
    pub fn open(&self, volume: &str, path: &str, mode: OpenMode) -> Result<(File, u64), FileError> {
        if mode.is_mutating() && !self.writable(volume) {
            return Err(FileError::ReadOnly);
        }
        let target = self.resolve(volume, path)?;
        let file = OpenOptions::new()
            .read(mode.read)
            .write(mode.write)
            .create(mode.create && !mode.exclusive)
            .create_new(mode.exclusive)
            .truncate(mode.truncate)
            .open(&target)
            .map_err(to_file_error)?;
        let size = file.metadata().map_err(to_file_error)?.len();
        Ok((file, size))
    }
}

impl FileServer {
    /// Read from an open handle. Returns fewer bytes than asked at end of file.
    pub fn read(&self, handle: FileHandle, offset: u64, len: u32) -> Result<Vec<u8>, FileError> {
        // The handle is looked up under the caller's lock; the disk is touched
        // after it is released. See [`OpenFile`].
        let open = self.handle(handle)?;
        read_at(&open.file, offset, len as usize)
    }

    /// Write to an open handle.
    pub fn write(&self, handle: FileHandle, offset: u64, bytes: &[u8]) -> Result<u64, FileError> {
        let open = self.writable_handle(handle)?;
        write_at(&open.file, offset, bytes)?;
        Ok(bytes.len() as u64)
    }

    /// Set an open file's length.
    pub fn truncate(&mut self, handle: FileHandle, size: u64) -> Result<(), FileError> {
        self.set_size(handle, size, false)
    }

    /// Set an open file's length, or commit space for it.
    ///
    /// `reserve` picks which. Setting the length is `set_len`, which on every
    /// platform here leaves a hole rather than writing anything — cheap, and
    /// correct for a truncate. Reserving is the opposite request: the caller
    /// wants the space to *exist*, so that a write later cannot fail for want
    /// of it, and a hole is precisely what it must not get.
    ///
    /// The distinction is what a game installer relies on. Steam reserves a
    /// download's full size before fetching a byte, so that a disk filling up
    /// midway cannot leave an unusable install. Answering that reservation with
    /// `set_len` reported success while committing nothing — 55.9 GB of
    /// "allocated" files occupying 904 KB — and its preallocation pass stalled
    /// partway through.
    pub fn set_size(&self, handle: FileHandle, size: u64, reserve: bool) -> Result<(), FileError> {
        let open = self.writable_handle(handle)?;
        if reserve {
            return reserve_space(&open.file, size);
        }
        open.file.set_len(size).map_err(to_file_error)
    }

    /// The open file behind a handle, detached from the server.
    ///
    /// This is how a caller does disk work without holding the server's lock:
    /// take the handle, release the lock, then read or write through
    /// [`FileHandleRef`]. `read` and `write` above are the convenience form for
    /// callers that are not contending — the session uses this one, because
    /// holding the lock across a disk operation serialised every request in the
    /// system behind whichever one was unlucky enough to be first.
    pub fn detach(&self, handle: FileHandle) -> Result<FileHandleRef, FileError> {
        Ok(FileHandleRef(self.handle(handle)?))
    }

    /// As [`FileServer::detach`], refusing a handle not opened for writing.
    pub fn detach_writable(&self, handle: FileHandle) -> Result<FileHandleRef, FileError> {
        Ok(FileHandleRef(self.writable_handle(handle)?))
    }

    /// The open file behind a handle, cloned so the caller can drop the lock.
    fn handle(&self, handle: FileHandle) -> Result<OpenFile, FileError> {
        self.open.get(&handle).cloned().ok_or(FileError::BadHandle)
    }

    /// As [`FileServer::handle`], refusing a handle that was not opened for
    /// writing.
    fn writable_handle(&self, handle: FileHandle) -> Result<OpenFile, FileError> {
        let open = self.handle(handle)?;
        match open.writable {
            true => Ok(open),
            false => Err(FileError::ReadOnly),
        }
    }

    /// Release a handle. Unknown handles are ignored: a close is the peer saying
    /// it is finished, and there is nothing useful to report back.
    pub fn close(&mut self, handle: FileHandle) {
        self.open.remove(&handle);
    }
}

impl Namespace {
    /// Create a directory.
    pub fn mkdir(&self, volume: &str, path: &str) -> Result<(), FileError> {
        if !self.writable(volume) {
            return Err(FileError::ReadOnly);
        }
        std::fs::create_dir(self.resolve(volume, path)?).map_err(to_file_error)
    }

    /// Remove a file or an empty directory.
    pub fn remove(&self, volume: &str, path: &str) -> Result<(), FileError> {
        if !self.writable(volume) {
            return Err(FileError::ReadOnly);
        }
        let target = self.resolve(volume, path)?;
        let md = target.symlink_metadata().map_err(to_file_error)?;
        if md.is_dir() {
            std::fs::remove_dir(&target).map_err(to_file_error)
        } else {
            std::fs::remove_file(&target).map_err(to_file_error)
        }
    }

    /// Rename within one volume. Cross-volume moves are the caller's problem to
    /// decompose into copy+delete, exactly as they are locally.
    pub fn rename(&self, volume: &str, from: &str, to: &str) -> Result<(), FileError> {
        if !self.writable(volume) {
            return Err(FileError::ReadOnly);
        }
        let from = self.resolve(volume, from)?;
        let to = self.resolve(volume, to)?;
        std::fs::rename(from, to).map_err(to_file_error)
    }
}

/// An open file taken out of the server, so its I/O needs no lock.
///
/// Holds its own reference to the file, so the server's mutex can be released
/// the moment this is obtained — which is the whole point. See
/// [`FileServer::detach`].
pub struct FileHandleRef(OpenFile);

impl FileHandleRef {
    /// Read from the file. Short only at end of file.
    pub fn read(&self, offset: u64, len: u32) -> Result<Vec<u8>, FileError> {
        read_at(&self.0.file, offset, len as usize)
    }

    /// Write to the file, returning how many bytes were accepted.
    pub fn write(&self, offset: u64, bytes: &[u8]) -> Result<u64, FileError> {
        write_at(&self.0.file, offset, bytes)?;
        Ok(bytes.len() as u64)
    }

    /// Set the file's length, or commit space for it.
    pub fn set_size(&self, size: u64, reserve: bool) -> Result<(), FileError> {
        match reserve {
            true => reserve_space(&self.0.file, size),
            false => self.0.file.set_len(size).map_err(to_file_error),
        }
    }
}

/// Read `len` bytes from `offset`, without disturbing the file's cursor.
///
/// Short only at end of file: a short read reported as the whole answer would
/// silently truncate the file as the peer sees it, so this loops until the
/// buffer is full or the file really ends.
fn read_at(file: &File, offset: u64, len: usize) -> Result<Vec<u8>, FileError> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;

    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    while filled < buf.len() {
        #[cfg(unix)]
        let got = file.read_at(&mut buf[filled..], offset + filled as u64);
        #[cfg(windows)]
        let got = file.seek_read(&mut buf[filled..], offset + filled as u64);
        match got {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(to_file_error(e)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Write `bytes` at `offset`, without disturbing the file's cursor.
///
/// Loops for the same reason a positioned read does: a partial write is not an
/// error, and stopping at one would leave a hole in the file the peer believes
/// it wrote.
fn write_at(file: &File, offset: u64, bytes: &[u8]) -> Result<(), FileError> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;

    let mut written = 0usize;
    while written < bytes.len() {
        #[cfg(unix)]
        let put = file.write_at(&bytes[written..], offset + written as u64);
        #[cfg(windows)]
        let put = file.seek_write(&bytes[written..], offset + written as u64);
        match put {
            Ok(0) => {
                return Err(FileError::Io("the disk accepted no bytes".into()));
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(to_file_error(e)),
        }
    }
    Ok(())
}

/// Commit `size` bytes of storage to a file, without changing its length.
///
/// This is Windows' `AllocationSize`, relayed, and it keeps that contract:
/// `FileSize` stays where it was, and an allocation *below* the current length
/// truncates to it. Growing the length instead — which `posix_fallocate` does —
/// made every reservation look like a file already written to its full size,
/// so the mount reported gigabytes of zeroes as content and an append landed
/// after them rather than at the real end.
///
/// `fallocate(2)` with `FALLOC_FL_KEEP_SIZE` commits the blocks and leaves the
/// length alone. A filesystem that cannot do that (`EOPNOTSUPP`: FAT, some
/// network filesystems) keeps the file as it is: space not reserved in advance
/// is what every such filesystem gives a local program too, whereas faking the
/// reservation by moving the end of the file would corrupt what the caller
/// reads back.
///
/// `ENOSPC` is the answer that matters: an installer asking for room it cannot
/// have must be told so now rather than partway through a download.
#[cfg(target_os = "linux")]
fn reserve_space(file: &File, size: u64) -> Result<(), FileError> {
    use std::os::fd::AsRawFd;

    let len = file.metadata().map_err(to_file_error)?.len();
    if size < len {
        return file.set_len(size).map_err(to_file_error);
    }
    if size == 0 {
        return Ok(());
    }
    // SAFETY: `fd` is owned by `file` and valid for this call; the mode, offset
    // and length are plain integers.
    let rc = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_KEEP_SIZE,
            0,
            size as libc::off_t,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EOPNOTSUPP) => {
            tracing::debug!(
                size,
                "this filesystem cannot reserve space; leaving the file as it is"
            );
            Ok(())
        }
        _ => Err(to_file_error(err)),
    }
}

/// Windows commits space through the file's own allocation size, which is
/// exactly the request being relayed — so it is passed through as that, not as
/// `set_len`, which would move the end of the file.
#[cfg(target_os = "windows")]
fn reserve_space(file: &File, size: u64) -> Result<(), FileError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileAllocationInfo, SetFileInformationByHandle, FILE_ALLOCATION_INFO,
    };

    let info = FILE_ALLOCATION_INFO {
        AllocationSize: size as i64,
    };
    // SAFETY: the handle is owned by `file` and valid for this call; `info` is
    // a live, correctly sized `FILE_ALLOCATION_INFO` for the class named.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileAllocationInfo,
            (&info as *const FILE_ALLOCATION_INFO).cast(),
            std::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    };
    match ok {
        0 => Err(to_file_error(std::io::Error::last_os_error())),
        _ => Ok(()),
    }
}

/// No way to reserve without moving the end of the file here, so only the
/// truncating half of the contract is honoured.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn reserve_space(file: &File, size: u64) -> Result<(), FileError> {
    let len = file.metadata().map_err(to_file_error)?.len();
    if size < len {
        return file.set_len(size).map_err(to_file_error);
    }
    Ok(())
}

fn meta_from(name: String, md: &std::fs::Metadata, volume_writable: bool) -> FileMeta {
    let kind = if md.file_type().is_symlink() {
        FileKind::Symlink
    } else if md.is_dir() {
        FileKind::Dir
    } else {
        FileKind::File
    };
    let modified_unix = md
        .modified()
        .ok()
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                // Before the epoch: report the negative offset rather than
                // clamping, which would date the file to 1970.
                .or_else(|e| Ok::<i64, std::convert::Infallible>(-(e.duration().as_secs() as i64)))
                .ok()
        })
        .unwrap_or(0);
    FileMeta {
        name,
        kind,
        size: if md.is_dir() { 0 } else { md.len() },
        modified_unix,
        writable: volume_writable && !md.permissions().readonly(),
        allocated: allocated_bytes(md),
    }
}

/// What the file occupies on this machine's disk, when that is knowable.
///
/// Reported alongside the length because the two can differ by orders of
/// magnitude for a sparse file, and a mount that only knows the length makes
/// every disk-usage tool describe a volume as holding more than it can.
#[cfg(unix)]
fn allocated_bytes(md: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    // `blocks` counts 512-byte units regardless of the filesystem's own block
    // size — that is the unit `stat(2)` defines, not a guess.
    Some(md.blocks() * 512)
}

/// Windows does not offer the figure through `Metadata`.
///
/// `file_size()` there is the declared length, which is the number this exists
/// to correct rather than a second source for it. Reaching the real occupancy
/// means `GetFileInformationByHandleEx` on an open handle, which a listing of
/// thousands of entries cannot afford one file at a time — so this side says
/// "unknown" and the mount falls back to the length, exactly as before.
#[cfg(windows)]
fn allocated_bytes(_md: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(not(any(unix, windows)))]
fn allocated_bytes(_md: &std::fs::Metadata) -> Option<u64> {
    None
}

/// Map a local IO error onto the wire vocabulary.
fn to_file_error(e: std::io::Error) -> FileError {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => FileError::NotFound,
        K::PermissionDenied => FileError::Denied,
        K::AlreadyExists => FileError::Exists,
        K::NotADirectory | K::IsADirectory => FileError::WrongKind,
        K::DirectoryNotEmpty => FileError::NotEmpty,
        _ => FileError::Io(e.to_string()),
    }
}

/// Stand-in capacity for a volume whose real size cannot be read.
///
/// Never zero, and that is the whole point. A filesystem reporting a zero total
/// makes "percent used" a division by zero in every caller that computes it —
/// rclone's mount crashed programs with SIGFPE exactly this way — and a zero
/// free count makes others refuse to write, believing the disk full. A
/// plausible figure is wrong in a way nothing crashes on; zero is wrong in a way
/// that takes other programs down.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", test))]
pub(crate) const FALLBACK_TOTAL: u64 = 1 << 40; // 1 TiB
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", test))]
pub(crate) const FALLBACK_FREE: u64 = 1 << 39; // half of it

/// Total and free bytes for the filesystem holding `path`.
///
/// Falls back to [`FALLBACK_TOTAL`]/[`FALLBACK_FREE`] rather than zero when the
/// query fails.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", test))]
fn capacity_of(path: &Path) -> (u64, u64) {
    match raw_capacity(path) {
        // A total of zero is as dangerous as a failed query, so treat it the
        // same: some pseudo-filesystems really do report it.
        Some((total, free)) if total > 0 => (total, free),
        _ => (FALLBACK_TOTAL, FALLBACK_FREE),
    }
}

/// `statvfs` is POSIX, so one implementation covers both Unix hosts. The field
/// widths differ (macOS counts blocks in 32 bits, Linux in 64), which the casts
/// below absorb.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn raw_capacity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // Safe: `c_path` is a valid NUL-terminated string and `stat` is owned here.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    // `f_frsize` is the fragment size the block counts are in — not `f_bsize`,
    // which is only a hint about efficient I/O size and can differ.
    let unit = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    // `f_bavail`, not `f_bfree`: the latter counts blocks reserved for root,
    // which an ordinary process cannot actually use.
    Some((stat.f_blocks as u64 * unit, stat.f_bavail as u64 * unit))
}

#[cfg(target_os = "windows")]
fn raw_capacity(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_to_caller = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    // Safe: `wide` is a valid NUL-terminated wide string, and the out-params are
    // owned here. `GetDiskFreeSpaceExW` reports the caller's quota in the first
    // out-param, which is what a program can actually write.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        return None;
    }
    Some((total, free_to_caller))
}

#[cfg(all(
    not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
    test
))]
fn raw_capacity(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// The root, plus every disk mounted below it.
///
/// The root alone reaches all of them — a second disk is mounted under `/mnt`
/// and so already hangs off `/` — but reaching is not presenting. Offered as
/// one volume, a machine's other disks arrive at the peer as unnamed folders
/// several levels down, with no capacity of their own, no label the user gave
/// them, and no entry in a file manager's sidebar. On this machine that hid
/// 1.2 TB across four disks inside a single `/`, while the Windows half
/// offered every drive letter separately — the same machine presented two
/// different ways depending on which end you looked from.
///
/// Each disk therefore becomes its own volume, which is also what makes a USB
/// stick appear as the disk the user plugged in. The medium stays mounted
/// exactly once, by this machine, and peers reach it through file operations —
/// so any number of them use it at once without the cache divergence two
/// kernels mounting one stick would cause. See `docs/usb-design.md`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn detect_volumes() -> Vec<SharedVolume> {
    let root = PathBuf::from("/");
    let (total_bytes, free_bytes) = capacity_of(&root);
    let mut volumes = vec![SharedVolume {
        info: Volume {
            id: "/".to_string(),
            label: "Root".to_string(),
            writable: true,
            total_bytes,
            free_bytes,
            removable: false,
        },
        root,
    }];
    volumes.extend(detect_disks());
    volumes
}

/// Every disk mounted right now, as a volume of its own.
///
/// ## What counts as a disk
///
/// The same question a file manager answers when it decides what to put in its
/// sidebar, and answered the same way rather than by a rule of our own. udisks
/// distinguishes a volume backed by real hardware from one that is not by
/// whether the block device belongs to a *drive*: `/dev/sda1` reports a drive
/// with a model and a serial, while `/dev/loop0` reports none, because behind
/// it is a file. In sysfs that is exactly the difference between a device under
/// `/sys/devices/pci…` and one under `/sys/devices/virtual/`.
///
/// Reading it from sysfs rather than asking udisks over D-Bus keeps this
/// working on a machine with no desktop session and needs no second
/// implementation for Windows, which has no udisks at all. It is the same
/// answer from the same source udisks itself reads.
///
/// On this machine the test is decisive: 29 snap loop mounts and four real
/// disks, and a file manager shows the four. A rule based on the mount path
/// would have failed here — `/mnt` holds permanently mounted fixed disks, not
/// the removable media the name suggests.
#[cfg(target_os = "linux")]
fn detect_disks() -> Vec<SharedVolume> {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(target)) = (fields.next(), fields.next()) else {
            continue;
        };
        // The root is already offered above, and a bind mount of it would
        // otherwise arrive a second time under another name.
        if target == "/" {
            continue;
        }
        let Some(disk) = backing_disk(source) else {
            continue;
        };
        // One volume per filesystem: a disk mounted twice (a bind mount, or the
        // same device mounted at two points) is one disk, and offering it twice
        // would show the peer two drives that are the same storage.
        if !seen.insert(source.to_string()) {
            continue;
        }
        // The mount table escapes spaces as an octal `\040`, so a stick
        // labelled "MY DRIVE" arrives with an escape sequence in its path.
        // Written raw: `"\040"` in Rust source is a NUL followed by "40",
        // which would match nothing this file ever contains.
        let target = target.replace(r"\040", " ");
        let root = PathBuf::from(&target);
        let (total_bytes, free_bytes) = capacity_of(&root);
        out.push(SharedVolume {
            info: Volume {
                // The mount path is the id: stable while the filesystem is
                // mounted, which is exactly as long as the volume is offered,
                // and unique because two filesystems cannot share a mount point.
                id: target.clone(),
                label: disk_label(&root, &target, total_bytes),
                writable: true,
                total_bytes,
                free_bytes,
                removable: is_removable_disk(&disk),
            },
            root,
        });
    }
    out
}

/// The sysfs directory of the whole disk behind a block device, or `None` when
/// nothing physical is behind it.
///
/// A partition's sysfs entry is a child of its disk's, so the parent is taken
/// when the device is not itself a whole disk. Anything under
/// `/sys/devices/virtual` — loop, device-mapper, zram — has no drive behind it
/// and is not offered, which is what keeps a machine's snap mounts out of the
/// peer's file manager.
#[cfg(target_os = "linux")]
fn backing_disk(source: &str) -> Option<PathBuf> {
    let name = source.strip_prefix("/dev/")?;
    let path = std::fs::canonicalize(format!("/sys/class/block/{name}")).ok()?;
    if path.starts_with("/sys/devices/virtual") {
        return None;
    }
    // `removable` sits on the whole disk; its presence is also what marks this
    // entry as the disk rather than one of its partitions.
    match path.join("removable").exists() {
        true => Some(path),
        false => Some(path.parent()?.to_path_buf()),
    }
}

/// Whether the kernel calls this disk removable.
#[cfg(target_os = "linux")]
fn is_removable_disk(disk: &Path) -> bool {
    std::fs::read_to_string(disk.join("removable"))
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// A name for a volume, in the order a person would recognise it.
///
/// The mount point's last component first, because udisks names a mount after
/// the filesystem label and that is the name the user gave the disk. A disk
/// with no label is mounted at a path that says nothing either — so rather than
/// offer the peer a volume called `sdd2`, it is named for its size, exactly as
/// a file manager shows "239 GB Volume" for the same disk.
#[cfg(target_os = "linux")]
fn disk_label(root: &Path, target: &str, total_bytes: u64) -> String {
    let named = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.to_string());
    // A bare device name is not a label anybody chose.
    match named.starts_with("sd") || named.starts_with("nvme") || named.is_empty() {
        true => format!("{} Volume", human_size(total_bytes)),
        false => named,
    }
}

/// Round a capacity the way a file manager labels a disk: "239 GB", "1,0 TB".
///
/// Used on both platforms, for the same reason on each: a disk the user never
/// named has to be called something, and its size is what a file manager shows.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn human_size(bytes: u64) -> String {
    // Decimal units, as every disk vendor and file manager uses: a 240 GB SSD
    // labelled "223 GiB" reads as the wrong disk.
    const UNITS: [(&str, u64); 3] = [
        ("TB", 1_000_000_000_000),
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let whole = bytes / scale;
            return match whole >= 10 {
                true => format!("{whole} {unit}"),
                // One decimal below ten, so a 1.5 TB disk is not "1 TB".
                false => format!("{:.1} {unit}", bytes as f64 / scale as f64),
            };
        }
    }
    format!("{bytes} B")
}

/// macOS mounts removable media under `/Volumes`, but reporting which are
/// removable needs DiskArbitration — a framework this crate does not link for
/// the one machine that cannot host a mount anyway.
#[cfg(target_os = "macos")]
fn detect_disks() -> Vec<SharedVolume> {
    Vec::new()
}

#[cfg(target_os = "windows")]
fn detect_volumes() -> Vec<SharedVolume> {
    // No single root on Windows, so each drive letter that responds is its own
    // volume. Probed by asking for the root's metadata: enumerating with
    // GetLogicalDrives would also list drives that are present but unreadable
    // (an empty card reader), which would show up as a folder that always fails.
    let mut out = Vec::new();
    for letter in 'A'..='Z' {
        let root = PathBuf::from(format!("{letter}:\\"));
        if std::fs::metadata(&root).is_err() {
            continue;
        }
        // Not a disk of ours: this letter is a peer's volume, mounted here by
        // Cursio itself. Offering it back sends the peer its own files across
        // the network to arrive home under a new letter — a machine with two
        // real disks announced six, four of them the asking machine's own,
        // labelled with that machine's name. Following one is a round trip for
        // a file the caller already has, and a walk into it recurses.
        if is_own_mount(letter) {
            continue;
        }
        let (total_bytes, free_bytes) = capacity_of(&root);
        out.push(SharedVolume {
            info: Volume {
                id: format!("{letter}:"),
                label: volume_label(letter).unwrap_or_else(|| human_size(total_bytes)),
                writable: true,
                total_bytes,
                free_bytes,
                removable: is_removable_drive(letter),
            },
            root,
        });
    }
    out
}

/// The name the user gave this drive, as Explorer shows it.
///
/// The volume label, not a name of our own. This used to answer
/// `format!("Disk ({letter}:)")`, which says only what the caller already knew
/// — and the letter is the peer's private naming: it means something on the
/// machine that assigned it and nothing on the machine reading it, where two
/// Windows peers each offer a `C` that are different disks. The mount that
/// presents this then added the letter back, giving the reader `Disk (C:) (C)`.
///
/// `None` when the drive has no label — plenty of disks are never named — and
/// the caller falls back to the size, exactly as a file manager writes
/// "239 GB Volume" for the same disk.
#[cfg(target_os = "windows")]
fn volume_label(letter: char) -> Option<String> {
    let root: Vec<u16> = format!("{letter}:\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // `MAX_PATH + 1`, which is what `GetVolumeInformationW` documents as the
    // required size for the label buffer.
    let mut label = [0u16; 261];
    // Safe: `root` is a valid NUL-terminated wide string, and the buffer is
    // owned here and passed with its true length. The out-params this call does
    // not need are null, which the API accepts.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let end = label.iter().position(|&c| c == 0).unwrap_or(label.len());
    let name = String::from_utf16_lossy(&label[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Whether this drive letter is a peer's volume that Cursio mounted here.
///
/// Identified by the UNC name behind the letter. [`crate::winmount`] publishes
/// every mount as a network filesystem under `\cursio\<peer>-<volume>`, so that
/// prefix is exactly our own mounts and nothing else — a real network drive the
/// user mapped keeps its own server name and is offered to the peer like any
/// other disk, which is right: it is storage this machine can reach and the
/// peer may not.
///
/// Tested by the name rather than by `DRIVE_REMOTE`, which would also catch
/// those genuine mappings.
#[cfg(target_os = "windows")]
fn is_own_mount(letter: char) -> bool {
    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

    let device: Vec<u16> = format!("{letter}:")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `device` is a NUL-terminated UTF-16 string that outlives the call.
    if unsafe { GetDriveTypeW(device.as_ptr()) } != DRIVE_REMOTE {
        return false;
    }

    let mut name = [0u16; 512];
    let mut len = name.len() as u32;
    // SAFETY: both buffers are owned here and passed with their true lengths;
    // `len` is in-out and the API writes no more than it says.
    let ok = unsafe { WNetGetConnectionW(device.as_ptr(), name.as_mut_ptr(), &mut len) };
    if ok != 0 {
        return false;
    }
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
        .trim_start_matches('\\')
        .starts_with("cursio\\")
}

/// Whether a drive letter is a removable medium.
///
/// `GetDriveTypeW` answers directly, and is the whole of what Windows needs
/// here — the drive-letter enumeration above already found the volume, so
/// unlike Linux there is no mount table to walk or parent device to resolve.
#[cfg(target_os = "windows")]
fn is_removable_drive(letter: char) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    // `DRIVE_REMOVABLE` lives under `WindowsProgramming`, not beside the
    // function that returns it.
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOVABLE;

    let path: Vec<u16> = format!("{letter}:\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call.
    unsafe { GetDriveTypeW(path.as_ptr()) == DRIVE_REMOVABLE }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_volumes() -> Vec<SharedVolume> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file manager labels a disk in decimal units — a 240 GB SSD shown as
    /// "223 GiB" reads as a different disk than the one on the box.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_capacity_reads_the_way_a_disk_is_sold() {
        assert_eq!(human_size(238_927_478_784), "238 GB");
        assert_eq!(human_size(1_000_200_994_816), "1.0 TB");
        assert_eq!(human_size(500_105_217_024), "500 GB");
    }

    /// Below ten the whole number alone loses too much: a 1.5 TB disk must not
    /// be labelled the same as a 1.0 TB one.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_small_multiple_keeps_one_decimal() {
        assert_eq!(human_size(1_500_000_000_000), "1.5 TB");
        assert_eq!(human_size(2_000_000_000), "2.0 GB");
    }

    /// udisks mounts a disk under its filesystem label, so the last path
    /// component is the name the user gave it.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_labelled_disk_keeps_its_name() {
        let target = "/mnt/Programing";
        assert_eq!(
            disk_label(Path::new(target), target, 200_000_000_000),
            "Programing"
        );
    }

    /// A disk with no label is mounted at a path naming the device, and
    /// `sdd2` is not a name anybody chose. Named for its size instead, exactly
    /// as a file manager does.
    #[test]
    #[cfg(target_os = "linux")]
    fn an_unlabelled_disk_is_named_for_its_size() {
        let target = "/media/theroer/sdd2";
        assert_eq!(
            disk_label(Path::new(target), target, 238_927_478_784),
            "238 GB Volume"
        );
    }

    /// Nothing physical backs a loop device, which is why a file manager does
    /// not show one — and why 29 snap mounts must not reach the peer.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_loop_device_has_no_backing_disk() {
        // Skipped rather than failed where the machine has no loop devices:
        // the assertion is about what sysfs reports, not about this file.
        if std::path::Path::new("/sys/class/block/loop0").exists() {
            assert!(backing_disk("/dev/loop0").is_none());
        }
    }

    /// Anything that is not a block device at all — tmpfs, cgroup, proc — has
    /// no disk behind it either.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_pseudo_filesystem_has_no_backing_disk() {
        assert!(backing_disk("tmpfs").is_none());
        assert!(backing_disk("proc").is_none());
    }

    #[test]
    fn path_operations_run_while_the_server_is_locked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), b"x").expect("write");
        let server = std::sync::Mutex::new(server_on(tmp.path()));
        let ns = server.lock().expect("lock").namespace();

        // Another request holding the server — a handle being adopted or
        // closed — must not stop a listing, a stat or an open elsewhere.
        let held = server.lock().expect("lock");
        let listing = std::thread::scope(|s| {
            s.spawn(|| {
                let names: Vec<String> = ns
                    .read_dir("T:", "")
                    .expect("lists")
                    .into_iter()
                    .map(|m| m.name)
                    .collect();
                ns.stat("T:", "a.txt").expect("stats");
                ns.open("T:", "a.txt", OpenMode::read_only())
                    .expect("opens");
                names
            })
            .join()
            .expect("path work finished with the server locked")
        });
        drop(held);
        assert_eq!(listing, vec!["a.txt".to_string()]);
    }

    fn server_on(root: &Path) -> FileServer {
        FileServer::new(vec![SharedVolume {
            info: Volume {
                id: "T:".to_string(),
                label: "Test".to_string(),
                writable: true,
                total_bytes: FALLBACK_TOTAL,
                free_bytes: FALLBACK_FREE,
                removable: false,
            },
            root: root.to_path_buf(),
        }])
    }

    /// Capacity must never be reported as zero.
    ///
    /// A zero total makes "percent used" a division by zero in every caller that
    /// computes it — rclone's mount crashed programs with SIGFPE exactly this
    /// way — and a zero free count makes file managers refuse to copy, believing
    /// the disk full. A wrong-but-plausible figure breaks nothing; zero breaks
    /// other people's software.
    #[test]
    fn capacity_is_never_zero() {
        // A real path: whatever the system says, it must be usable.
        let (total, free) = capacity_of(Path::new("/"));
        assert!(total > 0, "a real filesystem must report a non-zero size");
        assert!(free <= total, "free space cannot exceed the total");

        // A path that cannot be queried falls back rather than answering zero.
        let (total, free) = capacity_of(Path::new("/nonexistent/path/for/this/test"));
        assert_eq!(total, FALLBACK_TOTAL);
        assert_eq!(free, FALLBACK_FREE);
        assert!(
            total > 0 && free > 0,
            "the fallback itself must be non-zero"
        );
    }

    /// A host with no volumes advertises the module and then answers every
    /// listing with nothing, which reads as a broken link rather than an
    /// unsupported platform — so each host this builds for must find at least
    /// one.
    #[test]
    fn a_supported_host_finds_something_to_share() {
        assert!(
            !detect_volumes().is_empty(),
            "this platform serves file requests but offers no volume"
        );
    }

    /// Every detected volume carries a usable capacity, since that is what the
    /// mount turns into `statfs`.
    #[test]
    fn detected_volumes_report_capacity() {
        for v in detect_volumes() {
            assert!(
                v.info.total_bytes > 0,
                "volume {:?} reported a zero total",
                v.info.id
            );
            assert!(
                v.info.free_bytes <= v.info.total_bytes,
                "volume {:?} reports more free than total",
                v.info.id
            );
        }
    }

    /// A path from the peer must never address anything outside its volume.
    ///
    /// The request is a string from another machine, so this is the boundary
    /// that keeps "share my disk" from meaning "share everything above it too".
    #[test]
    fn a_path_cannot_escape_its_volume() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let srv = server_on(tmp.path());

        for escape in ["../etc/shadow", "a/../../etc/shadow", "..", "a/.."] {
            let got = srv.resolve("T:", escape);
            assert!(
                matches!(got, Err(FileError::Denied)),
                "{escape:?} must be denied, got {got:?}"
            );
        }

        // A leading separator is not an escape: wire paths are volume-relative,
        // so `/etc/shadow` means `etc/shadow` *within this volume*. What matters
        // is that it lands under the root, which is asserted here rather than
        // assumed.
        let rooted = srv.resolve("T:", "/etc/shadow").expect("resolves");
        assert!(
            rooted.starts_with(tmp.path()),
            "a rooted-looking path must still land inside the volume, got {rooted:?}"
        );

        // Every accepted path must stay under the root, whatever it looks like.
        for ok in ["a/b", "./a", "a/./b", "//a//b//"] {
            let got = srv.resolve("T:", ok).expect("resolves");
            assert!(
                got.starts_with(tmp.path()),
                "{ok:?} escaped the volume: {got:?}"
            );
        }
    }

    /// Ordinary paths resolve under the volume root, separators and all.
    #[test]
    fn ordinary_paths_resolve_under_the_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let srv = server_on(tmp.path());

        let got = srv.resolve("T:", "a/b/c.txt").expect("resolves");
        assert_eq!(got, tmp.path().join("a").join("b").join("c.txt"));

        // Leading, doubled and trailing separators are noise, not components.
        let same = srv.resolve("T:", "/a//b/c.txt/").expect("resolves");
        assert_eq!(same, got, "separator noise must not change the target");

        // The volume root itself.
        assert_eq!(srv.resolve("T:", "").expect("root"), tmp.path());
    }

    /// A volume set changing under an open file must not disturb the file.
    ///
    /// Plugging a stick in, or pulling a disk out, changes the *set* — and the
    /// rescan that notices used to swap in a whole fresh `FileServer`. That
    /// dropped the open-file table with it, so every handle the peer held, on
    /// every volume, became `BadHandle` on its next use: a copy in progress
    /// failed at whatever point the user happened to touch an unrelated disk.
    #[test]
    fn a_volume_set_change_keeps_open_handles_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), b"before").expect("seed");
        let mut srv = server_on(tmp.path());

        let (handle, _) = srv
            .open(
                "T:",
                "a.txt",
                OpenMode {
                    read: true,
                    write: true,
                    create: false,
                    truncate: false,
                    exclusive: false,
                },
            )
            .expect("opens");

        // A rescan finds a second volume: the set changed, so the session
        // adopts it.
        let mut volumes = server_on(tmp.path()).ns.volumes.clone();
        volumes.push(SharedVolume {
            info: Volume {
                id: "U:".to_string(),
                label: "Added".to_string(),
                writable: true,
                total_bytes: FALLBACK_TOTAL,
                free_bytes: FALLBACK_FREE,
                removable: true,
            },
            root: tmp.path().to_path_buf(),
        });
        let scanned = FileServer::new(volumes);
        srv.adopt_volumes(scanned);

        assert_eq!(srv.volumes().len(), 2, "the new set is in place");
        assert_eq!(
            srv.write(handle, 0, b"after!").expect("still writable"),
            6,
            "the handle must survive a volume set change"
        );
    }

    /// Handle numbers must never be reused while the peer still holds them.
    ///
    /// The rescan that replaced the whole server also reset `next_handle` to 1,
    /// so after any volume change the next file opened was handed a number the
    /// peer was still using for a different file — and a write meant for one
    /// landed in the other. Silent corruption, which is worse than the lost
    /// handles above.
    #[test]
    fn handle_numbers_are_not_reused_after_a_volume_set_change() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), b"a").expect("seed");
        std::fs::write(tmp.path().join("b.txt"), b"b").expect("seed");
        let mut srv = server_on(tmp.path());

        let read_only = OpenMode {
            read: true,
            write: false,
            create: false,
            truncate: false,
            exclusive: false,
        };
        let (first, _) = srv.open("T:", "a.txt", read_only).expect("opens");

        srv.adopt_volumes(server_on(tmp.path()));

        let (second, _) = srv.open("T:", "b.txt", read_only).expect("opens");
        assert_ne!(
            first, second,
            "a handle still held by the peer must not be handed out again"
        );
    }

    /// Disk work must not hold the server's lock.
    ///
    /// This is the property that decides whether requests run concurrently or
    /// one at a time. It was not held: `server.lock().write(..)` keeps the
    /// temporary guard alive to the end of the expression, so every request in
    /// the system — for every file — queued behind whichever write was first.
    /// Measured against a peer downloading a game, that was 11 writes a second
    /// with 0.2 operations in flight against a window of 128.
    ///
    /// Asserted by holding the lock and doing I/O at the same time: if the
    /// handle still borrowed from the server, this would not compile, and if
    /// the I/O needed the lock, it would deadlock.
    #[test]
    fn disk_work_does_not_need_the_server_lock() {
        use std::sync::Mutex;

        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.bin"), b"0123456789").expect("seed");
        let server = Mutex::new(server_on(tmp.path()));

        let handle = {
            let mut guard = server.lock().expect("server");
            let (h, _) = guard
                .open(
                    "T:",
                    "a.bin",
                    OpenMode {
                        read: true,
                        write: true,
                        create: false,
                        truncate: false,
                        exclusive: false,
                    },
                )
                .expect("opens");
            h
        };

        let file = server
            .lock()
            .expect("server")
            .detach(handle)
            .expect("detaches");

        // The lock is free while the file is being used: taking it here would
        // hang if the handle still held it.
        let held = server.lock().expect("server");
        assert_eq!(file.read(0, 4).expect("reads"), b"0123");
        file.write(0, b"ABCD").expect("writes");
        assert_eq!(file.read(0, 4).expect("reads"), b"ABCD");
        drop(held);
    }

    /// Positioned I/O must not move a cursor other operations depend on.
    ///
    /// A detached handle is shared, so two operations on the same file run at
    /// once. With `seek` + `read` they would move one cursor under each other
    /// and return each other's bytes.
    #[test]
    fn concurrent_operations_on_one_file_do_not_disturb_each_other() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.bin"), (0..=255u8).collect::<Vec<_>>()).expect("seed");
        let mut srv = server_on(tmp.path());
        let (handle, _) = srv
            .open(
                "T:",
                "a.bin",
                OpenMode {
                    read: true,
                    write: false,
                    create: false,
                    truncate: false,
                    exclusive: false,
                },
            )
            .expect("opens");
        let file = std::sync::Arc::new(srv.detach(handle).expect("detaches"));

        // Interleaved reads at different offsets, each of which must see its
        // own bytes.
        let threads: Vec<_> = (0..8u64)
            .map(|i| {
                let file = std::sync::Arc::clone(&file);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let at = i * 16;
                        let got = file.read(at, 16).expect("reads");
                        assert_eq!(
                            got[0], at as u8,
                            "a concurrent read returned another read's bytes"
                        );
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("thread finishes");
        }
    }

    /// Reserving space must actually commit it, not leave a hole.
    ///
    /// The distinction is the whole point of the request: an installer reserves
    /// a download's full size up front so that a disk filling later cannot
    /// leave it half-written. `set_len` reports success while writing nothing,
    /// which is how 55.9 GB of Steam's "allocated" files came to occupy 904 KB
    /// — and why its preallocation pass stalled partway through.
    #[test]
    #[cfg(target_os = "linux")]
    fn reserving_space_commits_blocks_rather_than_making_a_hole() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.bin"), b"").expect("seed");
        let mut srv = server_on(tmp.path());
        let write = OpenMode {
            read: true,
            write: true,
            create: false,
            truncate: false,
            exclusive: false,
        };

        // A truncate is allowed to be a hole: nothing promised the space.
        let (h, _) = srv.open("T:", "a.bin", write).expect("opens");
        srv.set_size(h, 1024 * 1024, false).expect("truncates");
        let holey = tmp.path().join("a.bin").metadata().expect("stat");
        assert_eq!(holey.len(), 1024 * 1024, "the file is a megabyte long");
        assert_eq!(holey.blocks(), 0, "and occupies nothing: it is a hole");

        // A reservation must not be.
        srv.set_size(h, 1024 * 1024, true).expect("reserves");
        let committed = tmp.path().join("a.bin").metadata().expect("stat");
        assert!(
            committed.blocks() * 512 >= 1024 * 1024,
            "reserved space must exist on the medium; got {} bytes of blocks",
            committed.blocks() * 512
        );
    }

    /// A reservation commits room; it does not say how long the file is.
    ///
    /// Windows keeps the two apart (`AllocationSize` against `EndOfFile`), and
    /// an installer reserves a whole download before writing a byte of it.
    /// Moving the end of the file to the reservation made the mount report
    /// the full size as content, and anything appending wrote after gigabytes
    /// of zeroes.
    #[test]
    #[cfg(target_os = "linux")]
    fn reserving_space_leaves_the_length_alone_and_truncates_below_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.bin"), b"0123456789").expect("seed");
        let mut srv = server_on(tmp.path());
        let write = OpenMode {
            read: true,
            write: true,
            create: false,
            truncate: false,
            exclusive: false,
        };
        let (h, _) = srv.open("T:", "a.bin", write).expect("opens");

        srv.set_size(h, 1024 * 1024, true).expect("reserves");
        let len = tmp.path().join("a.bin").metadata().expect("stat").len();
        assert_eq!(len, 10, "a reservation must not move the end of the file");

        // An allocation below the length is the one case that does: Windows
        // truncates a file whose allocation shrinks past its end.
        srv.set_size(h, 4, true).expect("shrinks");
        let len = tmp.path().join("a.bin").metadata().expect("stat").len();
        assert_eq!(len, 4);
    }

    /// A reservation on a read-only handle is refused like any other write.
    #[test]
    fn reserving_space_needs_a_writable_handle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.bin"), b"x").expect("seed");
        let mut srv = server_on(tmp.path());
        let (h, _) = srv
            .open(
                "T:",
                "a.bin",
                OpenMode {
                    read: true,
                    write: false,
                    create: false,
                    truncate: false,
                    exclusive: false,
                },
            )
            .expect("opens");

        assert!(matches!(
            srv.set_size(h, 4096, true),
            Err(FileError::ReadOnly)
        ));
    }

    /// The peer's own disks must not be served back to it.
    ///
    /// Linux offers `/`, and the peer's drives are mounted below it at
    /// `~/Cursio/<peer>/…`. Serving that path sends the peer its own files
    /// across the network to come home under a new drive letter — seen live as
    /// a machine with two disks announcing six, four of which were the asking
    /// machine's own. Following one is a round trip for a file the caller
    /// already has, and a directory walk into it recurses.
    #[test]
    fn the_peers_own_mounts_are_never_served_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let loops = tmp.path().join("Cursio");
        std::fs::create_dir_all(loops.join("Lightmoon").join("V")).expect("peer mount");
        std::fs::write(tmp.path().join("ours.txt"), b"ours").expect("seed");

        let mut srv = server_on(tmp.path());
        srv.exclude(vec![loops.clone()]);

        // Refused however it is addressed.
        assert!(matches!(
            srv.resolve("T:", "Cursio/Lightmoon/V"),
            Err(FileError::Denied)
        ));
        assert!(matches!(
            srv.resolve("T:", "Cursio/Lightmoon/V/SteamLibrary"),
            Err(FileError::Denied)
        ));
        // And the directory above it does not even offer it.
        let listing = srv.read_dir("T:", "").expect("lists");
        assert!(
            listing.iter().all(|e| e.name != "Cursio"),
            "the peer's own mounts must not appear in a listing: {:?}",
            listing.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        // Everything else on the volume is unaffected.
        assert!(listing.iter().any(|e| e.name == "ours.txt"));
        assert!(srv.resolve("T:", "ours.txt").is_ok());
    }

    /// An unknown volume is not a path error — there is nothing to resolve
    /// against.
    #[test]
    fn an_unknown_volume_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let srv = server_on(tmp.path());
        assert!(matches!(
            srv.resolve("Z:", "anything"),
            Err(FileError::BadHandle)
        ));
    }

    /// Read/write/stat round-trip through the same API the peer drives.
    #[test]
    fn a_file_round_trips_through_the_server() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut srv = server_on(tmp.path());

        let (h, size) = srv
            .open(
                "T:",
                "hello.txt",
                OpenMode {
                    read: true,
                    write: true,
                    create: true,
                    truncate: false,
                    exclusive: false,
                },
            )
            .expect("creates");
        assert_eq!(size, 0, "a fresh file is empty");

        assert_eq!(srv.write(h, 0, b"hello world").expect("writes"), 11);
        assert_eq!(srv.read(h, 0, 11).expect("reads"), b"hello world");
        // Past the end returns short rather than failing.
        assert_eq!(srv.read(h, 6, 100).expect("reads"), b"world");

        srv.truncate(h, 5).expect("truncates");
        assert_eq!(srv.read(h, 0, 100).expect("reads"), b"hello");
        srv.close(h);

        let meta = srv.stat("T:", "hello.txt").expect("stats");
        assert_eq!(meta.size, 5);
        assert_eq!(meta.kind, FileKind::File);

        // A closed handle is no longer usable.
        assert!(matches!(srv.read(h, 0, 1), Err(FileError::BadHandle)));
    }

    /// A read-only volume refuses every mutating request, including the open.
    #[test]
    fn a_read_only_volume_refuses_writes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("f.txt"), b"data").expect("seed");
        let mut srv = FileServer::new(vec![SharedVolume {
            info: Volume {
                id: "R:".to_string(),
                label: "ReadOnly".to_string(),
                writable: false,
                total_bytes: FALLBACK_TOTAL,
                free_bytes: FALLBACK_FREE,
                removable: false,
            },
            root: tmp.path().to_path_buf(),
        }]);

        assert!(matches!(
            srv.open(
                "R:",
                "f.txt",
                OpenMode {
                    write: true,
                    ..OpenMode::read_only()
                }
            ),
            Err(FileError::ReadOnly)
        ));
        assert!(matches!(srv.mkdir("R:", "new"), Err(FileError::ReadOnly)));
        assert!(matches!(
            srv.remove("R:", "f.txt"),
            Err(FileError::ReadOnly)
        ));

        // Reading still works — that is the point of a read-only share.
        let (h, _) = srv
            .open("R:", "f.txt", OpenMode::read_only())
            .expect("opens");
        assert_eq!(srv.read(h, 0, 4).expect("reads"), b"data");
    }

    /// A listing describes links as links, so the mount side decides what to do
    /// with one rather than being handed its target silently.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_symlink_is_reported_as_a_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink("/etc/shadow", tmp.path().join("escape")).expect("symlink");
        let srv = server_on(tmp.path());

        let entries = srv.read_dir("T:", "").expect("lists");
        let link = entries
            .iter()
            .find(|e| e.name == "escape")
            .expect("present");
        assert_eq!(
            link.kind,
            FileKind::Symlink,
            "a link must not be reported as its target"
        );
    }

    /// A link's target crosses the wire exactly as stored, including one that
    /// leads out of the volume.
    ///
    /// Reporting a link without being able to answer for it is what made every
    /// symlink on the volume unreadable and untraversable. Resolving it here
    /// instead would be worse than useless: `/etc/shadow` is outside the shared
    /// tree, and the mount side can only recognise that if it receives the
    /// target unresolved.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_links_target_is_returned_unresolved() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink("/etc/shadow", tmp.path().join("escape")).expect("symlink");
        std::os::unix::fs::symlink("inside.txt", tmp.path().join("relative")).expect("symlink");
        let srv = server_on(tmp.path());

        assert_eq!(srv.read_link("T:", "escape").expect("reads"), "/etc/shadow");
        assert_eq!(
            srv.read_link("T:", "relative").expect("reads"),
            "inside.txt"
        );
    }

    /// Reading a link on something that is not one says so, rather than
    /// surfacing the `InvalidInput` the OS reports for it — which is
    /// indistinguishable from a malformed request.
    #[cfg(target_os = "linux")]
    #[test]
    fn reading_a_link_on_a_regular_file_reports_the_wrong_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("plain.txt"), b"data").expect("writes");
        let srv = server_on(tmp.path());

        assert!(matches!(
            srv.read_link("T:", "plain.txt"),
            Err(FileError::WrongKind)
        ));
        assert!(matches!(
            srv.read_link("T:", "missing"),
            Err(FileError::NotFound)
        ));
    }

    /// A link cannot be used to read outside the volume, the same as any other
    /// path: the escape is refused before the link is ever consulted.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_request_cannot_escape_its_volume() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let srv = server_on(tmp.path());

        assert!(matches!(
            srv.read_link("T:", "../outside"),
            Err(FileError::Denied)
        ));
    }
}
