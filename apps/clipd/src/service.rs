//! Installing `clipd` so it runs by itself: a systemd user service.
//!
//! The daemon has to be running for anything to be recorded, so requiring the
//! user to start it by hand means an empty history after every reboot. A
//! systemd **user** service (not system-wide) is the right unit here: it starts
//! at login, runs as the user, and inherits the session environment the
//! clipboard needs — a system service would have no display to talk to.
//!
//! ## Binding to the graphical session
//!
//! The unit is tied to `graphical-session.target` rather than `default.target`.
//! The clipboard only exists once there is a display server, so starting
//! earlier would just fail and retry. `PartOf=` also means the daemon stops
//! when the session ends, instead of lingering after logout.
//!
//! ## Why the binary is copied
//!
//! Installation copies the executable to `~/.local/bin` and points both the
//! service and the hotkey at that copy, rather than at wherever it was built.
//! A unit referencing `target/debug/clipd` breaks the moment the project is
//! rebuilt in release mode, moved, or cleaned — and it breaks silently, as a
//! shortcut that stops doing anything.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Unit name, fixed so reinstalling replaces rather than accumulates.
const UNIT: &str = "clipd.service";

/// Install the binary, write the unit, and start it.
pub fn install(data_dir: Option<&Path>) -> Result<()> {
    if !has_systemctl() {
        bail!("`systemctl` not found; this command needs systemd");
    }

    let installed = install_binary()?;
    let unit_path = unit_dir()?.join(UNIT);
    std::fs::create_dir_all(unit_path.parent().unwrap())
        .with_context(|| format!("creating {}", unit_path.display()))?;
    std::fs::write(&unit_path, unit_text(&installed, data_dir))
        .with_context(|| format!("writing {}", unit_path.display()))?;

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT])?;

    println!("installed  {}", installed.display());
    println!("service    {}", unit_path.display());
    println!("\nclipd now starts automatically at login.");
    println!("  status:  systemctl --user status {UNIT}");
    println!("  logs:    journalctl --user -u {UNIT} -f");
    println!("  stop:    systemctl --user disable --now {UNIT}");
    Ok(())
}

/// Stop the service and remove the unit. The binary and history are left alone.
pub fn uninstall() -> Result<()> {
    if !has_systemctl() {
        bail!("`systemctl` not found; this command needs systemd");
    }

    // Best-effort: a unit that is already gone shouldn't make this fail.
    let _ = systemctl(&["disable", "--now", UNIT]);

    let unit_path = unit_dir()?.join(UNIT);
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("removing {}", unit_path.display()))?;
    }
    systemctl(&["daemon-reload"])?;

    println!("service removed. History and the installed binary were kept.");
    Ok(())
}

/// Copy the running executable to `~/.local/bin`, returning its new path.
///
/// Returns the current path unchanged if it is already the installed copy, so
/// re-running is harmless (and so the service can't be asked to copy over a
/// file it is currently executing).
fn install_binary() -> Result<PathBuf> {
    let current = std::env::current_exe().context("locating this executable")?;
    let target = bin_dir()?.join("clipd");

    if current == target {
        return Ok(target);
    }

    std::fs::create_dir_all(target.parent().unwrap())
        .with_context(|| format!("creating {}", target.parent().unwrap().display()))?;

    // Replace rather than write in place: overwriting a running binary fails
    // with ETXTBSY, and an unlinked one keeps working for processes using it.
    if target.exists() {
        std::fs::remove_file(&target).with_context(|| format!("replacing {}", target.display()))?;
    }
    std::fs::copy(&current, &target)
        .with_context(|| format!("copying {} to {}", current.display(), target.display()))?;

    warn_if_not_on_path(target.parent().unwrap());
    Ok(target)
}

fn unit_text(exe: &Path, data_dir: Option<&Path>) -> String {
    let mut command = format!("{} daemon", exe.display());
    if let Some(dir) = data_dir {
        command = format!("{} --data-dir {}", command, dir.display());
    }

    format!(
        "[Unit]\n\
         Description=clipd — clipboard history\n\
         Documentation=https://github.com/THEROER/cursio\n\
         # The clipboard only exists once there is a display server.\n\
         After=graphical-session.target\n\
         PartOf=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={command}\n\
         # A clipboard watcher that dies takes the history with it, so always\n\
         # bring it back; the delay keeps a persistent failure from spinning.\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n"
    )
}

/// Warn when the install directory isn't on `PATH`, since `clipd` would then
/// only be runnable by full path — confusing, and easy to miss.
fn warn_if_not_on_path(dir: &Path) {
    let on_path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|p| p == dir))
        .unwrap_or(false);
    if !on_path {
        println!(
            "note: {} is not on your PATH; add it to run `clipd` by name.",
            dir.display()
        );
    }
}

fn bin_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

fn unit_dir() -> Result<PathBuf> {
    // Respect XDG_CONFIG_HOME when set, falling back to the default location.
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config).join("systemd").join("user"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user"))
}

fn has_systemctl() -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join("systemctl").is_file()))
        .unwrap_or(false)
}

fn systemctl(args: &[&str]) -> Result<()> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    let status = Command::new("systemctl")
        .args(&full)
        .status()
        .context("running systemctl")?;
    if !status.success() {
        bail!("systemctl {} failed", full.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_starts_with_the_graphical_session() {
        // The clipboard needs a display; starting at default.target would fail
        // and retry until one appeared.
        let text = unit_text(Path::new("/home/u/.local/bin/clipd"), None);
        assert!(text.contains("After=graphical-session.target"));
        assert!(text.contains("WantedBy=graphical-session.target"));
        assert!(text.contains("PartOf=graphical-session.target"));
    }

    #[test]
    fn unit_runs_the_daemon_from_the_installed_path() {
        let text = unit_text(Path::new("/home/u/.local/bin/clipd"), None);
        assert!(text.contains("ExecStart=/home/u/.local/bin/clipd daemon"));
        // A unit pointing into a build tree breaks on the next rebuild.
        assert!(!text.contains("target/debug"));
    }

    #[test]
    fn unit_carries_a_custom_data_dir() {
        let text = unit_text(
            Path::new("/home/u/.local/bin/clipd"),
            Some(Path::new("/tmp/history")),
        );
        assert!(text.contains("ExecStart=/home/u/.local/bin/clipd daemon --data-dir /tmp/history"));
    }

    #[test]
    fn unit_restarts_the_watcher() {
        // If the watcher dies unnoticed, nothing is recorded until the next
        // login — the failure is invisible, so restarting matters.
        let text = unit_text(Path::new("/usr/bin/clipd"), None);
        assert!(text.contains("Restart=always"));
        assert!(text.contains("RestartSec="));
    }
}
