//! Noticing a program that is walking a peer's whole disk.
//!
//! A mounted peer volume looks local, so everything that walks local disks
//! walks it too: `find /`, a desktop indexer, a backup tool, an IDE scanning
//! `~` for projects. Locally that costs a few seconds of disk. Here every
//! directory it lists is a round trip to another machine, a walk of a whole
//! disk is hundreds of thousands of them, and while it runs the link is busy
//! with metadata the user never asked for — the file copy they did ask for
//! crawls, and nothing on screen says why.
//!
//! The mount is the one place that knows who is asking: FUSE hands every
//! request the caller's PID, and WinFsp hands it to the open that precedes a
//! listing. So each listing is attributed to a process, and a process that
//! lists many *distinct* directories in a short time is reported by PID and
//! command line, which is enough for the user to recognise it and stop it.
//!
//! ## Distinct directories, not listings
//!
//! A file manager re-lists the folder it shows whenever anything changes, and a
//! shell completing a path lists the same parent over and over. Neither is a
//! walk, and counting calls would report both. A walk is *breadth*: many
//! different directories, each listed about once. [`WALK_DIRS`] of them inside
//! [`WALK_WINDOW`] is far beyond anything a person browsing does and well
//! inside what `find` does in its first second.
//!
//! ## Ends when it goes quiet
//!
//! A walker is reported until it has listed nothing for [`WALK_IDLE`]. The
//! mount only runs when asked, so a walker that simply stopped is noticed by
//! [`sweep`], which the session calls on its own schedule.
//!
//! ## Why a process-wide detector with a reporter hook
//!
//! Mounts are built per volume and per peer, their callbacks run on FUSE and
//! WinFsp threads, and this crate is below `cursio-core`, which owns the
//! machine's problem list. One detector behind a lock, and one reporter the
//! core installs with [`set_reporter`], keep the mounts free of both concerns.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How many distinct directories within [`WALK_WINDOW`] make a walk.
pub const WALK_DIRS: usize = 200;

/// The window those directories must fall in.
pub const WALK_WINDOW: Duration = Duration::from_secs(10);

/// How long a reported walker must be silent to count as finished.
pub const WALK_IDLE: Duration = Duration::from_secs(20);

/// A process found walking a mounted peer volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walker {
    pub pid: u32,
    /// Its command line where the OS will say, else its image name, else
    /// nothing.
    pub command: Option<String>,
    /// The peer whose disk is being walked.
    pub peer: String,
    /// The peer's volume id, as on the wire.
    pub volume: String,
}

/// What the detector tells its reporter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkEvent {
    Started(Walker),
    Ended(Walker),
}

/// Which mount and which process a trail belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WalkKey {
    pub peer: String,
    pub volume: String,
    pub pid: u32,
}

struct Trail {
    /// Distinct directories listed within the window, with when each was last
    /// listed. Emptied once the walker is reported: from then on only
    /// `last` matters, which keeps a walk of a million directories from
    /// growing a million entries here.
    dirs: HashMap<u64, Instant>,
    last: Instant,
    reported: bool,
}

/// The heuristic, free of clocks and processes so it can be tested.
#[derive(Default)]
pub struct WalkDetector {
    trails: HashMap<WalkKey, Trail>,
}

impl WalkDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `key` listed directory `dir` at `now`.
    ///
    /// `true` exactly once per walk: the listing that crosses the threshold.
    pub fn observe(&mut self, key: &WalkKey, dir: u64, now: Instant) -> bool {
        let trail = self.trails.entry(key.clone()).or_insert_with(|| Trail {
            dirs: HashMap::new(),
            last: now,
            reported: false,
        });
        trail.last = now;
        if trail.reported {
            return false;
        }
        trail.dirs.insert(dir, now);
        trail
            .dirs
            .retain(|_, at| now.saturating_duration_since(*at) <= WALK_WINDOW);
        if trail.dirs.len() < WALK_DIRS {
            return false;
        }
        trail.reported = true;
        trail.dirs = HashMap::new();
        true
    }

    /// Forget trails that went quiet, returning the reported ones among them.
    pub fn expire(&mut self, now: Instant) -> Vec<WalkKey> {
        let mut ended = Vec::new();
        self.trails.retain(|key, trail| {
            let quiet = now.saturating_duration_since(trail.last);
            let keep = match trail.reported {
                true => quiet < WALK_IDLE,
                false => quiet <= WALK_WINDOW,
            };
            if !keep && trail.reported {
                ended.push(key.clone());
            }
            keep
        });
        ended
    }
}

