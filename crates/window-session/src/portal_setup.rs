//! Installing Cursio as this user's ScreenCast portal backend, and taking it
//! out again.
//!
//! Three files, all per-user, nothing under `/usr`:
//!
//! - `~/.local/share/xdg-desktop-portal/portals/cursio.portal` declares the
//!   backend (`xdg-desktop-portal` reads user-level definitions since 1.18).
//! - `~/.local/share/dbus-1/services/…cursio.service` lets D-Bus start
//!   `cursio portal relay` when no daemon holds the name, so screen sharing
//!   on this machine never depends on Cursio running.
//! - `~/.config/xdg-desktop-portal/<desktop>-portals.conf` routes ScreenCast
//!   here. `xdg-desktop-portal` reads **one** configuration file — the first
//!   it finds, user directory first — and ignores the rest, so this is a copy
//!   of the one in effect with the ScreenCast line added; writing only that
//!   line would drop every other portal's choice (the Secret backend, say).
//!
//! The portal reads its configuration once, at start: the change applies
//! after `systemctl --user restart xdg-desktop-portal` or a new login. That
//! restart ends every portal session open at the moment, Cursio's own input
//! capture included, so it is left to the user rather than done here.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Marks a configuration file as ours, so uninstall never deletes the user's.
const MARKER: &str =
    "# Written by `cursio portal install`; `cursio portal uninstall` restores the default.";
const PORTAL_FILE: &str = "cursio.portal";

fn home_dir(var: &str, fallback: &str) -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(fallback))
}

/// The current desktops, lowercased, as `xdg-desktop-portal` matches them.
fn desktops() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The configuration file `xdg-desktop-portal` uses now, if any, in its own
/// search order (user config, system config, data dirs; desktop-specific
/// before generic in each).
fn config_in_effect(user_config: &Path, desktops: &[String]) -> Option<PathBuf> {
    let mut dirs = vec![user_config.to_path_buf()];
    let config_dirs = std::env::var("XDG_CONFIG_DIRS").unwrap_or_else(|_| "/etc/xdg".into());
    dirs.extend(
        config_dirs
            .split(':')
            .map(|d| Path::new(d).join("xdg-desktop-portal")),
    );
    dirs.push(PathBuf::from("/etc/xdg-desktop-portal"));
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(
        data_dirs
            .split(':')
            .map(|d| Path::new(d).join("xdg-desktop-portal")),
    );
    for dir in dirs {
        for desktop in desktops {
            let file = dir.join(format!("{desktop}-portals.conf"));
            if file.is_file() {
                return Some(file);
            }
        }
        let file = dir.join("portals.conf");
        if file.is_file() {
            return Some(file);
        }
    }
    None
}

/// `base` with ScreenCast routed to Cursio: an existing ScreenCast line is
/// replaced, otherwise one is added to `[preferred]`.
pub fn route_screencast(base: &str) -> String {
    const KEY: &str = "org.freedesktop.impl.portal.ScreenCast";
    let mut out = vec![MARKER.to_string()];
    let mut in_preferred = false;
    let mut placed = false;
    let mut saw_preferred = false;
    for line in base.lines() {
        if line.trim() == MARKER {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_preferred && !placed {
                out.push(format!("{KEY}=cursio"));
                placed = true;
            }
            in_preferred = trimmed == "[preferred]";
            saw_preferred |= in_preferred;
        }
        if in_preferred && trimmed.starts_with(KEY) {
            if !placed {
                out.push(format!("{KEY}=cursio"));
                placed = true;
            }
            continue;
        }
        out.push(line.to_string());
    }
    if !saw_preferred {
        out.push("[preferred]".into());
        out.push("default=gnome;gtk;".into());
    }
    if !placed {
        out.push(format!("{KEY}=cursio"));
    }
    out.join("\n") + "\n"
}

