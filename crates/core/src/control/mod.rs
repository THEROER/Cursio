//! The daemon's local control socket: what the tray, the app and `cursio
//! status` ask a running node, and how they change it.
//!
//! ## Why a socket and not an embedded node
//!
//! The app does not run the mesh itself. The node needs things a UI process
//! should not own: on Windows it self-elevates for UIPI, on Linux its window
//! module drives GTK4 while Tauri's webview is GTK3 — two toolkits that cannot
//! share a process — and it must keep running headless under a service
//! manager when no window is open. So the node stays the `cursio` binary, and
//! everything else is a client of this socket. The same split as the LeavePulse
//! launcher: shells render views they are handed and hold no state of their own.
//!
//! ## Local only, owner only
//!
//! A Unix socket in the user's runtime directory (0700 directory, 0600 socket)
//! or a named pipe whose DACL names only this user and SYSTEM, with remote
//! clients rejected. Anyone who can reach it can open a pairing window, so it
//! is exactly as private as the trust store it guards. See [`transport`].
//!
//! ## Wire format
//!
//! One JSON object per line, each way. A request carries an `id` the reply
//! echoes, so a client may have several in flight on one connection and match
//! each answer to its question. After [`Command::Subscribe`] the node also
//! pushes the status, with id 0, whenever it changes — the LeavePulse
//! launcher's arrangement. Polling was simpler, but a UI that waits on its own
//! poll queues every click behind it, and shows a toggle's result a poll late.
//!
//! ## Changing things goes through the config file
//!
//! A module toggle or a new grid position is written to `config.toml` and
//! applied to the running node at once (see [`crate::live`]); the node's own
//! watcher applies the same way when something else — a text editor, a
//! script — changes the file. One path for every change, and the file always
//! says what is running. A field a command-line flag pins cannot be changed
//! that way, and a setting only a restart applies is named in the reply
//! (`restart_needed`), rather than either pretending or restarting unasked.

pub mod transport;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use cursio_protocol::{DeviceKind, NodeStatus, PeerId, Position};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Overrides};
use crate::diagnostics::Problem;
use crate::metrics::LatencyReading;
use crate::pairing_window::{PairingCode, PairingOutcome, PairingState, PairingWindow};

/// Bumped when a request or reply changes shape, so a stale app says "update
/// me" rather than misreading a field. v2: `restart_needed` in the status and
/// in every save reply, now that changes apply without a restart. v3:
/// `subscribe`, and status pushed with id 0. v4: the node's `pid`, `build`
/// and `installed_version`, so the app can show and roll back what runs. v5:
/// `update` in the status and `update_apply`, for builds from the mesh. v6:
/// `forget`, removing a machine from the mesh. v7: `streams` in the status and
/// `set_stream_volume`, the listener's volume on each peer stream. v8:
/// `set_name`, and `available` in the status. v9: `share`, sending a link,
/// text or file to a peer.
pub const CONTROL_VERSION: u32 = 9;

/// The id a pushed message carries. Requests number from 1.
pub const PUSH_ID: u64 = 0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub reply: Reply,
}

/// A module switch in `config.toml`, by the name the file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSwitch {
    Input,
    Clipboard,
    Audio,
    Files,
    Usb,
    RemoteScreen,
    ScreenHost,
}

impl ModuleSwitch {
    /// The switches a node built from `modules` can act on. Input is not a
    /// registry module, so whether it can is the caller's to say; one window
    /// module answers for both screen switches.
    pub fn available(modules: &[cursio_protocol::ModuleId], input: bool) -> Vec<Self> {
        use cursio_protocol::ModuleId;
        let mut switches = Vec::new();
        if input {
            switches.push(Self::Input);
        }
        for (id, switch) in [
            (ModuleId::CLIPBOARD, Self::Clipboard),
            (ModuleId::AUDIO, Self::Audio),
            (ModuleId::FILES, Self::Files),
            (ModuleId::USB, Self::Usb),
        ] {
            if modules.contains(&id) {
                switches.push(switch);
            }
        }
        if modules.contains(&ModuleId::WINDOW) {
            switches.extend([Self::RemoteScreen, Self::ScreenHost]);
        }
        switches
    }

