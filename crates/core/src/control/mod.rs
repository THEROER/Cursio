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
//! echoes, so a client may pipeline. There are no pushed events: status is a
//! few hundred bytes and a UI that polls it every second or two is simpler
//! than a subscription protocol, and cannot fall behind.
//!
//! ## Changing things goes through the config file
//!
//! A module toggle or a new grid position is written to `config.toml`, and the
//! node's own watcher restarts it (see [`crate::config`]). One path for every
//! change, whoever made it — the app, a text editor, a script — and the file
//! always says what is running. A field a command-line flag pins cannot be
//! changed that way, and the reply says so instead of pretending.

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
/// me" rather than misreading a field.
pub const CONTROL_VERSION: u32 = 1;

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
    /// The file was written; the node restarts on its own if that changed
    /// what runs. `pinned` names fields the change could not reach.
    Saved {
        pinned: Vec<String>,
    },
    Pairing {
        state: Option<PairingState>,
        last: Option<PairingOutcome>,
    },
    Done,
    Error {
        message: String,
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
    pub pinned: Vec<String>,
    pub config_path: PathBuf,
    pub latency: Vec<LatencyReading>,
    pub problems: Vec<Problem>,
    pub pairing: Option<PairingState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    pub id: PeerId,
    pub name: String,
    pub position: Position,
    pub status: NodeStatus,
    pub started_at: SystemTime,
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
    running: Config,
    pairing: PairingWindow,
    restart: Arc<tokio::sync::Notify>,
}

impl NodeState {
    pub fn new(
        identity: &crate::NodeIdentity,
        config_path: PathBuf,
        overrides: Overrides,
        running: Config,
        pairing: PairingWindow,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                node: NodeView {
                    id: identity.peer_id,
                    name: identity.display_name.clone(),
                    position: running.position,
                    status: NodeStatus::unknown(),
                    started_at: SystemTime::now(),
                },
                known: Vec::new(),
                links: BTreeMap::new(),
            })),
            config_path,
            overrides,
            running,
            pairing,
            restart: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set_local_status(&self, status: NodeStatus) {
        self.inner().node.status = status;
    }

    pub fn set_known(&self, known: Vec<KnownPeerView>) {
        self.inner().known = known;
    }

    pub fn link_up(&self, peer: PeerId, name: &str, link: LinkView) {
        crate::metrics::note_peer_status(peer, link.status);
        self.inner().links.insert(peer, (name.to_owned(), link));
    }

    pub fn link_down(&self, peer: &PeerId) {
        self.inner().links.remove(peer);
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
        Status {
            version: CONTROL_VERSION,
            node: inner.node.clone(),
            peers,
            modules: self.running.modules,
            pinned: self.pinned(),
            config_path: self.config_path.clone(),
            latency: crate::metrics::input_latency(),
            problems: crate::diagnostics::snapshot(),
            pairing: self.pairing.state(),
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
                self.pairing_reply()
            }
            Command::PairEnter { code } => match PairingCode::parse(&code) {
                Ok(code) => {
                    self.pairing.open_entered_code(&code, None);
                    self.pairing_reply()
                }
                Err(e) => error(e),
            },
            Command::PairCancel => {
                self.pairing.cancel();
                self.pairing_reply()
            }
            Command::Pairing => self.pairing_reply(),
            Command::Restart => {
                self.restart.notify_one();
                Reply::Done
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &std::path::Path, overrides: Overrides) -> NodeState {
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
            },
        };
        let line = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&line).unwrap(), reply);
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
        let Reply::Saved { pinned } = node.handle(Command::SetModule {
            module: ModuleSwitch::Usb,
            enabled: true,
        }) else {
            panic!("expected saved");
        };
        assert!(pinned.is_empty());
        assert!(Config::load(&dir.join("config.toml")).unwrap().modules.usb);

        let Reply::Saved { pinned } = node.handle(Command::SetModule {
            module: ModuleSwitch::Audio,
            enabled: false,
        }) else {
            panic!("expected saved");
        };
        assert_eq!(pinned, vec!["modules.audio".to_owned()]);
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
}
