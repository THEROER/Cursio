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
    /// Position in `entries` by exact name, so answering one name from a
    /// listing of thousands is neither a scan nor a copy of the whole.
    by_name: HashMap<String, usize>,
    /// Every name lowercased: a name that matches only this way may still be
    /// there on a Windows owner, which compares names without case.
    folded: std::collections::HashSet<String>,
    at: Instant,
}

impl Listing {
    fn new(entries: Vec<FileMeta>) -> Self {
        let by_name = entries
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.clone(), i))
            .collect();
        let folded = entries.iter().map(|m| m.name.to_lowercase()).collect();
        Self {
            entries,
            by_name,
            folded,
            at: Instant::now(),
        }
    }

    /// What this listing says about `name`, or `None` when only the owner can
    /// say: the name differs from a listed one by case alone.
    fn answer(&self, name: &str) -> Option<Result<FileMeta, cursio_protocol::FileError>> {
        if let Some(&i) = self.by_name.get(name) {
            return Some(Ok(self.entries[i].clone()));
        }
        if self.folded.contains(&name.to_lowercase()) {
            return None;
        }
        Some(Err(cursio_protocol::FileError::NotFound))
    }
}

/// What a held listing says about one name.
enum Listed {
    /// No listing of the directory is fresh.
    NotHeld,
    /// One is, but the name matches a listed one only without case.
    Undecided,
    Answer(Result<FileMeta, cursio_protocol::FileError>),
}

/// Questions about names a directory's listing could have answered, counted to
/// notice the directory being swept.
struct Misses {
    count: usize,
    since: Instant,
    /// How many entries the directory had when it was last listed, which
    /// decides how many misses its next listing is worth.
    last_listed: Option<usize>,
    /// A listing is on its way: another thread's miss asks for its one path
    /// rather than listing the same directory a second time.
    listing: bool,
    /// The owner refused the listing in this window — a directory that may be
    /// entered but not read — so the window's misses are asked one by one.
    refused: bool,
}

/// One listing is worth this many entries per miss in its directory.
///
/// A listing is one round trip whose reply grows with the directory, and a
/// miss is one round trip of tens of bytes. A directory of a few hundred is
/// worth listing for the second name asked; one of tens of thousands — a
/// system directory, a package store — only once many of its names are being
/// asked about, or two `stat`s a second there would pull megabytes.
const ENTRIES_PER_MISS: usize = 256;

/// The fewest misses in one window that make a sweep.
///
/// Two: one name is an application checking one path; a second distinct
/// name in the same directory within [`META_TTL`] is a walk — `git status`,
/// a build's dependency scan — which goes on to ask about the rest.
const SWEEP_MISSES: usize = 2;

/// How many directories' miss counts to hold at once.
const MAX_MISS_DIRS: usize = MAX_LISTINGS * 4;

