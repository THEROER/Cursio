//! Link tests kept: one JSON line per test in the data directory, newest
//! last, trimmed to [`KEEP`].
//!
//! A test that runs by itself is worth something only if it can be read
//! later — "was the link this slow yesterday too?" is the question it
//! answers — and it is what spaces those tests out: the last one's time, read
//! back after a restart, is when the next is due.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use cursio_protocol::probe::ProbeResult;
use serde::{Deserialize, Serialize};

/// Tests kept across every peer: twice a day for each of a few machines is
/// months of them.
pub const KEEP: usize = 1000;

const FILE: &str = "link-tests.jsonl";

/// One kept test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub peer: String,
    #[serde(flatten)]
    pub result: ProbeResult,
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn path() -> Option<PathBuf> {
    crate::data_dir::data_dir().map(|d| d.join(FILE))
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Keep `result` of a test of the link to `peer`.
pub fn record(peer: &str, result: &ProbeResult) -> Result<(), String> {
    let path = path().ok_or("this machine has no data directory")?;
    let _held = lock();
    record_at(&path, peer, result, KEEP)
}

fn record_at(
    path: &std::path::Path,
    peer: &str,
    result: &ProbeResult,
    keep: usize,
) -> Result<(), String> {
    let line = serde_json::to_string(&Kept {
        peer: peer.to_owned(),
        result: result.clone(),
    })
    .map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut kept = read_at(path);
    if kept.len() >= keep {
        kept.drain(..=kept.len() - keep);
        let mut text = String::new();
        for k in &kept {
            text.push_str(&serde_json::to_string(k).map_err(|e| e.to_string())?);
            text.push('\n');
        }
        text.push_str(&line);
        text.push('\n');
        return std::fs::write(path, text)
            .map_err(|e| format!("cannot write {}: {e}", path.display()));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    writeln!(file, "{line}").map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn read_at(path: &std::path::Path) -> Vec<Kept> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    // A line this build cannot read — torn by a crash mid-write, or written by
    // a newer build — is skipped rather than costing every other test.
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Every kept test of the link to `peer` (any peer when `None`), oldest
/// first.
pub fn read(peer: Option<&str>) -> Vec<Kept> {
    let Some(path) = path() else {
        return Vec::new();
    };
    let _held = lock();
    read_at(&path)
        .into_iter()
        .filter(|k| peer.is_none_or(|p| k.peer.eq_ignore_ascii_case(p)))
        .collect()
}

/// When the link to `peer` was last tested, Unix seconds.
pub fn last_at(peer: &str) -> Option<u64> {
    read(Some(peer)).last().map(|k| k.result.at_unix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::probe::{Direction, RoundTrips};

    fn result(at: u64) -> ProbeResult {
        ProbeResult {
            at_unix: at,
            path: Some("network".into()),
            idle: RoundTrips::default(),
            download: Direction {
                bytes_per_sec: 50_000_000,
                loaded: RoundTrips::default(),
            },
            upload: Direction::default(),
            automatic: false,
        }
    }

    #[test]
    fn kept_tests_read_back_oldest_first_and_stay_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        for at in 0..15 {
            let peer = if at % 2 == 0 { "Lightmoon" } else { "dark" };
            record_at(&path, peer, &result(at), 10).unwrap();
        }
        let kept = read_at(&path);
        assert_eq!(kept.len(), 10);
        assert_eq!(kept.first().unwrap().result.at_unix, 5);
        assert_eq!(kept.last().unwrap().result.at_unix, 14);
    }

    #[test]
    fn a_torn_line_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        record_at(&path, "Lightmoon", &result(1), KEEP).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"peer\":\"Light")
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        record_at(&path, "Lightmoon", &result(2), KEEP).unwrap();
        assert_eq!(
            read_at(&path)
                .iter()
                .map(|k| k.result.at_unix)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}
