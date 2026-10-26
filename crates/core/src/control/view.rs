//! How the node describes itself and its peers to a window: the status,
//! each peer and its link, and why a module is not running on one.

use crate::control::ModuleSwitch;
use crate::control::NodeState;
use crate::control::CONTROL_VERSION;
use crate::diagnostics::Problem;
use crate::metrics::LatencyReading;
use crate::pairing_window::PairingState;
use cursio_protocol::DeviceKind;
use cursio_protocol::NodeStatus;
use cursio_protocol::PeerId;
use cursio_protocol::Position;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::time::SystemTime;

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
    /// Every connected phone that reports calls, and its calls now.
    #[serde(default)]
    pub calls: Vec<crate::calls_hub::PhoneCalls>,
    /// Phones whose messages can be read now. Defaulted from a v9 node.
    #[serde(default)]
    pub messages: Vec<crate::messages_hub::MessagesPeer>,
    /// Machines heard announcing themselves on the LAN. Defaulted from a v10
    /// node.
    #[serde(default)]
    pub nearby: Vec<NearbyMachine>,
    /// Remote-screen views open here and what each can show. Defaulted from a
    /// v13 node.
    #[serde(default)]
    pub screens: Vec<crate::screen_views::ScreenView>,
    /// Machines whose galleries can be browsed now. Defaulted from a v14
    /// node.
    #[serde(default)]
    pub gallery: Vec<crate::gallery_hub::GalleryPeer>,
    /// This computer as a phone's Bluetooth headset, for the settings. `None`
    /// where the build carries no Bluetooth calls, and from a v16 node.
    #[serde(default)]
    pub call_setup: Option<crate::calls_hub::CallSetup>,
    /// This machine and every connected one on the shared desk. Defaulted
    /// from a v19 node.
    #[serde(default)]
    pub desk: crate::desk::DeskView,
}

/// A machine this node has heard announce itself on the LAN.
///
/// Heard, not reachable: mDNS says a machine exists and where, and says
/// nothing when it leaves, so `last_seen` is how old that knowledge is. What
/// it is for is the case pairing cannot explain on its own — an empty list
/// means no code typed anywhere will ever reach this node by discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NearbyMachine {
    pub id: PeerId,
    pub name: String,
    pub addr: std::net::SocketAddr,
    /// Already in this node's mesh, so a code would add nothing.
    pub paired: bool,
    pub last_seen: SystemTime,
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
    /// Modules this node offers that could run on this link but do not: one
    /// side has it switched off, or the peer's build is older. Worth showing.
    pub skipped: Vec<(String, String)>,
    /// Modules that have no meaning between these two machines — two phones
    /// with nothing to read from each other's SMS, or a duty a phone has no
    /// backend for. Kept apart from `skipped` so a window does not present
    /// them as something to fix. Empty from a v12 node.
    #[serde(default)]
    pub inapplicable: Vec<String>,
    pub since: SystemTime,
    /// What carries the link. Defaulted from a v11 node, which only had the
    /// network.
    #[serde(default)]
    pub path: LinkPath,
    /// When `path` last changed, so a window can say how long a machine has
    /// been held over Bluetooth. `None` while it is the one the link started
    /// on, and from an older node.
    #[serde(default)]
    pub path_since: Option<SystemTime>,
    /// The peer's part of the shared desk, as it last said. Empty from a v19
    /// node.
    #[serde(default)]
    pub layout: cursio_protocol::Layout,
}

/// What carries a link: the same mesh either way, at very different speeds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkPath {
    /// TCP over Wi-Fi or Ethernet.
    #[default]
    Network,
    /// An RFCOMM channel to a paired device, kept for when the network is not
    /// there: a few hundred kilobytes a second, enough for everything but
    /// screens.
    Bluetooth,
}

impl LinkPath {
    /// The path a transport's [`kind`](cursio_transport::Transport::kind) names.
    pub fn of(kind: &str) -> Self {
        if kind == cursio_transport::stream::BLUETOOTH {
            LinkPath::Bluetooth
        } else {
            LinkPath::Network
        }
    }
}

