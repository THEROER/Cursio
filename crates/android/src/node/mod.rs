//! The node itself: one thread, one runtime, one mesh.
//!
//! The app gets a handle it can start, stop and ask about. Everything the
//! handle wraps runs on a thread this module owns, because the session layer
//! is `!Send` by design (see `mesh.rs`) and a JVM thread is the wrong place to
//! put a Tokio runtime anyway — Android reuses and parks those, and a runtime
//! parked mid-session is a link the peer sees as dead.
//!
//! The phone is a full mesh member: it listens, dials the peers it trusts,
//! pairs while a window is open, and runs the same link bring-up and session
//! loop as the desktop (`link::identify`/`secure`, `session::run_session_live`).
//! What it does not have is the desktop's machine-wide input capture, so its
//! sessions carry no input of their own; the modules it runs are the ones with
//! an Android backend, which today is audio.
//!
//! It publishes into the same [`NodeState`] the desktop's control socket
//! serves, so the Cursio app draws a phone with the code that draws a
//! desktop: peers, links, pairing, settings and stream volumes all answer the
//! same [`Command`](cursio_core::control::Command)s. The app runs in this
//! process and calls [`NodeState::handle`] directly; there is no socket to
//! open on a phone, where the temp directory a desktop uses is not writable.
//!
//! The loop below is deliberately smaller than the CLI's `run_mesh` rather than
//! a copy of it: everything that loop adds — capture, AirPlay placement,
//! self-update — is a desktop duty. The steps both share live in
//! `cursio-core`, which is what keeps the two from drifting.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::node::serve::serve;
use cursio_core::audio_gate::MicGate;
use cursio_core::config::{Config, Overrides};
use cursio_core::control::{NodeState, Presence};
use cursio_core::live::Shared as LiveShared;
use cursio_core::mesh::SharedRegistry;
use cursio_core::module::ModuleRegistry;
use cursio_core::node_status::NodeStatusMonitor;
use cursio_core::pairing_window::PairingWindow;
use cursio_core::NodeIdentity;
use cursio_protocol::{PeerId, Position};

mod bluetooth;
mod link;
mod serve;

/// Where the phone stands in the layout. It never takes the cursor
/// (`DeviceKind::Phone`), so no edge of it is ever crossed; the position only
/// has to be one no desktop is likely to claim.
const PHONE_POSITION: Position = Position::new(0, 1);

/// What the notification line reads, from arbitrary JVM threads.
#[derive(Default)]
struct Shared {
    /// The address the listener actually bound, once it has.
    bound: Mutex<Option<SocketAddr>>,
    /// Peers seen on the LAN, by display name.
    seen: Mutex<Vec<String>>,
    /// Cleared to ask the node thread to wind down.
    running: AtomicBool,
    /// Each live link's input channel, by peer: what the touchpad writes to.
    /// A phone never holds the cursor, so its session forwards everything on
    /// this channel to the peer — a touch surface needs nothing more.
    inputs: Mutex<HashMap<PeerId, (String, cursio_core::session::InputSender)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A running node.
pub struct NodeHandle {
    shared: Arc<Shared>,
    control: NodeState,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NodeHandle {
    /// Bring the node up on its own thread.
    ///
    /// Returns once the thread is spawned, not once the listener is bound:
    /// binding can fail (a port in use, no network yet) and a service that
    /// waited for it would block Android's `onStartCommand` past its deadline.
    pub fn start(display_name: String, port: u16) -> anyhow::Result<Self> {
        let mut identity = NodeIdentity::load_or_create()?;
        // Android's hostname is a meaningless `localhost` on most devices, so a
        // phone that has never been named takes the one the app passes (the
        // device's own name). Only then: once the user renames it, the saved
        // name is the answer, and overwriting it at every start was why the
        // phone could not be renamed.
        if !display_name.is_empty()
            && cursio_core::identity::is_default_name(&identity.display_name)
        {
            identity.rename(&display_name)?;
        }
        let config_path = cursio_core::config::default_path()?;
        let config = Config::load(&config_path)?;
        let mic_gate = LiveShared::new(MicGate::from_config(config.audio.mic_gate.as_deref())?);

        let applier: cursio_core::control::Applier = {
            let mic_gate = mic_gate.clone();
            Arc::new(move |running: &Config, next: &Config| {
                let plan = cursio_core::live::plan(running, next);
                if let Some(gate) = &plan.mic_gate {
                    match MicGate::from_config(gate.as_deref()) {
                        Ok(gate) => mic_gate.set(gate),
                        Err(error) => tracing::warn!(%error, "keeping the running mic gate"),
                    }
                }
            })
        };
        // One window, shared: the app opens it through `control`, the mesh
        // loop reads it to decide whom to pair with.
        let pairing = PairingWindow::closed();
        let control = NodeState::new(
            &identity,
            config_path,
            Overrides::default(),
            config,
            pairing.clone(),
            applier,
        );

        let shared = Arc::new(Shared::default());
        shared.running.store(true, Ordering::SeqCst);
        let node = Node {
            shared: Arc::clone(&shared),
            control: control.clone(),
            pairing,
            identity,
            mic_gate,
            port,
        };
        let thread = std::thread::Builder::new()
            .name("cursio-node".into())
            .spawn(move || run(node))
            .map_err(|e| anyhow::anyhow!("cannot spawn the node thread: {e}"))?;

        Ok(Self {
            shared,
            control,
            thread: Some(thread),
        })
    }

    /// Ask the node to stop and wait for its thread.
    ///
    /// Waits rather than detaching so the listener is closed before this
    /// returns — Android restarts services eagerly, and the next instance
    /// would otherwise fail to bind a port the previous one still holds.
    pub fn stop(mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        tracing::info!("node stopped");
    }

    /// What the app talks to: the same state a desktop's control socket serves.
    pub fn control(&self) -> NodeState {
        self.control.clone()
    }

    /// The peers a touchpad can drive right now — every live link — by id
    /// and name.
    pub fn input_targets(&self) -> Vec<(PeerId, String)> {
        let mut targets: Vec<_> = lock(&self.shared.inputs)
            .iter()
            .map(|(id, (name, _))| (*id, name.clone()))
            .collect();
        targets.sort_by(|a, b| a.1.cmp(&b.1));
        targets
    }

    /// A channel into one peer's session, for the touchpad. `None` when that
    /// peer has no live link.
    pub fn input_to(&self, peer: &PeerId) -> Option<cursio_core::session::InputSender> {
        lock(&self.shared.inputs)
            .get(peer)
            .map(|(_, tx)| tx.clone())
    }

    /// One line describing what the node can see, for the service notification.
    pub fn status_line(&self) -> String {
        let bound = *lock(&self.shared.bound);
        let online: Vec<String> = self
            .control
            .status()
            .peers
            .into_iter()
            .filter(|p| p.presence == Presence::Online)
            .map(|p| p.name)
            .collect();
        let seen = lock(&self.shared.seen);
        status_line(bound, &online, &seen)
    }
}

fn status_line(bound: Option<SocketAddr>, online: &[String], seen: &[String]) -> String {
    match bound {
        None => "starting".to_string(),
        Some(_) if !online.is_empty() => format!("connected to {}", online.join(", ")),
        Some(addr) if seen.is_empty() => format!("listening on {addr}"),
        Some(addr) => format!("listening on {addr} · sees {}", seen.join(", ")),
    }
}

/// Everything the node thread is handed.
struct Node {
    shared: Arc<Shared>,
    control: NodeState,
    pairing: PairingWindow,
    identity: NodeIdentity,
    mic_gate: LiveShared<MicGate>,
    port: u16,
}

/// The node thread: a current-thread runtime and the mesh's own `LocalSet`.
fn run(node: Node) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(error = %e, "cannot build the node runtime");
            return;
        }
    };

    // Sessions share state via `Rc` deliberately, so they are spawned onto a
    // `LocalSet` rather than the multi-thread scheduler.
    let shared = Arc::clone(&node.shared);
    let local = tokio::task::LocalSet::new();
    if let Err(e) = local.block_on(&runtime, serve(node)) {
        tracing::error!(error = %e, "node stopped with an error");
    }
    shared.running.store(false, Ordering::SeqCst);
}