/// The parent directory and last component of a wire path.
fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
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
    /// Per directory, the names asked about that no listing could answer.
    misses: Mutex<HashMap<(String, String), Misses>>,
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
            misses: Mutex::new(HashMap::new()),
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
            Listing::new(entries),
        );
    }

    /// What a fresh listing of `path`'s directory says about it.
    fn listed(&self, volume: &str, path: &str) -> Listed {
        let (parent, name) = split_path(path);
        let key = (volume.to_string(), parent.to_string());
        let listings = self.listings.lock().expect("listing cache");
        match listings.get(&key) {
            Some(held) if held.at.elapsed() < META_TTL => match held.answer(name) {
                Some(answer) => Listed::Answer(answer),
                None => Listed::Undecided,
            },
            _ => Listed::NotHeld,
        }
    }

    /// Count a miss in `dir`, and say whether this one should list it.
    fn note_miss(&self, volume: &str, dir: &str) -> bool {
        let mut misses = self.misses.lock().expect("miss counts");
        if misses.len() >= MAX_MISS_DIRS {
            misses.clear();
        }
        let held = misses
            .entry((volume.to_string(), dir.to_string()))
            .or_insert(Misses {
                count: 0,
                since: Instant::now(),
                last_listed: None,
                listing: false,
                refused: false,
            });
        if held.since.elapsed() >= META_TTL {
            held.count = 0;
            held.since = Instant::now();
            held.refused = false;
        }
        held.count += 1;
        let needed = (held.last_listed.unwrap_or(0) / ENTRIES_PER_MISS).max(SWEEP_MISSES);
        if held.listing || held.refused || held.count < needed {
            return false;
        }
        held.listing = true;
        true
    }

    /// Record how the listing [`Self::note_miss`] asked for went.
    fn listed_dir(&self, volume: &str, dir: &str, outcome: Option<usize>) {
        let mut misses = self.misses.lock().expect("miss counts");
        if let Some(held) = misses.get_mut(&(volume.to_string(), dir.to_string())) {
            held.listing = false;
            match outcome {
                Some(len) => {
                    held.last_listed = Some(len);
                    held.count = 0;
                    held.since = Instant::now();
                }
                None => held.refused = true,
            }
        }
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

    /// The answer remembered for a path, or `ask`'s, which is then
    /// remembered — failures included, as [`Self::get`] explains.
    pub fn get_or_ask(
        &self,
        volume: &str,
        path: &str,
        ask: impl FnOnce() -> Result<FileMeta, cursio_protocol::FileError>,
    ) -> Result<FileMeta, cursio_protocol::FileError> {
        if let Some(hit) = self.get(volume, path) {
            return hit;
        }
        let fresh = ask();
        self.put(volume, path, fresh.clone());
        fresh
    }

    /// As [`Self::get_or_ask`], answering from the directory's listing when
    /// one is held — and fetching that listing once the directory is being
    /// swept.
    ///
    /// A tree walked by name never lists anything: `git status` `lstat`s
    /// every tracked file to compare it with the index, and a build checks
    /// each dependency it knows of. Each name was its own round trip, so
    /// `git diff --stat` over two thousand files on a peer's disk took 33 s
    /// of almost pure waiting. The names such a walk asks about cluster by
    /// directory, so the second distinct miss in one directory within
    /// [`META_TTL`] lists it — one round trip — and the rest of that
    /// directory is answered here, including the names that are not there.
    ///
    /// A listing answers for [`META_TTL`] from when it was fetched, and the
    /// kernel may then hold the answer for its own `CACHE_TTL`: a change made
    /// on the owner can take up to the sum of the two to appear, against one
    /// of them for a path asked about directly.
    pub fn get_or_ask_swept(
        &self,
        volume: &str,
        path: &str,
        ask: impl FnOnce() -> Result<FileMeta, cursio_protocol::FileError>,
        list: impl FnOnce(&str) -> Result<Vec<FileMeta>, cursio_protocol::FileError>,
    ) -> Result<FileMeta, cursio_protocol::FileError> {
        if let Some(hit) = self.get(volume, path) {
            return hit;
        }
        if path.is_empty() {
            return self.get_or_ask(volume, path, ask);
        }
        match self.listed(volume, path) {
            Listed::Answer(answer) => return answer,
            Listed::Undecided => return self.get_or_ask(volume, path, ask),
            Listed::NotHeld => {}
        }
        let (dir, _) = split_path(path);
        if self.note_miss(volume, dir) {
            match list(dir) {
                Ok(entries) => {
                    self.listed_dir(volume, dir, Some(entries.len()));
                    self.put_listing(volume, dir, entries);
                    if let Listed::Answer(answer) = self.listed(volume, path) {
                        return answer;
                    }
                }
                Err(e) => {
                    // A directory that may be entered but not read is an
                    // ordinary thing on Windows; the path is asked about on its
                    // own, and that answer carries any error that matters.
                    tracing::debug!(volume, dir, error = ?e, "listing a swept directory was refused");
                    self.listed_dir(volume, dir, None);
                }
            }
        }
        self.get_or_ask(volume, path, ask)
    }
}

/// How long the volume list is trusted before being refetched.
///
/// Far longer than [`META_TTL`]: drives do not come and go the way file
/// contents change, and this list is consulted on every path resolution.
const VOLUMES_TTL: Duration = Duration::from_secs(30);

/// The peer's volume list, fetched once and refetched when it goes stale.
///
/// Cached because a mount consults it to resolve every path, and the root of
/// a mount is nothing but this list: without it every listing of the mount
/// point is a round trip.
#[derive(Default)]
pub struct VolumeList {
    fetched: Mutex<(Vec<cursio_protocol::Volume>, Option<Instant>)>,
}

