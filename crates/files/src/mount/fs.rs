//! The FUSE filesystem itself.
//!
//! Callbacks arrive on fuser's own threads — several at once, and with `&self`
//! — so all mutable state lives behind a mutex and every reply is sent from the
//! thread that received the call. Nothing here touches the tokio runtime; see
//! [`super::bridge`] for why.
//!
//! ## What is deliberately left to fuser's default
//!
//! Most unimplemented callbacks answer `ENOSYS` *and log a warning each time*,
//! so the ones worth answering are answered — even where the answer is a
//! refusal, since a refusal the kernel can cache beats a question it asks
//! forever. Four are left alone on purpose:
//!
//! - `getlk`/`setlk`: `ENOSYS` is what makes the kernel handle locking itself,
//!   locally. Answering here would advertise locks that coordinate across the
//!   mesh, which nothing in the protocol delivers — two machines would each be
//!   told they hold the same exclusive lock.
//! - `bmap`: only meaningful for a block-backed filesystem, which this is not.
//! - `ioctl`/`poll`: no device semantics to expose, and no readiness to report.
//!
//! These are asked for rarely enough that the warning is not the nuisance the
//! per-`exec` and per-`close` ones were.

use std::ffi::OsStr;
use std::sync::Mutex;

use cursio_protocol::{FileError, FileKind, OpenMode};
use fuser::{
    Errno, FileAttr, FileType, Filesystem, INodeNo, InitFlags, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};

use super::attr::{attr_from, dir_attr, root_attr, to_errno, CACHE_TTL};
use super::inode::{join, Entry, InodeTable, ROOT_INO};
use crate::bridge::{mount_threads, PeerLink};
use crate::server::{FALLBACK_FREE, FALLBACK_TOTAL};
use crate::volume_dir_name;

/// Ask for one kernel capability, saying so if this kernel does not have it.
///
/// Named individually rather than requested as one set: a kernel missing any
/// one of them would otherwise refuse the whole set, and the rest are still
/// worth having.
fn request(config: &mut fuser::KernelConfig, flag: InitFlags, what: &str) {
    if config.add_capabilities(flag).is_err() {
        tracing::debug!(
            what,
            "this kernel does not offer the capability; going without"
        );
    }
}

/// Generation number for every inode we hand out.
///
/// Zero throughout: generations only matter when inode numbers are reused
/// across a filesystem's lifetime, which this table never does — a number is
/// retired when the kernel forgets it and the next allocation takes a fresh one.
const GENERATION: fuser::Generation = fuser::Generation(0);

/// One of the peer's volumes, presented as a filesystem.
pub struct RemoteFs {
    peer: PeerLink,
    state: Mutex<State>,
    /// What the peer last said about a path.
    ///
    /// The kernel caches an answer for `CACHE_TTL` once it has one, but the
    /// question still reaches this filesystem the first time and every time the
    /// entry expires — and an application walking a tree asks about each path
    /// several times over in quick succession. Measured on a real transfer,
    /// `LOOKUP` was 5.4 calls per file and `GETATTR` another 2, against 1.2
    /// writes: two thirds of a copy's round trips were questions about paths
    /// rather than the bytes of them.
    ///
    /// The Windows half has had this since it was written; this side went
    /// without, which is most of why the same copy costs so much more here.
    meta: crate::metacache::MetaCache,
    volumes: crate::metacache::VolumeList,
    uid: u32,
    gid: u32,
    /// The peer volume this mount *is*.
    ///
    /// One mount per volume, because a peer's disk should look here the way it
    /// looks on the machine that owns it: its own root, its own capacity, its
    /// own entry in a file manager's sidebar. Presenting them as folders under
    /// one mount — which is what this did — meant the mount could report only
    /// one capacity for all of them, and a file manager showed a single device
    /// whose free space matched no disk the user has. It is also what the
    /// Windows half does with drive letters, so both sides now agree.
    ///
    /// gvfs picks each FUSE mount up through its UDisks2 monitor as its own
    /// `GProxyMount`, so one mount per volume is exactly one sidebar entry per
    /// volume, each with its own eject button.
    bound: String,
    /// Whose disk this is, for naming it when something walks it.
    peer_name: String,
}

