//! The app's line to the running `cursio` node.
//!
//! The app never runs the mesh: it is a client of the node's control socket
//! (see `cursio_core::control`), exactly like `cursio status`. What lives here
//! is keeping one connection open, reopening it when the node restarts —
//! which it does on every config change — and starting a node when none is
//! running.

use std::path::PathBuf;

use cursio_core::control::transport::{self, Client, Stream};
use cursio_core::control::{Command, ModuleSwitch, Reply, Status};
use cursio_protocol::Position;
use serde::Serialize;
use tokio::sync::Mutex;

/// Errors the UI shows as they are, so they say what to do.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum NodeError {
    /// No node answered. The UI offers to start one.
    NotRunning { message: String },
    Failed { message: String },
}

impl NodeError {
    fn failed(e: impl std::fmt::Display) -> Self {
        Self::Failed {
            message: e.to_string(),
        }
    }
}

#[derive(Default)]
pub struct NodeLink {
    client: Mutex<Option<Client<Stream>>>,
}

impl NodeLink {
    /// Send one command, reconnecting once if the node went away since the
    /// last one — a restart after a config change looks exactly like that.
    pub async fn request(&self, command: Command) -> Result<Reply, NodeError> {
        let mut slot = self.client.lock().await;
        for attempt in 0..2 {
            if slot.is_none() {
                let endpoint = transport::endpoint().map_err(NodeError::failed)?;
                match transport::connect(&endpoint).await {
                    Ok(client) => *slot = Some(client),
                    Err(e) => {
                        return Err(NodeError::NotRunning {
                            message: e.to_string(),
                        })
                    }
                }
            }
            let client = slot.as_mut().expect("connected above");
            match client.request(command.clone()).await {
                Ok(Reply::Error { message }) => return Err(NodeError::Failed { message }),
                Ok(reply) => return Ok(reply),
                Err(e) if attempt == 0 => {
                    tracing::debug!(error = %e, "control connection dropped; reconnecting");
                    *slot = None;
                }
                Err(e) => return Err(NodeError::failed(e)),
            }
        }
        unreachable!("the loop returns on its second pass")
    }

    pub async fn status(&self) -> Result<Status, NodeError> {
        match self.request(Command::Status).await? {
            Reply::Status(status) => Ok(*status),
            other => Err(NodeError::failed(format!("unexpected reply {other:?}"))),
        }
    }
}

/// The `cursio` executable: beside this one when installed together, else
/// whatever `PATH` finds.
pub fn node_executable() -> PathBuf {
    let name = if cfg!(windows) { "cursio.exe" } else { "cursio" };
    std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name(name))
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Start a node that outlives this app.
///
/// Through the service manager when autostart installed a unit, so there is
/// one node and one place that owns it; otherwise detached from this process
/// so closing the window does not take the mesh down with it.
pub fn start_node() -> Result<(), NodeError> {
    #[cfg(target_os = "linux")]
    if crate::autostart::node_unit_installed() {
        let status = std::process::Command::new("systemctl")
            .args(["--user", "start", crate::autostart::NODE_UNIT])
            .status()
            .map_err(NodeError::failed)?;
        return match status.success() {
            true => Ok(()),
            false => Err(NodeError::failed(format!(
                "systemctl --user start {} failed; see `journalctl --user -u {}`",
                crate::autostart::NODE_UNIT,
                crate::autostart::NODE_UNIT
            ))),
        };
    }
    let mut command = std::process::Command::new(node_executable());
    command
        .arg("mesh")
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
    command.spawn().map(drop).map_err(|e| {
        NodeError::failed(format!(
            "could not start {}: {e}",
            node_executable().display()
        ))
    })
}

pub mod commands {
    use super::*;
    use tauri::State;

    #[tauri::command]
    pub async fn node_status(link: State<'_, NodeLink>) -> Result<Status, NodeError> {
        link.status().await
    }

    #[tauri::command]
    pub async fn node_config(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
        link.request(Command::Config).await
    }

    #[tauri::command]
    pub async fn node_set_module(
        link: State<'_, NodeLink>,
        module: ModuleSwitch,
        enabled: bool,
    ) -> Result<Reply, NodeError> {
        link.request(Command::SetModule { module, enabled }).await
    }

    #[tauri::command]
    pub async fn node_set_position(
        link: State<'_, NodeLink>,
        x: i32,
        y: i32,
    ) -> Result<Reply, NodeError> {
        link.request(Command::SetPosition {
            position: Position::new(x, y),
        })
        .await
    }

    #[tauri::command]
    pub async fn node_pair_show(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
        link.request(Command::PairShow).await
    }

    #[tauri::command]
    pub async fn node_pair_enter(
        link: State<'_, NodeLink>,
        code: String,
    ) -> Result<Reply, NodeError> {
        link.request(Command::PairEnter { code }).await
    }

    #[tauri::command]
    pub async fn node_pair_cancel(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
        link.request(Command::PairCancel).await
    }

    #[tauri::command]
    pub async fn node_pairing(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
        link.request(Command::Pairing).await
    }

    #[tauri::command]
    pub async fn node_restart(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
        link.request(Command::Restart).await
    }

    #[tauri::command]
    pub fn node_start() -> Result<(), NodeError> {
        start_node()
    }
}
