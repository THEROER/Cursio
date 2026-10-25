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
//!
//! On a phone the node is in this process (`cursio_android`), so there is no
//! socket: a command goes straight to its `NodeState::handle`, and the status
//! follows its revisions. The events and errors the window sees are the same.

#[cfg(desktop)]
use std::path::PathBuf;
use std::time::Duration;

#[cfg(desktop)]
use cursio_core::control::transport::{self, Client};
use cursio_core::control::{Command, ModuleSwitch, Reply, Status};
#[cfg(desktop)]
use cursio_protocol::Position;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// Carries the node's [`Status`] each time it changes.
pub const STATUS_EVENT: &str = "node://status";
/// Carries a [`NodeError`] when the node stops answering.
pub const DOWN_EVENT: &str = "node://down";

/// Longest a request may take before the UI is told so. A node answers in
/// microseconds; this is only for one that is wedged or going away.
#[cfg(desktop)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest a request answered by a phone may take: the node itself waits
/// [`cursio_core::messages_hub::ANSWER_WAIT`] for the phone, and says which
/// one did not answer; this only catches a node that never replies at all.
#[cfg(desktop)]
const PHONE_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest one gallery answer may take: the node waits
/// [`cursio_core::gallery_hub::ANSWER_WAIT`] for the peer itself.
#[cfg(desktop)]
const GALLERY_TIMEOUT: Duration = Duration::from_secs(25);

/// Longest saving one gallery item may take: a long video is many slices,
/// each answered in turn.
#[cfg(desktop)]
const GALLERY_SAVE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
    #[cfg(desktop)]
    client: std::sync::Mutex<Option<Client>>,
    latest: tokio::sync::watch::Sender<Latest>,
}

impl Default for NodeLink {
    fn default() -> Self {
        Self {
            #[cfg(desktop)]
            client: std::sync::Mutex::new(None),
            latest: tokio::sync::watch::channel(None).0,
        }
    }
}

impl NodeLink {
    #[cfg(desktop)]
    fn connected(&self) -> Option<Client> {
        self.client
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .filter(Client::is_connected)
    }

    #[cfg(desktop)]
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
    #[cfg(desktop)]
    pub fn watch(&self) -> tokio::sync::watch::Receiver<Latest> {
        self.latest.subscribe()
    }

    /// Send one command on the shared connection, or on a fresh one while the
    /// follower is between connections, within [`REQUEST_TIMEOUT`].
    #[cfg(desktop)]
    pub async fn request(&self, command: Command) -> Result<Reply, NodeError> {
        self.request_within(command, REQUEST_TIMEOUT).await
    }

    /// [`Self::request`] with a deadline of the caller's: a command the node
    /// answers only once a peer has is given longer than one it answers itself.
    #[cfg(desktop)]
    pub async fn request_within(
        &self,
        command: Command,
        deadline: Duration,
    ) -> Result<Reply, NodeError> {
        let client = match self.connected() {
            Some(client) => client,
            None => connect().await?.0,
        };
        let asked = command_name(&command);
        match tokio::time::timeout(deadline, client.request(command)).await {
            Err(_) => Err(NodeError::failed(format!(
                "the node did not answer `{asked}` within {} s; it may be restarting, or busy with a peer",
                deadline.as_secs()
            ))),
            Ok(Err(e)) => Err(NodeError::NotRunning {
                message: e.to_string(),
            }),
            Ok(Ok(Reply::Error { message })) => Err(NodeError::Failed { message }),
            Ok(Ok(reply)) => Ok(reply),
        }
    }

    /// Hand one command to the node in this process. Off the async runtime,
    /// since a config change writes the file before it answers.
    #[cfg(mobile)]
    pub async fn request(&self, command: Command) -> Result<Reply, NodeError> {
        let state = in_process()?;
        let reply = tauri::async_runtime::spawn_blocking(move || state.handle(command))
            .await
            .map_err(NodeError::failed)?;
        match reply {
            Reply::Error { message } => Err(NodeError::Failed { message }),
            reply => Ok(reply),
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

/// A command's wire name (`set_config`, `status`), for saying which one went
/// unanswered without printing its whole payload.
#[cfg(desktop)]
fn command_name(command: &Command) -> String {
    serde_json::to_value(command)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "a command".into())
}

/// The node in this process, once the service has started it.
#[cfg(mobile)]
fn in_process() -> Result<cursio_core::control::NodeState, NodeError> {
    cursio_android::control().ok_or_else(|| NodeError::NotRunning {
        message: "Cursio is not running on this phone".into(),
    })
}

/// How often a phone's status is re-read without a change to wake it: latency
/// and problems change without bumping the node's revision, as on a desktop,
/// where the socket's periodic push covers them.
#[cfg(mobile)]
const STATUS_REFRESH: Duration = Duration::from_secs(1);

/// Follow the node in this process for the life of the app, turning its
/// changes into the same events a desktop's socket pushes become.
#[cfg(mobile)]
pub fn follow(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let state = match in_process() {
                Ok(state) => state,
                Err(error) => {
                    if app.state::<NodeLink>().publish(Err(error.clone())) {
                        let _ = app.emit(DOWN_EVENT, &error);
                    }
                    tokio::time::sleep(RECONNECT).await;
                    continue;
                }
            };
            let mut revisions = state.revisions();
            // Until the node goes away: the service stopping it drops the slot,
            // which is what `in_process` reads.
            while cursio_android::is_running() {
                let status = state.status();
                if app.state::<NodeLink>().publish(Ok(status.clone())) {
                    let _ = app.emit(STATUS_EVENT, &status);
                }
                let _ = tokio::time::timeout(STATUS_REFRESH, revisions.changed()).await;
            }
        }
    });
}

#[cfg(desktop)]
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
#[cfg(desktop)]
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
#[cfg(desktop)]
async fn relay(
    app: &AppHandle,
    client: &Client,
    mut pushes: tokio::sync::mpsc::Receiver<Reply>,
) -> NodeError {
    match tokio::time::timeout(REQUEST_TIMEOUT, client.request(Command::Subscribe)).await {
        Ok(Ok(Reply::Subscribed)) => {
            // An older node does not know the command; its views keep their icons.
            if crate::tray::shown() {
                let _ = client.request(Command::TrayAttached).await;
            }
        }
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

/// The `cursio` executable: the install's stable copy, which always starts the
/// current version; else beside this one; else whatever `PATH` finds.
#[cfg(desktop)]
pub fn node_executable() -> PathBuf {
    if let Some(stable) = cursio_core::update::layout::stable_executable("cursio") {
        return stable;
    }
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

/// What to register for the app itself: the stable copy when installed, since
/// this process may be running from a version directory a prune will remove.
#[cfg(desktop)]
pub fn app_executable() -> std::io::Result<PathBuf> {
    match cursio_core::update::layout::stable_executable("cursio-app") {
        Some(stable) => Ok(stable),
        None => std::env::current_exe(),
    }
}

/// Start a node that outlives this app.
///
/// Through the service manager when autostart installed a unit, so there is
/// one node and one place that owns it; otherwise detached from this process
/// so closing the window does not take the mesh down with it.
#[cfg(desktop)]
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

pub mod commands;