struct State {
    inodes: InodeTable,
    /// Our inode-scoped file handles mapped to the peer's.
    handles: std::collections::HashMap<u64, cursio_protocol::FileHandle>,
    next_handle: u64,
}

/// One `readdirplus` entry, held while the listing is assembled.
///
/// `counts` marks the entries that take a kernel reference when sent — every
/// real child, but not the `.` and `..` positions.
struct Plus {
    ino: u64,
    attr: FileAttr,
    name: String,
    counts: bool,
}

impl RemoteFs {
    pub fn new(peer: PeerLink, bound: impl Into<String>) -> Self {
        Self {
            peer,
            meta: crate::metacache::MetaCache::new(),
            volumes: crate::metacache::VolumeList::default(),
            bound: bound.into(),
            peer_name: String::new(),
            state: Mutex::new(State {
                inodes: InodeTable::new(),
                handles: std::collections::HashMap::new(),
                next_handle: 1,
            }),
            // Safe: these are plain getuid/getgid calls with no preconditions.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// Name the peer this volume belongs to.
    pub fn of_peer(mut self, peer_name: &str) -> Self {
        self.peer_name = peer_name.to_string();
        self
    }

    /// The peer's volumes, refetched when the cached list has expired.
    fn volumes(&self) -> Result<Vec<cursio_protocol::Volume>, FileError> {
        self.volumes.get(|| self.peer.volumes())
    }

    /// Ask the peer about a path, or answer from what it said a moment ago.
    ///
    /// A negative answer is remembered too: an application that checks whether
    /// a file exists before creating it asks about a name that is not there,
    /// and asking the peer again each time costs the same round trip as a real
    /// one.
    fn stat_path(&self, volume: &str, path: &str) -> Result<cursio_protocol::FileMeta, FileError> {
        self.meta
            .get_or_ask(volume, path, || self.peer.stat(volume, path))
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
        // Gone, along with anything inside it if it was a directory.
        self.meta.forget_dir(&volume, &path);
        match self.peer.remove(&volume, &path) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(to_errno(&e)),
        }
    }

    /// Resolve an inode to the path it names.
    ///
    /// The mount root resolves to the bound volume's own root rather than to
    /// [`Entry::Root`]: this mount *is* one volume, so `/` here is that
    /// volume's `/`. Translating in one place keeps every caller — lookup,
    /// getattr, readdir, create, unlink — working on paths without each having
    /// to know that the root is special, and leaves [`Entry::Root`] meaning
    /// only "the inode table's origin".
    fn entry_of(&self, ino: u64) -> Option<Entry> {
        let entry = self
            .state
            .lock()
            .expect("inode state")
            .inodes
            .get(ino)
            .cloned()?;
        Some(match entry {
            Entry::Root => Entry::Path {
                volume: self.bound.clone(),
                path: String::new(),
            },
            other => other,
        })
    }
}

impl Filesystem for RemoteFs {
    /// Ask the kernel to do the work it can do closer to the application.
    ///
    /// Every capability here removes round trips that this filesystem would
    /// otherwise make one at a time, and each of them helps the case that
    /// matters most: a person dragging a folder in a file manager, or an
    /// installer writing a file. Those are single-threaded and strictly
    /// sequential — the parallelism has to come from somewhere else, and the
    /// kernel is the only party positioned to create it without the
    /// application knowing.
    ///
    /// Left unrequested, none of it happens: the default is that the kernel
    /// forwards each operation as the application made it. Measured against
    /// that default, a copy of 300 small files cost 457 ms per file, nearly all
    /// of it round trips.
    ///
    /// Failures are logged and skipped rather than fatal. A capability the
    /// running kernel does not have is a missed optimisation, not a reason to
    /// refuse the mount — and the set differs across the kernels Cursio runs
    /// on.
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // Buffered writes are collected by the kernel and handed over in large
        // pieces, instead of arriving one application-sized write at a time.
        // This is what the Windows half already gets from the WinFsp cache
        // manager; without it the Linux mount carries every 4 KiB write across
        // the link on its own and waits out the round trip.
        request(config, InitFlags::FUSE_WRITEBACK_CACHE, "writeback cache");
        // Lookups and directory reads may overlap. A tree walk is otherwise
        // strictly serial, which is exactly the shape of a file manager's copy.
        request(config, InitFlags::FUSE_PARALLEL_DIROPS, "parallel dirops");
        // `readdir` carries the attributes with it, so the `lookup` per entry
        // that follows a listing never has to be asked for. This is the same
        // idea NFSv4 and SMB2 call compounding: fewer, larger requests.
        request(config, InitFlags::FUSE_DO_READDIRPLUS, "readdirplus");
        request(
            config,
            InitFlags::FUSE_READDIRPLUS_AUTO,
            "adaptive readdirplus",
        );
        // `FUSE_ASYNC_READ` and `FUSE_BIG_WRITES` are not named here: the
        // crate requests both for every filesystem it mounts.
        Ok(())
    }

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
                let meta = match self.stat_path(&volume, &child) {
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

    /// No extended attributes exist here, and that is an answer, not a gap.
    ///
    /// The peer's protocol carries none, so there is nothing to forward. Left
    /// unimplemented this answers `ENOSYS`, which the kernel reads as "this
    /// filesystem cannot do xattrs at all" — but it asks anyway, once per
    /// executable it is about to run, for `security.capability`. `ENODATA`
    /// ("this file has no such attribute") is both the truth and the answer the
    /// kernel caches, so the warning stops repeating on every exec.
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: fuser::ReplyXattr,
    ) {
        reply.error(Errno::NO_XATTR);
    }

