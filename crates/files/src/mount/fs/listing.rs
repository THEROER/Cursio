//! Directory listings: `readdir`, and `readdirplus`, which carries each
//! entry's attributes and so answers the `lookup` per entry that follows.

use cursio_protocol::{FileError, FileKind, FileMeta};
use fuser::{Errno, FileAttr, FileType, INodeNo, ReplyDirectory, Request};

use super::{RemoteFs, GENERATION};
use crate::mount::attr::{attr_from, dir_attr, root_attr, to_errno, CACHE_TTL};
use crate::mount::inode::{join, Entry, ROOT_INO};
use crate::volume_dir_name;

impl RemoteFs {
    /// The peer's listing of a directory, each entry with the inode it has
    /// here — resolved, not referenced: the caller counts only what it sends.
    ///
    /// The listing already carries what a `stat` of each entry would return,
    /// so it is kept: this one request has answered every `lookup` and
    /// `getattr` that follows a directory read, which is what makes a listing
    /// worth a round trip instead of one per name. It is the same economy
    /// NFSv4 and SMB2 get by compounding operations into one request — here
    /// the compound already exists on the wire and its answers were being
    /// thrown away.
    fn peer_listing(&self, volume: &str, path: &str) -> Result<Vec<(u64, FileMeta)>, FileError> {
        let entries = self.peer.read_dir(volume, path)?;
        let mut state = self.state.lock().expect("inode state");
        Ok(entries
            .into_iter()
            .map(|meta| {
                let child_path = join(path, &meta.name);
                let child = state.inodes.resolve(volume, &child_path);
                self.meta.put(volume, &child_path, Ok(meta.clone()));
                (child, meta)
            })
            .collect())
    }

    /// `readdir`: names only.
    pub(super) fn list(&self, req: &Request, ino: INodeNo, offset: u64, mut reply: ReplyDirectory) {
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
                let entries = match self.peer_listing(volume, path) {
                    Ok(e) => e,
                    Err(e) => return reply.error(to_errno(&e)),
                };
                for (child, meta) in entries {
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
    /// each one sent is counted — see [`InodeTable::lookup`](crate::mount::inode::InodeTable::lookup). The reference is
    /// taken at the moment the entry goes into the buffer, never before: an
    /// entry that does not fit is one the kernel never received, and counting
    /// it would leak an inode that nothing will ever forget. `.` and `..` are
    /// offset positions rather than entries the kernel references, so they are
    /// emitted without being counted.
    pub(super) fn list_plus(
        &self,
        req: &Request,
        ino: INodeNo,
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
                let entries = match self.peer_listing(volume, path) {
                    Ok(e) => e,
                    Err(e) => return reply.error(to_errno(&e)),
                };
                for (child, meta) in entries {
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
