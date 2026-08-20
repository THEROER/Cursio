//! The FUSE filesystem itself.
//!
//! Callbacks arrive on fuser's own threads — several at once, and with `&self`
//! — so all mutable state lives behind a mutex and every reply is sent from the
//! thread that received the call. Nothing here touches the tokio runtime; see
//! [`super::bridge`] for why.

use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::Duration;

use cursio_protocol::{FileError, FileKind, OpenMode};
use fuser::{
    Errno, FileType, Filesystem, INodeNo, MountOption, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};

use super::attr::{attr_from, dir_attr, root_attr, to_errno, CACHE_TTL};
use super::inode::{join, Entry, InodeTable, ROOT_INO};
use crate::bridge::{mount_threads, PeerLink};
use crate::server::{FALLBACK_FREE, FALLBACK_TOTAL};
use crate::volume_dir_name;

/// Generation number for every inode we hand out.
///
/// Zero throughout: generations only matter when inode numbers are reused
/// across a filesystem's lifetime, which this table never does — a number is
/// retired when the kernel forgets it and the next allocation takes a fresh one.
const GENERATION: fuser::Generation = fuser::Generation(0);

/// The peer's volumes, presented as a filesystem.
pub struct RemoteFs {
    peer: PeerLink,
    state: Mutex<State>,
    uid: u32,
    gid: u32,
}

struct State {
    inodes: InodeTable,
    /// Volume list, fetched once at mount and refreshed when it goes stale.
    /// Cached because it is the mount root's entire contents: without this,
    /// every `ls` of the mount point is a round trip.
    volumes: Vec<cursio_protocol::Volume>,
    volumes_fetched: Option<std::time::Instant>,
    /// Our inode-scoped file handles mapped to the peer's.
    handles: std::collections::HashMap<u64, cursio_protocol::FileHandle>,
    next_handle: u64,
}

/// How long the volume list is trusted before being refetched.
///
/// Far longer than [`CACHE_TTL`]: drives do not come and go the way file
/// contents change, and this list is consulted on every path resolution.
const VOLUMES_TTL: Duration = Duration::from_secs(30);

