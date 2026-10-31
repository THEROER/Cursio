//! The newest lines of this node's own log, for a peer or a person asking
//! through [`crate::diag_session`].
//!
//! A day's file is tens of megabytes and a phone answers from flash, so the
//! files are read from their end, newest first, and the scan stops at the
//! first line older than the request reaches back to — or at [`MAX_SCAN`]
//! bytes, saying so. Lines are compared by their leading timestamp as text:
//! the appender writes RFC 3339 in UTC, which sorts as it reads, so neither
//! side needs a date library or the other's time zone.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cursio_protocol::diag::{DiagResponse, MAX_FILTER_BYTES, MAX_LINES, MAX_LOG_BYTES};

/// Most bytes of log one request reads. A filter that matches nothing must
/// not have a phone read a week of files to say so.
pub const MAX_SCAN: u64 = 64 * 1024 * 1024;

const BLOCK: usize = 1024 * 1024;

/// What to take from the log; see [`cursio_protocol::diag::DiagRequest::Logs`].
#[derive(Debug, Clone, Default)]
pub struct Tail {
    pub after: Option<String>,
    pub within: Option<Duration>,
    pub contains: Option<String>,
    pub max_lines: u32,
    /// Another of Cursio's logs in the same directory, by its base name;
    /// the node's own when absent.
    pub log: Option<String>,
}

impl Tail {
    /// Bound a request from a peer before the disk sees it.
    pub fn bounded(mut self) -> Result<Self, String> {
        if self
            .contains
            .as_ref()
            .is_some_and(|c| c.len() > MAX_FILTER_BYTES)
        {
            return Err(format!("a log filter is at most {MAX_FILTER_BYTES} bytes"));
        }
        if self.after.as_ref().is_some_and(|a| a.len() > 64) {
            return Err("not a log timestamp".into());
        }
        if let Some(log) = &self.log {
            if !cursio_protocol::diag::valid_log_name(log) {
                return Err(format!(
                    "{log:?} is not one of Cursio's logs; try cursio-mount-helper or cursio-app"
                ));
            }
        }
        self.max_lines = self.max_lines.clamp(1, MAX_LINES);
        Ok(self)
    }
}

/// The log files named `file_name` and rotated from it in `dir`, newest first.
fn files(dir: &Path, file_name: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == file_name || n.starts_with(&format!("{file_name}.")))
        })
        .collect();
    // A dated suffix sorts by date; the undated name, if any, is the newest.
    found.sort_by(|a, b| {
        let undated = |p: &PathBuf| p.file_name().and_then(|n| n.to_str()) == Some(file_name);
        undated(b).cmp(&undated(a)).then_with(|| b.cmp(a))
    });
    Ok(found)
}

/// The newest lines in `dir`'s log named `file_name` that `tail` asks for,
/// oldest first.
pub fn read(
    dir: &Path,
    file_name: &str,
    tail: &Tail,
    now: SystemTime,
) -> Result<DiagResponse, String> {
    let since = tail.within.map(|within| timestamp(now - within));
    let mut picked: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    let mut scanned = 0u64;
    let paths = files(dir, file_name).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    'files: for path in paths {
        let mut done = false;
        let result = backwards(&path, &mut scanned, |line| {
            if let Some(stamp) = leading_timestamp(line) {
                let too_old = tail.after.as_deref().is_some_and(|after| stamp <= after)
                    || since.as_deref().is_some_and(|since| stamp < since);
                if too_old {
                    done = true;
                    return false;
                }
            }
            if tail.contains.as_deref().is_some_and(|c| !line.contains(c)) {
                return true;
            }
            if picked.len() as u32 >= tail.max_lines || bytes + line.len() > MAX_LOG_BYTES {
                truncated = true;
                done = true;
                return false;
            }
            bytes += line.len() + 1;
            picked.push(line.to_string());
            true
        });
        match result {
            Ok(Scan::Finished) => {}
            Ok(Scan::OutOfBudget) => {
                truncated = true;
                break 'files;
            }
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        }
        if done {
            break;
        }
    }
    picked.reverse();
    Ok(DiagResponse::Logs {
        lines: picked,
        truncated,
    })
}

enum Scan {
    Finished,
    OutOfBudget,
}

/// Hand `line` each line of `path`, last first, until it answers `false`.
fn backwards(
    path: &Path,
    scanned: &mut u64,
    mut line: impl FnMut(&str) -> bool,
) -> std::io::Result<Scan> {
    let mut file = File::open(path)?;
    let mut end = file.seek(SeekFrom::End(0))?;
    // Bytes of a line whose start lies in a block not read yet.
    let mut carry: Vec<u8> = Vec::new();
    while end > 0 {
        if *scanned >= MAX_SCAN {
            return Ok(Scan::OutOfBudget);
        }
        let start = end.saturating_sub(BLOCK as u64);
        let mut block = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut block)?;
        *scanned += block.len() as u64;
        block.extend_from_slice(&carry);
        let mut rest = block.as_slice();
        while let Some(newline) = rest.iter().rposition(|&b| b == b'\n') {
            let text = &rest[newline + 1..];
            rest = &rest[..newline];
            if !text.is_empty() && !line(&String::from_utf8_lossy(text)) {
                return Ok(Scan::Finished);
            }
        }
        carry = rest.to_vec();
        end = start;
    }
    if !carry.is_empty() {
        line(&String::from_utf8_lossy(&carry));
    }
    Ok(Scan::Finished)
}