pub fn install() -> anyhow::Result<()> {
    let data = home_dir("XDG_DATA_HOME", ".local/share")?;
    let config = home_dir("XDG_CONFIG_HOME", ".config")?;
    let exe = std::env::current_exe().context("cannot find this executable")?;

    let portals = data.join("xdg-desktop-portal/portals");
    std::fs::create_dir_all(&portals)?;
    std::fs::write(
        portals.join(PORTAL_FILE),
        format!(
            "[portal]\nDBusName={}\nInterfaces=org.freedesktop.impl.portal.ScreenCast;\n",
            cursio_window::linux_portal::BUS_NAME
        ),
    )?;

    let services = data.join("dbus-1/services");
    std::fs::create_dir_all(&services)?;
    std::fs::write(
        services.join(format!("{}.service", cursio_window::linux_portal::BUS_NAME)),
        format!(
            "[D-BUS Service]\nName={}\nExec={} portal relay\n",
            cursio_window::linux_portal::BUS_NAME,
            exe.display()
        ),
    )?;

    let desktops = desktops();
    let first = desktops
        .first()
        .context("XDG_CURRENT_DESKTOP is not set; run this inside the desktop session")?;
    let user_config = config.join("xdg-desktop-portal");
    let base = config_in_effect(&user_config, &desktops)
        .map(|f| std::fs::read_to_string(&f).with_context(|| format!("read {}", f.display())))
        .transpose()?
        .unwrap_or_default();
    std::fs::create_dir_all(&user_config)?;
    let target = user_config.join(format!("{first}-portals.conf"));
    std::fs::write(&target, route_screencast(&base))?;
    println!(
        "installed; ScreenCast is routed to Cursio by {}",
        target.display()
    );
    println!(
        "it takes effect when xdg-desktop-portal restarts: log out and in, or run\n  \
         systemctl --user restart xdg-desktop-portal\n\
         (the restart ends screen shares and input capture open at that moment, Cursio's included)"
    );
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let data = home_dir("XDG_DATA_HOME", ".local/share")?;
    let config = home_dir("XDG_CONFIG_HOME", ".config")?.join("xdg-desktop-portal");
    let _ = std::fs::remove_file(data.join("xdg-desktop-portal/portals").join(PORTAL_FILE));
    let _ = std::fs::remove_file(
        data.join("dbus-1/services")
            .join(format!("{}.service", cursio_window::linux_portal::BUS_NAME)),
    );
    if let Ok(entries) = std::fs::read_dir(&config) {
        for entry in entries.flatten() {
            let path = entry.path();
            let ours = std::fs::read_to_string(&path).is_ok_and(|text| text.contains(MARKER));
            if ours {
                std::fs::remove_file(&path)?;
                println!("removed {}", path.display());
            }
        }
    }
    println!(
        "uninstalled; restart xdg-desktop-portal (or log in again) to use GNOME's picker alone"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_keeps_every_other_portal_choice() {
        let base =
            "[preferred]\ndefault=gnome;gtk;\norg.freedesktop.impl.portal.Secret=gnome-keyring;\n";
        let routed = route_screencast(base);
        assert!(routed.contains("default=gnome;gtk;"));
        assert!(routed.contains("org.freedesktop.impl.portal.Secret=gnome-keyring;"));
        assert!(routed.contains("org.freedesktop.impl.portal.ScreenCast=cursio"));
        assert!(routed.starts_with(MARKER));
    }

    #[test]
    fn an_existing_screencast_choice_is_replaced_not_duplicated() {
        let base = "[preferred]\ndefault=gnome;\norg.freedesktop.impl.portal.ScreenCast=gnome;\n";
        let routed = route_screencast(base);
        assert_eq!(routed.matches("impl.portal.ScreenCast=").count(), 1);
        assert!(routed.contains("ScreenCast=cursio"));
    }

    #[test]
    fn installing_twice_changes_nothing() {
        let base = "[preferred]\ndefault=gnome;gtk;\n";
        let once = route_screencast(base);
        assert_eq!(route_screencast(&once), once);
    }

    #[test]
    fn with_no_configuration_at_all_gnome_stays_the_default() {
        let routed = route_screencast("");
        assert!(routed.contains("[preferred]\ndefault=gnome;gtk;"));
        assert!(routed.contains("ScreenCast=cursio"));
    }
}
