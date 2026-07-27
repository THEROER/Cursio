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