/// The modules a phone's node is built with (`crates/android/src/node.rs`).
/// A phone lacking any other module is not running an old build; it has no
/// backend for that duty, and listing it as missing would ask for an update
/// that cannot help.
pub(super) const PHONE_MODULES: [cursio_protocol::ModuleId; 9] = [
    cursio_protocol::ModuleId::AUDIO,
    cursio_protocol::ModuleId::WINDOW,
    cursio_protocol::ModuleId::INPUT,
    cursio_protocol::ModuleId::SHARE,
    cursio_protocol::ModuleId::NOTIFY,
    cursio_protocol::ModuleId::CALLS,
    cursio_protocol::ModuleId::MESSAGES,
    cursio_protocol::ModuleId::FILES,
    cursio_protocol::ModuleId::MEDIA,
];

/// Split what negotiation skipped into what is worth showing as off
/// ([`LinkView::skipped`], with the reason) and what has no meaning between
/// these two machines ([`LinkView::inapplicable`]).
pub fn link_skips(
    negotiated: &cursio_protocol::Negotiated,
    peer_kind: DeviceKind,
) -> (Vec<(String, String)>, Vec<String>) {
    use cursio_protocol::SkipReason;
    let phone = matches!(peer_kind, DeviceKind::Phone | DeviceKind::Tablet);
    let mut skipped = Vec::new();
    let mut inapplicable = Vec::new();
    for (id, why) in &negotiated.skipped {
        let pointless = match why {
            SkipReason::RoleMismatch { .. } => true,
            SkipReason::PeerLacks => phone && !PHONE_MODULES.contains(id),
            SkipReason::VersionMismatch { .. } => false,
        };
        if pointless {
            inapplicable.push(id.to_string());
        } else {
            skipped.push((id.to_string(), why.to_string()));
        }
    }
    (skipped, inapplicable)
}

/// A known peer as the mesh loop last published it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownPeerView {
    pub id: PeerId,
    pub name: String,
    pub presence: Presence,
}

impl NodeState {
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
                // The name the peer greeted this link with, not the one the
                // registry was seeded with: a phone paired under its model
                // number and renamed since kept showing the model number,
                // because only mDNS refreshed the registry and a hotspot's
                // multicast never reaches it.
                Some(p) => {
                    p.name = name.clone();
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
        let nearby = inner.nearby();
        let screens = inner.screens.clone();
        let linked: Vec<(PeerId, String, cursio_protocol::Layout)> = inner
            .links
            .iter()
            .map(|(id, (name, link))| (*id, name.clone(), link.layout.clone()))
            .collect();
        drop(inner);
        let running = self.running();
        let own = cursio_protocol::Layout {
            screens,
            place: running.layout.place,
            barriers: running.layout.barriers.clone(),
            offsets: running.layout.offsets.clone(),
        };
        let desk = crate::desk::view(
            &std::iter::once(crate::desk::Machine {
                id: node.id,
                name: &node.name,
                this: true,
                layout: &own,
            })
            .chain(
                linked
                    .iter()
                    .map(|(id, name, layout)| crate::desk::Machine {
                        id: *id,
                        name,
                        this: false,
                        layout,
                    }),
            )
            .collect::<Vec<_>>(),
        );
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
            calls: crate::calls_hub::hub().phones(),
            messages: crate::messages_hub::hub().peers(),
            nearby,
            screens: crate::screen_views::list(),
            gallery: crate::gallery_hub::hub().peers(),
            call_setup: crate::calls_hub::hub().setup(),
            desk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Overrides;
    use crate::control::test_support::state;
    use crate::control::test_support::temp_dir;
    use crate::control::NodeState;
    use cursio_protocol::DeviceKind;
    use cursio_protocol::NodeStatus;
    use cursio_protocol::PeerId;
    use cursio_protocol::Position;
    use std::time::SystemTime;

    use cursio_protocol::{ModuleId, ModuleRole, Negotiated, SkipReason};

    #[test]
    fn modules_with_nothing_to_exchange_between_two_machines_are_not_shown_as_off() {
        let negotiated = Negotiated {
            agreed: vec![ModuleId::AUDIO],
            skipped: vec![
                (
                    ModuleId::MESSAGES,
                    SkipReason::RoleMismatch {
                        local: ModuleRole::Source,
                        peer: ModuleRole::Source,
                    },
                ),
                (ModuleId::CLIPBOARD, SkipReason::PeerLacks),
            ],
        };

        let (skipped, inapplicable) = link_skips(&negotiated, DeviceKind::Phone);

        assert!(skipped.is_empty());
        assert_eq!(
            inapplicable,
            vec!["messages".to_string(), "clipboard".to_string()]
        );
    }

    #[test]
    fn a_module_an_older_build_lacks_stays_shown_as_off_with_its_reason() {
        let negotiated = Negotiated {
            agreed: vec![],
            skipped: vec![
                (ModuleId::CALLS, SkipReason::PeerLacks),
                (
                    ModuleId::AUDIO,
                    SkipReason::VersionMismatch { local: 2, peer: 1 },
                ),
            ],
        };

        let (skipped, inapplicable) = link_skips(&negotiated, DeviceKind::Desktop);

        assert_eq!(
            skipped.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(),
            ["calls", "audio"]
        );
        assert!(inapplicable.is_empty());
    }

    #[test]
    fn a_link_view_from_a_node_before_v12_reads_without_inapplicable_modules() {
        let json = r#"{"position":{"x":0,"y":0},"device_kind":"Phone","status":{},"modules":[],"skipped":[],"since":{"secs_since_epoch":0,"nanos_since_epoch":0}}"#;
        let view: LinkView = serde_json::from_str(json).unwrap();
        assert!(view.inapplicable.is_empty());
    }

    #[test]
    fn a_link_from_a_node_that_knew_only_the_network_reads_as_network() {
        let json = r#"{"position":{"x":1,"y":0},"device_kind":"Desktop","status":{},
            "modules":[],"skipped":[],"since":{"secs_since_epoch":0,"nanos_since_epoch":0}}"#;
        let link: LinkView = serde_json::from_str(json).unwrap();
        assert_eq!(link.path, LinkPath::Network);
    }

