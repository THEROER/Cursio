//! Logs the node keeps for applications that send their lines instead of
//! writing a file the node can read (`ClientMsg::Log`).
//!
//! An Android app writes in a private directory no other app may open, and
//! some applications keep no log file at all; for them `cursio logs
//! <machine> --app <id>` read nothing. Their lines now come over the
//! connection the SDK already holds, and the node writes them where it
//! writes its own state: `<data>/apps/<app>/logs/<process>.log.<date>`, one
//! file a day per process (`main` for the main one), named the way Cursio's
//! own logs are, so the reader, the merge across processes and machines, the
//! report and reading after the application exits all take it as they take
//! any log. The registry records the file as that process's log.
//!
//! **Bounded.** Fourteen days are kept, a process writes at most
//! [`DAY_CAP`] a day — past it one line says so and the rest of the day is
//! not kept — and a line is cut at [`cursio_app_protocol::MAX_LOG_LINE`]
//! bytes, its line breaks flattened so one line stays one entry.
//!
//! **An application's own file wins.** A process that also named its log
//! directory (`App::logs`) is read from there; lines it sends are not kept a
//! second time.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use cursio_app_protocol::{clip_log_line, LogSpec};

use super::Key;

/// Days of a process's log kept.
pub const KEEP_DAYS: u64 = 14;

/// Most bytes one process's log takes in a day.
pub const DAY_CAP: u64 = 64 * 1024 * 1024;

/// Where the node keeps applications' logs, and the file open for each
/// process today.
pub struct AppLogs {
    root: PathBuf,
    open: Mutex<BTreeMap<Key, Today>>,
}

struct Today {
    date: String,
    file: File,
    written: u64,
    capped: bool,
}

/// This node's keeper, under its data directory. A test's goes to a
/// directory of its own, never the person's.
pub fn app_logs() -> &'static AppLogs {
    static LOGS: OnceLock<AppLogs> = OnceLock::new();
    LOGS.get_or_init(|| {
        let root = match cfg!(test) {
            true => {
                std::env::temp_dir().join(format!("cursio-test-app-logs-{}", std::process::id()))
            }
            false => crate::data_dir::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("apps"),
        };
        AppLogs::at(root)
    })
}

impl AppLogs {
    pub fn at(root: PathBuf) -> Self {
        AppLogs {
            root,
            open: Mutex::new(BTreeMap::new()),
        }
    }

    /// Where `key`'s lines are kept, as the registry records a log.
    pub fn spec(&self, key: &Key) -> LogSpec {
        LogSpec {
            dir: self.root.join(key.app.as_str()).join("logs"),
            file: format!(
                "{}.log",
                key.process.as_ref().map_or("main", |p| p.as_str())
            ),
        }
    }

    /// Whether `spec` is a log this keeper writes, as opposed to a file an
    /// application named itself.
    pub fn keeps(&self, spec: &LogSpec) -> bool {
        spec.dir.starts_with(&self.root)
    }

    /// Append `lines` to `key`'s log, after a line saying `dropped` were
    /// lost before them when there were any.
    pub fn append(
        &self,
        key: &Key,
        lines: &[String],
        dropped: u64,
        now: SystemTime,
    ) -> std::io::Result<()> {
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        let stamp = crate::log_tail::timestamp(now);
        let date = stamp[..10].to_owned();
        if open.get(key).is_none_or(|today| today.date != date) {
            let spec = self.spec(key);
            open.insert(key.clone(), open_day(&spec, &date)?);
            prune(&spec, now);
        }
        let today = open.get_mut(key).expect("opened above");
        let mut out = String::new();
        if dropped > 0 {
            out.push_str(&format!(
                "{stamp}.000000Z  WARN cursio: {dropped} log lines were dropped here; the application logged faster than they reached its node\n"
            ));
        }
        for line in lines {
            let line = clip_log_line(line).replace(['\n', '\r'], " ");
            match crate::log_tail::leading_timestamp(&line) {
                Some(_) => out.push_str(&line),
                None => out.push_str(&format!("{stamp}.000000Z {line}")),
            }
            out.push('\n');
        }
        if today.capped {
            return Ok(());
        }
        if today.written + out.len() as u64 > DAY_CAP {
            today.capped = true;
            let note = format!(
                "{stamp}.000000Z  WARN cursio: this log reached {} MiB today; the rest of the day is not kept\n",
                DAY_CAP / (1024 * 1024)
            );
            today.file.write_all(note.as_bytes())?;
            today.written += note.len() as u64;
            return Ok(());
        }
        today.file.write_all(out.as_bytes())?;
        today.written += out.len() as u64;
        Ok(())
    }
}

