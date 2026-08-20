//! Mapping between the kernel's inode numbers and remote paths.
//!
//! FUSE addresses everything by inode: after the first `lookup`, the kernel
//! refers to a file by number and never mentions its path again. The peer, in
//! contrast, only understands `(volume, path)`. This table is what stands
//! between the two.
//!
//! ## Why entries cannot simply be dropped
//!
//! The kernel keeps its own reference count per inode and tells us about it:
//! every `lookup` it performs is one reference, and `forget` returns some
//! number of them. While that count is above zero the kernel may issue requests
//! against the inode — *including after the file has been deleted or renamed*.
//! An entry may therefore only be removed when its count reaches zero.
//!
//! Getting this wrong is not a leak but a correctness bug: reuse a number the
//! kernel still holds and the next read on that stale inode silently lands on
//! whatever file inherited it.

use std::collections::HashMap;

/// The root of the mount — fixed by the FUSE protocol.
pub const ROOT_INO: u64 = 1;

/// What an inode number stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// The mount root, whose children are the peer's volumes.
    Root,
    /// A path inside one of the peer's volumes. The path is `/`-separated and
    /// volume-relative, exactly as it goes on the wire; empty means the
    /// volume's own root.
    Path { volume: String, path: String },
}

/// Inode table for one mount.
pub struct InodeTable {
    entries: HashMap<u64, Tracked>,
    /// Reverse index, so a repeated `lookup` of the same path returns the same
    /// inode rather than minting a new one each time. Without it a directory
    /// listing refreshed in a loop would grow the table without bound.
    by_path: HashMap<(String, String), u64>,
    next: u64,
}