impl RemoteFs {
    pub fn new(peer: PeerLink) -> Self {
        Self {
            peer,
            state: Mutex::new(State {
                inodes: InodeTable::new(),
                volumes: Vec::new(),
                volumes_fetched: None,
                handles: std::collections::HashMap::new(),
                next_handle: 1,
            }),
            // Safe: these are plain getuid/getgid calls with no preconditions.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// The peer's volumes, refetched when the cached list has expired.
    fn volumes(&self) -> Result<Vec<cursio_protocol::Volume>, FileError> {
        {
            let state = self.state.lock().expect("inode state");
            if let Some(at) = state.volumes_fetched {
                if at.elapsed() < VOLUMES_TTL && !state.volumes.is_empty() {
                    return Ok(state.volumes.clone());
                }
            }
        }
        // Fetched without the lock held: this is a network round trip, and
        // holding the mutex across it would serialise every other callback
        // behind it.
        let fetched = self.peer.volumes()?;
        let mut state = self.state.lock().expect("inode state");
        state.volumes = fetched.clone();
        state.volumes_fetched = Some(std::time::Instant::now());
        Ok(fetched)
    }

    /// The peer's handle behind one of ours.
    fn remote_handle(&self, fh: fuser::FileHandle) -> Option<cursio_protocol::FileHandle> {
        self.state
            .lock()
            .expect("inode state")
            .handles
            .get(&fh.0)
            .copied()
    }

    /// Resolve `(parent inode, child name)` to the wire path it addresses.
    ///
    /// Refuses anything directly under the mount root: that level holds the
    /// volumes themselves, which are ours to present, not the peer's to create.
    fn child_path(&self, parent: INodeNo, name: &OsStr) -> Result<(String, String), Errno> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        match self.entry_of(parent.0) {
            Some(Entry::Path { volume, path }) => Ok((volume, join(&path, name))),
            // Creating or deleting a "volume" is not a thing.
            Some(Entry::Root) => Err(Errno::EPERM),
            None => Err(Errno::ENOENT),
        }
    }

    /// Open a file solely to truncate it, for a `truncate()` that came without
    /// a handle.
    fn open_for_truncate(&self, ino: INodeNo) -> Option<cursio_protocol::FileHandle> {
        let Some(Entry::Path { volume, path }) = self.entry_of(ino.0) else {
            return None;
        };
        let mode = OpenMode {
            read: false,
            write: true,
            create: false,
            truncate: false,
            exclusive: false,
        };
        self.peer.open(&volume, &path, mode).ok().map(|(h, _)| h)
    }

    /// Shared body of `unlink` and `rmdir` — the peer distinguishes files from
    /// directories itself, so both are the same request.
    fn remove_child(&self, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let (volume, path) = match self.child_path(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        match self.peer.remove(&volume, &path) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(to_errno(&e)),
        }
    }

    /// Resolve an inode to the path it names.
    fn entry_of(&self, ino: u64) -> Option<Entry> {
        self.state
            .lock()
            .expect("inode state")
            .inodes
            .get(ino)
            .cloned()
    }
}

impl Filesystem for RemoteFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            // A name the peer could never have sent: wire paths are UTF-8.
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(parent_entry) = self.entry_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match parent_entry {
            // Children of the mount root are the volumes themselves.
            Entry::Root => {
                let volumes = match self.volumes() {
                    Ok(v) => v,
                    Err(e) => return reply.error(to_errno(&e)),
                };
                let Some(vol) = volumes.iter().find(|v| volume_dir_name(&v.id) == name) else {
                    return reply.error(Errno::ENOENT);
                };
                let ino = self
                    .state
                    .lock()
                    .expect("inode state")
                    .inodes
                    .lookup(&vol.id, "");
                reply.entry(&CACHE_TTL, &dir_attr(ino, self.uid, self.gid), GENERATION);
            }
            Entry::Path { volume, path } => {
                let child = join(&path, name);
                let meta = match self.peer.stat(&volume, &child) {
                    Ok(m) => m,
                    Err(e) => return reply.error(to_errno(&e)),
                };
                let ino = self
                    .state
                    .lock()
                    .expect("inode state")
                    .inodes
                    .lookup(&volume, &child);
                reply.entry(
                    &CACHE_TTL,
                    &attr_from(ino, &meta, self.uid, self.gid),
                    GENERATION,
                );
            }
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.state
            .lock()
            .expect("inode state")
            .inodes
            .forget(ino.0, nlookup);
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        match self.entry_of(ino.0) {
            Some(Entry::Root) => reply.attr(&CACHE_TTL, &root_attr(self.uid, self.gid)),
            // A volume's own directory is ours, not the peer's, so its
            // attributes are synthesised rather than fetched.
            Some(Entry::Path { path, .. }) if path.is_empty() => {
                reply.attr(&CACHE_TTL, &dir_attr(ino.0, self.uid, self.gid))
            }
            Some(Entry::Path { volume, path }) => match self.peer.stat(&volume, &path) {
                Ok(meta) => reply.attr(&CACHE_TTL, &attr_from(ino.0, &meta, self.uid, self.gid)),
                Err(e) => reply.error(to_errno(&e)),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    /// Read a symlink's target.
    ///
    /// The kernel asks this for anything a listing reported as a link, and the
    /// peer answers with the target exactly as stored. Nothing is resolved on
    /// either side: the string is interpreted against this mounted tree, so a
    /// link pointing out of the volume dangles here rather than reaching into
    /// the owner's disk.
    ///
    /// Answering matters beyond the one entry — while this was `ENOSYS`, every
    /// link was an object that could be neither read nor traversed, and a
    /// directory walk tripped over the same one again and again, each attempt a
    /// wasted round trip.
    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.entry_of(ino.0) {
            // The root and the volume directories are ours, and neither is a
            // link.
            Some(Entry::Root) => reply.error(Errno::EINVAL),
            Some(Entry::Path { path, .. }) if path.is_empty() => reply.error(Errno::EINVAL),
            Some(Entry::Path { volume, path }) => match self.peer.read_link(&volume, &path) {
                Ok(target) => reply.data(target.as_bytes()),
                Err(e) => reply.error(to_errno(&e)),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    /// Nothing to flush.
    ///
    /// Writes are not buffered here: each one is a `Write` request answered by
    /// the peer before the caller returns, so by the time a `close()` reaches
    /// this there is nothing left to push. Left unimplemented it answers
    /// `ENOSYS` — which the kernel tolerates, but which logs a warning on every
    /// single file close.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(entry) = self.entry_of(ino.0) else {
            return reply.error(Errno::ENOENT);
        };

        // `.` and `..` come first and are part of the offset sequence, so a
        // resumed readdir skips them along with everything else already sent.
        let mut listing: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (ROOT_INO, FileType::Directory, "..".to_string()),
        ];

        match &entry {
            Entry::Root => {
                let volumes = match self.volumes() {
                    Ok(v) => v,
                    Err(e) => return reply.error(to_errno(&e)),
                };
                let mut state = self.state.lock().expect("inode state");
                for vol in &volumes {
                    // A readdir entry does not give the kernel a reference —
                    // only lookup and readdirplus do — so the inode is resolved
                    // without counting one. Counting here would leak entries
                    // that are never forgotten.
                    let child = state.inodes.lookup(&vol.id, "");
                    state.inodes.forget(child, 1);
                    listing.push((child, FileType::Directory, volume_dir_name(&vol.id)));
                }
            }
            Entry::Path { volume, path } => {
                let entries = match self.peer.read_dir(volume, path) {
                    Ok(e) => e,
                    Err(e) => return reply.error(to_errno(&e)),
                };
                let mut state = self.state.lock().expect("inode state");
                for meta in entries {
                    let child_path = join(path, &meta.name);
                    let child = state.inodes.lookup(volume, &child_path);
                    state.inodes.forget(child, 1);
                    let kind = match meta.kind {
                        FileKind::Dir => FileType::Directory,
                        FileKind::Symlink => FileType::Symlink,
                        FileKind::File => FileType::RegularFile,
                    };
                    listing.push((child, kind, meta.name));
                }
            }
        }

        // The offset the kernel sends back is the one attached to the last
        // entry it accepted, and it means "resume after this" — so each entry
        // carries the index of the NEXT one. Getting this off by one either
        // loops forever or silently drops a file.
        for (i, (child_ino, kind, name)) in listing.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child_ino), i as u64 + 1, kind, name) {
                // Buffer full: stop here and let the kernel ask again from the
                // offset of the last entry that fit.
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // fuser's default answers all zeros, which is actively harmful: callers
        // compute "percent used" from these numbers, so a zero total is a
        // division by zero in someone else's process — rclone's mount crashed
        // programs with SIGFPE this way — and a zero free count makes file
        // managers refuse to copy, believing the disk full.
        //
        // Several volumes are summed into one figure, because the mount is one
        // filesystem as far as `statfs` is concerned. The peer's own fallback
        // guarantees each is non-zero, so the total is too.
        let (total, free) = match self.volumes() {
            Ok(volumes) if !volumes.is_empty() => volumes.iter().fold((0u64, 0u64), |(t, f), v| {
                (
                    t.saturating_add(v.total_bytes),
                    f.saturating_add(v.free_bytes),
                )
            }),
            // The peer has not said yet, or cannot be reached. Answering an
            // error here would make `df` and every file manager show the mount
            // as broken while it is merely still waking up.
            _ => (FALLBACK_TOTAL, FALLBACK_FREE),
        };

        let bsize = 4096u32;
        let blocks = total / bsize as u64;
        let bfree = free / bsize as u64;
        reply.statfs(
            blocks,
            bfree,
            // `bavail` — what an unprivileged process may use. No root
            // reservation exists here, so it equals `bfree`.
            bfree,
            // Inode counts are not modelled: the peer does not report them, and
            // inventing a total would make `df -i` show a fabricated ratio.
            // `statfs(2)` says undefined fields are `-1`, which `df -i` renders
            // as "-" rather than as a real zero-inode filesystem. (NFS answers 0
            // here instead; -1 is what the manual actually asks for.) Unlike the
            // block counts above, nothing divides by these.
            u64::MAX,
            u64::MAX,
            bsize,
            255,
            bsize,
        );
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
        let Some(Entry::Path { volume, path }) = self.entry_of(ino.0) else {
            // The root and volume directories are not openable as files.
            return reply.error(Errno::EISDIR);
        };

        let mode = open_mode_from(flags);

        match self.peer.open(&volume, &path, mode) {
            Ok((remote, _size)) => {
                let mut state = self.state.lock().expect("inode state");
                let fh = state.next_handle;
                state.next_handle += 1;
                state.handles.insert(fh, remote);
                // No FopenFlags: the kernel may cache this file's pages, which
                // is what makes sequential reads fast over a slow link.
                reply.opened(fuser::FileHandle(fh), fuser::FopenFlags::empty());
            }
            Err(e) => reply.error(to_errno(&e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let remote = {
            let state = self.state.lock().expect("inode state");
            state.handles.get(&fh.0).copied()
        };
        let Some(remote) = remote else {
            return reply.error(Errno::EBADF);
        };
        match self.peer.read(remote, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(to_errno(&e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock: Option<fuser::LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        let Some(remote) = self.remote_handle(fh) else {
            return reply.error(Errno::EBADF);
        };
        match self.peer.write(remote, offset, data.to_vec()) {
            // A short write would make the caller retry the remainder; the peer
            // writes all or fails, so this is always the full length.
            Ok(written) => reply.written(written as u32),
            Err(e) => reply.error(to_errno(&e)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let (volume, path) = match self.child_path(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };

        let mut mode = open_mode_from(fuser::OpenFlags(flags));
        // This callback exists precisely to create the file, whatever the flags
        // say about opening one that already exists.
        mode.create = true;
        mode.exclusive = flags & libc::O_EXCL != 0;

        let (remote, size) = match self.peer.open(&volume, &path, mode) {
            Ok(v) => v,
            Err(e) => return reply.error(to_errno(&e)),
        };

        let (ino, fh) = {
            let mut state = self.state.lock().expect("inode state");
            let ino = state.inodes.lookup(&volume, &path);
            let fh = state.next_handle;
            state.next_handle += 1;
            state.handles.insert(fh, remote);
            (ino, fh)
        };

        let meta = cursio_protocol::FileMeta {
            name: path.rsplit('/').next().unwrap_or(&path).to_string(),
            kind: FileKind::File,
            size,
            modified_unix: now_unix(),
            writable: true,
        };
        reply.created(
            &CACHE_TTL,
            &attr_from(ino, &meta, self.uid, self.gid),
            GENERATION,
            fuser::FileHandle(fh),
            fuser::FopenFlags::empty(),
        );
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let (volume, path) = match self.child_path(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        if let Err(e) = self.peer.mkdir(&volume, &path) {
            return reply.error(to_errno(&e));
        }
        let ino = self
            .state
            .lock()
            .expect("inode state")
            .inodes
            .lookup(&volume, &path);
        reply.entry(&CACHE_TTL, &dir_attr(ino, self.uid, self.gid), GENERATION);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_child(parent, name, reply);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_child(parent, name, reply);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (volume, from) = match self.child_path(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        let (new_volume, to) = match self.child_path(newparent, newname) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        // A move between volumes is a copy plus a delete, which the caller must
        // do itself — exactly as it would locally across two filesystems.
        if volume != new_volume {
            return reply.error(Errno::EXDEV);
        }
        if let Err(e) = self.peer.rename(&volume, &from, &to) {
            return reply.error(to_errno(&e));
        }
        // The kernel keeps using the same inode after a rename, so the mapping
        // has to follow the file or every later read fails on a path that no
        // longer exists.
        let mut state = self.state.lock().expect("inode state");
        let ino = state.inodes.lookup(&volume, &from);
        state.inodes.forget(ino, 1);
        state.inodes.rename(ino, &volume, &to);
        drop(state);
        reply.ok();
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<fuser::FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Only truncation is honoured. Modes, owners and timestamps have no
        // faithful meaning across Linux and Windows, and silently accepting
        // them would report success for something that never happened — so
        // they are ignored, which is what `chmod` on a foreign filesystem does
        // anyway.
        if let Some(size) = size {
            let remote = fh
                .and_then(|fh| self.remote_handle(fh))
                .or_else(|| self.open_for_truncate(ino));
            let Some(remote) = remote else {
                return reply.error(Errno::EBADF);
            };
            if let Err(e) = self.peer.truncate(remote, size) {
                return reply.error(to_errno(&e));
            }
        }
        self.getattr(_req, ino, fh, reply);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        _lock: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let remote = self
            .state
            .lock()
            .expect("inode state")
            .handles
            .remove(&fh.0);
        if let Some(remote) = remote {
            self.peer.close(remote);
        }
        reply.ok();
    }
}

/// Now, as seconds since the epoch — for a file we just created and whose
/// timestamp the peer has not yet told us.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Translate the kernel's open flags into what the peer understands.
///
/// `OpenFlags` is a transparent wrapper over the raw `open(2)` flags, so the
/// access mode comes from its accessor and the rest are tested directly.
fn open_mode_from(flags: fuser::OpenFlags) -> OpenMode {
    let acc = flags.acc_mode();
    OpenMode {
        read: !matches!(acc, fuser::OpenAccMode::O_WRONLY),
        write: !matches!(acc, fuser::OpenAccMode::O_RDONLY),
        // The kernel resolves creation itself and calls `create`, so an `open`
        // never has to make the file.
        create: false,
        truncate: flags.0 & libc::O_TRUNC != 0,
        exclusive: false,
    }
}

/// A live mount. Dropping it unmounts.
pub struct MountHandle {
    /// Dropping this unmounts and joins the FUSE threads — which is the whole
    /// cleanup path, since `AutoUnmount` is deliberately not used (see `mount`).
    _session: fuser::BackgroundSession,
    mountpoint: std::path::PathBuf,
}

impl MountHandle {
    pub fn mountpoint(&self) -> &std::path::Path {
        &self.mountpoint
    }
}

/// Unmount whatever a previous run left at this path, if anything.
///
/// Best-effort and deliberately quiet about the ordinary case: nothing mounted
/// means `fusermount3` fails, which is exactly what a clean start looks like.
/// `-z` (lazy) so a mount whose peer is gone detaches immediately rather than
/// blocking on a filesystem that will never answer — the point is to get out of
/// the way, and any process still holding it keeps its own view until it exits.
fn release_stale_mount(mountpoint: &std::path::Path) {
    if !is_mounted(mountpoint) {
        return;
    }
    for tool in ["fusermount3", "fusermount"] {
        let ok = std::process::Command::new(tool)
            .args(["-u", "-z"])
            .arg(mountpoint)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            tracing::info!(
                mount = %mountpoint.display(),
                "released a mount left over from a previous run"
            );
            return;
        }
    }
    tracing::warn!(
        mount = %mountpoint.display(),
        "a previous mount is still in place and could not be released; \
         mounting will likely fail — clear it with `fusermount3 -u`"
    );
}

/// Whether this path is currently a mount point.
///
/// Read from `/proc/mounts` rather than by stat'ing the path: stat on a wedged
/// FUSE mount is precisely the call that blocks forever, and this runs at
/// startup where a hang would take the whole session down with it.
fn is_mounted(mountpoint: &std::path::Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    // Paths appear with spaces and tabs escaped as octal; comparing the escaped
    // form of ours avoids decoding every line.
    let needle = mountpoint.to_string_lossy().replace(' ', "\\040");
    mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .any(|p| p == needle)
}

/// The session configuration the mount runs under.
///
/// Split out from [`mount`] so the settings can be asserted on without mounting
/// anything: everything here is a decision, and the concurrency ones are silent
/// when wrong — the filesystem still works, only far slower.
fn mount_config(peer_name: &str) -> fuser::Config {
    let mut config = fuser::Config::default();

    // Every callback in this filesystem blocks on a round trip: `PeerLink::call`
    // parks the calling thread until the peer answers. On the default single
    // event loop that serialises the entire mount — one request in flight at a
    // time, each paying the full latency — so copying many small files runs at
    // one file per round trip however much bandwidth is free. The engine at the
    // far end already pipelines (`pump_requests` drains several per pass) and
    // `RemoteFs` takes `&self` behind a mutex, so filling that pipe only ever
    // needed threads to wait in parallel.
    config.n_threads = Some(mount_threads());
    // Give each worker its own `/dev/fuse` fd, so they take requests from the
    // kernel independently instead of contending on a single descriptor.
    config.clone_fd = true;

    config.mount_options = vec![
        MountOption::FSName(format!("cursio-{peer_name}")),
        MountOption::Subtype("cursio".to_string()),
        // The peer's disks are the user's own; nothing here should honour
        // set-user-id bits or device nodes that arrived over a network.
        MountOption::NoSuid,
        MountOption::NoDev,
        // Deliberately NOT `x-gvfs-show`, which is what would put this in a
        // file manager's sidebar as a drive. `x-*` options are userspace ones,
        // interpreted by libmount from an fstab entry; fuser passes CUSTOM
        // straight to the kernel as a mount flag, which never reaches the
        // gvfs monitor and at best is ignored. Presenting a peer's removable
        // medium as a drive needs a gvfs mount rather than a flag here, so the
        // volume is offered separately (see `SharedVolume`) and the sidebar
        // question is left to whatever presents it.
        // Deliberately NOT AutoUnmount: fuser refuses it unless the session ACL
        // is widened to `allow_other` or `allow_root`, and either would expose
        // the peer's files — and through them the peer's whole disk — to every
        // account on this machine. That is a real loss of privacy to buy
        // cleanup after a crash, which `MountHandle`'s Drop already handles on
        // every ordinary exit. A crash leaves a stale mount point, cleared with
        // `fusermount -u`; that is the better trade.
    ];
    config
}

/// Mount the peer's volumes at `mountpoint`, returning once they are visible.
pub fn mount(
    peer: PeerLink,
    mountpoint: impl AsRef<std::path::Path>,
    peer_name: &str,
) -> std::io::Result<MountHandle> {
    let mountpoint = mountpoint.as_ref().to_path_buf();
    // Clear a mount left behind by a previous run before creating anything.
    //
    // Without `AutoUnmount` (see below) a crash — or a process killed while the
    // filesystem was wedged — leaves the old mount in place, and the next start
    // fails with `EEXIST` on a point that looks like an ordinary directory. The
    // user would have to know to run `fusermount3 -u` by hand, so do it here.
    release_stale_mount(&mountpoint);
    std::fs::create_dir_all(&mountpoint)?;

    let config = mount_config(peer_name);
    let session = fuser::Session::new(RemoteFs::new(peer), &mountpoint, &config)?.spawn()?;
    tracing::info!(mount = %mountpoint.display(), peer = peer_name, "peer volumes mounted");
    Ok(MountHandle {
        _session: session,
        mountpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mount check must answer from `/proc/mounts` and never stat the path.
    ///
    /// Stat'ing a wedged FUSE mount is the exact call that blocks forever, and
    /// this runs at startup — a hang there would take the whole session with
    /// it. An ordinary directory is not a mount point, and a path that does not
    /// exist is not either; both must answer instantly.
    #[test]
    fn the_mount_check_never_touches_the_filesystem() {
        let tmp = std::env::temp_dir().join("cursio-not-a-mount-point");
        let _ = std::fs::create_dir_all(&tmp);
        let started = std::time::Instant::now();
        assert!(!is_mounted(&tmp), "a plain directory is not a mount point");
        assert!(!is_mounted(std::path::Path::new("/nonexistent/path/here")));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "the check must be a /proc read, not a filesystem call"
        );
        // The root always is one, so a true answer is reachable too.
        assert!(is_mounted(std::path::Path::new("/")));
        let _ = std::fs::remove_dir(&tmp);
    }

    /// The mount must serve requests concurrently.
    ///
    /// Every callback here blocks on a round trip, so the thread count is the
    /// number of operations that can be in flight. Left at the default of one,
    /// the filesystem still passes every other test and still works — it just
    /// serialises on network latency, which showed up as a copy of many small
    /// files crawling while the link sat idle. Nothing else fails when this
    /// regresses, so it is asserted directly.
    #[test]
    fn the_mount_serves_requests_concurrently() {
        let config = mount_config("peer");
        assert_eq!(
            config.n_threads,
            Some(mount_threads()),
            "a single event loop would serialise every blocking round trip"
        );
        assert!(
            config.clone_fd,
            "workers sharing one fd contend for kernel requests"
        );
    }

    /// The security-relevant mount options must stay on: they are what keeps a
    /// peer's disk from carrying set-user-id bits or device nodes onto this
    /// machine, and nothing observable breaks if they silently go missing.
    #[test]
    fn the_mount_refuses_setuid_and_device_nodes() {
        let config = mount_config("peer");
        assert!(config.mount_options.contains(&MountOption::NoSuid));
        assert!(config.mount_options.contains(&MountOption::NoDev));
        assert_eq!(
            config.acl,
            fuser::SessionACL::Owner,
            "widening the ACL would expose the peer's disk to every local account"
        );
    }

    /// Open flags must survive the crossing, or a file opened for writing
    /// arrives read-only on the peer and every write fails.
    #[test]
    fn open_flags_translate() {
        let ro = open_mode_from(fuser::OpenFlags(libc::O_RDONLY));
        assert!(ro.read && !ro.write, "O_RDONLY reads only");
        assert!(!ro.is_mutating(), "a read-only open must not look mutating");

        let wo = open_mode_from(fuser::OpenFlags(libc::O_WRONLY));
        assert!(!wo.read && wo.write, "O_WRONLY writes only");

        let rw = open_mode_from(fuser::OpenFlags(libc::O_RDWR));
        assert!(rw.read && rw.write, "O_RDWR does both");

        let trunc = open_mode_from(fuser::OpenFlags(libc::O_WRONLY | libc::O_TRUNC));
        assert!(trunc.truncate, "O_TRUNC must reach the peer");
        assert!(trunc.is_mutating());
    }
}
