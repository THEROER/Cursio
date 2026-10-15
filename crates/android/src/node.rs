//! The node itself: one thread, one runtime, one mesh.
//!
//! Kotlin gets a handle it can start, stop and ask about. Everything the handle
//! wraps runs on a thread this module owns, because the session layer is
//! `!Send` by design (see `mesh.rs`) and a JVM thread is the wrong place to put
//! a Tokio runtime anyway — Android reuses and parks those, and a runtime
//! parked mid-session is a link the peer sees as dead.
//!
//! The phone is a full mesh member: it listens, dials the peers it trusts,
//! pairs while a window is open, and runs the same link bring-up and session
//! loop as the desktop (`link::identify`/`secure`, `session::run_session_live`).
//! What it does not have is the desktop's machine-wide input capture, so its
//! sessions carry no input of their own; the modules it runs are the ones with
//! an Android backend, which today is audio.
//!
//! The loop below is deliberately smaller than the CLI's `run_mesh` rather than
//! a copy of it: everything that loop adds — capture, the control socket,
//! AirPlay placement, self-update — is a desktop duty. The steps both share
//! live in `cursio-core`, which is what keeps the two from drifting.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cursio_core::audio_gate::MicGate;
use cursio_core::audio_session::{AudioFactory, LocalMixer};
use cursio_core::live::Shared as LiveShared;
use cursio_core::mesh::SharedRegistry;
use cursio_core::module::ModuleRegistry;
use cursio_core::node_status::NodeStatusMonitor;
use cursio_core::pairing_window::{PairingCode, PairingOutcome, PairingWindow};
use cursio_core::registry::PeerState;
use cursio_core::NodeIdentity;
use cursio_protocol::{PeerId, Position, ServiceOffer};
use cursio_transport::discovery;
use cursio_transport::pairing::{Role, TrustStore};
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use serde::Serialize;

/// Where the phone stands in the layout. It never takes the cursor
/// (`DeviceKind::Phone`), so no edge of it is ever crossed; the position only
/// has to be one no desktop is likely to claim.
const PHONE_POSITION: Position = Position::new(0, 1);

/// What the node can say about itself, read from arbitrary JVM threads.
///
/// Everything here is behind its own lock and updated from the node thread,
/// because the reader is a UI poll or a notification refresh and must never
/// wait on the session loop.
struct Shared {
    /// The address the listener actually bound, once it has.
    bound: Mutex<Option<SocketAddr>>,
    /// This node as peers see it: display name and peer id.
    me: Mutex<Option<(String, String)>>,
    /// Peers seen on the LAN, by display name, for the notification line.
    seen: Mutex<Vec<String>>,
    /// The known-peer set as of the last sweep, for the app.
    peers: Mutex<Vec<PeerView>>,
    /// The modules each linked peer agreed to.
    modules: Mutex<HashMap<PeerId, Vec<String>>>,
    /// Cleared to ask the node thread to wind down.
    running: AtomicBool,
    pairing: PairingWindow,
    mixer: LocalMixer,
    mic_gate: LiveShared<MicGate>,
    /// `mic_gate` as the user spelled it, to hand back to the app.
    mic_gate_text: Mutex<String>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            bound: Mutex::new(None),
            me: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
            peers: Mutex::new(Vec::new()),
            modules: Mutex::new(HashMap::new()),
            running: AtomicBool::new(false),
            pairing: PairingWindow::closed(),
            mixer: LocalMixer::default(),
            mic_gate: LiveShared::new(MicGate::Auto),
            mic_gate_text: Mutex::new("auto".into()),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct PeerView {
    id: String,
    name: String,
    presence: &'static str,
    modules: Vec<String>,
}

#[derive(Serialize)]
struct PairingView {
    kind: &'static str,
    code: Option<String>,
    expires_in_s: u64,
}

#[derive(Serialize)]
struct AudioView {
    mic_gate: String,
    streams: Vec<StreamView>,
}

#[derive(Serialize)]
struct StreamView {
    id: u32,
    peer: String,
    name: String,
    direction: &'static str,
    volume: f32,
    muted: bool,
    playing: bool,
}

#[derive(Serialize)]
struct StatusView {
    running: bool,
    name: Option<String>,
    peer_id: Option<String>,
    bound: Option<String>,
    peers: Vec<PeerView>,
    pairing: Option<PairingView>,
    last_pairing: Option<PairingOutcome>,
    audio: AudioView,
}

/// A running node, owned by the JNI layer.
pub struct NodeHandle {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NodeHandle {
    /// Bring the node up on its own thread.
    ///
    /// Returns once the thread is spawned, not once the listener is bound:
    /// binding can fail (a port in use, no network yet) and a service that
    /// waited for it would block Android's `onStartCommand` past its deadline.
    pub fn start(display_name: String, port: u16, mic_gate: &str) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared::default());
        shared.running.store(true, Ordering::SeqCst);
        if let Err(e) = set_mic_gate(&shared, mic_gate) {
            tracing::warn!(error = %e, "keeping the automatic mic gate");
        }

        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("cursio-node".into())
            .spawn(move || run(thread_shared, display_name, port))
            .map_err(|e| anyhow::anyhow!("cannot spawn the node thread: {e}"))?;

