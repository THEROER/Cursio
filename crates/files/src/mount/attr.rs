//! Turning wire metadata into what the kernel expects, and errors back into
//! errnos.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cursio_protocol::{FileError, FileKind, FileMeta};
use fuser::{Errno, FileAttr, FileType};

use super::inode::ROOT_INO;

/// How long the kernel may trust a cached attribute or lookup.
///
/// A local filesystem can cache indefinitely because nothing changes a file
/// without going through it. Here the other machine owns the data and can
/// change it at any moment, so anything cached is a guess with an expiry date.
///
/// One second is the FUSE default and the right trade for a desktop: it keeps a
/// directory listing or a repeated `stat` from becoming a round trip each time
/// — which over a 6 ms link is the difference between a folder opening
/// instantly and visibly crawling — while keeping the window in which a change
/// made on the peer is invisible here down to something a user reads as "just
/// refreshed".
pub const CACHE_TTL: Duration = Duration::from_secs(1);

/// Permissions reported for entries the peer says are writable.
///
/// `0o755`/`0o644`: readable and traversable by the owner, no group or world
/// write. The peer's real permissions are not modelled — a Windows ACL has no
/// faithful translation — so these describe what *this* machine may do, which
/// is what an application actually needs to know.
const PERM_RW_DIR: u16 = 0o755;
const PERM_RW_FILE: u16 = 0o644;
const PERM_RO_DIR: u16 = 0o555;
const PERM_RO_FILE: u16 = 0o444;

/// Build the kernel's view of a file from what the peer told us.
///
/// `uid`/`gid` are the mounting user's: the files belong, as far as this machine
/// is concerned, to whoever mounted them. Reporting the peer's numeric ids would
/// name users that do not exist here.
pub fn attr_from(ino: u64, meta: &FileMeta, uid: u32, gid: u32) -> FileAttr {
    let kind = match meta.kind {
        FileKind::Dir => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
        FileKind::File => FileType::RegularFile,
    };
    let perm = match (kind, meta.writable) {
        (FileType::Directory, true) => PERM_RW_DIR,
        (FileType::Directory, false) => PERM_RO_DIR,
        (_, true) => PERM_RW_FILE,
        (_, false) => PERM_RO_FILE,
    };
    let mtime = unix_to_system_time(meta.modified_unix);
    FileAttr {
        ino: fuser::INodeNo(ino),
        size: meta.size,
        // Reported in 512-byte units, rounded up: `du` and friends read this,
        // and a file of one byte still occupies a block.
        blocks: meta.size.div_ceil(512),
        // Only modification time survives the crossing; the peer does not send
        // atime or ctime, and inventing distinct values would be a lie. Using
        // mtime for all three is the conventional answer.
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind,
        perm,
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// Attributes for the mount root and for each volume's own directory.
///
/// These have no counterpart on the peer — the root is our invention, one
/// directory per volume — so their metadata is synthesised rather than fetched.
pub fn dir_attr(ino: u64, uid: u32, gid: u32) -> FileAttr {
    let now = SystemTime::now();
    FileAttr {
        ino: fuser::INodeNo(ino),
        size: 0,
        blocks: 0,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: FileType::Directory,
        perm: PERM_RW_DIR,
        nlink: 2,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// The root inode's attributes.
pub fn root_attr(uid: u32, gid: u32) -> FileAttr {
    dir_attr(ROOT_INO, uid, gid)
}

fn unix_to_system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        // Before 1970: Windows can hold such timestamps, and subtracting keeps
        // them rather than clamping every one of them to the epoch.
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// Map a wire error onto the errno the kernel expects.
///
/// Every variant has a definite answer: a filesystem that returns the wrong
/// errno makes applications misbehave in ways that look like corruption (a
/// caller retrying forever on what should have been `ENOENT`, say).
pub fn to_errno(e: &FileError) -> Errno {
    match e {
        FileError::NotFound => Errno::ENOENT,
        FileError::Denied => Errno::EACCES,
        FileError::ReadOnly => Errno::EROFS,
        FileError::WrongKind => Errno::ENOTDIR,
        FileError::NotEmpty => Errno::ENOTEMPTY,
        FileError::Exists => Errno::EEXIST,
        FileError::BadHandle => Errno::EBADF,
        // Anything else is an I/O error: the operation failed for a reason the
        // caller cannot act on specifically.
        FileError::Io(_) => Errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(kind: FileKind, size: u64, writable: bool) -> FileMeta {
        FileMeta {
            name: "x".into(),
            kind,
            size,
            modified_unix: 1_700_000_000,
            writable,
        }
    }

    /// A read-only file must not be reported as writable, or applications will
    /// offer to save into it and fail at the last moment.
    #[test]
    fn permissions_follow_writability() {
        let rw = attr_from(2, &meta(FileKind::File, 10, true), 1000, 1000);
        assert_eq!(rw.perm, PERM_RW_FILE);
        let ro = attr_from(2, &meta(FileKind::File, 10, false), 1000, 1000);
        assert_eq!(ro.perm, PERM_RO_FILE);
        assert_eq!(ro.uid, 1000, "files belong to whoever mounted them");
    }

    /// Block counts round up: `du` on a one-byte file must not read zero.
    #[test]
    fn blocks_round_up() {
        assert_eq!(attr_from(2, &meta(FileKind::File, 0, true), 0, 0).blocks, 0);
        assert_eq!(attr_from(2, &meta(FileKind::File, 1, true), 0, 0).blocks, 1);
        assert_eq!(
            attr_from(2, &meta(FileKind::File, 512, true), 0, 0).blocks,
            1
        );
        assert_eq!(
            attr_from(2, &meta(FileKind::File, 513, true), 0, 0).blocks,
            2
        );
    }

    /// A timestamp from before 1970 must survive rather than collapse to the
    /// epoch — Windows really does hold them.
    #[test]
    fn pre_epoch_timestamps_survive() {
        let mut m = meta(FileKind::File, 1, true);
        m.modified_unix = -86_400; // one day before the epoch
        let attr = attr_from(2, &m, 0, 0);
        assert!(
            attr.mtime < UNIX_EPOCH,
            "a pre-epoch mtime must stay before the epoch"
        );
    }

    /// Every error has a definite errno; `EIO` is the fallback, not the default.
    #[test]
    fn errors_map_to_distinct_errnos() {
        assert_eq!(to_errno(&FileError::NotFound), Errno::ENOENT);
        assert_eq!(to_errno(&FileError::Denied), Errno::EACCES);
        assert_eq!(to_errno(&FileError::ReadOnly), Errno::EROFS);
        assert_eq!(to_errno(&FileError::Exists), Errno::EEXIST);
        assert_eq!(to_errno(&FileError::NotEmpty), Errno::ENOTEMPTY);
        assert_eq!(to_errno(&FileError::BadHandle), Errno::EBADF);
        assert_eq!(to_errno(&FileError::Io("x".into())), Errno::EIO);
    }

    /// The cache window has to be short enough that a change on the peer shows
    /// up promptly, and long enough to be worth having.
    #[test]
    fn the_cache_window_is_a_desktop_compromise() {
        assert!(CACHE_TTL >= Duration::from_millis(500));
        assert!(
            CACHE_TTL <= Duration::from_secs(5),
            "a longer window makes the peer's changes invisible for too long"
        );
    }
}