static DETECTOR: Mutex<Option<WalkDetector>> = Mutex::new(None);
static REPORTER: OnceLock<fn(&WalkEvent)> = OnceLock::new();

/// Where walk reports go. The first call wins; later ones are ignored, since
/// one process has one problem list.
pub fn set_reporter(reporter: fn(&WalkEvent)) {
    let _ = REPORTER.set(reporter);
}

/// A mount callback's report: `pid` listed directory `dir` of `peer`'s
/// `volume`.
///
/// Cheap on the common path — one lock and a map insert — because it runs on
/// every listing. The command line is read only when a walk is first reported.
pub fn observe(peer: &str, volume: &str, pid: u32, dir: u64) {
    // PID 0 is the kernel acting on nobody's behalf (FUSE's own requests,
    // WinFsp operations that carry no caller), and Cursio's own work on its
    // mount is not a walk the user needs telling about.
    if pid == 0 || pid == std::process::id() || REPORTER.get().is_none() {
        return;
    }
    let key = WalkKey {
        peer: peer.to_string(),
        volume: volume.to_string(),
        pid,
    };
    let now = Instant::now();
    let (started, ended) = {
        let mut guard = DETECTOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let detector = guard.get_or_insert_with(WalkDetector::new);
        (detector.observe(&key, dir, now), detector.expire(now))
    };
    if started {
        report(&WalkEvent::Started(walker(&key, command_of(pid))));
    }
    for key in ended {
        report(&WalkEvent::Ended(walker(&key, None)));
    }
}

/// End the walks that went quiet since the last listing anywhere.
pub fn sweep() {
    let ended = {
        let mut guard = DETECTOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.as_mut() {
            Some(detector) => detector.expire(Instant::now()),
            None => return,
        }
    };
    for key in ended {
        report(&WalkEvent::Ended(walker(&key, None)));
    }
}

fn walker(key: &WalkKey, command: Option<String>) -> Walker {
    Walker {
        pid: key.pid,
        command,
        peer: key.peer.clone(),
        volume: key.volume.clone(),
    }
}

fn report(event: &WalkEvent) {
    if let Some(reporter) = REPORTER.get() {
        reporter(event);
    }
}

/// A stable number for a directory path, for mounts that know directories by
/// path rather than by inode.
pub fn dir_key(volume: &str, path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    volume.hash(&mut hasher);
    path.hash(&mut hasher);
    hasher.finish()
}

/// Longest command line kept. A walker's first words say what it is; the rest
/// is usually a list of paths.
const COMMAND_MAX: usize = 160;

/// Turn `/proc/<pid>/cmdline` into one readable line.
pub fn cmdline_to_string(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let line = text
        .split('\0')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        return None;
    }
    Some(match line.char_indices().nth(COMMAND_MAX) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    })
}

/// What `pid` is, as far as the OS will say to an unprivileged process.
#[cfg(target_os = "linux")]
pub fn command_of(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    // A kernel thread has an empty command line; its name is still there.
    cmdline_to_string(&raw).or_else(|| {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|c| c.trim().to_string())
    })
}

/// What `pid` is, as far as the OS will say to an unprivileged process.
///
/// The image path rather than the command line: reading another process's
/// command line on Windows means reading its memory, which needs rights over
/// it that `PROCESS_QUERY_LIMITED_INFORMATION` deliberately does not grant.
#[cfg(target_os = "windows")]
pub fn command_of(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: plain handle query; a null handle is checked before use.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: `process` is a live handle opened above, and `buffer`/`len`
    // describe a writable buffer of exactly that many UTF-16 units.
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len)
    };
    // SAFETY: closed exactly once, on every path after it was opened.
    unsafe { CloseHandle(process) };
    (ok != 0).then(|| String::from_utf16_lossy(&buffer[..len as usize]))
}

