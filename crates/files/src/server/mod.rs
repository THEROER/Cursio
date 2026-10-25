//! Serving this machine's volumes to the peer.
//!
//! Turns [`FilesMsg`] requests into local filesystem calls. Everything here runs
//! on behalf of *the other machine*, so the guiding rule is that a request is
//! untrusted input: a path is a string that arrived over a socket, not something
//! this process constructed.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use crate::server::detect::detect_volumes;
use crate::server::io::read_at;
use crate::server::io::reserve_space;
use crate::server::io::write_at;
use cursio_protocol::{FileError, FileHandle, FileKind, FileMeta, OpenMode, Volume};

mod capacity;
mod detect;
mod io;

pub(crate) use capacity::capacity_of;
#[cfg(any(
    target_os = "linux",
    all(target_os = "windows", target_env = "msvc"),
    test
))]
pub(crate) use capacity::{FALLBACK_FREE, FALLBACK_TOTAL};

/// A volume this machine offers, and where it actually lives.
#[derive(Clone)]
pub struct SharedVolume {
    pub info: Volume,
    /// Native path the volume's root maps to: `/` or `C:\`.
    pub root: PathBuf,
    /// Whether every path must be checked to stay under `root` once symlinks
    /// are followed. True for a folder offered by a share policy: a whole
    /// volume has nothing outside it for a link to reach, a folder does.
    pub confined: bool,
}

/// Serves the local filesystem to one peer.
///
/// Owns the open-file table: handles are indices into it, so a handle the peer
/// invents addresses nothing.
pub struct FileServer {
    ns: std::sync::Arc<Namespace>,
    open: HashMap<FileHandle, OpenFile>,
    next_handle: FileHandle,
    /// How many handles the peer may hold at once; see [`MAX_OPEN_HANDLES`].
    max_open: usize,
}