    /// The empty attribute list, which `getxattr` above implies.
    ///
    /// A zero `size` asks how large the name list would be rather than for the
    /// list itself; both answers are empty here. Reporting `ENOSYS` instead
    /// makes `cp -a` and `rsync -X` treat the mount as broken rather than as
    /// one that simply carries no attributes.
    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: fuser::ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    /// Nothing to flush.
    ///
    /// Buffering happens in the kernel, not here: with `FUSE_WRITEBACK_CACHE`
    /// the page cache collects an application's small writes and hands them
    /// over in large pieces, but each piece that reaches [`RemoteFs::write`] is
    /// still a `Write` the peer answers before the caller returns. So anything
    /// the kernel had has already been pushed by the time a `close()` arrives,
    /// and there is nothing here that could still fail.
    ///
    /// That is what makes the writeback cache safe to ask for. A scheme that
    /// acknowledged a write before the peer stored it would need this call to
    /// report the failure instead — and would then depend on the caller
    /// checking a `close()` return that most programs ignore.
    ///
    /// Left unimplemented it answers `ENOSYS` — which the kernel tolerates, but
    /// which logs a warning on every single file close.
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
        req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(entry) = self.entry_of(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        // Once per listing: a large directory arrives in several calls, and
        // only the first says a new directory was opened.
        if offset == 0 {
            crate::walk::observe(&self.peer_name, &self.bound, req.pid(), ino.0);
        }

        // `.` and `..` come first and are part of the offset sequence, so a
        // resumed readdir skips them along with everything else already sent.
        let mut listing: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (ROOT_INO, FileType::Directory, "..".to_string()),
        ];

        match &entry {
            // Unreachable: `entry_of` resolves the root to the bound volume's
            // own root, so every listing is the peer's. Listing the volumes
            // here is what the mount did when it stood for all of them.
            Entry::Root => {}
            Entry::Path { volume, path } => {
                let entries = match self.peer.read_dir(volume, path) {
                    Ok(e) => e,
                    Err(e) => return reply.error(to_errno(&e)),
                };
                let mut state = self.state.lock().expect("inode state");
                for meta in entries {
                    let child_path = join(path, &meta.name);
                    let child = state.inodes.resolve(volume, &child_path);
                    let kind = match meta.kind {
                        FileKind::Dir => FileType::Directory,
                        FileKind::Symlink => FileType::Symlink,
                        FileKind::File => FileType::RegularFile,
                    };
                    // Kept for the `lookup` per entry that follows any listing,
                    // exactly as in `readdirplus`: the answers arrived with
                    // this one request and cost nothing to hold.
                    self.meta.put(volume, &child_path, Ok(meta.clone()));
                    listing.push((child, kind, meta.name));
                }
            }
        }