/// No way to ask on this platform, and no mount here to ask for.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn command_of(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pid: u32) -> WalkKey {
        WalkKey {
            peer: "Lightmoon".into(),
            volume: "C:".into(),
            pid,
        }
    }

    #[test]
    fn listing_many_distinct_directories_quickly_is_a_walk_reported_once() {
        let mut d = WalkDetector::new();
        let t0 = Instant::now();
        let mut reports = 0;
        for dir in 0..(WALK_DIRS as u64 * 3) {
            if d.observe(&key(42), dir, t0 + Duration::from_millis(dir)) {
                reports += 1;
            }
        }
        assert_eq!(reports, 1, "one walk is one report, however long it runs");
    }

    /// A file manager re-listing the folder it shows, or a shell completing a
    /// path, lists the same few directories over and over. That is not a walk.
    #[test]
    fn relisting_the_same_directories_is_not_a_walk() {
        let mut d = WalkDetector::new();
        let t0 = Instant::now();
        for n in 0..10_000u64 {
            assert!(!d.observe(&key(7), n % 5, t0 + Duration::from_millis(n / 10)));
        }
    }

    /// Someone browsing folder after folder for an afternoon eventually lists
    /// hundreds of directories, but never hundreds within seconds.
    #[test]
    fn distinct_directories_spread_over_time_are_not_a_walk() {
        let mut d = WalkDetector::new();
        let t0 = Instant::now();
        for dir in 0..(WALK_DIRS as u64 * 4) {
            let at = t0 + Duration::from_secs(dir);
            assert!(!d.observe(&key(9), dir, at), "reported at directory {dir}");
        }
    }

    #[test]
    fn two_processes_are_counted_separately() {
        let mut d = WalkDetector::new();
        let t0 = Instant::now();
        let half = WALK_DIRS as u64 / 2 + 1;
        for dir in 0..half {
            assert!(!d.observe(&key(1), dir, t0));
            assert!(!d.observe(&key(2), half + dir, t0));
        }
    }

    #[test]
    fn a_walk_ends_once_the_walker_goes_quiet() {
        let mut d = WalkDetector::new();
        let t0 = Instant::now();
        for dir in 0..WALK_DIRS as u64 {
            d.observe(&key(42), dir, t0);
        }
        assert!(d.expire(t0 + WALK_IDLE / 2).is_empty(), "still going");
        assert_eq!(d.expire(t0 + WALK_IDLE), vec![key(42)]);
        assert!(d.expire(t0 + WALK_IDLE * 2).is_empty(), "ended once");
    }

    /// A process that never crossed the threshold is forgotten silently: it
    /// was never reported, so there is nothing to end.
    #[test]
    fn a_quiet_process_that_never_walked_ends_nothing() {
        let mut d = WalkDetector::new();
        let t0 = Instant::now();
        d.observe(&key(5), 1, t0);
        assert!(d.expire(t0 + WALK_WINDOW * 2).is_empty());
    }

    #[test]
    fn a_command_line_reads_as_one_line_and_is_cut_when_long() {
        assert_eq!(
            cmdline_to_string(b"find\0/home/theroer/Cursio\0-name\0*.rs\0").as_deref(),
            Some("find /home/theroer/Cursio -name *.rs")
        );
        assert_eq!(cmdline_to_string(b""), None);
        let long = format!("tracker-miner-fs-3\0{}", "x".repeat(500));
        let cut = cmdline_to_string(long.as_bytes()).expect("a line");
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), COMMAND_MAX + 1);
    }

    #[test]
    fn the_same_path_on_the_same_volume_is_the_same_directory() {
        assert_eq!(dir_key("C:", "Users"), dir_key("C:", "Users"));
        assert_ne!(dir_key("C:", "Users"), dir_key("D:", "Users"));
    }
}