/// The most files one peer may hold open here at a time.
///
/// Every handle is a descriptor of this process, which also serves the mesh,
/// the audio and the peers' mounts from the same table. A peer that opened
/// without closing — whatever the reason on its side — used to take the node to
/// its limit, and then everything failed with EMFILE: every other file request,
/// the volume scan (so the peer unmounted every drive), new connections. Past
/// this the peer's open is refused and nothing else is touched. Far above what
/// a real workload holds at once — a game download keeps a few hundred — and
/// far below the limit the node raises itself to at start.
pub const MAX_OPEN_HANDLES: usize = 4096;

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
            max_open: MAX_OPEN_HANDLES,
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

    /// Detect what this machine can share, and offer what `policy` allows of
    /// it. See [`crate::share`].
    pub fn detect_shared(policy: &crate::SharePolicy) -> Self {
        Self::new(policy.apply(detect_volumes()))
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
        Ok((self.adopt(file, mode.write)?, size))
    }

    /// Give a file opened through [`Namespace::open`] a handle, or refuse it
    /// once the peer holds [`MAX_OPEN_HANDLES`] — the file is closed again on
    /// the way out.
    pub fn adopt(&mut self, file: File, writable: bool) -> Result<FileHandle, FileError> {
        if self.open.len() >= self.max_open {
            return Err(FileError::Io(format!(
                "the peer already holds {} files open here; close some first",
                self.open.len()
            )));
        }
        let handle = self.next_handle;
        self.next_handle += 1;
        self.open.insert(
            handle,
            OpenFile {
                file: std::sync::Arc::new(file),
                writable,
            },
        );
        Ok(handle)
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
        // The last component is left unfollowed here: a link inside a shared
        // folder may be listed, stat'ed, renamed or removed as the link it is.
        // Only the operations that go *through* it — see
        // [`Namespace::resolve_through`] — must find its target inside too.
        let parent = match out == vol.root {
            true => None,
            false => out.parent(),
        };
        if vol.confined && parent.is_some_and(|p| !stays_under(&vol.root, p)) {
            return Err(FileError::Denied);
        }
        Ok(out)
    }

    /// As [`Namespace::resolve`], for an operation the OS will carry through a
    /// final symlink — opening a file, listing a directory.
    fn resolve_through(&self, volume: &str, path: &str) -> Result<PathBuf, FileError> {
        let out = self.resolve(volume, path)?;
        let confined_root = self
            .volumes
            .iter()
            .find(|v| v.info.id == volume && v.confined)
            .map(|v| &v.root);
        match confined_root {
            Some(root) if !stays_under(root, &out) => Err(FileError::Denied),
            _ => Ok(out),
        }
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
        let dir = self.resolve_through(volume, path)?;
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
        let target = self.resolve_through(volume, path)?;
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
    ///
    /// The file comes back rather than being dropped here, so a caller holding
    /// the server behind a lock can let go of the lock first: dropping the last
    /// reference is the OS close, and that can wait on the disk.
    pub fn close(&mut self, handle: FileHandle) -> Option<FileHandleRef> {
        self.open.remove(&handle).map(FileHandleRef)
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

/// Whether `path`, with every symlink along it followed, is still under `root`.
///
/// Refusing `..` keeps a request's *spelling* inside the root; a link inside a
/// shared folder pointing at `/etc` would still carry it out, because the OS
/// follows links while opening. So the deepest part of the path that exists is
/// resolved and compared: a name that does not exist yet (a file being
/// created) can only land where its existing parent does.
///
/// A root that cannot be resolved confines to nothing, which is the safe
/// reading of a folder that has vanished.
fn stays_under(root: &Path, path: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let mut probe = path;
    loop {
        match probe.canonicalize() {
            Ok(real) => return real.starts_with(&root),
            Err(_) => match probe.parent() {
                Some(parent) => probe = parent,
                None => return false,
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
            confined: false,
        }])
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
            confined: false,
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

    #[test]
    fn a_peer_holding_too_many_files_is_refused_until_it_closes_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), b"a").expect("seed");
        let mut srv = server_on(tmp.path());
        srv.max_open = 2;

        let first = srv
            .open("T:", "a.txt", OpenMode::read_only())
            .expect("opens");
        srv.open("T:", "a.txt", OpenMode::read_only())
            .expect("opens");
        assert!(matches!(
            srv.open("T:", "a.txt", OpenMode::read_only()),
            Err(FileError::Io(_))
        ));
        // Everything that needs no descriptor still works meanwhile.
        srv.stat("T:", "a.txt").expect("stats");

        srv.close(first.0);
        srv.open("T:", "a.txt", OpenMode::read_only())
            .expect("a closed handle makes room");
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
            confined: false,
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

    /// A shared folder must not lead out of itself through a symlink the OS
    /// would follow while opening — the escape `..` refusal cannot see.
    #[cfg(unix)]
    #[test]
    fn a_shared_folder_refuses_a_path_through_a_link_that_leaves_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let shared = tmp.path().join("Public");
        let secret = tmp.path().join("secret");
        std::fs::create_dir(&shared).expect("shared");
        std::fs::create_dir(&secret).expect("secret");
        std::fs::write(secret.join("key"), b"k").expect("key");
        std::fs::write(shared.join("note.txt"), b"n").expect("note");
        std::os::unix::fs::symlink(&secret, shared.join("way-out")).expect("link");

        let policy =
            crate::SharePolicy::from_args(&[shared.display().to_string()]).expect("policy");
        let mut srv = FileServer::new(policy.apply(Vec::new()));
        let id = srv.volumes()[0].id.clone();

        assert!(srv.stat(&id, "note.txt").is_ok(), "inside is served");
        assert!(
            srv.stat(&id, "way-out").is_ok(),
            "the link itself is inside and may be listed"
        );
        assert!(matches!(
            srv.open(&id, "way-out/key", OpenMode::read_only()),
            Err(FileError::Denied)
        ));
        assert!(matches!(
            srv.stat(&id, "way-out/new-file"),
            Err(FileError::Denied)
        ));
        assert!(matches!(
            srv.read_dir(&id, "way-out"),
            Err(FileError::Denied)
        ));
    }

    /// Read-only is decided here, whatever the peer's mount believes.
    #[test]
    fn a_read_only_root_refuses_every_change() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), b"a").expect("writes");
        let policy = crate::SharePolicy::from_args(&[format!("{}:ro", tmp.path().display())])
            .expect("policy");
        let mut srv = FileServer::new(policy.apply(Vec::new()));
        let id = srv.volumes()[0].id.clone();

        assert!(srv.open(&id, "a.txt", OpenMode::read_only()).is_ok());
        let write = OpenMode {
            read: true,
            write: true,
            create: false,
            truncate: false,
            exclusive: false,
        };
        assert!(matches!(
            srv.open(&id, "a.txt", write),
            Err(FileError::ReadOnly)
        ));
        assert!(matches!(srv.mkdir(&id, "d"), Err(FileError::ReadOnly)));
        assert!(matches!(srv.remove(&id, "a.txt"), Err(FileError::ReadOnly)));
        assert!(matches!(
            srv.rename(&id, "a.txt", "b.txt"),
            Err(FileError::ReadOnly)
        ));
    }
}
