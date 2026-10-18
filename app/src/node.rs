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
        match tokio::time::timeout(deadline, client.request(command)).await {
            Err(_) => Err(NodeError::failed(format!(
                "the node did not answer within {} s",
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

    /// Desktop only: a phone never takes the cursor, so it has no place in the
    /// layout to move.
    #[cfg(desktop)]
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

    /// The node has no command for one audio field, so this edits the file as
    /// written (not the effective config, which would bake flags into it) and
    /// hands the whole of it back. `value` is what the file takes: `auto`,
    /// `off` or a level in dBFS.
    ///
    /// Checked here because the node does not check it on save: a running
    /// node only declines to apply a bad value, but the file keeps it and the
    /// next start fails on it. The rule is `audio_gate::MicGate`'s, which the
    /// app cannot call — it lives behind the core's `audio` feature.
    #[tauri::command]
    pub async fn node_set_mic_gate(
        link: State<'_, NodeLink>,
        value: String,
    ) -> Result<Reply, NodeError> {
        let level_ok = value
            .parse::<f32>()
            .is_ok_and(|db| (-100.0..=0.0).contains(&db));
        if !matches!(value.as_str(), "auto" | "off") && !level_ok {
            return Err(NodeError::failed(format!(
                "mic gate {value:?} is not \"auto\", \"off\" or a level in dBFS between -100 and 0"
            )));
        }
        let mut config = match link.request(Command::Config).await? {
            Reply::Config { file, .. } => *file,
            other => {
                return Err(NodeError::failed(format!(
                    "the node answered a config request with {other:?}"
                )));
            }
        };
        config.audio.mic_gate = Some(value);
        link.request(Command::SetConfig { config }).await
    }

    /// Replace the `[calls]` section of the config file, like
    /// [`node_set_mic_gate`]: the file's own config, not the effective one.
    #[tauri::command]
    pub async fn node_set_calls(
        link: State<'_, NodeLink>,
        calls: cursio_core::config::CallsConfig,
    ) -> Result<Reply, NodeError> {
        let mut config = match link.request(Command::Config).await? {
            Reply::Config { file, .. } => *file,
            other => {
                return Err(NodeError::failed(format!(
                    "the node answered a config request with {other:?}"
                )));
            }
        };
        config.calls = calls;
        link.request(Command::SetConfig { config }).await
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

    /// Desktop only: restarting re-executes the `cursio` process, and a phone's
    /// node is this app's process.
    #[cfg(desktop)]
    #[tauri::command]
    pub async fn node_restart(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
        link.request(Command::Restart).await
    }

    /// Desktop only: a phone is updated by installing the app.
    #[cfg(desktop)]
    #[tauri::command]
    pub async fn node_update_apply(link: State<'_, NodeLink>) -> Result<Reply, NodeError> {
        link.request(Command::UpdateApply).await
    }

    /// `peer` as the status carries it: the id, not the name, since two
    /// machines may share a name and only one is meant.
    #[tauri::command]
    pub async fn node_forget(link: State<'_, NodeLink>, peer: String) -> Result<Reply, NodeError> {
        let peer = peer
            .parse()
            .map_err(|e| NodeError::failed(format!("`{peer}` is not a machine id: {e}")))?;
        link.request(Command::Forget { peer }).await
    }

    /// Rename this machine. Saved at once; links take it when they next come
    /// up, so the status lists `name` among what waits for a restart.
    #[tauri::command]
    pub async fn node_set_name(
        link: State<'_, NodeLink>,
        name: String,
    ) -> Result<Reply, NodeError> {
        link.request(Command::SetName { name }).await
    }

    /// The listener's volume on one peer stream playing here.
    #[tauri::command]
    pub async fn node_set_stream_volume(
        link: State<'_, NodeLink>,
        stream: u32,
        volume: f32,
        muted: bool,
    ) -> Result<Reply, NodeError> {
        link.request(Command::SetStreamVolume {
            stream,
            volume,
            muted,
        })
        .await
    }

    /// A phone's conversations, newest first. `peer` is its id.
    #[cfg(desktop)]
    #[tauri::command]
    pub async fn messages_conversations(
        link: State<'_, NodeLink>,
        peer: String,
        limit: u32,
        before_ms: Option<i64>,
    ) -> Result<Reply, NodeError> {
        link.request_within(
            Command::ListConversations {
                peer,
                limit,
                before_ms,
            },
            PHONE_TIMEOUT,
        )
        .await
    }

    /// One conversation's messages, newest first.
    #[cfg(desktop)]
    #[tauri::command]
    pub async fn messages_thread(
        link: State<'_, NodeLink>,
        peer: String,
        thread_id: i64,
        limit: u32,
        before_ms: Option<i64>,
    ) -> Result<Reply, NodeError> {
        link.request_within(
            Command::ListMessages {
                peer,
                thread_id,
                limit,
                before_ms,
            },
            PHONE_TIMEOUT,
        )
        .await
    }

    #[cfg(desktop)]
    #[tauri::command]
    pub async fn messages_send(
        link: State<'_, NodeLink>,
        peer: String,
        addresses: Vec<String>,
        body: String,
    ) -> Result<Reply, NodeError> {
        link.request_within(
            Command::SendSms {
                peer,
                addresses,
                body,
            },
            PHONE_TIMEOUT,
        )
        .await
    }

    #[cfg(desktop)]
    #[tauri::command]
    pub async fn messages_mark_read(
        link: State<'_, NodeLink>,
        peer: String,
        thread_id: i64,
    ) -> Result<Reply, NodeError> {
        link.request_within(Command::MarkSmsRead { peer, thread_id }, PHONE_TIMEOUT)
            .await
    }

    /// Put a gallery command to the machine it names: through the node's
    /// socket on a desktop, straight to the in-process node on a phone.
    async fn gallery(
        #[cfg_attr(mobile, allow(unused_variables))] link: State<'_, NodeLink>,
        command: Command,
    ) -> Result<Reply, NodeError> {
        #[cfg(desktop)]
        {
            let deadline = match command {
                Command::GallerySave { .. } => GALLERY_SAVE_TIMEOUT,
                _ => GALLERY_TIMEOUT,
            };
            link.request_within(command, deadline).await
        }
        #[cfg(mobile)]
        match cursio_core::control::ask_gallery(&command).await {
            Some(Reply::Error { message }) => Err(NodeError::Failed { message }),
            Some(reply) => Ok(reply),
            None => Err(NodeError::failed("not a gallery command")),
        }
    }

    /// A page of a machine's gallery, newest first. `peer` is its id.
    #[tauri::command]
    pub async fn gallery_list(
        link: State<'_, NodeLink>,
        peer: String,
        limit: u32,
        after: Option<cursio_protocol::gallery::PageMark>,
        filter: cursio_protocol::gallery::MediaFilter,
    ) -> Result<Reply, NodeError> {
        gallery(
            link,
            Command::GalleryList {
                peer,
                limit,
                after,
                filter,
            },
        )
        .await
    }

    #[tauri::command]
    pub async fn gallery_thumbnail(
        link: State<'_, NodeLink>,
        peer: String,
        item: String,
        max_px: u32,
    ) -> Result<Reply, NodeError> {
        gallery(link, Command::GalleryThumbnail { peer, item, max_px }).await
    }

    #[tauri::command]
    pub async fn gallery_read(
        link: State<'_, NodeLink>,
        peer: String,
        item: String,
        offset: u64,
        len: u32,
    ) -> Result<Reply, NodeError> {
        gallery(
            link,
            Command::GalleryRead {
                peer,
                item,
                offset,
                len,
            },
        )
        .await
    }

    /// Copy one item into this computer's `Pictures/Cursio/<peer>`.
    #[cfg(desktop)]
    #[tauri::command]
    pub async fn gallery_save(
        link: State<'_, NodeLink>,
        peer: String,
        item: String,
        name: String,
    ) -> Result<Reply, NodeError> {
        gallery(link, Command::GallerySave { peer, item, name }).await
    }

    /// Answer, decline, end or move the voice of a phone's call. `phone` is
    /// its name as the status's `calls` gives it.
    #[cfg(desktop)]
    #[tauri::command]
    pub async fn node_call(
        link: State<'_, NodeLink>,
        phone: String,
        action: cursio_core::calls_hub::CallAction,
    ) -> Result<Reply, NodeError> {
        link.request(Command::Call { phone, action }).await
    }

    /// Show a phone's screen in a window here, or stop.
    #[cfg(desktop)]
    #[tauri::command]
    pub async fn node_phone_screen(
        link: State<'_, NodeLink>,
        phone: String,
        show: bool,
    ) -> Result<Reply, NodeError> {
        link.request(Command::PhoneScreen { peer: phone, show })
            .await
    }

    /// Off the window's thread: `systemctl` or a process spawn can take a
    /// noticeable moment, and a synchronous command would freeze the window
    /// for it.
    #[cfg(desktop)]
    #[tauri::command]
    pub async fn node_start() -> Result<(), NodeError> {
        tauri::async_runtime::spawn_blocking(start_node)
            .await
            .map_err(NodeError::failed)?
    }

    /// On a phone the node belongs to the foreground service, not the window:
    /// starting it means starting the service, which outlives this Activity.
    #[cfg(mobile)]
    #[tauri::command]
    pub async fn node_start(app: AppHandle) -> Result<(), NodeError> {
        crate::mobile::start_service(&app).map_err(NodeError::failed)
    }
}
