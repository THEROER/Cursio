//! Opening a console on the running daemon's log.
//!
//! Cursio already writes a rotating file; showing it live is `tail -f` and a
//! terminal. What differs between platforms is only *which* terminal, and that
//! is the whole content of this file.
//!
//! A terminal is spawned rather than a window drawn: the point of this build is
//! to watch what the daemon is doing, and a real terminal already scrolls,
//! selects, searches and copies. Reimplementing that in a tray application
//! would be a worse version of something every desktop ships.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The log file to follow: the newest one in the log directory.
///
/// Newest rather than "today's", because the appender rotates on its own
/// schedule and names files after the day it started writing them — so after
/// midnight the file being written to is still yesterday's name.
pub fn newest_log() -> Result<PathBuf> {
    let dir = cursio_log::log_dir();
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("reading the log directory {}", dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        // The lock file the daemon uses to claim AirPlay lives here too, and
        // following it would show an empty window forever.
        if entry.path().extension().is_some_and(|ext| ext == "lock") {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        if newest.as_ref().is_none_or(|(seen, _)| modified > *seen) {
            newest = Some((modified, entry.path()));
        }
    }
    newest.map(|(_, path)| path).with_context(|| {
        format!(
            "no log file in {} yet — the daemon writes one as it starts",
            dir.display()
        )
    })
}

/// Open a terminal following `path`.
pub fn open_console(path: &Path) -> Result<()> {
    spawn_terminal(path).with_context(|| format!("opening a console on {}", path.display()))
}

#[cfg(target_os = "macos")]
fn spawn_terminal(path: &Path) -> Result<()> {
    // `open -a Terminal <file>` opens the file, not a command, so the command
    // goes through a throwaway script Terminal is asked to run. AppleScript
    // would avoid the file but needs Automation permission, which is a consent
    // dialog for something the user just clicked a button to ask for.
    let script = std::env::temp_dir().join("cursio-logs.command");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec tail -n 200 -f {}\n", shell_quote(path)),
    )?;
    // Executable, or Terminal opens it in an editor instead of running it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }
    std::process::Command::new("open").arg(&script).spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn spawn_terminal(path: &Path) -> Result<()> {
    // `Get-Content -Wait` is PowerShell's `tail -f`. Launched through `cmd
    // /c start` so the console gets a window of its own rather than inheriting
    // this process's (which, for a tray application, is none at all).
    std::process::Command::new("cmd")
        .args(["/c", "start", "", "powershell", "-NoExit", "-Command"])
        .arg(format!(
            "Get-Content -Path '{}' -Tail 200 -Wait",
            path.display()
        ))
        .spawn()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn spawn_terminal(path: &Path) -> Result<()> {
    // No single terminal exists to call, and `x-terminal-emulator` is a Debian
    // convention rather than a standard — so the ones people actually have are
    // tried in turn. `-e` is understood by all of them; the newer ones prefer
    // their own spelling but keep accepting this one.
    let command = format!("tail -n 200 -f {}", shell_quote(path));
    let candidates = [
        "x-terminal-emulator",
        "gnome-terminal",
        "konsole",
        "xfce4-terminal",
        "alacritty",
        "kitty",
        "foot",
        "xterm",
    ];
    for terminal in candidates {
        let spawned = std::process::Command::new(terminal)
            .arg("-e")
            .arg("sh")
            .arg("-c")
            .arg(&command)
            .spawn();
        if spawned.is_ok() {
            return Ok(());
        }
    }
    anyhow::bail!(
        "no terminal emulator found; tried {}",
        candidates.join(", ")
    )
}

/// Quote a path for a POSIX shell.
///
/// Log directories sit under the user's home, which routinely contains spaces
/// and occasionally an apostrophe.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_path_with_a_space_survives_the_shell() {
        use super::shell_quote;
        use std::path::Path;

        assert_eq!(
            shell_quote(Path::new("/Users/a b/logs/cursio.log")),
            "'/Users/a b/logs/cursio.log'"
        );
    }

    /// A home directory with an apostrophe in it would otherwise end the quote
    /// and hand the rest of the path to the shell as commands.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_apostrophe_cannot_escape_the_quoting() {
        use super::shell_quote;
        use std::path::Path;

        let quoted = shell_quote(Path::new("/Users/o'brien/log"));
        assert_eq!(quoted, r"'/Users/o'\''brien/log'");
        assert!(!quoted.contains("; "), "nothing may follow the quote");
    }
}
