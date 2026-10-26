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

mod arrange;
mod handle;
#[cfg(test)]
mod test_support;
mod view;
mod wire;

pub use arrange::Arranger;
pub use handle::{ask_gallery, handle_waiting};
pub use view::{
    link_skips, KnownPeerView, LinkPath, LinkView, NearbyMachine, NodeView, PeerView, Presence,
    Status,
};
pub use wire::{Command, ModuleSwitch, Reply, Request, Response, CONTROL_VERSION, PUSH_ID};

pub mod transport;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use cursio_protocol::{NodeStatus, PeerId, Position};

use crate::config::{Config, Overrides};
use crate::control::handle::error;
use crate::control::handle::running_install_version;
use crate::pairing_window::PairingWindow;

struct Inner {
    node: NodeView,
    known: Vec<KnownPeerView>,
    links: BTreeMap<PeerId, (String, LinkView)>,
    /// The name this process greets peers with, while it differs from the
    /// saved one: links carry a name from their greeting.
    greeted_as: Option<String>,
    available: Vec<ModuleSwitch>,
    /// Every machine discovery reported, by id: name, address, when.
    nearby: BTreeMap<PeerId, (String, std::net::SocketAddr, SystemTime)>,
    /// This machine's screens as last read.
    screens: Vec<cursio_protocol::Screen>,
}

impl Inner {
    /// Strangers first, since they are who a code is for, then by name.
    fn nearby(&self) -> Vec<NearbyMachine> {
        let mut machines: Vec<NearbyMachine> = self
            .nearby
            .iter()
            .map(|(id, (name, addr, last_seen))| NearbyMachine {
                id: *id,
                name: name.clone(),
                addr: *addr,
                paired: self.known.iter().any(|k| k.id == *id) || self.links.contains_key(id),
                last_seen: *last_seen,
            })
            .collect();
        machines.sort_by(|a, b| a.paired.cmp(&b.paired).then_with(|| a.name.cmp(&b.name)));
        machines
    }
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
    /// Carries an arrangement of a peer to its link; set by the binary.
    on_arrange: Arc<Mutex<Option<arrange::Arranger>>>,
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
                nearby: BTreeMap::new(),
                screens: Vec::new(),
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
            on_arrange: Arc::new(Mutex::new(None)),
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

    fn node_id(&self) -> PeerId {
        self.inner().node.id
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

    /// Publish this machine's status and every known peer as the registry
    /// now has them. The CLI's mesh loop and the phone's node both call it
    /// each sweep.
    pub fn publish_mesh(&self, registry: &crate::mesh::SharedRegistry, local: NodeStatus) {
        use crate::registry::PeerState;
        self.set_local_status(local);
        self.set_known(
            registry
                .borrow()
                .all()
                .map(|peer| KnownPeerView {
                    id: peer.peer_id,
                    name: peer.display_name.clone(),
                    presence: match peer.state {
                        PeerState::Online => Presence::Online,
                        PeerState::Connecting => Presence::Connecting,
                        PeerState::Offline { .. } => Presence::Offline,
                    },
                })
                .collect(),
        );
    }

    /// This machine's screens as just read; a change wakes subscribers.
    pub fn set_local_screens(&self, screens: Vec<cursio_protocol::Screen>) {
        let moved = std::mem::replace(&mut self.inner().screens, screens.clone()) != screens;
        if moved {
            self.changed();
        }
    }

    pub fn link_up(&self, peer: PeerId, name: &str, link: LinkView) {
        crate::metrics::note_peer_status(peer, link.status);
        self.inner().links.insert(peer, (name.to_owned(), link));
        self.changed();
    }

    /// The session to `peer` now runs over `paths`. A link's path was the
    /// kind of the connection that started it, so a session that moved to
    /// Bluetooth when the network went still showed as on the network — in
    /// the app, the GNOME menu and `cursio status` alike. With no path at
    /// all (the session waiting for one) the last one shown stays.
    pub fn link_paths(&self, peer: &PeerId, paths: cursio_transport::resume::Paths) {
        let path = if paths.network {
            LinkPath::Network
        } else if paths.bluetooth {
            LinkPath::Bluetooth
        } else {
            return;
        };
        let changed = match self.inner().links.get_mut(peer) {
            Some((_, link)) if link.path != path => {
                link.path = path;
                link.path_since = Some(SystemTime::now());
                true
            }
            _ => false,
        };
        if changed {
            self.changed();
        }
    }

    /// A running link renegotiated: the peer moved, or the modules changed.
    /// The rest of what the link reported stays, the peer's latest status
    /// included.
    pub fn link_reconfigured(
        &self,
        peer: &PeerId,
        position: Position,
        layout: cursio_protocol::Layout,
        modules: Vec<String>,
        (skipped, inapplicable): (Vec<(String, String)>, Vec<String>),
    ) {
        if let Some((_, link)) = self.inner().links.get_mut(peer) {
            link.position = position;
            link.layout = layout;
            link.modules = modules;
            link.skipped = skipped;
            link.inapplicable = inapplicable;
        }
        self.changed();
    }

    pub fn link_down(&self, peer: &PeerId) {
        self.inner().links.remove(peer);
        self.changed();
    }

    /// Discovery heard `peer` announce itself at `addr`.
    ///
    /// Subscribers are woken only when a machine is new or moved: mDNS
    /// repeats itself, and a repeat changes nothing a window shows but the age.
    pub fn saw(&self, peer: PeerId, name: &str, addr: std::net::SocketAddr) {
        let moved = {
            let mut inner = self.inner();
            let previous = inner
                .nearby
                .insert(peer, (name.to_owned(), addr, SystemTime::now()));
            previous.is_none_or(|(was_name, was_addr, _)| was_name != name || was_addr != addr)
        };
        if moved {
            self.changed();
        }
    }

    /// Resolves when a client asked for a restart.
    pub async fn restart_requested(&self) {
        self.restart.notified().await
    }
}
