//! The app's line to the running `cursio` node.
//!
//! The app never runs the mesh: it is a client of the node's control socket
//! (see `cursio_core::control`), exactly like `cursio status`. The shape is
//! the LeavePulse launcher's: one connection, subscribed, whose pushes are
//! re-emitted as Tauri events ([`STATUS_EVENT`], [`DOWN_EVENT`]) so the window
//! and the tray redraw when the node changes rather than when they next ask;
//! requests share that connection and are matched by id, each with a deadline,
//! so a click never waits behind another request or on a node that stopped
//! answering.
//!
//! It used to hold one connection behind an async mutex and send requests one
//! at a time. A settings change restarted the node, the next request waited
//! out the reconnect, and the window's own status poll queued behind it — the
//! page froze for as long as that took.

use std::path::PathBuf;
use std::time::Duration;

use cursio_core::control::transport::{self, Client};
use cursio_core::control::{Command, ModuleSwitch, Reply, Status};
use cursio_protocol::Position;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// Carries the node's [`Status`] each time it changes.
pub const STATUS_EVENT: &str = "node://status";
/// Carries a [`NodeError`] when the node stops answering.
pub const DOWN_EVENT: &str = "node://down";

/// Longest a request may take before the UI is told so. A node answers in
/// microseconds; this is only for one that is wedged or going away.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How soon to look for a node again once it has gone.
const RECONNECT: Duration = Duration::from_secs(1);

/// Errors the UI shows as they are, so they say what to do.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum NodeError {
    /// No node answered. The UI offers to start one.
    NotRunning {
        message: String,
    },
    Failed {
        message: String,
    },
}

impl NodeError {
    fn failed(e: impl std::fmt::Display) -> Self {
        Self::Failed {
            message: e.to_string(),
        }
    }
}

/// What the app last heard from the node: its status, or why there is none.
/// `None` until the first attempt to reach it has finished.
pub type Latest = Option<Result<Status, NodeError>>;

pub struct NodeLink {
    /// The follower's connection, shared by requests. The lock is held only
    /// to clone or replace it, never across a request.
    client: std::sync::Mutex<Option<Client>>,
    latest: tokio::sync::watch::Sender<Latest>,
}

impl Default for NodeLink {
    fn default() -> Self {
        Self {
            client: std::sync::Mutex::new(None),
            latest: tokio::sync::watch::channel(None).0,
        }
    }
}

impl NodeLink {
    fn connected(&self) -> Option<Client> {
        self.client
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .filter(Client::is_connected)
    }

    fn set_client(&self, client: Option<Client>) {
        *self.client.lock().unwrap_or_else(|p| p.into_inner()) = client;
    }

    /// Record what was heard; answers whether it differs from before.
    fn publish(&self, latest: Result<Status, NodeError>) -> bool {
        self.latest.send_if_modified(|slot| {
            let changed = slot.as_ref() != Some(&latest);
            *slot = Some(latest);
            changed
        })
    }

    /// Follows every change the node pushes. For the tray, which has no
    /// window to receive events.
    pub fn watch(&self) -> tokio::sync::watch::Receiver<Latest> {
        self.latest.subscribe()
    }

    /// Send one command on the shared connection, or on a fresh one while the
    /// follower is between connections, within [`REQUEST_TIMEOUT`].
    pub async fn request(&self, command: Command) -> Result<Reply, NodeError> {
        let client = match self.connected() {
            Some(client) => client,
            None => connect().await?.0,
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, client.request(command)).await {
            Err(_) => Err(NodeError::failed(format!(
                "the node did not answer within {} s",
                REQUEST_TIMEOUT.as_secs()
            ))),
            Ok(Err(e)) => Err(NodeError::NotRunning {
                message: e.to_string(),
            }),
            Ok(Ok(Reply::Error { message })) => Err(NodeError::Failed { message }),
            Ok(Ok(reply)) => Ok(reply),
        }
    }

    /// The status as last pushed, or asked for when nothing has been yet.
    pub async fn status(&self) -> Result<Status, NodeError> {
        if let Some(latest) = self.latest.borrow().clone() {
            return latest;
        }
        match self.request(Command::Status).await? {
            Reply::Status(status) => Ok(*status),
            other => Err(NodeError::failed(format!("unexpected reply {other:?}"))),
        }
    }
}

async fn connect() -> Result<(Client, tokio::sync::mpsc::Receiver<Reply>), NodeError> {
    let endpoint = transport::endpoint().map_err(NodeError::failed)?;
    transport::connect(&endpoint)
        .await
        .map_err(|e| NodeError::NotRunning {
            message: e.to_string(),
        })
}

/// Keep a subscribed connection to the node for the life of the app, and turn
/// what it pushes into events.
///
/// A node that is not running, or goes away, is looked for again every
/// [`RECONNECT`]; the window hears [`DOWN_EVENT`] once, not once a second.
pub fn follow(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let error = match connect().await {
                Ok((client, pushes)) => {
                    app.state::<NodeLink>().set_client(Some(client.clone()));
                    let ended = relay(&app, &client, pushes).await;
                    app.state::<NodeLink>().set_client(None);
                    ended
                }
                Err(error) => error,
            };
            if app.state::<NodeLink>().publish(Err(error.clone())) {
                let _ = app.emit(DOWN_EVENT, &error);
            }
            tokio::time::sleep(RECONNECT).await;
        }
    });
}

/// Subscribe on `client` and emit each pushed status until the connection
/// ends; answers why it ended.
async fn relay(
    app: &AppHandle,
    client: &Client,
    mut pushes: tokio::sync::mpsc::Receiver<Reply>,
) -> NodeError {
    match tokio::time::timeout(REQUEST_TIMEOUT, client.request(Command::Subscribe)).await {
        Ok(Ok(Reply::Subscribed)) => {}
        Ok(Ok(other)) => {
            return NodeError::failed(format!(
                "the running node does not push its status ({other:?}); restart it with this \
                 version of Cursio"
            ))
        }
        Ok(Err(e)) => {
            return NodeError::NotRunning {
                message: e.to_string(),
            }
        }
        Err(_) => {
            return NodeError::failed(
                "the running node did not answer a subscription; it is older than this app — \
                 restart it with this version of Cursio",
            )
        }
    }
    while let Some(reply) = pushes.recv().await {
        if let Reply::Status(status) = reply {
            if app.state::<NodeLink>().publish(Ok((*status).clone())) {
                let _ = app.emit(STATUS_EVENT, &*status);
            }
        }
    }
    NodeError::NotRunning {
        message: "the node closed the connection".into(),
    }
}

/// The `cursio` executable: beside this one when installed together, else
/// whatever `PATH` finds.
pub fn node_executable() -> PathBuf {
    let name = if cfg!(windows) {
        "cursio.exe"
    } else {
        "cursio"
    };
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