    fn field(self) -> &'static str {
        match self {
            Self::Input => "modules.input",
            Self::Clipboard => "modules.clipboard",
            Self::Audio => "modules.audio",
            Self::Files => "modules.files",
            Self::Usb => "modules.usb",
            Self::RemoteScreen => "modules.remote_screen",
            Self::ScreenHost => "modules.screen_host",
        }
    }

    fn slot(self, config: &mut Config) -> &mut bool {
        let m = &mut config.modules;
        match self {
            Self::Input => &mut m.input,
            Self::Clipboard => &mut m.clipboard,
            Self::Audio => &mut m.audio,
            Self::Files => &mut m.files,
            Self::Usb => &mut m.usb,
            Self::RemoteScreen => &mut m.remote_screen,
            Self::ScreenHost => &mut m.screen_host,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Ping,
    Status,
    /// The file as written, the effective config, and what flags pin.
    Config,
    /// Replace the file with this config.
    SetConfig {
        config: Config,
    },
    SetModule {
        module: ModuleSwitch,
        enabled: bool,
    },
    SetPosition {
        position: Position,
    },
    /// Open a window with a fresh code for this machine to show.
    PairShow,
    /// Open a window with a code read off another machine.
    PairEnter {
        code: String,
    },
    PairCancel,
    Pairing,
    /// Restart the node from the top, re-reading everything.
    Restart,
    /// Push the status on this connection whenever it changes, starting now.
    Subscribe,
    /// Switch to the build from the mesh that is installed and waiting. The
    /// node stops and comes back as that build, or as this one if it fails.
    UpdateApply,
    /// Remove a paired machine from the mesh: its key, its link, and builder
    /// trust. It has to be paired again to come back. See [`crate::forget`].
    Forget {
        peer: PeerId,
    },
    /// Rename this machine. Saved at once; peers see it once their links are
    /// brought up again, so the status names `name` until a restart.
    SetName {
        name: String,
    },
    /// Send a link, text or a file on this machine to a peer, to open, copy
    /// or keep. `peer` is its id or its name.
    Share {
        peer: String,
        target: String,
        action: cursio_protocol::share::ShareAction,
    },
    /// Set the listener's volume (linear, up to [`MAX_LOCAL_VOLUME`]) and
    /// mute on one peer stream playing here. See [`crate::mixer`].
    ///
    /// [`MAX_LOCAL_VOLUME`]: crate::mixer::MAX_LOCAL_VOLUME
    SetStreamVolume {
        // Not `id`: a command is flattened into its `Request`, whose own `id`
        // would collide with it on the wire.
        stream: u32,
        volume: f32,
        muted: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Pong {
        version: u32,
    },
    Status(Box<Status>),
    Config {
        path: PathBuf,
        file: Box<Config>,
        effective: Box<Config>,
        pinned: Vec<String>,
    },
    /// The file was written and what it changed is already running.
    /// `pinned` names fields the change could not reach; `restart_needed`
    /// names settings, this one or earlier, that wait for a restart.
    Saved {
        pinned: Vec<String>,
        restart_needed: Vec<String>,
    },
    Pairing {
        state: Option<PairingState>,
        last: Option<PairingOutcome>,
    },
    Done,
    /// Status will now be pushed on this connection.
    Subscribed,
    Error {
        message: String,
    },
    /// The machine is no longer part of this mesh.
    Forgotten {
        name: String,
        /// It had been trusted to sign builds for this machine.
        was_builder: bool,
    },
}

/// Everything a status window shows, in one reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub version: u32,
    pub node: NodeView,
    pub peers: Vec<PeerView>,
    /// What this node is configured to offer, after flags.
    pub modules: crate::config::Modules,
    /// The switches this build can act on; the rest name modules it has no
    /// backend for, which a window should not offer. Empty from a v7 node,
    /// which means "unknown", not "none".
    #[serde(default)]
    pub available: Vec<ModuleSwitch>,
    pub pinned: Vec<String>,
    /// Settings changed since start that only a restart applies, named as in
    /// the file. Empty when what runs is what the file says.
    pub restart_needed: Vec<String>,
    pub config_path: PathBuf,
    pub latency: Vec<LatencyReading>,
    pub problems: Vec<Problem>,
    pub pairing: Option<PairingState>,
    /// Builds arriving over the mesh. Defaulted from a v4 node.
    #[serde(default)]
    pub update: crate::update::agent::UpdateStatus,
    /// Peer streams playing here, with the listener's volume on each.
    /// Defaulted from a v6 node.
    #[serde(default)]
    pub streams: Vec<crate::mixer::MixerStream>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    pub id: PeerId,
    pub name: String,
    pub position: Position,
    pub status: NodeStatus,
    pub started_at: SystemTime,
    /// The node's process, for an update or rollback to wait on and stop.
    /// Defaulted (0) from a v3 node, so a newer app still reads its status.
    #[serde(default)]
    pub pid: u32,
    /// The build's crate version, whatever it was installed as.
    #[serde(default)]
    pub build: String,
    /// The install version this process runs from (`versions/<v>/`), or
    /// `None` for a build started from anywhere else (a checkout, a copy).
    #[serde(default)]
    pub installed_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Online,
    Connecting,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerView {
    pub id: PeerId,
    pub name: String,
    pub presence: Presence,
    /// Known only once a link has come up in this process.
    pub link: Option<LinkView>,
}

/// What a live link agreed, from the peer's greeting and negotiation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkView {
    pub position: Position,
    pub device_kind: DeviceKind,
    /// The peer's greeting status, replaced by any later report of it.
    pub status: NodeStatus,
    pub modules: Vec<String>,
    /// Modules this node offers that do not run on this link, and why.
    pub skipped: Vec<(String, String)>,
    pub since: SystemTime,
}

/// A known peer as the mesh loop last published it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownPeerView {
    pub id: PeerId,
    pub name: String,
    pub presence: Presence,
}

