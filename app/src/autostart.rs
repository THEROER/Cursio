//! Starting Cursio at login — opt-in, two separate switches.
//!
//! The node and the tray are separate because they are separate needs: a
//! machine that only lends its disks wants the node and no icon, and the tray
//! is useless without a node but must not be what keeps one alive.
//!
//! **Linux**: the node is a systemd user unit bound to
//! `graphical-session.target`, so it starts with the desktop (it captures
//! input and talks to the portal, which need the session's environment) and
//! `Restart=on-failure` brings it back from a crash; the tray is an XDG
//! autostart entry, which is what a desktop reads for applications.
//! Enabling does not start the unit now: a node started by hand may already
//! hold the port, and a unit failing on it in a loop is worse than waiting for
//! the next login.
//!
//! **Windows**: `HKCU\…\Run` values. Per user, needs no rights, and the node
//! asks for elevation itself when it starts — as it does from a terminal —
//! with declining still leaving a working node. A scheduled task with highest
//! privileges would skip that prompt but can only be created elevated, which
//! this app is not.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Autostart {
    pub node: bool,
    pub tray: bool,
}

#[cfg(target_os = "linux")]
pub const NODE_UNIT: &str = "cursio.service";

#[cfg(target_os = "linux")]
mod platform {
    use std::path::PathBuf;

    use super::NODE_UNIT;

    const TARGET: &str = "graphical-session.target";

    fn config_home() -> Result<PathBuf, String> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok_or_else(|| "neither XDG_CONFIG_HOME nor HOME is set".to_owned())
    }

    fn unit_path() -> Result<PathBuf, String> {
        Ok(config_home()?.join("systemd/user").join(NODE_UNIT))
    }

    fn desktop_path() -> Result<PathBuf, String> {
        Ok(config_home()?.join("autostart/cursio-app.desktop"))
    }

    pub fn unit_installed() -> bool {
        unit_path().is_ok_and(|p| p.exists())
    }

    fn systemctl(args: &[&str]) -> Result<(), String> {
        let status = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .status()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        match status.success() {
            true => Ok(()),
            false => Err(format!("systemctl --user {} failed", args.join(" "))),
        }
    }

    pub fn node_enabled() -> bool {
        config_home().is_ok_and(|home| {
            home.join("systemd/user")
                .join(format!("{TARGET}.wants"))
                .join(NODE_UNIT)
                .exists()
        })
    }

    pub fn set_node(on: bool) -> Result<(), String> {
        let path = unit_path()?;
        if on {
            let exe = crate::node::node_executable();
            let unit = format!(
                "[Unit]\n\
                 Description=Cursio mesh node\n\
                 After={TARGET}\n\
                 PartOf={TARGET}\n\
                 \n\
                 [Service]\n\
                 ExecStart=\"{}\" mesh\n\
                 Restart=on-failure\n\
                 RestartSec=3\n\
                 \n\
                 [Install]\n\
                 WantedBy={TARGET}\n",
                exe.display()
            );
            std::fs::create_dir_all(path.parent().expect("unit path has a parent"))
                .and_then(|()| std::fs::write(&path, unit))
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            systemctl(&["daemon-reload"])?;
            systemctl(&["enable", NODE_UNIT])
        } else {
            if path.exists() {
                systemctl(&["disable", NODE_UNIT])?;
                std::fs::remove_file(&path)
                    .map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
            }
            systemctl(&["daemon-reload"])
        }
    }

    pub fn tray_enabled() -> bool {
        desktop_path().is_ok_and(|p| p.exists())
    }

    pub fn set_tray(on: bool) -> Result<(), String> {
        let path = desktop_path()?;
        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let entry = format!(
                "[Desktop Entry]\n\
                 Type=Application\n\
                 Name=Cursio\n\
                 Comment=Status and settings for the Cursio mesh\n\
                 Exec=\"{}\" --hidden\n\
                 Terminal=false\n\
                 X-GNOME-Autostart-enabled=true\n",
                exe.display()
            );
            std::fs::create_dir_all(path.parent().expect("autostart path has a parent"))
                .and_then(|()| std::fs::write(&path, entry))
                .map_err(|e| format!("cannot write {}: {e}", path.display()))
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const NODE_VALUE: &str = "Cursio";
    const TRAY_VALUE: &str = "CursioApp";

    fn reg(args: &[&str]) -> Result<bool, String> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("reg")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|out| out.status.success())
            .map_err(|e| format!("cannot run reg.exe: {e}"))
    }

    fn enabled(value: &str) -> bool {
        reg(&["query", RUN_KEY, "/v", value]).unwrap_or(false)
    }

    fn set(value: &str, command: Option<String>) -> Result<(), String> {
        let ok = match &command {
            Some(command) => reg(&[
                "add", RUN_KEY, "/v", value, "/t", "REG_SZ", "/d", command, "/f",
            ])?,
            None if enabled(value) => reg(&["delete", RUN_KEY, "/v", value, "/f"])?,
            None => true,
        };
        match ok {
            true => Ok(()),
            false => Err(format!("reg.exe could not change {RUN_KEY}\\{value}")),
        }
    }

    pub fn node_enabled() -> bool {
        enabled(NODE_VALUE)
    }

    pub fn set_node(on: bool) -> Result<(), String> {
        let exe = crate::node::node_executable();
        set(
            NODE_VALUE,
            on.then(|| format!("\"{}\" mesh", exe.display())),
        )
    }

    pub fn tray_enabled() -> bool {
        enabled(TRAY_VALUE)
    }

    pub fn set_tray(on: bool) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        set(
            TRAY_VALUE,
            on.then(|| format!("\"{}\" --hidden", exe.display())),
        )
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod platform {
    const UNSUPPORTED: &str = "starting at login is only set up on Linux and Windows so far";

    pub fn node_enabled() -> bool {
        false
    }
    pub fn set_node(_on: bool) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn tray_enabled() -> bool {
        false
    }
    pub fn set_tray(_on: bool) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
}

#[cfg(target_os = "linux")]
pub fn node_unit_installed() -> bool {
    platform::unit_installed()
}

pub fn current() -> Autostart {
    Autostart {
        node: platform::node_enabled(),
        tray: platform::tray_enabled(),
    }
}

pub mod commands {
    use super::*;

    // Both run `reg.exe` or `systemctl`, which take tens to hundreds of
    // milliseconds. A synchronous command runs on the thread that draws the
    // window, and the window froze for exactly that long; these wait on a
    // blocking-pool thread instead.

    #[tauri::command]
    pub async fn autostart_get() -> Result<Autostart, String> {
        tauri::async_runtime::spawn_blocking(current)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn autostart_set(node: bool, tray: bool) -> Result<Autostart, String> {
        tauri::async_runtime::spawn_blocking(move || {
            let now = current();
            if now.node != node {
                platform::set_node(node)?;
            }
            if now.tray != tray {
                platform::set_tray(tray)?;
            }
            Ok(current())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}