    #[test]
    fn names_a_bluetooth_transport_as_the_bluetooth_path() {
        assert_eq!(LinkPath::of("bluetooth"), LinkPath::Bluetooth);
        assert_eq!(LinkPath::of("tcp"), LinkPath::Network);
        assert_eq!(LinkPath::of("encrypted"), LinkPath::Network);
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
                inapplicable: vec![],
                since: SystemTime::now(),
                path: LinkPath::Network,
                path_since: None,
                layout: Default::default(),
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
    fn a_linked_peer_is_shown_by_the_name_it_greeted_with() {
        let dir = temp_dir("renamed");
        let node = state(&dir, Overrides::default());
        let phone = PeerId::new_v4();
        node.set_known(vec![KnownPeerView {
            id: phone,
            name: "2412DPC0AG".into(),
            presence: Presence::Offline,
        }]);
        node.link_up(
            phone,
            "POCO X7 Pro",
            LinkView {
                position: Position::new(1, 0),
                device_kind: DeviceKind::Phone,
                status: NodeStatus::unknown(),
                modules: vec![],
                skipped: vec![],
                inapplicable: vec![],
                since: SystemTime::now(),
                path: LinkPath::Network,
                path_since: None,
                layout: Default::default(),
            },
        );
        assert_eq!(node.status().peers[0].name, "POCO X7 Pro");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_that_moves_to_bluetooth_shows_as_on_bluetooth() {
        use cursio_transport::resume::Paths;

        let dir = temp_dir("paths");
        let node = state(&dir, Overrides::default());
        let peer = PeerId::new_v4();
        node.link_up(
            peer,
            "laptop",
            LinkView {
                position: Position::new(1, 0),
                device_kind: DeviceKind::Desktop,
                status: NodeStatus::unknown(),
                modules: vec![],
                skipped: vec![],
                inapplicable: vec![],
                since: SystemTime::now(),
                path: LinkPath::Network,
                path_since: None,
                layout: Default::default(),
            },
        );
        let shown = |node: &NodeState| node.status().peers[0].link.as_ref().unwrap().path;
        node.link_paths(
            &peer,
            Paths {
                network: false,
                bluetooth: true,
            },
        );
        assert_eq!(shown(&node), LinkPath::Bluetooth);
        assert!(
            node.status().peers[0]
                .link
                .as_ref()
                .unwrap()
                .path_since
                .is_some(),
            "a window can say since when"
        );
        node.link_paths(&peer, Paths::default());
        assert_eq!(
            shown(&node),
            LinkPath::Bluetooth,
            "waiting for a path keeps the last"
        );
        node.link_paths(
            &peer,
            Paths {
                network: true,
                bluetooth: true,
            },
        );
        assert_eq!(
            shown(&node),
            LinkPath::Network,
            "the network wins when both carry it"
        );
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