struct Inner {
    node: NodeView,
    known: Vec<KnownPeerView>,
    links: BTreeMap<PeerId, (String, LinkView)>,
    /// The name this process greets peers with, while it differs from the
    /// saved one: links carry a name from their greeting.
    greeted_as: Option<String>,
    available: Vec<ModuleSwitch>,
}

/// The daemon's state as the control socket sees it.
///
/// The mesh loop is `!Send` and owns the real registry; it *publishes* into
/// this, and the socket's handler reads from it on any thread. Stale by at
/// most one sweep, which is below what a person watching a window notices.
#[derive(Clone)]
pub struct NodeState {
    inner: Arc<Mutex<Inner>>,
    config_path: PathBuf,
    overrides: Overrides,
    /// What the process started with, against which [`live::restart_needed`]
    /// measures.
    ///
    /// [`live::restart_needed`]: crate::live::restart_needed
    started: Arc<Config>,
    running: Arc<Mutex<Config>>,
    applier: Applier,
    pairing: PairingWindow,
    restart: Arc<tokio::sync::Notify>,
    /// Bumped whenever something a status shows changed here, which is what
    /// wakes subscribers. What changes elsewhere — latency, problems — is
    /// caught by their periodic look.
    revision: Arc<tokio::sync::watch::Sender<u64>>,
    /// Tells the running mesh a machine was removed; set by the binary, which
    /// owns the links. Without one, removal is the on-disk half alone.
    on_forget: Arc<Mutex<Option<Forgetter>>>,
    /// The listener's per-stream volumes; the same one the audio factory
    /// hands every engine. See [`NodeState::mixer`].
    mixer: crate::mixer::LocalMixer,
}