/// What every session on this node shares. Single-threaded, like the sessions.
struct NodeCtx {
    identity: NodeIdentity,
    registry: SharedRegistry,
    modules: ModuleRegistry,
    status: RefCell<NodeStatusMonitor>,
    control: NodeState,
    pairing: PairingWindow,
    shared: Arc<Shared>,
    /// When to open Bluetooth links, and to which devices.
    bluetooth: RefCell<cursio_core::bluetooth_link::Dialer>,
    /// Since when each known computer has been off the network.
    unreachable_since: RefCell<HashMap<PeerId, std::time::Instant>>,
}

impl NodeCtx {
    /// Whether the user asked for a Bluetooth standby beside the network.
    fn standby(&self) -> bool {
        self.control.running().bluetooth.standby
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::serve::note_seen;
    use cursio_transport::discovery;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    /// The notification is the only place a phone user sees whether this is
    /// working without opening the app, so "no address yet" must read as
    /// starting rather than as a node that is up and sees nobody.
    #[test]
    fn an_unbound_node_reads_as_starting_not_as_alone() {
        assert_eq!(status_line(None, &[], &names(&["Lightmoon"])), "starting");
    }

    #[test]
    fn a_linked_peer_outranks_what_the_lan_shows() {
        let addr = Some("192.168.1.7:46073".parse().expect("addr"));
        assert_eq!(
            status_line(addr, &[], &[]),
            "listening on 192.168.1.7:46073"
        );
        assert_eq!(
            status_line(addr, &[], &names(&["Nocturne"])),
            "listening on 192.168.1.7:46073 · sees Nocturne"
        );
        assert_eq!(
            status_line(addr, &names(&["Nocturne"]), &names(&["Nocturne"])),
            "connected to Nocturne"
        );
    }

    /// mDNS re-announces the same peer every few seconds and on every network
    /// change; listing each sighting would grow the line without end.
    #[test]
    fn a_peer_seen_repeatedly_is_listed_once() {
        let shared = Shared::default();
        let registry = cursio_core::mesh::registry_from_trust(PeerId::nil()).expect("registry");
        let peer = discovery::DiscoveredPeer {
            peer_id: PeerId::from_u128(1),
            display_name: "Lightmoon".into(),
            addr: "192.168.1.5:46073".parse().expect("addr"),
        };
        for _ in 0..3 {
            note_seen(&registry, &shared, &peer);
        }
        assert_eq!(lock(&shared.seen).len(), 1);
    }
}