impl VolumeList {
    /// The list, from `fetch` when the one held has expired or is empty.
    pub fn get(
        &self,
        fetch: impl FnOnce() -> Result<Vec<cursio_protocol::Volume>, cursio_protocol::FileError>,
    ) -> Result<Vec<cursio_protocol::Volume>, cursio_protocol::FileError> {
        if let (list, Some(at)) = &*self.fetched.lock().expect("volume list") {
            if at.elapsed() < VOLUMES_TTL && !list.is_empty() {
                return Ok(list.clone());
            }
        }
        // Fetched without the lock held: this is a network round trip, and
        // holding the lock across it would serialise every other callback
        // behind it.
        let fetched = fetch()?;
        *self.fetched.lock().expect("volume list") = (fetched.clone(), Some(Instant::now()));
        Ok(fetched)
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

    /// Asks `cache` about `path` the way a mount does, counting what reached
    /// the peer.
    fn ask(
        cache: &MetaCache,
        path: &str,
        listing: &[FileMeta],
        stats: &std::cell::Cell<u32>,
        lists: &std::cell::Cell<u32>,
    ) -> Result<FileMeta, FileError> {
        cache.get_or_ask_swept(
            "C:",
            path,
            || {
                stats.set(stats.get() + 1);
                let (_, name) = split_path(path);
                listing
                    .iter()
                    .find(|m| m.name.eq_ignore_ascii_case(name))
                    .cloned()
                    .ok_or(FileError::NotFound)
            },
            |_| {
                lists.set(lists.get() + 1);
                Ok(listing.to_vec())
            },
        )
    }

    /// `git status` over a peer's disk: the second name asked about in one
    /// directory lists it, and every other name there — present or not — is
    /// answered without another round trip.
    #[test]
    fn a_directory_swept_by_name_is_listed_once_and_answers_the_rest() {
        let c = MetaCache::new();
        let listing = [meta("a", 1), meta("b", 2), meta("c", 3), meta("d", 4)];
        let (stats, lists) = (std::cell::Cell::new(0), std::cell::Cell::new(0));

        assert_eq!(ask(&c, "src/a", &listing, &stats, &lists).unwrap().size, 1);
        assert_eq!((stats.get(), lists.get()), (1, 0), "one name is one stat");

        assert_eq!(ask(&c, "src/b", &listing, &stats, &lists).unwrap().size, 2);
        assert_eq!(ask(&c, "src/c", &listing, &stats, &lists).unwrap().size, 3);
        assert_eq!(ask(&c, "src/d", &listing, &stats, &lists).unwrap().size, 4);
        assert!(matches!(
            ask(&c, "src/absent", &listing, &stats, &lists),
            Err(FileError::NotFound)
        ));
        assert_eq!(
            (stats.get(), lists.get()),
            (1, 1),
            "the rest of the directory cost one listing"
        );
    }

    /// A Windows owner finds `readme.md` when `README.md` is there; the listing
    /// cannot say so, and must neither refuse the name nor list again.
    #[test]
    fn a_name_matching_a_listed_one_only_without_case_is_asked_about() {
        let c = MetaCache::new();
        let listing = [meta("README.md", 1), meta("x", 2)];
        let (stats, lists) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
        c.put_listing("C:", "doc", listing.to_vec());

        let got = ask(&c, "doc/readme.md", &listing, &stats, &lists);
        assert_eq!(got.unwrap().name, "README.md");
        assert_eq!((stats.get(), lists.get()), (1, 0));
    }

    /// A directory the owner will not list is asked about name by name for the
    /// rest of the window, rather than every other miss trying again.
    #[test]
    fn a_refused_listing_falls_back_to_asking_without_retrying() {
        let c = MetaCache::new();
        let (stats, lists) = (std::cell::Cell::new(0u32), std::cell::Cell::new(0u32));
        for name in ["a", "b", "c", "d"] {
            let _ = c.get_or_ask_swept(
                "C:",
                &format!("locked/{name}"),
                || {
                    stats.set(stats.get() + 1);
                    Ok(meta(name, 0))
                },
                |_| {
                    lists.set(lists.get() + 1);
                    Err(FileError::Denied)
                },
            );
        }
        assert_eq!(stats.get(), 4, "every name still answered");
        assert_eq!(lists.get(), 1, "the refusal is not asked for again");
    }

    /// A directory of thousands is listed again only once enough of its names
    /// are being asked about: two `stat`s a second in a system directory must
    /// not pull its whole listing each time.
    #[test]
    fn a_large_directory_needs_more_misses_before_it_is_listed_again() {
        let c = MetaCache::new();
        assert!(!c.note_miss("C:", "big"));
        assert!(
            c.note_miss("C:", "big"),
            "unknown size: the second miss lists"
        );
        c.listed_dir("C:", "big", Some(10 * ENTRIES_PER_MISS));

        let lists_at = (1..=10).find(|_| c.note_miss("C:", "big"));
        assert_eq!(lists_at, Some(10));
    }

    /// A volume list still fresh is answered without asking the peer, and a
    /// failed fetch is not remembered: the next question asks again.
    #[test]
    fn the_volume_list_is_fetched_once_while_fresh() {
        let volumes = VolumeList::default();
        let asked = std::cell::Cell::new(0);
        let fetch = || {
            asked.set(asked.get() + 1);
            Ok(vec![cursio_protocol::Volume {
                id: "C:".into(),
                label: "Windows (C:)".into(),
                writable: true,
                total_bytes: 1,
                free_bytes: 1,
                removable: false,
            }])
        };
        assert!(volumes.get(|| Err(FileError::NotFound)).is_err());
        assert_eq!(volumes.get(fetch).unwrap().len(), 1);
        assert_eq!(volumes.get(fetch).unwrap()[0].id, "C:");
        assert_eq!(asked.get(), 1);
    }
}