/// Carries a removal into the running node. See [`NodeState::set_on_forget`].
pub type Forgetter = Arc<dyn Fn(PeerId) + Send + Sync>;

/// Carries a config change into the running node: given what runs and what
/// should, make it so. Supplied by the binary, which owns the pieces a change
/// reaches — the switches, the links' settings, the factories' options.
pub type Applier = Arc<dyn Fn(&Config, &Config) + Send + Sync>;

impl NodeState {
    pub fn new(
        identity: &crate::NodeIdentity,
        config_path: PathBuf,
        overrides: Overrides,
        running: Config,
        pairing: PairingWindow,
        applier: Applier,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                node: NodeView {
                    id: identity.peer_id,
                    name: identity.display_name.clone(),
                    position: running.position,
                    status: NodeStatus::unknown(),
                    started_at: SystemTime::now(),
                    pid: std::process::id(),
                    build: env!("CARGO_PKG_VERSION").to_owned(),
                    installed_version: running_install_version(),
                },
                known: Vec::new(),
                links: BTreeMap::new(),
                greeted_as: None,
                available: Vec::new(),
            })),
            config_path,
            overrides,
            started: Arc::new(running.clone()),
            running: Arc::new(Mutex::new(running)),
            applier,
            pairing,
            restart: Arc::new(tokio::sync::Notify::new()),
            revision: Arc::new(tokio::sync::watch::channel(0).0),
            on_forget: Arc::new(Mutex::new(None)),
            mixer: crate::mixer::LocalMixer::default(),
        }
    }

    /// Where [`Command::Forget`] reaches the running mesh: ending the link and
    /// dropping the registry entry, which only the mesh loop can do.
    pub fn set_on_forget(&self, forgetter: Forgetter) {
        *self
            .on_forget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(forgetter);
    }

    fn forget(&self, peer: PeerId) -> Reply {
        let forgotten = match crate::forget::forget_on_disk(peer, &self.config_path) {
            Ok(forgotten) => forgotten,
            Err(e) => return error(e),
        };
        let forgetter = self
            .on_forget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(forgetter) = forgetter {
            forgetter(peer);
        }
        {
            // Out of the status now, not at the mesh's next sweep, so the
            // window that asked does not show the machine for another moment.
            let mut inner = self.inner();
            inner.known.retain(|k| k.id != peer);
            inner.links.remove(&peer);
        }
        self.changed();
        Reply::Forgotten {
            name: forgotten.name,
            was_builder: forgotten.was_builder,
        }
    }

    fn changed(&self) {
        self.revision.send_modify(|r| *r = r.wrapping_add(1));
    }

    /// Which module switches this build can act on. Set once by whoever built
    /// the module registry, which is the only place that knows.
    pub fn set_available(&self, available: Vec<ModuleSwitch>) {
        self.inner().available = available;
        self.changed();
    }

    fn rename(&self, name: &str) -> Reply {
        let mut identity = match crate::NodeIdentity::load_or_create() {
            Ok(identity) => identity,
            Err(e) => return error(e),
        };
        if let Err(e) = identity.rename(name) {
            return error(e);
        }
        {
            let mut inner = self.inner();
            if inner.greeted_as.is_none() {
                inner.greeted_as = Some(inner.node.name.clone());
            }
            inner.node.name = identity.display_name;
        }
        self.changed();
        Reply::Done
    }

    /// The listener's per-stream volumes, for the audio factory to share
    /// with every engine, so what the socket sets is what plays.
    pub fn mixer(&self) -> crate::mixer::LocalMixer {
        self.mixer.clone()
    }

    /// Resolves each time something a status shows changes.
    pub fn revisions(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revision.subscribe()
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn running_config(&self) -> std::sync::MutexGuard<'_, Config> {
        self.running.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The config the node runs now, after flags.
    pub fn running(&self) -> Config {
        self.running_config().clone()
    }

    /// Apply `next` to the running node, and name what still waits for a
    /// restart. Applying what already runs changes nothing, so the file
    /// watcher may repeat a change the control socket already made.
    pub fn apply(&self, next: Config) -> Vec<String> {
        let mut running = self.running_config();
        if *running != next {
            (self.applier)(&running, &next);
            self.inner().node.position = next.position;
            *running = next;
            self.changed();
        }
        crate::live::restart_needed(&self.started, &running)
    }

    /// Apply whatever the file now says.
    pub fn apply_file(&self) -> Result<Vec<String>, crate::config::ConfigError> {
        let file = Config::load(&self.config_path)?;
        Ok(self.apply(file.with_overrides(&self.overrides)))
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn overrides(&self) -> &Overrides {
        &self.overrides
    }

    /// Published every sweep, so a subscriber is woken only when it differs.
    pub fn set_local_status(&self, status: NodeStatus) {
        let moved = std::mem::replace(&mut self.inner().node.status, status) != status;
        if moved {
            self.changed();
        }
    }

    /// Published every sweep, so a subscriber is woken only when it differs.
    pub fn set_known(&self, known: Vec<KnownPeerView>) {
        let moved = {
            let mut inner = self.inner();
            let moved = inner.known != known;
            inner.known = known;
            moved
        };
        if moved {
            self.changed();
        }
    }

    pub fn link_up(&self, peer: PeerId, name: &str, link: LinkView) {
        crate::metrics::note_peer_status(peer, link.status);
        self.inner().links.insert(peer, (name.to_owned(), link));
        self.changed();
    }

    /// A running link renegotiated: the peer moved, or the modules changed.
    /// The rest of what the link reported stays, the peer's latest status
    /// included.
    pub fn link_reconfigured(
        &self,
        peer: &PeerId,
        position: Position,
        modules: Vec<String>,
        skipped: Vec<(String, String)>,
    ) {
        if let Some((_, link)) = self.inner().links.get_mut(peer) {
            link.position = position;
            link.modules = modules;
            link.skipped = skipped;
        }
        self.changed();
    }

    pub fn link_down(&self, peer: &PeerId) {
        self.inner().links.remove(peer);
        self.changed();
    }

    /// Resolves when a client asked for a restart.
    pub async fn restart_requested(&self) {
        self.restart.notified().await
    }

    pub fn status(&self) -> Status {
        let inner = self.inner();
        let mut peers: Vec<PeerView> = inner
            .known
            .iter()
            .map(|k| PeerView {
                id: k.id,
                name: k.name.clone(),
                presence: k.presence,
                link: None,
            })
            .collect();
        // A peer paired in this process is linked before the registry, seeded
        // from the trust store at startup, knows it.
        for (id, (name, link)) in &inner.links {
            let mut link = link.clone();
            if let Some(status) = crate::metrics::peer_status(id) {
                link.status = status;
            }
            match peers.iter_mut().find(|p| p.id == *id) {
                Some(p) => {
                    p.presence = Presence::Online;
                    p.link = Some(link);
                }
                None => peers.push(PeerView {
                    id: *id,
                    name: name.clone(),
                    presence: Presence::Online,
                    link: Some(link),
                }),
            }
        }
        peers.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        let node = inner.node.clone();
        let renamed = inner
            .greeted_as
            .as_ref()
            .is_some_and(|greeted| *greeted != node.name);
        let available = inner.available.clone();
        drop(inner);
        let running = self.running();
        let mut restart_needed = crate::live::restart_needed(&self.started, &running);
        if renamed {
            restart_needed.push("name".to_owned());
        }
        Status {
            version: CONTROL_VERSION,
            node,
            peers,
            modules: running.modules,
            available,
            restart_needed,
            pinned: self.pinned(),
            config_path: self.config_path.clone(),
            latency: crate::metrics::input_latency(),
            problems: crate::diagnostics::snapshot(),
            pairing: self.pairing.state(),
            update: crate::update::agent::status(),
            streams: self.mixer.streams(),
        }
    }

    fn pinned(&self) -> Vec<String> {
        self.overrides
            .pinned()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Change the file with `edit`, reporting which touched fields a flag
    /// overrides anyway.
    fn edit_file(&self, touched: &[&str], edit: impl FnOnce(&mut Config)) -> Reply {
        let mut file = match Config::load(&self.config_path) {
            Ok(file) => file,
            Err(e) => return error(e),
        };
        edit(&mut file);
        if let Err(e) = file.save(&self.config_path) {
            return error(e);
        }
        let pinned = self.overrides.pinned();
        Reply::Saved {
            pinned: touched
                .iter()
                .filter(|f| pinned.contains(f))
                .map(|f| (*f).to_owned())
                .collect(),
            restart_needed: self.apply(file.with_overrides(&self.overrides)),
        }
    }

    pub fn handle(&self, command: Command) -> Reply {
        match command {
            Command::Ping => Reply::Pong {
                version: CONTROL_VERSION,
            },
            Command::Status => Reply::Status(Box::new(self.status())),
            Command::Config => match Config::load(&self.config_path) {
                Ok(file) => Reply::Config {
                    path: self.config_path.clone(),
                    effective: Box::new(file.clone().with_overrides(&self.overrides)),
                    file: Box::new(file),
                    pinned: self.pinned(),
                },
                Err(e) => error(e),
            },
            Command::SetConfig { config } => {
                if let Err(e) = config.save(&self.config_path) {
                    return error(e);
                }
                Reply::Saved {
                    pinned: self.pinned(),
                    restart_needed: self.apply(config.with_overrides(&self.overrides)),
                }
            }
            Command::SetModule { module, enabled } => {
                self.edit_file(&[module.field()], |c| *module.slot(c) = enabled)
            }
            Command::SetPosition { position } => {
                self.edit_file(&["position"], |c| c.position = position)
            }
            Command::PairShow => {
                self.pairing.open_shown_code();
                self.changed();
                self.pairing_reply()
            }
            Command::PairEnter { code } => match PairingCode::parse(&code) {
                Ok(code) => {
                    self.pairing.open_entered_code(&code, None);
                    self.changed();
                    self.pairing_reply()
                }
                Err(e) => error(e),
            },
            Command::PairCancel => {
                self.pairing.cancel();
                self.changed();
                self.pairing_reply()
            }
            Command::Pairing => self.pairing_reply(),
            Command::Restart => {
                self.restart.notify_one();
                Reply::Done
            }
            // The connection, not the node, keeps subscriptions; see
            // `transport::serve_connection`.
            Command::Subscribe => Reply::Subscribed,
            Command::UpdateApply => match crate::update::agent::global() {
                Some(agent) => match agent.apply_ready() {
                    Ok(_) => {
                        self.changed();
                        Reply::Done
                    }
                    Err(e) => error(e),
                },
                None => Reply::Error {
                    message: "updates are not running on this node".into(),
                },
            },
            Command::Forget { peer } => self.forget(peer),
            Command::SetName { name } => self.rename(&name),
            Command::Share {
                peer,
                target,
                action,
            } => match crate::share_sink::share(&peer, &target, action) {
                Ok(()) => Reply::Done,
                Err(e) => error(e),
            },
            Command::SetStreamVolume {
                stream,
                volume,
                muted,
            } => {
                if self.mixer.set(stream, volume, muted) {
                    self.changed();
                    Reply::Done
                } else {
                    error(format!("no stream {stream} is playing here"))
                }
            }
        }
    }

    fn pairing_reply(&self) -> Reply {
        Reply::Pairing {
            state: self.pairing.state(),
            last: self.pairing.last_outcome(),
        }
    }
}

fn error(e: impl std::fmt::Display) -> Reply {
    Reply::Error {
        message: e.to_string(),
    }
}

/// Which installed version this process is, read off its own path: a stable
/// copy has already handed over to `versions/<v>/`, so the path names it.
fn running_install_version() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    crate::update::layout::InstallLayout::for_user()
        .ok()?
        .version_of(&exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &std::path::Path, overrides: Overrides) -> NodeState {
        state_applying(dir, overrides, Arc::new(|_: &Config, _: &Config| {}))
    }

    fn state_applying(dir: &std::path::Path, overrides: Overrides, applier: Applier) -> NodeState {
        let identity = crate::NodeIdentity {
            peer_id: PeerId::new_v4(),
            display_name: "desk".into(),
        };
        NodeState::new(
            &identity,
            dir.join("config.toml"),
            overrides,
            Config::default(),
            PairingWindow::closed(),
            applier,
        )
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cursio-control-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_command_and_reply_survives_the_wire() {
        let commands = [
            Command::Ping,
            Command::Status,
            Command::SetModule {
                module: ModuleSwitch::RemoteScreen,
                enabled: true,
            },
            Command::SetPosition {
                position: Position::new(-1, 2),
            },
            Command::PairEnter {
                code: "ABCD-EFGH".into(),
            },
            Command::SetConfig {
                config: Config::default(),
            },
            Command::UpdateApply,
            Command::Forget {
                peer: PeerId::from_u128(3),
            },
            Command::SetStreamVolume {
                stream: 4,
                volume: 0.5,
                muted: true,
            },
        ];
        for (id, command) in commands.into_iter().enumerate() {
            let request = Request {
                id: id as u64,
                command,
            };
            let line = serde_json::to_string(&request).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        }
        let reply = Response {
            id: 9,
            reply: Reply::Saved {
                pinned: vec!["position".into()],
                restart_needed: vec!["port".into()],
            },
        };
        let line = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&line).unwrap(), reply);
    }

    #[test]
    fn a_stream_volume_reaches_the_engines_mixer_and_the_status() {
        let dir = temp_dir("volume");
        let node = state(&dir, Overrides::default());
        let refused = node.handle(Command::SetStreamVolume {
            stream: 7,
            volume: 0.5,
            muted: false,
        });
        assert!(matches!(refused, Reply::Error { .. }), "got {refused:?}");

        let engines = node.mixer();
        engines.add(7, "Nocturne", "Firefox");
        let reply = node.handle(Command::SetStreamVolume {
            stream: 7,
            volume: 0.5,
            muted: false,
        });
        assert_eq!(reply, Reply::Done);
        assert_eq!(engines.level(7), 0.5);
        assert_eq!(node.status().streams[0].volume, 0.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_a_machine_that_was_never_paired_is_refused_and_leaves_the_mesh_alone() {
        let dir = temp_dir("forget");
        let node = state(&dir, Overrides::default());
        let told = Arc::new(Mutex::new(Vec::new()));
        let seen = told.clone();
        node.set_on_forget(Arc::new(move |peer| seen.lock().unwrap().push(peer)));

        let reply = node.handle(Command::Forget {
            peer: PeerId::new_v4(),
        });

        assert!(matches!(reply, Reply::Error { .. }), "got {reply:?}");
        assert!(told.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_module_toggle_is_written_to_the_file_and_a_pinned_one_says_so() {
        let dir = temp_dir("toggle");
        let node = state(
            &dir,
            Overrides {
                audio: Some(true),
                ..Overrides::default()
            },
        );
        let Reply::Saved { pinned, .. } = node.handle(Command::SetModule {
            module: ModuleSwitch::Usb,
            enabled: true,
        }) else {
            panic!("expected saved");
        };
        assert!(pinned.is_empty());
        assert!(Config::load(&dir.join("config.toml")).unwrap().modules.usb);

        let Reply::Saved { pinned, .. } = node.handle(Command::SetModule {
            module: ModuleSwitch::Audio,
            enabled: false,
        }) else {
            panic!("expected saved");
        };
        assert_eq!(pinned, vec!["modules.audio".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_toggle_is_applied_before_the_reply_and_only_a_restart_only_setting_is_named() {
        let dir = temp_dir("apply");
        let applied = Arc::new(Mutex::new(Vec::new()));
        let seen = applied.clone();
        let node = state_applying(
            &dir,
            Overrides::default(),
            Arc::new(move |_: &Config, next: &Config| seen.lock().unwrap().push(next.clone())),
        );
        let Reply::Saved { restart_needed, .. } = node.handle(Command::SetModule {
            module: ModuleSwitch::Files,
            enabled: false,
        }) else {
            panic!("expected saved");
        };
        assert!(restart_needed.is_empty());
        assert!(!applied.lock().unwrap()[0].modules.files);
        assert!(!node.status().modules.files);

        let Reply::Saved { restart_needed, .. } = node.handle(Command::SetModule {
            module: ModuleSwitch::RemoteScreen,
            enabled: true,
        }) else {
            panic!("expected saved");
        };
        assert_eq!(restart_needed, vec!["modules.remote_screen".to_owned()]);
        assert_eq!(node.status().restart_needed, restart_needed);

        // The watcher reading the same file again applies nothing twice.
        node.apply_file().unwrap();
        assert_eq!(applied.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shown_code_is_reported_and_a_bad_entered_one_is_refused() {
        let dir = temp_dir("pair");
        let node = state(&dir, Overrides::default());
        let Reply::Pairing { state: Some(s), .. } = node.handle(Command::PairShow) else {
            panic!("expected an open window");
        };
        assert!(s.code.is_some());
        assert_eq!(node.status().pairing, Some(s));
        assert!(matches!(
            node.handle(Command::PairEnter {
                code: "nope".into()
            }),
            Reply::Error { .. }
        ));
        node.handle(Command::PairCancel);
        assert_eq!(node.status().pairing, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_linked_peer_shows_online_with_what_it_negotiated() {
        let dir = temp_dir("peers");
        let node = state(&dir, Overrides::default());
        let (known, fresh) = (PeerId::new_v4(), PeerId::new_v4());
        node.set_known(vec![KnownPeerView {
            id: known,
            name: "laptop".into(),
            presence: Presence::Offline,
        }]);
        node.link_up(
            fresh,
            "phone",
            LinkView {
                position: Position::new(1, 0),
                device_kind: DeviceKind::Phone,
                status: NodeStatus {
                    battery_percent: Some(55),
                    ..NodeStatus::unknown()
                },
                modules: vec!["clipboard".into()],
                skipped: vec![],
                since: SystemTime::now(),
            },
        );
        let peers = node.status().peers;
        assert_eq!(peers.len(), 2);
        let phone = peers.iter().find(|p| p.id == fresh).unwrap();
        assert_eq!(phone.presence, Presence::Online);
        assert_eq!(
            phone.link.as_ref().unwrap().status.battery_percent,
            Some(55)
        );
        node.link_down(&fresh);
        assert_eq!(node.status().peers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_names_the_process_and_build_an_update_would_replace() {
        let dir = temp_dir("build");
        let node = state(&dir, Overrides::default()).status().node;
        assert_eq!(node.pid, std::process::id());
        assert_eq!(node.build, env!("CARGO_PKG_VERSION"));
        // A test binary lives in target/, not in an install's versions/.
        assert_eq!(node.installed_version, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_status_from_a_node_without_update_state_still_reads() {
        let dir = temp_dir("v4");
        let status = state(&dir, Overrides::default()).status();
        let mut json = serde_json::to_value(&status).unwrap();
        json.as_object_mut().unwrap().remove("update");
        let read: Status = serde_json::from_value(json).unwrap();
        assert_eq!(read.update, crate::update::agent::UpdateStatus::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
