//! What is installed, and going back to the previous version.
//!
//! Read straight from the install layout (`cursio_core::update::layout`) rather
//! than asked of the node: the versions on disk are there whether or not a node
//! runs, and a rollback is most wanted exactly when the new one does not come
//! up. Switching is not done here — it is `cursio update rollback`, the same
//! helper `apply` uses, started detached so it can stop the node, outlive this
//! window, and roll forward again if the old version fails its health check.

use cursio_core::update::layout::InstallLayout;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct InstallView {
    /// Where versions live, for the window to show; `None` when this platform
    /// has no per-user root.
    root: Option<String>,
    current: Option<String>,
    previous: Option<String>,
    /// Oldest first, as the layout lists them.
    installed: Vec<String>,
}

pub fn view() -> InstallView {
    match InstallLayout::for_user() {
        Ok(layout) => InstallView {
            root: Some(layout.root().display().to_string()),
            current: layout.current(),
            previous: layout.previous(),
            installed: layout.installed(),
        },
        Err(_) => InstallView {
            root: None,
            current: None,
            previous: None,
            installed: Vec::new(),
        },
    }
}

/// Start `cursio update rollback`, waiting on and stopping `node_pid` (0 when
/// no node runs).
pub fn roll_back(node_pid: u32) -> Result<(), String> {
    let layout = InstallLayout::for_user().map_err(|e| e.to_string())?;
    if layout.previous().is_none() {
        return Err("there is no previous version to go back to".into());
    }
    let exe = crate::node::node_executable();
    let mut command = std::process::Command::new(&exe);
    command.args(["update", "rollback", "--stop"]);
    if node_pid != 0 {
        command.args(["--wait-pid", &node_pid.to_string()]);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command
        .spawn()
        .map(drop)
        .map_err(|e| format!("could not start {}: {e}", exe.display()))
}

pub mod commands {
    use super::*;

    #[tauri::command]
    pub async fn install_status() -> Result<InstallView, String> {
        tauri::async_runtime::spawn_blocking(view)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn install_rollback(node_pid: u32) -> Result<(), String> {
        tauri::async_runtime::spawn_blocking(move || roll_back(node_pid))
            .await
            .map_err(|e| e.to_string())?
    }
}
