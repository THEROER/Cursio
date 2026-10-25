//! The kernel's calls, answered: each FUSE callback turned into a question for
//! the peer or a refusal the kernel can cache.
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

use crate::mount::attr::{attr_from, dir_attr, root_attr, to_errno, CACHE_TTL};
use crate::mount::fs::GENERATION;
use crate::mount::inode::{join, Entry};
use crate::mount::RemoteFs;
use crate::server::{FALLBACK_FREE, FALLBACK_TOTAL};
use crate::volume_dir_name;
use cursio_protocol::{FileKind, OpenMode};
use fuser::{
    Errno, Filesystem, INodeNo, InitFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use std::ffi::OsStr;

/// Ask for one kernel capability, saying so if this kernel does not have it.
///
/// Named individually rather than requested as one set: a kernel missing any
/// one of them would otherwise refuse the whole set, and the rest are still
/// worth having.
pub(super) fn request(config: &mut fuser::KernelConfig, flag: InitFlags, what: &str) {
    if config.add_capabilities(flag).is_err() {
        tracing::debug!(
            what,
            "this kernel does not offer the capability; going without"
        );
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
        reply: ReplyDirectory,
    ) {
        self.list(req, ino, offset, reply)
    }

    fn readdirplus(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        reply: fuser::ReplyDirectoryPlus,
    ) {
        self.list_plus(req, ino, offset, reply)
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
pub(super) fn open_mode_from(flags: fuser::OpenFlags) -> OpenMode {
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

#[cfg(test)]
mod tests {
    use super::*;

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
