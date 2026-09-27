//! A short-lived cache of what the peer said about a path.
//!
//! Every `stat` is a network round trip, and applications ask for the same path
//! several times in a row as a matter of course: Explorer stats an entry it has
//! just listed, an installer stats a file it is about to create, and a build
//! tool stats each header it opens. Measured against a peer with a 9 ms round
//! trip, Steam's preallocation pass spent about 150 ms per file — seventeen
//! round trips each, for a file it was creating from scratch.
//!
//! ## Why a TTL rather than invalidation alone
//!
//! The owner's disk changes without telling us: the person sitting at that
//! machine can edit a file at any moment, and nothing in the protocol
//! announces it. So an entry is never trusted for long — [`META_TTL`] is short
//! enough that a stale answer is the same order of staleness a local
//! filesystem's own attribute cache allows, and long enough that a burst of
//! repeated questions about one path costs a single round trip.
//!
//! Operations *this* machine performs are a different matter: those we know
//! about exactly, and the entry is dropped at once rather than left to expire.
//! That is what [`MetaCache::forget`] and [`MetaCache::forget_dir`] are for —
//! writing, truncating, creating and deleting all invalidate immediately, so a
//! caller never reads back its own change as the value it had before.
//!
//! ## What is deliberately not cached
//!
//! File *contents*. A stale byte is corruption, whereas a stale size is a
//! momentarily wrong number in a listing, and the two are not comparable. Reads
//! always go to the peer.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cursio_protocol::FileMeta;

/// How long a cached answer is trusted.
///
/// Chosen to match the attribute-cache lifetime the FUSE mount already hands
/// the kernel (`mount::attr::CACHE_TTL`), so both halves of the crate go stale
/// on the same schedule and a user comparing the two sees one answer rather
/// than two. Longer would be measurably faster and would start showing edits
/// made on the owner's machine noticeably late.
pub const META_TTL: Duration = Duration::from_secs(1);

/// How many paths to remember at once.
///
/// A directory walk touches thousands of paths and never revisits most of them,
/// so an unbounded map would grow for the life of the mount to hold answers
/// nothing will ask for again. The cap is well above any single directory's
/// working set — a listing of 1815 entries, which Forza's audio directory
/// really is, fits several times over.
const MAX_ENTRIES: usize = 8192;

/// How many directory listings to remember at once.
///
/// Smaller than [`MAX_ENTRIES`] because a tree holds far fewer directories than
/// paths, and each listing is the whole directory rather than one entry — a
/// cap of a few hundred covers what any application has open while keeping the
/// memory this holds bounded.
const MAX_LISTINGS: usize = 256;

/// What the peer last said about a path, and when.
struct Entry {
    meta: Result<FileMeta, cursio_protocol::FileError>,
    at: Instant,
}

/// A directory's contents, and when they were fetched.
struct Listing {
    entries: Vec<FileMeta>,
    at: Instant,
}

/// Answers about paths, remembered briefly.
///
/// Keyed by `(volume, path)` exactly as the wire addresses them, so no
/// normalisation can make two different paths collide.
pub struct MetaCache {
    entries: Mutex<HashMap<(String, String), Entry>>,
    /// Whole directory listings.
    ///
    /// Windows enumerates a directory in batches and asks the filesystem again
    /// for each one, so a single Explorer window or a single pass of an
    /// installer produces many identical listings. Measured on a peer serving
    /// Steam: 1980 listings in 30 seconds — 66 a second — of directories
    /// holding up to 1815 entries each. At roughly 177 KiB a listing that is
    /// more than 11 MB/s of metadata, which is the whole of what the link can
    /// carry; the file writes sharing it then missed the mount's 3 s deadline
    /// and Windows reported the drive as failing.
    listings: Mutex<HashMap<(String, String), Listing>>,
}