        Ok(Self {
            shared,
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

    /// One line describing what the node can see, for the service notification.
    pub fn status_line(&self) -> String {
        let bound = *lock(&self.shared.bound);
        let online: Vec<String> = lock(&self.shared.peers)
            .iter()
            .filter(|p| p.presence == "online")
            .map(|p| p.name.clone())
            .collect();
        let seen = lock(&self.shared.seen);
        match (bound, online.len()) {
            (None, _) => "starting".to_string(),
            (Some(_), 1) => format!("connected to {}", online[0]),
            (Some(_), n) if n > 1 => format!("connected to {}", online.join(", ")),
            (Some(addr), _) if seen.is_empty() => format!("listening on {addr}"),
            (Some(addr), _) => format!("listening on {addr} · sees {}", seen.join(", ")),
        }
    }

    /// Everything the app shows, as JSON.
    pub fn status_json(&self) -> String {
        let shared = &self.shared;
        let me = lock(&shared.me).clone();
        let pairing = shared.pairing.state().map(|s| PairingView {
            kind: match s.kind {
                cursio_core::pairing_window::PairingKind::ShownCode => "shown_code",
                cursio_core::pairing_window::PairingKind::EnteredCode => "entered_code",
                cursio_core::pairing_window::PairingKind::Password => "password",
            },
            code: s.code,
            expires_in_s: s
                .expires_at
                .duration_since(std::time::SystemTime::now())
                .map_or(0, |d| d.as_secs()),
        });
        let view = StatusView {
            running: shared.running.load(Ordering::SeqCst),
            name: me.as_ref().map(|(name, _)| name.clone()),
            peer_id: me.map(|(_, id)| id),
            bound: lock(&shared.bound).map(|a| a.to_string()),
            peers: lock(&shared.peers).clone(),
            pairing,
            last_pairing: shared.pairing.last_outcome(),
            audio: AudioView {
                mic_gate: lock(&shared.mic_gate_text).clone(),
                streams: shared
                    .mixer
                    .streams()
                    .into_iter()
                    .map(|s| StreamView {
                        id: s.id,
                        peer: s.peer,
                        name: s.name,
                        direction: "in",
                        volume: s.volume,
                        muted: s.muted,
                        playing: true,
                    })
                    .collect(),
            },
        };
        serde_json::to_string(&view).unwrap_or_else(|_| "{}".into())
    }

    /// Open the pairing window with a code read off another machine.
    pub fn enter_code(&self, code: &str) -> anyhow::Result<()> {
        let code = PairingCode::parse(code).map_err(|e| anyhow::anyhow!("{e}"))?;
        self.shared.pairing.open_entered_code(&code, None);
        tracing::info!("pairing window open with an entered code");
        Ok(())
    }

    /// Open the pairing window with a fresh code for this phone to show.
    pub fn show_code(&self) -> String {
        let state = self.shared.pairing.open_shown_code();
        tracing::info!("pairing window open with a shown code");
        state.code.unwrap_or_default()
    }

    pub fn cancel_pairing(&self) {
        self.shared.pairing.cancel();
    }

    /// Set one playing stream's volume and mute here.
    pub fn set_stream_volume(&self, id: u32, volume: f32, muted: bool) -> bool {
        self.shared.mixer.set(id, volume, muted)
    }

    pub fn set_mic_gate(&self, value: &str) -> anyhow::Result<()> {
        set_mic_gate(&self.shared, value)
    }
}

fn set_mic_gate(shared: &Shared, value: &str) -> anyhow::Result<()> {
    let gate: MicGate = value.parse()?;
    shared.mic_gate.set(gate);
    *lock(&shared.mic_gate_text) = value.trim().to_owned();
    tracing::info!(?gate, "mic gate set");
    Ok(())
}

/// The node thread: a current-thread runtime and the mesh's own `LocalSet`.
fn run(shared: Arc<Shared>, display_name: String, port: u16) {
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
    let local = tokio::task::LocalSet::new();
    if let Err(e) = local.block_on(&runtime, serve(Arc::clone(&shared), display_name, port)) {
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
    shared: Arc<Shared>,
}

/// Listen, announce, dial, and run a session per link until asked to stop.
async fn serve(shared: Arc<Shared>, display_name: String, port: u16) -> anyhow::Result<()> {
    let mut identity = NodeIdentity::load_or_create()?;
    // The phone's name is the user's, not the hostname: Android's hostname is
    // a meaningless `localhost` on most devices, and this is the label every
    // peer shows for this machine.
    if !display_name.is_empty() && identity.display_name != display_name {
        identity.display_name = display_name;
    }
    *lock(&shared.me) = Some((identity.display_name.clone(), identity.peer_id.to_string()));
    tracing::info!(id = %identity.peer_id, name = %identity.display_name, "node identity");

    let listener = TcpListenerHandle::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    let bound = listener.local_addr();
    *lock(&shared.bound) = Some(bound);
    tracing::info!(%bound, "listening for peers");

    let config = cursio_core::link::discovery_config();
    let instance = discovery::mdns_label(&identity.display_name, "cursio-node");
    // Held for its `Drop`, which withdraws the mDNS record: a phone that walks
    // out of the room should stop being advertised rather than linger as an
    // address nobody can reach.
    let _announcement = discovery::announce_with(
        identity.peer_id,
        &identity.display_name,
        &instance,
        bound.port(),
        &config,
    )?;
    tracing::info!("announced on the LAN as {instance}");

    let registry = cursio_core::mesh::registry_from_trust(identity.peer_id)?;
    for peer in registry.borrow().all() {
        tracing::info!(peer = %peer.display_name, id = %peer.peer_id, "known peer");
    }

    let modules = ModuleRegistry::new().with(Box::new(AudioFactory {
        enabled: true,
        render_device: LiveShared::new(None),
        mic_gate: shared.mic_gate.clone(),
        mixer: shared.mixer.clone(),
    }));
    for advert in modules.advertise() {
        tracing::info!(module = %advert.id, version = advert.version, "module available");
    }

    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let self_id = identity.peer_id;
    let browse_config = config.clone();
    // Discovery blocks its thread, so it gets one of its own rather than the
    // only thread this runtime has.
    std::thread::Builder::new()
        .name("cursio-browse".into())
        .spawn(move || {
            if let Err(e) = discovery::browse_with(self_id, &browse_config, |peer| {
                let _ = seen_tx.send(peer);
            }) {
                tracing::warn!(error = %e, "discovery ended");
            }
        })
        .map_err(|e| anyhow::anyhow!("cannot spawn the discovery thread: {e}"))?;

    let ctx = Rc::new(NodeCtx {
        identity,
        registry,
        modules,
        status: RefCell::new(NodeStatusMonitor::new()),
        shared,
    });

    // Machines seen announcing themselves that this node does not trust yet,
    // kept only while a pairing window is open. Emptied by the sweep that
    // dials them, so a refusal is retried on the next sighting, not hammered.
    let mut strangers: HashMap<PeerId, (String, SocketAddr)> = HashMap::new();
    let mut sweep = tokio::time::interval(cursio_core::mesh::sweep_interval());
    loop {
        if !ctx.shared.running.load(Ordering::SeqCst) {
            tracing::info!("stop requested");
            return Ok(());
        }

        tokio::select! {
            // Checked on a timer rather than only on an event, so a node with
            // no peers still notices it was asked to stop.
            _ = tokio::time::sleep(STOP_CHECK_INTERVAL) => {}

            accepted = listener.accept() => match accepted {
                Ok(transport) => {
                    tracing::info!(addr = %transport.peer_addr(), "peer dialled in");
                    spawn_session(transport, Rc::clone(&ctx), Role::Responder, None);
                }
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            },

            Some(peer) = seen_rx.recv() => {
                note_seen(&ctx.registry, &ctx.shared, &peer);
                let known = ctx.registry.borrow().is_known(&peer.peer_id);
                if !known && ctx.shared.pairing.is_open()
                    && strangers.insert(peer.peer_id, (peer.display_name.clone(), peer.addr)).is_none()
                {
                    tracing::info!(peer = %peer.display_name, addr = %peer.addr, "unpaired machine found while pairing");
                }
            }

            _ = sweep.tick() => {
                publish_peers(&ctx);
                if ctx.shared.pairing.is_open() {
                    for (peer_id, (name, addr)) in std::mem::take(&mut strangers) {
                        if !cursio_core::registry::should_dial(self_id, peer_id) {
                            continue;
                        }
                        match TcpTransport::connect(addr).await {
                            Ok(t) => {
                                tracing::info!(peer = %name, %addr, "dialled an unpaired machine to pair with it");
                                spawn_session(t, Rc::clone(&ctx), Role::Initiator, None);
                            }
                            Err(e) => tracing::warn!(peer = %name, error = %e, "could not reach the machine to pair with"),
                        }
                    }
                }
                for target in cursio_core::mesh::next_dial_targets(self_id, &ctx.registry) {
                    match TcpTransport::connect(target.addr).await {
                        Ok(t) => {
                            tracing::info!(peer = %target.display_name, addr = %target.addr, "dialled peer");
                            if let Ok(mut store) = TrustStore::load() {
                                let _ = store.remember_addr(&target.peer_id, target.addr);
                            }
                            spawn_session(t, Rc::clone(&ctx), Role::Initiator, Some(target.peer_id));
                        }
                        Err(e) => {
                            tracing::debug!(peer = %target.display_name, error = %e, "dial failed; will retry");
                            ctx.registry.borrow_mut().mark_offline(target.peer_id);
                        }
                    }
                }
            }
        }
    }
}

/// How often the loop wakes to notice a stop request.
///
/// Short enough that stopping feels immediate to a user closing the app, long
/// enough to be free: this wakes a parked thread, and on a phone that is a
/// radio and a CPU that would rather stay asleep.
const STOP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

fn spawn_session(transport: TcpTransport, ctx: Rc<NodeCtx>, role: Role, expected: Option<PeerId>) {
    tokio::task::spawn_local(async move {
        if let Some(id) = expected {
            ctx.registry.borrow_mut().set_state(id, PeerState::Online);
        }
        let addr = transport.peer_addr();
        let who = Rc::new(std::cell::Cell::new(expected));
        let result = run_link(transport, &ctx, role, addr, &who).await;
        match &result {
            Ok(()) => tracing::info!("peer session ended"),
            Err(e) => tracing::warn!(error = %e, "peer session ended with error"),
        }
        if let Some(id) = who.get() {
            ctx.registry.borrow_mut().mark_offline(id);
            lock(&ctx.shared.modules).remove(&id);
        }
        publish_peers(&ctx);
    });
}

/// Greet, pair if this is a first meeting, secure, and run the session.
async fn run_link(
    transport: TcpTransport,
    ctx: &NodeCtx,
    role: Role,
    addr: SocketAddr,
    who: &std::cell::Cell<Option<PeerId>>,
) -> anyhow::Result<()> {
    let password = ctx.shared.pairing.password();
    let plan = cursio_core::link::Handshake {
        identity: &ctx.identity,
        password: &password,
        role,
        registry: &ctx.modules,
        position: PHONE_POSITION,
        service: ServiceOffer::none(),
        status: ctx.status.borrow().current(),
        sends_input: false,
    };
    let identified = cursio_core::link::identify(transport, &plan).await?;
    let peer_id = identified.peer.peer_id;
    let peer_name = identified.peer.display_name.clone();
    who.set(Some(peer_id));
    if ctx.registry.borrow().is_known(&peer_id) {
        ctx.registry
            .borrow_mut()
            .set_state(peer_id, PeerState::Online);
        tracing::info!(peer = %peer_name, "peer online");
    } else if ctx.shared.pairing.is_open() {
        tracing::info!(peer = %peer_name, "unknown machine; pairing it");
    } else {
        tracing::warn!(peer = %peer_name, "untrusted machine; not admitted");
    }

    let stranger =
        !password.is_empty() && TrustStore::load().is_ok_and(|store| store.get(&peer_id).is_none());
    let link = match cursio_core::link::secure(identified, &plan).await {
        Ok(link) => link,
        Err(error) => {
            if stranger
                && matches!(
                    error.downcast_ref::<cursio_transport::pairing::PairingError>(),
                    Some(
                        cursio_transport::pairing::PairingError::ConfirmationFailed
                            | cursio_transport::pairing::PairingError::Rejected(_)
                    )
                )
            {
                ctx.shared.pairing.note_failure();
            }
            return Err(error);
        }
    };
    if stranger {
        ctx.shared.pairing.note_paired(&peer_name);
        // The desktop waits for the next restart or for the peer to call; a
        // phone the user just paired should show and dial the machine at once.
        ctx.registry
            .borrow_mut()
            .admit(peer_id, peer_name.clone(), Some(addr));
        ctx.registry
            .borrow_mut()
            .set_state(peer_id, PeerState::Online);
        tracing::info!(peer = %peer_name, "paired");
    }
    lock(&ctx.shared.modules).insert(
        peer_id,
        link.modules.iter().map(|m| m.id().to_string()).collect(),
    );
    publish_peers(ctx);

    let mut status_cursor = ctx.status.borrow().cursor();
    let result = cursio_core::session::run_session_live(
        link.transport,
        link.cursor,
        None,
        |_event| {},
        None,
        |_ownership| {},
        link.modules,
        || ctx.status.borrow_mut().poll(&mut status_cursor),
        None,
    )
    .await;
    match result {
        Ok(()) | Err(cursio_core::session::SessionError::PeerLeft) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Copy the known-peer set out for the app.
fn publish_peers(ctx: &NodeCtx) {
    let modules = lock(&ctx.shared.modules);
    let peers = ctx
        .registry
        .borrow()
        .all()
        .map(|peer| PeerView {
            id: peer.peer_id.to_string(),
            name: peer.display_name.clone(),
            presence: match peer.state {
                PeerState::Online => "online",
                PeerState::Connecting => "connecting",
                PeerState::Offline { .. } => "offline",
            },
            modules: modules.get(&peer.peer_id).cloned().unwrap_or_default(),
        })
        .collect();
    *lock(&ctx.shared.peers) = peers;
}

/// Record a sighting, and keep the notification's peer list current.
fn note_seen(registry: &SharedRegistry, shared: &Shared, peer: &discovery::DiscoveredPeer) {
    let known = registry
        .borrow_mut()
        .note_seen(peer.peer_id, &peer.display_name, peer.addr);
    if known {
        tracing::debug!(peer = %peer.display_name, addr = %peer.addr, "known peer seen");
    } else {
        tracing::debug!(peer = %peer.display_name, addr = %peer.addr, "untrusted machine seen");
    }

    let mut seen = lock(&shared.seen);
    if !seen.iter().any(|name| name == &peer.display_name) {
        seen.push(peer.display_name.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(bound: Option<&str>, seen: &[&str], online: &[&str]) -> NodeHandle {
        let shared = Arc::new(Shared::default());
        *lock(&shared.bound) = bound.map(|b| b.parse().expect("addr"));
        *lock(&shared.seen) = seen.iter().map(|s| (*s).to_string()).collect();
        *lock(&shared.peers) = online
            .iter()
            .map(|name| PeerView {
                id: String::new(),
                name: (*name).to_string(),
                presence: "online",
                modules: vec!["audio".into()],
            })
            .collect();
        NodeHandle {
            shared,
            thread: None,
        }
    }

    /// The notification is the only place a phone user sees whether this is
    /// working, so "no address yet" must read as starting rather than as a node
    /// that is up and sees nobody.
    #[test]
    fn an_unbound_node_reads_as_starting_not_as_alone() {
        assert_eq!(handle(None, &["Lightmoon"], &[]).status_line(), "starting");
    }

    #[test]
    fn a_linked_peer_outranks_what_the_lan_shows() {
        let addr = Some("192.168.1.7:46073");
        assert_eq!(
            handle(addr, &[], &[]).status_line(),
            "listening on 192.168.1.7:46073"
        );
        assert_eq!(
            handle(addr, &["Nocturne"], &[]).status_line(),
            "listening on 192.168.1.7:46073 · sees Nocturne"
        );
        assert_eq!(
            handle(addr, &["Nocturne"], &["Nocturne"]).status_line(),
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

    #[test]
    fn status_json_carries_what_the_app_reads() {
        let node = handle(Some("192.168.1.7:46073"), &[], &["Nocturne"]);
        let json: serde_json::Value = serde_json::from_str(&node.status_json()).expect("json");
        assert_eq!(json["bound"], "192.168.1.7:46073");
        assert_eq!(json["peers"][0]["name"], "Nocturne");
        assert_eq!(json["peers"][0]["presence"], "online");
        assert_eq!(json["audio"]["mic_gate"], "auto");
        assert!(json["pairing"].is_null());
    }

    #[test]
    fn a_mistyped_mic_gate_is_refused_and_the_old_one_kept() {
        let node = handle(None, &[], &[]);
        assert!(node.set_mic_gate("loud").is_err());
        assert_eq!(*lock(&node.shared.mic_gate_text), "auto");
        node.set_mic_gate("-55").expect("valid level");
        assert_eq!(node.shared.mic_gate.get(), MicGate::Fixed(-55.0));
    }
}