struct Tracked {
    entry: Entry,
    /// Outstanding kernel references; see the module docs.
    lookups: u64,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeTable {
    pub fn new() -> Self {
        let mut entries = HashMap::new();
        entries.insert(
            ROOT_INO,
            Tracked {
                entry: Entry::Root,
                // The root is never forgotten while the filesystem is mounted,
                // so it starts with a reference the kernel can never return.
                lookups: 1,
            },
        );
        Self {
            entries,
            by_path: HashMap::new(),
            next: ROOT_INO + 1,
        }
    }

    /// What an inode stands for, if we still know.
    pub fn get(&self, ino: u64) -> Option<&Entry> {
        self.entries.get(&ino).map(|t| &t.entry)
    }

    /// Inode for a path, allocating one if this is the first time we've seen it,
    /// and counting one kernel reference either way.
    ///
    /// Call this for exactly the replies that hand the kernel a new reference
    /// (`lookup`, `create`, and the entries of a `readdirplus`) — one call per
    /// reference, or the counts drift and entries are freed while still in use.
    pub fn lookup(&mut self, volume: &str, path: &str) -> u64 {
        let key = (volume.to_string(), path.to_string());
        if let Some(&ino) = self.by_path.get(&key) {
            if let Some(t) = self.entries.get_mut(&ino) {
                t.lookups += 1;
            }
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.entries.insert(
            ino,
            Tracked {
                entry: Entry::Path {
                    volume: key.0.clone(),
                    path: key.1.clone(),
                },
                lookups: 1,
            },
        );
        self.by_path.insert(key, ino);
        ino
    }

    /// The inode for a path, allocating one if needed, **without** counting a
    /// kernel reference.
    ///
    /// For replies that name an inode but hand out no reference: `readdir`, and
    /// `readdirplus` while it assembles a listing it may not send in full.
    ///
    /// An entry left at zero references is one the kernel was never handed, so
    /// it will never send a `forget` for it — the caller must therefore pair
    /// this with either [`Self::reference`], once the kernel does receive the
    /// entry, or [`Self::release_unreferenced`] once it is clear it will not.
    /// Leaving it is a slow leak: a re-listed directory tree would add an entry
    /// per file per pass, and nothing would ever take them out.
    pub fn resolve(&mut self, volume: &str, path: &str) -> u64 {
        let key = (volume.to_string(), path.to_string());
        if let Some(&ino) = self.by_path.get(&key) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.entries.insert(
            ino,
            Tracked {
                entry: Entry::Path {
                    volume: key.0.clone(),
                    path: key.1.clone(),
                },
                lookups: 0,
            },
        );
        self.by_path.insert(key, ino);
        ino
    }

    /// Drop an entry that was resolved but never handed to the kernel.
    ///
    /// A no-op if the inode is referenced — `readdir` names entries the kernel
    /// may already hold from an earlier `lookup`, and those must survive.
    pub fn release_unreferenced(&mut self, ino: u64) {
        if ino == ROOT_INO {
            return;
        }
        let Some(tracked) = self.entries.get(&ino) else {
            return;
        };
        if tracked.lookups > 0 {
            return;
        }
        if let Entry::Path { volume, path } = &tracked.entry {
            self.by_path.remove(&(volume.clone(), path.clone()));
        }
        self.entries.remove(&ino);
    }

    /// Count one kernel reference against an inode already resolved.
    ///
    /// Paired with [`Self::resolve`] for `readdirplus`: the number is settled
    /// while the listing is built, and the reference is taken only for the
    /// entries that actually reach the kernel. Referencing at resolve time
    /// would leak every entry that did not fit in the reply buffer, since the
    /// kernel never learns of those and so never forgets them.
    pub fn reference(&mut self, ino: u64) {
        if let Some(t) = self.entries.get_mut(&ino) {
            t.lookups += 1;
        }
    }

    /// Return `n` kernel references, dropping the entry once none remain.
    pub fn forget(&mut self, ino: u64, n: u64) {
        // The root is permanent: forgetting it would leave the mount unable to
        // resolve anything.
        if ino == ROOT_INO {
            return;
        }
        let Some(tracked) = self.entries.get_mut(&ino) else {
            return;
        };
        tracked.lookups = tracked.lookups.saturating_sub(n);
        if tracked.lookups > 0 {
            return;
        }
        if let Entry::Path { volume, path } = &tracked.entry {
            self.by_path.remove(&(volume.clone(), path.clone()));
        }
        self.entries.remove(&ino);
    }

    /// Re-point an inode at a new path, for a rename.
    ///
    /// The kernel keeps using the same inode after a rename, so the mapping has
    /// to follow the file; leaving it on the old path would make every later
    /// read fail with `ENOENT` even though the file is right there.
    pub fn rename(&mut self, ino: u64, volume: &str, new_path: &str) {
        let Some(tracked) = self.entries.get_mut(&ino) else {
            return;
        };
        if let Entry::Path { volume: v, path: p } = &tracked.entry {
            self.by_path.remove(&(v.clone(), p.clone()));
        }
        tracked.entry = Entry::Path {
            volume: volume.to_string(),
            path: new_path.to_string(),
        };
        self.by_path
            .insert((volume.to_string(), new_path.to_string()), ino);
    }

    /// How many inodes are currently tracked, for diagnostics.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Join a directory path with a child name, in wire form.
///
/// Wire paths are `/`-separated and volume-relative, with the volume root as
/// the empty string — so joining onto the root must not produce a leading `/`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The parent of a wire path, or `None` at a volume root.
pub fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(head, _)| head).or({
        if path.is_empty() {
            None
        } else {
            Some("")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same path must keep the same inode, or the table grows without bound
    /// as a directory is re-listed.
    #[test]
    fn one_path_keeps_one_inode() {
        let mut t = InodeTable::new();
        let a = t.lookup("C:", "Users/file.txt");
        let b = t.lookup("C:", "Users/file.txt");
        assert_eq!(a, b, "the same path must map to the same inode");
        assert_ne!(a, ROOT_INO, "a file must not collide with the root");

        let other = t.lookup("C:", "Users/other.txt");
        assert_ne!(a, other, "different paths need different inodes");
    }

    /// An inode is only released once the kernel has returned every reference.
    ///
    /// Freeing early would let a later request against a still-referenced inode
    /// resolve to nothing — or worse, to whatever path reused the number.
    #[test]
    fn an_inode_survives_until_every_reference_is_returned() {
        let mut t = InodeTable::new();
        let ino = t.lookup("C:", "a.txt");
        t.lookup("C:", "a.txt"); // kernel looked it up twice
        t.lookup("C:", "a.txt"); // and again

        t.forget(ino, 1);
        assert!(t.get(ino).is_some(), "two references remain");
        t.forget(ino, 2);
        assert!(t.get(ino).is_none(), "all references returned; entry freed");

        // A fresh lookup of the same path allocates again, cleanly.
        let again = t.lookup("C:", "a.txt");
        assert!(t.get(again).is_some());
    }

    /// `readdirplus` counts a reference only for entries it actually sent, so
    /// referencing an already-resolved inode must be its own step — a listing
    /// whose tail did not fit must not leave those inodes referenced forever.
    #[test]
    fn a_reference_can_be_counted_without_resolving_a_path() {
        let mut t = InodeTable::new();
        let ino = t.resolve("C:", "a.txt"); // named in a listing, not yet sent
        assert_eq!(t.resolve("C:", "a.txt"), ino, "resolving twice is stable");

        t.reference(ino);
        assert!(t.get(ino).is_some(), "the entry the kernel received is held");
        t.forget(ino, 1);
        assert!(t.get(ino).is_none());

        // Referencing an inode we no longer track is a no-op, not a panic:
        // the kernel may have forgotten it between resolve and send.
        t.reference(4242);

        // Releasing must spare an entry the kernel does hold — a listing names
        // inodes that an earlier `lookup` already referenced.
        let held = t.lookup("C:", "held.txt");
        t.release_unreferenced(held);
        assert!(t.get(held).is_some(), "a referenced entry survives release");
    }

    /// Forgetting more references than were handed out must not underflow.
    #[test]
    fn over_forgetting_is_survivable() {
        let mut t = InodeTable::new();
        let ino = t.lookup("C:", "a.txt");
        t.forget(ino, 99);
        assert!(t.get(ino).is_none());
        // And forgetting an inode we no longer know is a no-op, not a panic.
        t.forget(ino, 1);
        t.forget(4242, 1);
    }

    /// The exact `readdirplus` sequence: a listing is resolved whole, but only
    /// the prefix that fits the reply buffer is sent, and the kernel resumes
    /// from there. Every entry must end up with exactly one reference per time
    /// the kernel was actually handed it — no more, or the inode is never
    /// freed; no fewer, and the number is reused while the kernel still holds
    /// it, which is the silent-corruption case the module docs open with.
    #[test]
    fn an_unsent_readdirplus_entry_holds_no_reference() {
        let mut t = InodeTable::new();
        let names = ["a.txt", "b.txt", "c.txt"];

        // Pass one: resolve all three, send only the first two.
        let inos: Vec<u64> = names.iter().map(|n| t.resolve("C:", n)).collect();
        for ino in &inos[..2] {
            t.reference(*ino);
        }
        // The third was resolved but never sent, so it holds no reference and
        // the kernel will never forget it — the listing drops it itself.
        t.release_unreferenced(inos[2]);
        assert!(t.get(inos[2]).is_none(), "the entry that did not fit is free");

        // Pass two: the kernel asks again from the third, which is sent now.
        let resumed = t.resolve("C:", names[2]);
        t.reference(resumed);

        // Each of the three is held exactly once: one forget frees it.
        for (i, n) in names.iter().enumerate() {
            let ino = if i == 2 { resumed } else { inos[i] };
            t.forget(ino, 1);
            assert!(t.get(ino).is_none(), "{n} was referenced more than once");
        }
    }

    /// The root must outlive any amount of forgetting: losing it would leave
    /// the mount unable to resolve a single path.
    #[test]
    fn the_root_is_permanent() {
        let mut t = InodeTable::new();
        t.forget(ROOT_INO, u64::MAX);
        assert_eq!(t.get(ROOT_INO), Some(&Entry::Root));
    }

    /// After a rename the kernel keeps using the old inode, so it has to point
    /// at the new path.
    #[test]
    fn a_rename_moves_the_inode_to_the_new_path() {
        let mut t = InodeTable::new();
        let ino = t.lookup("C:", "old.txt");
        t.rename(ino, "C:", "new.txt");

        assert_eq!(
            t.get(ino),
            Some(&Entry::Path {
                volume: "C:".into(),
                path: "new.txt".into()
            })
        );
        // Looking up the new path finds the same inode, not a second one.
        assert_eq!(t.lookup("C:", "new.txt"), ino);
        // And the old name is free to be a different file now.
        assert_ne!(t.lookup("C:", "old.txt"), ino);
    }

    /// Wire paths join without a stray leading separator at a volume root.
    #[test]
    fn paths_join_in_wire_form() {
        assert_eq!(join("", "Users"), "Users");
        assert_eq!(join("Users", "theroer"), "Users/theroer");
        assert_eq!(parent_of("Users/theroer/a.txt"), Some("Users/theroer"));
        assert_eq!(parent_of("Users"), Some(""));
        assert_eq!(parent_of(""), None, "a volume root has no parent");
    }
}
