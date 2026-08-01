//! Opening a console on the running daemon's log.
//!
//! Cursio already writes a rotating file; showing it live is `tail -f` and a
//! terminal. What differs between platforms is only *which* terminal, and that
//! is the whole content of this file.
//!
//! A terminal is spawned rather than a window drawn, because a real terminal
//! already scrolls, selects, searches and copies. Drawing a log viewer would be
//! a worse version of something every desktop ships.
//!
//! This matters most where the daemon has no console of its own: started by a
//! launcher, or — on Windows — relaunched elevated, where the original process
//! steps aside and the one doing the work has no window attached to anything
//! the user can see.

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
        // Matched by name rather than by excluding what is not wanted. The
        // directory also holds the AirPlay lock and the script this console is
        // launched from — and that script, written moments before the search,
        // is reliably the newest file in there. A console following its own
        // launcher shows one line and never another.
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(LOG_FILE_PREFIX)
        {
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

/// What the rotating appender names its files, before the date suffix.
const LOG_FILE_PREFIX: &str = "cursio.log";

/// What the console window calls itself.
///
/// A terminal titles itself after whatever it is running, which here is a path
/// to a log file inside the platform's data directory — long, identical
/// between machines up to the username, and telling the user nothing they did
/// not already know. Every terminal worth the name honours the OSC 0 escape
/// below, so the name is set from inside the script rather than by arguing with
/// each emulator's own flag for it.
const CONSOLE_TITLE: &str = "cursiod";

/// The escape sequence that renames the window, ready to be run by a shell.
fn title_command() -> String {
    format!("printf '\\033]0;{CONSOLE_TITLE}\\007'")
}

/// Open a terminal following `path`.
pub fn open_console(path: &Path) -> Result<()> {
    spawn_terminal(path).with_context(|| format!("opening a console on {}", path.display()))
}

#[cfg(target_os = "macos")]
fn spawn_terminal(path: &Path) -> Result<()> {
    // The command goes through a throwaway script because `open` launches
    // files, not command lines. AppleScript would avoid the file but needs
    // Automation permission — a consent dialog for something the user just
    // asked for outright.
    //
    // Written under the log directory rather than the temp one: a `.command`
    // in `$TMPDIR` is launched by whatever happens to be registered for the
    // extension, and when that is an editor — or nothing — `open` reports
    // success and no window appears.
    // Named after the daemon, because Terminal falls back to the script's own
    // name whenever the title has not been set yet.
    let script = cursio_log::log_dir().join(format!("{CONSOLE_TITLE}.command"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n{}\nexec tail -n 200 -f {}\n",
            title_command(),
            shell_quote(path)
        ),
    )?;
    // Executable, or Terminal opens it in an editor instead of running it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }
    // `-a Terminal` names the application instead of trusting the extension's
    // handler, and `-n` forces a new instance so an already-running Terminal
    // with no windows still produces one.
    let status = std::process::Command::new("open")
        .arg("-a")
        .arg("Terminal")
        .arg(&script)
        .status()?;
    if !status.success() {
        anyhow::bail!("Terminal refused to open the log script ({status})");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn spawn_terminal(path: &Path) -> Result<()> {
    // `Get-Content -Wait` is PowerShell's `tail -f`. Launched through `cmd
    // /c start` so the console gets a window of its own rather than inheriting
    // this process's (which, for a tray application, is none at all).
    std::process::Command::new("cmd")
        // The first quoted argument to `start` is the window title, not a
        // stray empty string: leaving it blank is what left the console named
        // after the full path to the log.
        .args([
            "/c",
            "start",
            CONSOLE_TITLE,
            "powershell",
            "-NoExit",
            "-Command",
        ])
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
    let command = format!("{}; tail -n 200 -f {}", title_command(), shell_quote(path));
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