        // The offset the kernel sends back is the one attached to the last
        // entry it accepted, and it means "resume after this" — so each entry
        // carries the index of the NEXT one. Getting this off by one either
        // loops forever or silently drops a file.
        for (i, (child_ino, kind, name)) in listing.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*child_ino), i as u64 + 1, *kind, name) {
                // Buffer full: stop here and let the kernel ask again from the
                // offset of the last entry that fit.
                break;
            }
        }

        // A readdir hands out no references at all, so every inode resolved
        // above is one the kernel will never forget. Those it already holds
        // from an earlier lookup are spared; the rest would accumulate on every
        // re-listing of the same directory.
        {
            let mut state = self.state.lock().expect("inode state");
            for (child_ino, _, _) in &listing {
                state.inodes.release_unreferenced(*child_ino);
            }
        }
        reply.ok();
    }

    /// `readdir` that carries each entry's attributes with it.
    ///
    /// The kernel issues this instead of `readdir` when it intends to stat the
    /// listing anyway — which `ls -l`, every file manager and every `find` do.
    /// Answering `ENOSYS` does not break that, it just makes the kernel fall
    /// back to `readdir` followed by one `lookup` per entry, and each of those
    /// lookups is a `Stat` round trip to the peer. A directory of two hundred
    /// files costs two hundred sequential round trips over the link.
    ///
    /// The peer's `ReadDir` already answers with a full `FileMeta` per entry, so
    /// those attributes are in hand before the kernel asks: one round trip
    /// serves the whole listing.
    ///
    /// Unlike `readdir`, an entry here **does** hand the kernel a reference, so
    /// each one sent is counted — see [`InodeTable::lookup`]. The reference is
    /// taken at the moment the entry goes into the buffer, never before: an
    /// entry that does not fit is one the kernel never received, and counting
    /// it would leak an inode that nothing will ever forget. `.` and `..` are
    /// offset positions rather than entries the kernel references, so they are
    /// emitted without being counted.
    fn readdirplus(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: fuser::ReplyDirectoryPlus,
    ) {
        let Some(entry) = self.entry_of(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        if offset == 0 {
            crate::walk::observe(&self.peer_name, &self.bound, req.pid(), ino.0);
        }

        // Gather the listing first, without touching reference counts: the
        // peer's answer may fail, and a failed reply must leave the table
        // exactly as it was.
        let mut listing: Vec<Plus> = Vec::new();

        // A directory of our own — the mount root or a volume's own directory —
        // has no counterpart on the peer, so its attributes are synthesised the
        // way `getattr` synthesises them.
        let self_attr = match &entry {
            Entry::Root => root_attr(self.uid, self.gid),
            Entry::Path { .. } => dir_attr(ino.0, self.uid, self.gid),
        };
        listing.push(Plus {
            ino: ino.0,
            attr: self_attr,
            name: ".".to_string(),
            counts: false,
        });
        listing.push(Plus {
            ino: ROOT_INO,
            attr: root_attr(self.uid, self.gid),
            name: "..".to_string(),
            counts: false,
        });

        match &entry {
            Entry::Root => {
                let volumes = match self.volumes() {
                    Ok(v) => v,
                    Err(e) => return reply.error(to_errno(&e)),
                };
                let mut state = self.state.lock().expect("inode state");
                for vol in &volumes {
                    // Resolving the number is not the same as referencing it:
                    // the count is taken below, only for entries actually sent.
                    let child = state.inodes.resolve(&vol.id, "");
                    listing.push(Plus {
                        ino: child,
                        attr: dir_attr(child, self.uid, self.gid),
                        name: volume_dir_name(&vol.id),
                        counts: true,
                    });
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
                    let child = state.inodes.resolve(volume, &child_path);
                    // The listing already carries what a `stat` of each entry
                    // would return, so keep it: this one request has answered
                    // every `lookup` and `getattr` that follows a directory
                    // read, which is what makes a listing worth a round trip
                    // instead of one per name. It is the same economy NFSv4 and
                    // SMB2 get by compounding operations into one request —
                    // here the compound already exists on the wire and its
                    // answers were being thrown away.
                    self.meta.put(volume, &child_path, Ok(meta.clone()));
                    listing.push(Plus {
                        ino: child,
                        attr: attr_from(child, &meta, self.uid, self.gid),
                        name: meta.name,
                        counts: true,
                    });
                }
            }
        }

        // Offsets mean the same thing as in `readdir`: each entry carries the
        // index of the next one, and the kernel resumes after the last it took.
        let mut sent = offset as usize;
        for (i, item) in listing.iter().enumerate().skip(offset as usize) {
            if reply.add(
                INodeNo(item.ino),
                i as u64 + 1,
                &item.name,
                &CACHE_TTL,
                &item.attr,
                GENERATION,
            ) {
                // Buffer full. This entry was not accepted, so it gets no
                // reference; the kernel asks again from here.
                break;
            }
            sent = i + 1;
        }

        {
            let mut state = self.state.lock().expect("inode state");
            for (i, item) in listing.iter().enumerate() {
                if !item.counts {
                    continue;
                }
                if (offset as usize..sent).contains(&i) {
                    state.inodes.reference(item.ino);
                } else {
                    // Resolved while building the listing but never sent —
                    // either already behind the resume offset, or past what fit.
                    state.inodes.release_unreferenced(item.ino);
                }
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
        // The figures are those of the one peer volume this mount is bound to.
        //
        // This used to sum every volume the peer offered, because the mount
        // stood for all of them — so a machine with five disks showed as one
        // filesystem of their combined size, a figure matching no disk the user
        // has, and a file manager's free-space bar meant nothing. One mount per
        // volume removes the question.
        let (total, free) = match self.volumes() {
            Ok(volumes) => volumes
                .iter()
                .find(|v| v.id == self.bound)
                .map(|v| (v.total_bytes, v.free_bytes))
                // Bound to a volume the peer no longer lists: it was unplugged,
                // or the link has not answered yet. A plausible figure keeps
                // `df` and file managers usable until the mount goes away.
                .unwrap_or((FALLBACK_TOTAL, FALLBACK_FREE)),
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

    /// Nothing is buffered, so there is nothing to force to disk.
    ///
    /// Every `Write` is a request the peer answers before the caller returns,
    /// so by the time an `fsync` arrives the bytes are as durable as this
    /// filesystem can make them. Answering `ENOSYS` would be read by callers as
    /// "the write may not have landed" — SQLite and any editor that fsyncs
    /// before replacing a file treat that as data loss and refuse to proceed,
    /// which is a good deal worse than the warning it also logs.
    ///
    /// What this cannot promise is the peer's own disk cache: `fsync` there is
    /// not part of the protocol. The honest reading is "delivered and
    /// acknowledged", which is the same guarantee a network filesystem gives.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// Directories are never buffered either; see [`RemoteFs::fsync`].
    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// Permission is the peer's to grant, so this only answers for the mount.
    ///
    /// The real decision happens when the peer serves an `Open` — it knows its
    /// own ACLs, and a read-only volume refuses writes there. Repeating a guess
    /// here would only be able to disagree with it. What the kernel needs from
    /// this call is that the mount does not deny traversal outright: the
    /// attributes returned by `getattr` already carry the modes, and the kernel
    /// checks them itself when mounted with `default_permissions`.
    ///
    /// Left unimplemented this answers `ENOSYS`, which the kernel tolerates but
    /// which logs a warning for every `access(2)` — one per path component that
    /// anything on this machine examines.
    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    /// Extended attributes are not stored, and that is a decision.
    ///
    /// The protocol carries no attribute set, so there is nowhere to put one:
    /// accepting it would silently drop what the caller believes it saved.
    /// `EOPNOTSUPP` is what a filesystem without xattr support answers, and
    /// what `cp -a` and `rsync -X` are written to expect — unlike `ENOSYS`,
    /// which they report as an error against the whole copy.
    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EOPNOTSUPP);
    }

    /// Nothing is stored, so there is never anything to remove.
    ///
    /// `ENODATA` rather than `EOPNOTSUPP`: the caller asked to remove one named
    /// attribute, and the truthful answer is that this file does not have it.
    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::NO_XATTR);
    }

    /// Device nodes, FIFOs and sockets do not cross the link.
    ///
    /// The protocol has no vocabulary for them, and a mount that could create
    /// them would be a way to place a device node on another machine's disk —
    /// which is why `nodev` and `nosuid` are set on the mount in the first
    /// place. `EPERM` says the operation is refused rather than unrecognised.
    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    /// Links are read but never created.
    ///
    /// `ReadLink` is the whole of the protocol's link support: a target can be
    /// reported, but nothing asks the peer to make one. Creating a symlink here
    /// would also mean writing a path that this machine interprets and the peer
    /// stores — the two do not agree on what a path looks like when one of them
    /// is Windows.
    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    /// Hard links have no representation on the wire.
    ///
    /// The inode table maps one number to one path; a second name for the same
    /// file is a relationship it cannot express, and the peer is never told
    /// about it either.
    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    /// Preallocation is not something the peer can be asked for.
    ///
    /// `EOPNOTSUPP` is the documented answer, and callers that care fall back
    /// to writing zeroes. `ENOSYS` would be read by `posix_fallocate` as a
    /// reason to fail rather than to fall back.
    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _offset: u64,
        _length: u64,
        _mode: i32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EOPNOTSUPP);
    }

    /// Server-side copy is not in the protocol.
    ///
    /// Refusing sends the caller down the ordinary read-then-write path, which
    /// costs the bytes twice over the link but is the only route that exists.
    /// Implementing this properly means a `CopyRange` the peer can serve
    /// without the data ever leaving its disk — worth having, but it is a
    /// protocol change, not a mount one.
    fn copy_file_range(
        &self,
        _req: &Request,
        _ino_in: INodeNo,
        _fh_in: fuser::FileHandle,
        _offset_in: u64,
        _ino_out: INodeNo,
        _fh_out: fuser::FileHandle,
        _offset_out: u64,
        _len: u64,
        _flags: fuser::CopyFileRangeFlags,
        reply: fuser::ReplyWrite,
    ) {
        reply.error(Errno::EOPNOTSUPP);
    }

    /// Holes are not reported, so seeking to one has no answer to give.
    ///
    /// `SEEK_HOLE`/`SEEK_DATA` ask where a sparse file's gaps are; the peer
    /// sends sizes and bytes, never an extent map. `EOPNOTSUPP` makes the
    /// caller treat the file as fully allocated, which is the safe reading —
    /// guessing would have it skip data that is really there.
    fn lseek(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _offset: i64,
        _whence: i32,
        reply: fuser::ReplyLseek,
    ) {
        reply.error(Errno::EOPNOTSUPP);
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
        ino: INodeNo,
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
        // The size and modification time this changes are exactly what the
        // cache holds, so it must not answer the next `getattr` with what it
        // knew before the write.
        if let Some(Entry::Path { volume, path }) = self.entry_of(ino.0) {
            self.meta.forget(&volume, &path);
        }
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
        // The `lookup` that preceded this was very likely a `NotFound`, and
        // the cache would keep answering it for the rest of its lifetime.
        self.meta.forget(&volume, &path);

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
            modified_unix: crate::now_unix(),
            writable: true,
            allocated: None,
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
        self.meta.forget(&volume, &path);
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
        // Both ends move, and a directory takes its contents with it.
        self.meta.forget_dir(&volume, &from);
        self.meta.forget_dir(&volume, &to);
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
        // Whatever this call changed, the cached answer describes the file as
        // it was before it. `setattr` is 3.5 calls per file on a real copy —
        // second only to `lookup` — because that is how a tool restores
        // timestamps and permissions after writing.
        if let Some(Entry::Path { volume, path }) = self.entry_of(ino.0) {
            self.meta.forget(&volume, &path);
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

/// Translate the kernel's open flags into what the peer understands.
///
/// `OpenFlags` is a transparent wrapper over the raw `open(2)` flags, so the
/// access mode comes from its accessor and the rest are tested directly.
fn open_mode_from(flags: fuser::OpenFlags) -> OpenMode {
    let acc = flags.acc_mode();
    OpenMode {
        // Readable even when opened write-only. With `FUSE_WRITEBACK_CACHE` the
        // kernel fills a partially written page by reading it first, through
        // the very handle the application opened `O_WRONLY` — and a peer
        // handle without read access answers that with `EBADF`, which reaches
        // the application as a failed write to the middle of a file.
        read: true,
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

impl Drop for MountHandle {
    /// Remove the mount point once the mount is gone.
    ///
    /// Field order does the unmounting: `_session` is declared first, so it
    /// drops before this runs and the directory is empty by the time it is
    /// removed. Without this an unplugged disk leaves an empty folder behind
    /// under `~/Cursio/<peer>`, and a file manager shows it as a directory that
    /// is not a disk at all.
    ///
    /// `remove_dir` rather than `remove_dir_all`, and its failure ignored: it
    /// removes only an empty directory, so an unmount that did not take cannot
    /// take the peer's files with it.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.mountpoint);
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

/// Release every Cursio mount left under `root` by a previous run.
///
/// [`release_stale_mount`] clears one known path; this clears the ones whose
/// names are no longer known. A volume's directory is named after the disk, so
/// anything that changes the name — the peer relabelling a drive, or a version
/// of Cursio that named them differently — leaves the old mount behind with
/// nothing left to look it up by. They accumulate: a file manager's sidebar
/// fills with entries for disks that answer nothing, and `~/Cursio/<peer>`
/// cannot be tidied because each one is a live mount point.
///
/// Only `fuse.cursio` mounts under this root are touched, so a filesystem the
/// user mounted there themselves is left alone.
pub fn release_stale_peer_mounts(root: &std::path::Path) {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return;
    };
    let prefix = format!("{}/", root.to_string_lossy().replace(' ', "\\040"));
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_source), Some(target), Some(fstype)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if fstype != "fuse.cursio" || !target.starts_with(&prefix) {
            continue;
        }
        // Undo the escaping `/proc/mounts` applies, so the path handed to
        // `fusermount` is the real one.
        let target = target.replace("\\040", " ");
        release_stale_mount(std::path::Path::new(&target));
    }
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
        // No `x-gvfs-show` here, and none is needed: gvfs already picks this
        // up through its UDisks2 monitor as a `GProxyMount`, so the mount
        // appears in a file manager's sidebar with an eject button of its own.
        // The option would not have helped anyway — `x-*` options are userspace
        // ones that libmount interprets from an fstab entry, while fuser passes
        // CUSTOM straight to the kernel as a mount flag, where no gvfs monitor
        // ever sees it.
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

/// Mount ONE of the peer's volumes at `mountpoint`, returning once it is
/// visible.
///
/// Called once per volume the peer offers: `volume` is the wire id (`"C:"`,
/// `"/"`, a removable medium's mount path). Each mount is its own entry in a
/// file manager's sidebar, with the capacity of that disk alone — see
/// [`RemoteFs::bound`].
pub fn mount(
    peer: PeerLink,
    mountpoint: impl AsRef<std::path::Path>,
    peer_name: &str,
    volume: &str,
) -> std::io::Result<MountHandle> {
    let mountpoint = mountpoint.as_ref().to_path_buf();
    // Clear a mount left behind by a previous run before creating anything.
    //
    // Without `AutoUnmount` (see below) a crash — or a process killed while the
    // filesystem was wedged — leaves the old mount in place, and the next start
    // fails with `EEXIST` on a point that looks like an ordinary directory. The
    // user would have to know to run `fusermount3 -u` by hand, so do it here.
    // The parent first, then the mount point itself.
    //
    // The parent matters because this path moved: a mount used to be
    // `~/Cursio/<peer>` and is now `~/Cursio/<peer>/<volume>`, so a version
    // before that change — or a crash from one — leaves a mount sitting on the
    // very directory the volumes now live in. Every path inside a wedged FUSE
    // mount answers `ENOTCONN`, so `create_dir_all` below fails and every one
    // of the peer's volumes is refused. Seen live: all six of a peer's drives
    // failed with "Transport endpoint is not connected" and none returned,
    // because nothing retried and nothing cleared the parent.
    if let Some(parent) = mountpoint.parent() {
        release_stale_mount(parent);
    }
    release_stale_mount(&mountpoint);
    std::fs::create_dir_all(&mountpoint)?;

    let config = mount_config(peer_name);
    let session = fuser::Session::new(
        RemoteFs::new(peer, volume).of_peer(peer_name),
        &mountpoint,
        &config,
    )?
    .spawn()?;
    tracing::info!(mount = %mountpoint.display(), peer = peer_name, volume, "peer volume mounted");
    Ok(MountHandle {
        _session: session,
        mountpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repeated question about one path costs one round trip, not several.
    ///
    /// Measured on a real transfer through this mount, `lookup` was 5.4 calls
    /// per file and `getattr` another 2, against 1.2 writes — two thirds of a
    /// copy's round trips were questions about paths rather than the bytes of
    /// them. The Windows half has cached these since it was written; this side
    /// did not, which is most of why the same copy cost so much more here.
    #[test]
    fn repeated_questions_about_a_path_cost_one_round_trip() {
        let cache = crate::metacache::MetaCache::new();
        let meta = cursio_protocol::FileMeta {
            name: "a.txt".into(),
            kind: FileKind::File,
            size: 10,
            modified_unix: 0,
            writable: true,
            allocated: None,
        };
        assert!(
            cache.get("C:", "a.txt").is_none(),
            "nothing is known before the peer has been asked"
        );
        cache.put("C:", "a.txt", Ok(meta));
        assert!(
            cache.get("C:", "a.txt").is_some(),
            "the answer serves the questions that follow"
        );
    }

    /// A listing answers the questions that follow it.
    ///
    /// This is compounding of the kind NFSv4 and SMB2 do explicitly: one
    /// request returns what a `stat` of every entry would, and an application
    /// reading a directory then asks about each name in turn. The compound
    /// already existed on our wire — `read_dir` has always returned full
    /// metadata — and its answers were being discarded, so those 5.4 lookups
    /// per file each cost their own round trip.
    #[test]
    fn a_listing_answers_the_lookups_that_follow_it() {
        let cache = crate::metacache::MetaCache::new();
        let entries = ["a.txt", "b.txt", "c.txt"];
        for name in entries {
            cache.put(
                "C:",
                &join("dir", name),
                Ok(cursio_protocol::FileMeta {
                    name: name.into(),
                    kind: FileKind::File,
                    size: 1,
                    modified_unix: 0,
                    writable: true,
                    allocated: None,
                }),
            );
        }
        for name in entries {
            assert!(
                cache.get("C:", &join("dir", name)).is_some(),
                "{name} was answered by the listing, not by a round trip"
            );
        }
    }

    /// A change made here must not be readable as the value it had before.
    #[test]
    fn a_change_drops_what_was_cached_about_the_path() {
        let cache = crate::metacache::MetaCache::new();
        cache.put(
            "C:",
            "a.txt",
            Ok(cursio_protocol::FileMeta {
                name: "a.txt".into(),
                kind: FileKind::File,
                size: 10,
                modified_unix: 0,
                writable: true,
                allocated: None,
            }),
        );
        cache.forget("C:", "a.txt");
        assert!(
            cache.get("C:", "a.txt").is_none(),
            "a write, truncate or rename leaves nothing stale behind it"
        );
    }

    /// The mount root is the bound volume's own root, not a list of volumes.
    ///
    /// This is what makes one mount per volume work: every caller — lookup,
    /// getattr, readdir, create — resolves paths through `entry_of`, so
    /// translating the root in that one place is what turns "the peer's disks
    /// as folders under one mount" into "each disk mounted as itself". A mount
    /// that still answered [`Entry::Root`] here would list volumes inside a
    /// volume.
    #[test]
    fn the_mount_root_is_the_bound_volumes_own_root() {
        let (ops, _rx) = tokio::sync::mpsc::unbounded_channel();
        let fs = RemoteFs::new(PeerLink::new(ops), "C:");

        assert_eq!(
            fs.entry_of(ROOT_INO),
            Some(Entry::Path {
                volume: "C:".to_string(),
                path: String::new(),
            }),
            "the root must address the volume this mount is bound to"
        );
    }

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
        assert!(wo.write, "O_WRONLY writes");
        assert!(
            wo.read,
            "and reads too: the writeback cache fills partial pages through it"
        );

        let rw = open_mode_from(fuser::OpenFlags(libc::O_RDWR));
        assert!(rw.read && rw.write, "O_RDWR does both");

        let trunc = open_mode_from(fuser::OpenFlags(libc::O_WRONLY | libc::O_TRUNC));
        assert!(trunc.truncate, "O_TRUNC must reach the peer");
        assert!(trunc.is_mutating());
    }
}
