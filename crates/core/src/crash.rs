//! Saying so when the node crashed last time.
//!
//! A crashed node looks, from every other machine, like a network fault: the
//! peer "drops", redials, and nobody is told why. Lightmoon crashed seven
//! times in a week that way, and finding out took a shell on it, the Windows
//! event log, a crash dump and a debugger. So a node that starts after a
//! crash says it did, as one of its problems — in its status, in the app and
//! in `cursio report`, which a peer can read with no shell at all.
//!
//! Only a crash that left evidence is reported. A panic is caught by the hook
//! [`arm`] installs and written beside the log, where the next start finds it.
//! A native crash (the heap corruption above) never reaches Rust, but Windows
//! Error Reporting leaves a dump under `%LOCALAPPDATA%\CrashDumps`, and a dump
//! newer than the last one seen is one. A node that merely did not shut down
//! cleanly is not: an update on Windows stops the node with `TerminateProcess`,
//! and calling each of those a crash would make the report cry wolf.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::diagnostics::{self, Problem, Severity};

/// Where a panic is written for the next start to find.
const PANIC_FILE: &str = "last-panic.txt";

/// The newest crash dump already reported, so each is reported once.
#[cfg(windows)]
const DUMPS_SEEN: &str = "crash-dumps-seen.txt";

const PROBLEM: &str = "node.crashed";

/// Report a crash of the previous run, if it left evidence, and catch this
/// run's panics for the next.
pub fn arm() {
    let dir = cursio_log::log_dir();
    let mut found = Vec::new();
    if let Some(panic) = take_panic(&dir) {
        found.push(panic);
    }
    found.extend(new_dumps(&dir));
    if let Some(latest) = found.last() {
        tracing::warn!(crashes = found.len(), latest = %latest, "the node crashed before this start");
        diagnostics::raise(Problem {
            id: PROBLEM.into(),
            severity: Severity::Warning,
            summary: match found.len() {
                1 => format!("the node crashed before this start: {latest}"),
                n => format!("the node crashed {n} times before this start; the last: {latest}"),
            },
            hint: Some(
                "peers saw this as the link dropping; the log around that time says what it was doing"
                    .into(),
            ),
            actions: Vec::new(),
            since: SystemTime::now(),
        });
    }
    install_hook(dir);
}

/// The panic the previous run wrote, as one line; the file is consumed.
fn take_panic(dir: &Path) -> Option<String> {
    let path = dir.join(PANIC_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    if let Err(error) = std::fs::remove_file(&path) {
        tracing::warn!(%error, file = %path.display(), "cannot clear the record of the last panic; it will be reported again");
    }
    Some(text.lines().collect::<Vec<_>>().join(" "))
}

fn install_hook(dir: PathBuf) {
    // A phone's node restarts inside one process; one hook is enough.
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| chain_hook(dir));
}

fn chain_hook(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let at = crate::log_tail::timestamp(SystemTime::now());
        let place = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic with no message".into());
        let line = format!(
            "{at} panic in thread {} at {place}: {message}",
            thread.name().unwrap_or("unnamed")
        );
        tracing::error!(%line, "panic");
        let _ = std::fs::write(dir.join(PANIC_FILE), &line);
        previous(info);
    }));
}

/// Crash dumps of this program newer than the last reported, oldest first,
/// as "time (dump path)".
#[cfg(windows)]
fn new_dumps(log_dir: &Path) -> Vec<String> {
    let Some(dumps) = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("CrashDumps"))
    else {
        return Vec::new();
    };
    let seen_file = log_dir.join(DUMPS_SEEN);
    let seen: u64 = std::fs::read_to_string(&seen_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let mut found: Vec<(u64, PathBuf)> = std::fs::read_dir(&dumps)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("cursio.exe.")
        })
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            let secs = modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()?
                .as_secs();
            Some((secs, e.path()))
        })
        .filter(|(secs, _)| *secs > seen)
        .collect();
    found.sort();
    let Some((newest, _)) = found.last() else {
        return Vec::new();
    };
    if seen == 0 {
        // The first start that knows about dumps would report every crash the
        // machine ever had; it only marks where reporting begins.
        let _ = std::fs::write(&seen_file, newest.to_string());
        return Vec::new();
    }
    if let Err(error) = std::fs::write(&seen_file, newest.to_string()) {
        tracing::warn!(%error, "cannot record which crash dumps were reported; they will be reported again");
    }
    found
        .into_iter()
        .map(|(secs, path)| {
            let at = crate::log_tail::timestamp(
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            );
            format!("{at} (dump {})", path.display())
        })
        .collect()
}

/// Native crashes leave no dump this node knows where to find off Windows; a
/// Rust panic is still caught by the hook.
#[cfg(not(windows))]
fn new_dumps(_log_dir: &Path) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_written_by_the_last_run_is_reported_once() {
        let dir = std::env::temp_dir().join(format!("cursio-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(PANIC_FILE),
            "2026-10-31T16:00:00 panic in thread cursio-clipboard at a.rs:3: boom\n",
        )
        .unwrap();
        let first = take_panic(&dir).unwrap();
        assert!(first.contains("boom") && first.contains("cursio-clipboard"));
        assert_eq!(take_panic(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