impl Default for MetaCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MetaCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            listings: Mutex::new(HashMap::new()),
        }
    }

    /// A directory's contents, if they were fetched recently enough.
    pub fn listing(&self, volume: &str, path: &str) -> Option<Vec<FileMeta>> {
        let key = (volume.to_string(), path.to_string());
        let listings = self.listings.lock().expect("listing cache");
        let held = listings.get(&key)?;
        (held.at.elapsed() < META_TTL).then(|| held.entries.clone())
    }

    /// Remember a directory's contents.
    pub fn put_listing(&self, volume: &str, path: &str, entries: Vec<FileMeta>) {
        let mut listings = self.listings.lock().expect("listing cache");
        // Bounded like the path cache, and for the same reason: a walk touches
        // directories it will never revisit. Far fewer directories than paths,
        // so the cap is correspondingly smaller.
        if listings.len() >= MAX_LISTINGS {
            listings.clear();
        }
        listings.insert(
            (volume.to_string(), path.to_string()),
            Listing {
                entries,
                at: Instant::now(),
            },
        );
    }

    /// Forget a directory's contents, because something in it changed.
    fn forget_listing(&self, volume: &str, path: &str) {
        let key = (volume.to_string(), path.to_string());
        self.listings.lock().expect("listing cache").remove(&key);
    }

    /// The directory a path sits in, for invalidating its listing.
    fn parent_of(path: &str) -> &str {
        match path.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        }
    }

    /// What the peer said about this path, if it was said recently enough.
    ///
    /// A remembered *failure* counts: an application that stats a file which is
    /// not there usually asks again immediately, and `NotFound` is as much an
    /// answer as a `FileMeta`. Steam's preallocation pass asks for thousands of
    /// paths that do not exist yet, one after another.
    pub fn get(
        &self,
        volume: &str,
        path: &str,
    ) -> Option<Result<FileMeta, cursio_protocol::FileError>> {
        let key = (volume.to_string(), path.to_string());
        let entries = self.entries.lock().expect("meta cache");
        let entry = entries.get(&key)?;
        (entry.at.elapsed() < META_TTL).then(|| entry.meta.clone())
    }

    /// Remember what the peer said.
    pub fn put(
        &self,
        volume: &str,
        path: &str,
        meta: Result<FileMeta, cursio_protocol::FileError>,
    ) {
        let mut entries = self.entries.lock().expect("meta cache");
        if entries.len() >= MAX_ENTRIES {
            // Cleared wholesale rather than evicted one at a time. Every entry
            // expires within `META_TTL` anyway, so the cache is worth little
            // once it is this large, and an LRU's bookkeeping would cost more
            // than the round trips it saves on a walk that never looks back.
            entries.clear();
        }
        entries.insert(
            (volume.to_string(), path.to_string()),
            Entry {
                meta,
                at: Instant::now(),
            },
        );
    }

    /// Forget one path, because this machine just changed it.
    ///
    /// Its parent's listing goes too: a file that was created, deleted or
    /// resized changes what a listing of the directory holding it says, and a
    /// listing that still described the old contents would be read back as
    /// current.
    pub fn forget(&self, volume: &str, path: &str) {
        let key = (volume.to_string(), path.to_string());
        self.entries.lock().expect("meta cache").remove(&key);
        self.forget_listing(volume, Self::parent_of(path));
    }

    /// Forget a path and everything under it.
    ///
    /// For a rename or a delete of a directory: every descendant's cached
    /// answer describes a path that no longer exists.
    pub fn forget_dir(&self, volume: &str, path: &str) {
        let prefix = format!("{path}/");
        self.entries
            .lock()
            .expect("meta cache")
            .retain(|(v, p), _| v != volume || (p != path && !p.starts_with(&prefix)));
        self.listings
            .lock()
            .expect("listing cache")
            .retain(|(v, p), _| v != volume || (p != path && !p.starts_with(&prefix)));
        // And the directory this one sat in, which no longer contains it.
        self.forget_listing(volume, Self::parent_of(path));
    }

    /// Forget everything about a volume.
    pub fn clear(&self) {
        self.entries.lock().expect("meta cache").clear();
        self.listings.lock().expect("listing cache").clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::{FileError, FileKind};

    fn meta(name: &str, size: u64) -> FileMeta {
        FileMeta {
            name: name.to_string(),
            kind: FileKind::File,
            size,
            modified_unix: 0,
            writable: true,
            allocated: None,
        }
    }

    /// The point of the cache: asking twice costs one round trip.
    #[test]
    fn a_remembered_answer_comes_back() {
        let c = MetaCache::new();
        assert!(c.get("C:", "a.txt").is_none(), "nothing is known yet");

        c.put("C:", "a.txt", Ok(meta("a.txt", 42)));
        let got = c.get("C:", "a.txt").expect("remembered");
        assert_eq!(got.expect("ok").size, 42);
    }

    /// A refusal is an answer too.
    ///
    /// An installer asks about thousands of paths that do not exist yet — Steam
    /// stats every file of a download before creating it — and re-asking the
    /// peer to be told `NotFound` again is a round trip spent on a fact we
    /// already had.
    #[test]
    fn a_remembered_refusal_counts_as_an_answer() {
        let c = MetaCache::new();
        c.put("C:", "missing", Err(FileError::NotFound));
        assert!(matches!(
            c.get("C:", "missing"),
            Some(Err(FileError::NotFound))
        ));
    }

    /// A change made here must never be read back as the value it had before.
    #[test]
    fn a_path_this_machine_changed_is_forgotten_at_once() {
        let c = MetaCache::new();
        c.put("C:", "a.txt", Ok(meta("a.txt", 42)));
        c.forget("C:", "a.txt");
        assert!(c.get("C:", "a.txt").is_none());
    }

    /// Removing a directory has to take its descendants with it.
    #[test]
    fn forgetting_a_directory_forgets_what_was_inside_it() {
        let c = MetaCache::new();
        c.put("C:", "dir", Ok(meta("dir", 0)));
        c.put("C:", "dir/a.txt", Ok(meta("a.txt", 1)));
        c.put("C:", "dir/sub/b.txt", Ok(meta("b.txt", 2)));
        // A path that merely starts with the same letters is not inside it.
        c.put("C:", "directory.txt", Ok(meta("directory.txt", 3)));
        // Nor is the same name on another volume.
        c.put("D:", "dir/a.txt", Ok(meta("a.txt", 4)));

        c.forget_dir("C:", "dir");

        assert!(c.get("C:", "dir").is_none());
        assert!(c.get("C:", "dir/a.txt").is_none());
        assert!(c.get("C:", "dir/sub/b.txt").is_none());
        assert!(
            c.get("C:", "directory.txt").is_some(),
            "a prefix match is not a child"
        );
        assert!(
            c.get("D:", "dir/a.txt").is_some(),
            "another volume is another tree"
        );
    }

    /// The cache must not grow without limit on a directory walk.
    #[test]
    fn the_cache_is_bounded() {
        let c = MetaCache::new();
        for i in 0..MAX_ENTRIES + 100 {
            c.put("C:", &format!("f{i}"), Ok(meta("f", i as u64)));
        }
        let held = c.entries.lock().expect("meta cache").len();
        assert!(
            held <= MAX_ENTRIES,
            "cache grew to {held}, past its {MAX_ENTRIES} cap"
        );
    }
}