/// A line's leading RFC 3339 timestamp; `None` for a continuation line (a
/// backtrace, a wrapped message), which belongs to the line above it.
pub fn leading_timestamp(line: &str) -> Option<&str> {
    let stamp = line.split_whitespace().next()?;
    let b = stamp.as_bytes();
    (b.len() >= 20 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[0].is_ascii_digit())
        .then_some(stamp)
}

/// `at` as the appender writes it, to the second: `2026-10-31T16:11:12`.
/// A prefix of every line written in that second, so it compares with them
/// as text.
pub fn timestamp(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cursio-log-tail-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line(stamp: &str, text: &str) -> String {
        format!("2026-10-31T{stamp}.000000Z  INFO cursio::mesh: {text}\n")
    }

    #[test]
    fn a_timestamp_is_written_as_the_appender_writes_it() {
        let at = UNIX_EPOCH + Duration::from_secs(1_793_463_072);
        assert_eq!(timestamp(at), "2026-10-31T16:11:12");
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00");
    }

    #[test]
    fn the_newest_matching_lines_come_back_oldest_first_across_files() {
        let dir = dir("files");
        std::fs::write(
            dir.join("cursio.log.2026-10-30"),
            "2026-10-30T23:59:59.000000Z  INFO cursio::mesh: path ended\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("cursio.log.2026-10-31"),
            line("00:00:01", "path ended")
                + &line("00:00:02", "quiet")
                + &line("00:00:03", "path ended"),
        )
        .unwrap();
        std::fs::write(
            dir.join("cursio-mount-helper.log.2026-10-31"),
            line("00:00:04", "path ended"),
        )
        .unwrap();
        let tail = Tail {
            contains: Some("path ended".into()),
            max_lines: 10,
            ..Tail::default()
        };
        let DiagResponse::Logs { lines, truncated } =
            read(&dir, "cursio.log", &tail, SystemTime::now()).unwrap()
        else {
            panic!("not a log answer")
        };
        assert!(!truncated);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("2026-10-30T23:59:59"));
        assert!(lines[2].starts_with("2026-10-31T00:00:03"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_follower_gets_only_lines_after_its_mark_and_a_limit_says_it_cut() {
        let dir = dir("after");
        let text: String = (0..10)
            .map(|i| line(&format!("00:00:{i:02}"), "tick"))
            .collect();
        std::fs::write(dir.join("cursio.log.2026-10-31"), text).unwrap();
        let after = Tail {
            after: Some("2026-10-31T00:00:07.000000Z".into()),
            max_lines: 100,
            ..Tail::default()
        };
        let DiagResponse::Logs { lines, .. } =
            read(&dir, "cursio.log", &after, SystemTime::now()).unwrap()
        else {
            panic!("not a log answer")
        };
        assert_eq!(lines.len(), 2);
        let few = Tail {
            max_lines: 3,
            ..Tail::default()
        };
        let DiagResponse::Logs { lines, truncated } =
            read(&dir, "cursio.log", &few, SystemTime::now()).unwrap()
        else {
            panic!("not a log answer")
        };
        assert!(truncated);
        assert!(lines[2].starts_with("2026-10-31T00:00:09"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_window_reaches_back_by_the_answering_clock() {
        let dir = dir("within");
        std::fs::write(
            dir.join("cursio.log.2026-10-31"),
            line("16:00:00", "old") + &line("16:10:30", "new"),
        )
        .unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_793_463_072);
        let tail = Tail {
            within: Some(Duration::from_secs(60)),
            max_lines: 10,
            ..Tail::default()
        };
        let DiagResponse::Logs { lines, .. } = read(&dir, "cursio.log", &tail, now).unwrap() else {
            panic!("not a log answer")
        };
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("new"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lines_longer_than_a_block_survive_being_read_backwards() {
        let dir = dir("long");
        let long = "x".repeat(BLOCK * 2 + 17);
        std::fs::write(
            dir.join("cursio.log.2026-10-31"),
            line("00:00:01", &long) + &line("00:00:02", "short"),
        )
        .unwrap();
        let tail = Tail {
            max_lines: 10,
            ..Tail::default()
        };
        let mut scanned = 0;
        let mut seen = Vec::new();
        backwards(&dir.join("cursio.log.2026-10-31"), &mut scanned, |l| {
            seen.push(l.len());
            true
        })
        .unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1] > BLOCK * 2);
        assert!(read(&dir, "cursio.log", &tail, SystemTime::now()).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