fn open_day(spec: &LogSpec, date: &str) -> std::io::Result<Today> {
    std::fs::create_dir_all(&spec.dir)?;
    let path = spec.dir.join(format!("{}.{date}", spec.file));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    Ok(Today {
        date: date.to_owned(),
        file,
        written,
        capped: written >= DAY_CAP,
    })
}

/// Remove `spec`'s days older than [`KEEP_DAYS`].
fn prune(spec: &LogSpec, now: SystemTime) {
    let oldest = crate::log_tail::timestamp(now - Duration::from_secs(KEEP_DAYS * 86_400));
    let oldest = &oldest[..10];
    let prefix = format!("{}.", spec.file);
    let Ok(entries) = std::fs::read_dir(&spec.dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(date) = name.to_str().and_then(|n| n.strip_prefix(&prefix)) else {
            continue;
        };
        if date.len() == 10 && date < oldest {
            if let Err(error) = std::fs::remove_file(entry.path()) {
                tracing::debug!(%error, file = %entry.path().display(), "cannot remove an old application log");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_app_protocol::{AppId, ProcessName};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::UNIX_EPOCH;

    fn scratch() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cursio-app-logs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn key(process: Option<&str>) -> Key {
        Key {
            app: AppId::parse("launcher").unwrap(),
            process: process.map(|p| ProcessName::parse(p).unwrap()),
        }
    }

    fn day(n: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_793_563_884 + n * 86_400)
    }

    #[test]
    fn lines_land_in_a_dated_file_the_log_reader_finds() {
        let root = scratch();
        let logs = AppLogs::at(root.clone());
        let main = key(None);
        logs.append(
            &main,
            &[
                "2026-11-01T20:11:24.000001Z  INFO launcher: up".into(),
                "no stamp\nsecond".into(),
            ],
            2,
            day(0),
        )
        .unwrap();
        let spec = logs.spec(&main);
        assert_eq!(spec.file, "main.log");
        assert!(logs.keeps(&spec));
        let text = std::fs::read_to_string(spec.dir.join("main.log.2026-11-01")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("2 log lines were dropped"));
        assert_eq!(lines[1], "2026-11-01T20:11:24.000001Z  INFO launcher: up");
        assert_eq!(lines[2], "2026-11-01T20:11:24.000000Z no stamp second");
        let read = crate::log_tail::read(
            &spec.dir,
            &spec.file,
            &crate::log_tail::Tail {
                max_lines: 10,
                ..Default::default()
            },
            day(0),
        )
        .unwrap();
        let cursio_protocol::diag::DiagResponse::Logs { lines, .. } = read else {
            panic!("not a log answer")
        };
        assert_eq!(lines.len(), 3);
        assert_eq!(logs.spec(&key(Some("daemon"))).file, "daemon.log");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_new_day_starts_a_new_file_and_days_past_the_keep_are_removed() {
        let root = scratch();
        let logs = AppLogs::at(root.clone());
        let main = key(None);
        logs.append(&main, &["first day".into()], 0, day(0))
            .unwrap();
        logs.append(&main, &["a day later".into()], 0, day(1))
            .unwrap();
        let dir = logs.spec(&main).dir;
        assert!(dir.join("main.log.2026-11-01").is_file());
        assert!(dir.join("main.log.2026-11-02").is_file());
        logs.append(&main, &["much later".into()], 0, day(KEEP_DAYS + 1))
            .unwrap();
        assert!(!dir.join("main.log.2026-11-01").exists(), "past the keep");
        assert!(dir.join("main.log.2026-11-02").exists(), "still inside it");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_day_past_its_cap_says_so_once_and_keeps_nothing_more() {
        let root = scratch();
        let logs = AppLogs::at(root.clone());
        let main = key(None);
        let big = "x".repeat(cursio_app_protocol::MAX_LOG_LINE - 40);
        let batch: Vec<String> = (0..256).map(|_| big.clone()).collect();
        let per_batch = 256 * (big.len() as u64 + 29);
        for _ in 0..(DAY_CAP / per_batch) + 2 {
            logs.append(&main, &batch, 0, day(0)).unwrap();
        }
        logs.append(&main, &["after".into()], 0, day(0)).unwrap();
        let file = logs.spec(&main).dir.join("main.log.2026-11-01");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(std::fs::metadata(&file).unwrap().len() <= DAY_CAP + 1024);
        assert_eq!(text.matches("reached 64 MiB today").count(), 1);
        assert!(!text.contains("after"));
        let _ = std::fs::remove_dir_all(root);
    }
}
