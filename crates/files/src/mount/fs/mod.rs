//! The FUSE filesystem itself.
//!
//! Callbacks arrive on fuser's own threads — several at once, and with `&self`
//! — so all mutable state lives behind a mutex and every reply is sent from the
//! thread that received the call. Nothing here touches the tokio runtime; see
//! [`super::bridge`] for why.

use std::ffi::OsStr;
use std::sync::Mutex;

use cursio_protocol::{FileError, OpenMode};
use fuser::{Errno, INodeNo, ReplyEmpty};

use crate::bridge::PeerLink;
use crate::mount::attr::to_errno;
use crate::mount::inode::{join, Entry, InodeTable};

mod kernel;

mod handle;
mod listing;

pub use handle::{mount, release_stale_peer_mounts, MountHandle};

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
    /// The start of each file opened for reading, by our handle, as the owner
    /// sent it with the open; see [`Head`].
    heads: std::collections::HashMap<u64, Head>,
}

/// How much of a file to ask for with its open.
///
/// The kernel's first read of a file asks for up to 128 KiB, so a head this
/// long answers it whole, and nearly every source file fits.
const HEAD_BYTES: u32 = 128 * 1024;

/// How many open files keep a head at once. Each is up to [`HEAD_BYTES`]; a
/// program holding thousands of files open keeps the rest without, and they
/// read as before.
const MAX_HEADS: usize = 256;

/// A file's first bytes, received with its open.
///
/// Answers the reads that fall inside it without a round trip. Dropped when the
/// handle is released, and whenever this machine writes to or truncates the
/// file — through any handle — since it then no longer says what the file
/// holds.
struct Head {
    ino: u64,
    /// The file's size when it was opened.
    size: u64,
    bytes: Vec<u8>,
}

impl Head {
    /// The bytes a read of `len` at `offset` returns, if this head holds them:
    /// all of the range, or the rest of a file the head holds whole.
    fn read(&self, offset: u64, len: u32) -> Option<&[u8]> {
        let start = usize::try_from(offset).ok()?;
        if start > self.bytes.len() {
            return None;
        }
        let end = start.saturating_add(len as usize);
        let whole = self.bytes.len() as u64 == self.size;
        if end <= self.bytes.len() {
            Some(&self.bytes[start..end])
        } else if whole {
            Some(&self.bytes[start..])
        } else {
            None
        }
    }
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
                heads: std::collections::HashMap::new(),
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
    /// one. A directory being swept name by name is listed once instead — see
    /// [`MetaCache::get_or_ask_swept`](crate::metacache::MetaCache::get_or_ask_swept).
    fn stat_path(&self, volume: &str, path: &str) -> Result<cursio_protocol::FileMeta, FileError> {
        self.meta.get_or_ask_swept(
            volume,
            path,
            || self.peer.stat(volume, path),
            |dir| {
                self.peer
                    .read_dir_within(volume, dir, crate::metacache::SWEEP_LISTING_WAIT)
            },
        )
    }

    /// Drop every head held for `ino`: this machine is changing the file.
    fn forget_heads(&self, ino: u64) {
        self.state
            .lock()
            .expect("inode state")
            .heads
            .retain(|_, head| head.ino != ino);
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

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::FileKind;

    use crate::mount::inode::ROOT_INO;

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

    /// A file the head holds whole is read from it entirely, end of file
    /// included, with no round trip.
    #[test]
    fn a_small_file_is_read_whole_from_its_head() {
        let head = Head {
            ino: 2,
            size: 5,
            bytes: b"hello".to_vec(),
        };
        assert_eq!(head.read(0, 128 * 1024), Some(&b"hello"[..]));
        assert_eq!(head.read(3, 10), Some(&b"lo"[..]));
        assert_eq!(head.read(5, 10), Some(&b""[..]), "end of file");
    }

    /// Of a larger file the head answers only what it holds; a read reaching
    /// past it goes to the owner.
    #[test]
    fn a_read_past_a_partial_head_goes_to_the_owner() {
        let head = Head {
            ino: 2,
            size: 1000,
            bytes: vec![7; 100],
        };
        assert_eq!(head.read(10, 20).map(<[u8]>::len), Some(20));
        assert!(head.read(90, 20).is_none(), "straddles the end of the head");
        assert!(head.read(100, 1).is_none(), "wholly past it");
        assert!(head.read(5000, 1).is_none());
    }

    /// A write here makes every head of that file stale, whichever handle
    /// wrote; heads of other files stay.
    #[test]
    fn writing_a_file_drops_its_heads_and_no_others() {
        let (ops, _rx) = tokio::sync::mpsc::unbounded_channel();
        let fs = RemoteFs::new(PeerLink::new(ops), "C:");
        {
            let mut state = fs.state.lock().unwrap();
            for (fh, ino) in [(1, 7), (2, 7), (3, 8)] {
                state.heads.insert(
                    fh,
                    Head {
                        ino,
                        size: 1,
                        bytes: vec![0],
                    },
                );
            }
        }
        fs.forget_heads(7);
        let state = fs.state.lock().unwrap();
        assert_eq!(state.heads.keys().copied().collect::<Vec<_>>(), [3]);
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
}
