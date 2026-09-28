//! The newest lines of the node's log, read straight off disk.
//!
//! Not a control command: the app and the node run as the same user and log
//! into the same directory (`cursio_log::log_dir`), so the file is already in
//! reach — and reading it keeps working exactly when the log matters most, when
//! the node has stopped answering or never started. A socket command would go
//! quiet in the same moment the window needs to explain why.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// How much of the file's end is read. Enough for the lines the drawer shows,
/// without reading a day's worth of debug output to find them.
const TAIL_BYTES: u64 = 512 * 1024;

/// The newest file whose name starts with `prefix` — the daily appender names
/// them `<prefix>.<date>`, so the newest date sorts last.
fn newest(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{prefix}.")))
        })
        .collect();
    files.sort();
    files.pop()
}

/// The last `lines` lines of `path`, reading at most [`TAIL_BYTES`] from its
/// end; a first line cut by that window is dropped rather than shown half.
fn tail(path: &Path, lines: usize) -> std::io::Result<Vec<String>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = text.lines().collect();
    if start > 0 && !all.is_empty() {
        all.remove(0);
    }
    let from = all.len().saturating_sub(lines);
    Ok(all[from..].iter().map(|l| (*l).to_owned()).collect())
}

/// Which process's log: the node's (`cursio`) or this app's (`cursio-app`).
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogSource {
    Node,
    App,
}

pub fn read(source: LogSource, lines: usize) -> Result<Vec<String>, String> {
    let dir = cursio_core::logging::log_dir();
    let prefix = match source {
        LogSource::Node => "cursio.log",
        LogSource::App => "cursio-app.log",
    };
    let Some(path) = newest(&dir, prefix) else {
        return Err(format!("no {prefix} file in {}", dir.display()));
    };
    tail(&path, lines).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

pub mod commands {
    use super::*;

    /// Off the window's thread: a large log is a noticeable read.
    #[tauri::command]
    pub async fn log_tail(source: LogSource, lines: usize) -> Result<Vec<String>, String> {
        tauri::async_runtime::spawn_blocking(move || read(source, lines))
            .await
            .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cursio-app-logs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_newest_daily_file_is_read_and_the_app_log_is_not_taken_for_the_node() {
        let dir = temp_dir("newest");
        std::fs::write(dir.join("cursio.log.2026-09-27"), "old\n").unwrap();
        std::fs::write(dir.join("cursio.log.2026-09-28"), "new\n").unwrap();
        std::fs::write(dir.join("cursio-app.log.2026-09-29"), "app\n").unwrap();
        assert_eq!(
            newest(&dir, "cursio.log").unwrap(),
            dir.join("cursio.log.2026-09-28")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_keeps_only_the_last_lines() {
        let dir = temp_dir("tail");
        let path = dir.join("cursio.log.2026-09-28");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail(&path, 2).unwrap(), vec!["c", "d"]);
        assert_eq!(tail(&path, 10).unwrap(), vec!["a", "b", "c", "d"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
