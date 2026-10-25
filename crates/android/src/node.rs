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
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cursio_core::audio_gate::MicGate;
use cursio_core::audio_session::AudioFactory;
use cursio_core::config::{Config, Overrides};
use cursio_core::control::{LinkView, ModuleSwitch, NodeState, Presence};
use cursio_core::live::Shared as LiveShared;
use cursio_core::mesh::SharedRegistry;
use cursio_core::module::ModuleRegistry;
use cursio_core::node_status::NodeStatusMonitor;
use cursio_core::pairing_window::PairingWindow;
use cursio_core::registry::PeerState;
use cursio_core::NodeIdentity;
use cursio_protocol::{PeerId, Position, ServiceOffer};
use cursio_transport::discovery;
use cursio_transport::pairing::{Role, TrustStore};
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use cursio_transport::Transport;

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

/// Listen, announce, dial, and run a session per link until asked to stop.
async fn serve(node: Node) -> anyhow::Result<()> {
    let Node {
        shared,
        control,
        pairing,
        identity,
        mic_gate,
        port,
    } = node;
    tracing::info!(id = %identity.peer_id, name = %identity.display_name, "node identity");

    let listener = TcpListenerHandle::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    let bound = listener.local_addr();
    *lock(&shared.bound) = Some(bound);
    tracing::info!(%bound, "listening for peers");

    // Held for its `Drop`, which withdraws the mDNS record: a phone that walks
    // out of the room should stop being advertised rather than linger as an
    // address nobody can reach.
    let _announcement = cursio_core::link::announce(&identity, bound.port())?;

    let registry = cursio_core::mesh::registry_from_trust(identity.peer_id)?;
    for peer in registry.borrow().all() {
        tracing::info!(peer = %peer.display_name, id = %peer.peer_id, "known peer");
    }

    let modules = ModuleRegistry::new()
        .with(Box::new(AudioFactory {
            enabled: true,
            render_device: LiveShared::new(None),
            mic_gate,
            mixer: control.mixer(),
        }))
        .with(Box::new(cursio_core::share_session::ShareFactory {
            sink: crate::share::AndroidSink::shared(),
        }))
        .with(Box::new(cursio_core::notify_session::NotifyFactory {
            display: None,
            actor: Some(crate::notify::AndroidActor::shared()),
            rings: false,
        }))
        .with(Box::new(cursio_core::calls_session::CallsFactory {
            actor: Some(crate::calls::AndroidCallActor::shared()),
            computer: false,
        }))
        .with(Box::new(cursio_core::messages_session::MessagesFactory {
            store: Some(crate::messages::AndroidStore::shared()),
            view: false,
        }))
        .with(Box::new(cursio_core::media_session::MediaFactory {
            plays: true,
            shows: true,
            shows_only_computers: true,
        }))
        // The phone's shared storage, for a computer to mount. Offered always:
        // what it holds is empty until the user grants storage access, and the
        // module's rescan offers the volumes the moment that happens.
        .with(Box::new(cursio_core::files_session::FilesFactory {
            enabled: true,
            share: cursio_core::live::Shared::new(cursio_files::SharePolicy::everything()),
            role: cursio_protocol::ModuleRole::Source,
        }))
        .with(Box::new(cursio_core::gallery_session::GalleryFactory {
            store: Some(crate::gallery::AndroidGallery::shared()),
            view: true,
        }))
        // The phone's screen, shown on a computer and driven from it.
        .with(Box::new(
            cursio_core::window_session::WindowFactory::default(),
        ));
    cursio_core::media_hub::hub().set_backend(crate::media::AndroidBackend::shared());
    cursio_core::media_hub::hub().set_display(crate::media::AndroidDisplay::shared());
    cursio_core::phone_screen::set_backend(crate::screen::AndroidScreen::shared());
    for advert in modules.advertise() {
        tracing::info!(module = %advert.id, version = advert.version, "module available");
    }
    // Input too: the touchpad drives a peer's cursor through the links.
    control.set_available(ModuleSwitch::available(&modules.available(), true));

    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let self_id = identity.peer_id;
    let browse_config = cursio_core::link::discovery_config();
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
        control,
        pairing,
        shared: Arc::clone(&shared),
        bluetooth: RefCell::new(cursio_core::bluetooth_link::Dialer::default()),
        unreachable_since: RefCell::new(HashMap::new()),
    });

    // Machines seen announcing themselves that this node does not trust yet.
    //
    // Kept from every sighting, not only those made while a window is open:
    // mDNS reports a machine once when it appears, so a computer already on
    // the LAN when the user typed its code was never heard of again, and the
    // phone never called it. The desktop that showed the code does not call
    // either — it, too, saw the phone before its window opened — so the phone
    // calls every stranger while its window is open, whatever `should_dial`
    // would say. A machine that is not pairing just hangs up, which is not
    // counted against the code.
    let mut strangers: HashMap<PeerId, Stranger> = HashMap::new();
    let mut sweep = tokio::time::interval(cursio_core::mesh::sweep_interval());
    loop {
        if !shared.running.load(Ordering::SeqCst) {
            tracing::info!("stop requested");
            return Ok(());
        }

        tokio::select! {
            // Checked on a timer rather than only on an event, so a node with
            // no peers still notices it was asked to stop.
            _ = tokio::time::sleep(STOP_CHECK_INTERVAL) => {}

            accepted = listener.accept() => match accepted {
                Ok(transport) => {
                    let addr = transport.peer_addr();
                    tracing::info!(%addr, "peer dialled in");
                    spawn_session(transport, Rc::clone(&ctx), Role::Responder, None, Some(addr));
                }
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            },

            Some(peer) = seen_rx.recv() => {
                ctx.control.saw(peer.peer_id, &peer.display_name, peer.addr);
                note_seen(&ctx.registry, &shared, &peer);
                if !ctx.registry.borrow().is_known(&peer.peer_id) {
                    strangers.insert(peer.peer_id, Stranger {
                        name: peer.display_name.clone(),
                        addr: peer.addr,
                        called: None,
                    });
                }
            }

            _ = sweep.tick() => {
                publish(&ctx);
                strangers.retain(|id, _| !ctx.registry.borrow().is_known(id));
                if ctx.pairing.is_open() {
                    let now = std::time::Instant::now();
                    for stranger in strangers.values_mut() {
                        if stranger.called.is_some_and(|at| now.duration_since(at) < STRANGER_RECALL) {
                            continue;
                        }
                        stranger.called = Some(now);
                        match TcpTransport::connect(stranger.addr).await {
                            Ok(t) => {
                                tracing::info!(peer = %stranger.name, addr = %stranger.addr, "dialled an unpaired machine to pair with it");
                                let addr = Some(stranger.addr);
                                spawn_session(t, Rc::clone(&ctx), Role::Initiator, None, addr);
                            }
                            Err(e) => tracing::warn!(peer = %stranger.name, error = %e, "could not reach the machine to pair with"),
                        }
                    }
                } else {
                    for stranger in strangers.values_mut() {
                        stranger.called = None;
                    }
                }
                let targets = cursio_core::mesh::next_dial_targets(self_id, &ctx.registry);
                for (target, dialled) in cursio_core::mesh::dial(targets).await {
                    match dialled {
                        Ok(t) => {
                            tracing::info!(peer = %target.display_name, addr = %target.addr, "dialled peer");
                            if let Ok(mut store) = TrustStore::load() {
                                let _ = store.remember_addr(&target.peer_id, target.addr);
                            }
                            let addr = Some(target.addr);
                            spawn_session(t, Rc::clone(&ctx), Role::Initiator, Some(target.peer_id), addr);
                        }
                        Err(e) => {
                            tracing::debug!(peer = %target.display_name, error = %e, "dial failed; will retry");
                            ctx.registry.borrow_mut().mark_offline(target.peer_id);
                        }
                    }
                }
                dial_bluetooth(&ctx).await;
            }
        }
    }
}

/// A machine on the LAN this phone has not paired with.
struct Stranger {
    name: String,
    addr: SocketAddr,
    /// When it was last called during the open window.
    called: Option<std::time::Instant>,
}

/// How long a stranger that did not pair is left before being called again
/// while the window stays open: long enough not to hammer a machine that is
/// not pairing, short enough that one that opens its window is reached soon.
const STRANGER_RECALL: std::time::Duration = std::time::Duration::from_secs(10);

/// How often the loop wakes to notice a stop request.
///
/// Short enough that stopping feels immediate to a user closing the app, long
/// enough to be free: this wakes a parked thread, and on a phone that is a
/// radio and a CPU that would rather stay asleep.
const STOP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

fn spawn_session<T: Transport + 'static>(
    transport: T,
    ctx: Rc<NodeCtx>,
    role: Role,
    expected: Option<PeerId>,
    addr: Option<SocketAddr>,
) {
    tokio::task::spawn_local(async move {
        if let Some(id) = expected {
            ctx.registry.borrow_mut().set_state(id, PeerState::Online);
        }
        let who = std::cell::Cell::new(expected);
        let hold = LinkHold::default();
        let result = tokio::select! {
            result = run_link(transport, &ctx, role, addr, &who, &hold) => result,
            () = hold.superseded.notified() => {
                tracing::info!("a better link to this computer came up; closing this one");
                Ok(())
            }
        };
        match &result {
            Ok(()) => tracing::info!("peer session ended"),
            Err(e) => tracing::warn!(error = %e, "peer session ended with error"),
        }
        end_link(&ctx, who.get(), &hold);
        publish(&ctx);
    });
}

/// A session's hold on its peer's one link; see
/// [`PeerRegistry::claim_link`](cursio_core::registry::PeerRegistry::claim_link).
#[derive(Default)]
struct LinkHold {
    token: std::cell::Cell<Option<u64>>,
    /// Woken when a better link to the same computer replaces this one.
    superseded: Rc<tokio::sync::Notify>,
    /// This connection joined a running session, which now owns it.
    joined: std::cell::Cell<bool>,
    /// The session this connection started, ended with it.
    session: RefCell<Option<cursio_transport::resume::ResumeHandle>>,
}

/// Take the computer offline after a session to it ended — unless another
/// link to it is up, which is then its link.
fn end_link(ctx: &NodeCtx, who: Option<PeerId>, hold: &LinkHold) {
    if hold.joined.get() {
        return;
    }
    let Some(id) = who else { return };
    if let Some(session) = hold.session.borrow_mut().take() {
        session.end("the session ended");
        ctx.registry.borrow_mut().session_ended(id, session.tag());
    }
    if ctx.registry.borrow_mut().release_link(id, hold.token.get()) {
        ctx.registry.borrow_mut().mark_offline(id);
        ctx.control.link_down(&id);
    }
}

/// Greet, pair if this is a first meeting, secure, and run the session.
async fn run_link<T: Transport + 'static>(
    transport: T,
    ctx: &NodeCtx,
    role: Role,
    // `None` over Bluetooth, which has no address the network could dial.
    addr: Option<SocketAddr>,
    who: &std::cell::Cell<Option<PeerId>>,
    hold: &LinkHold,
) -> anyhow::Result<()> {
    let password = ctx.pairing.password();
    let plan = cursio_core::link::Handshake {
        identity: &ctx.identity,
        password: &password,
        role,
        registry: &ctx.modules,
        position: PHONE_POSITION,
        service: ServiceOffer::none(),
        status: ctx.status.borrow().current(),
        // The touchpad: a phone has no capture, but its touch surface is input
        // for whichever peer it is pointed at.
        sends_input: true,
        resumable: ctx.registry.borrow().resumable_tags(),
    };
    let path = cursio_core::control::LinkPath::of(transport.kind());
    let identified = cursio_core::link::identify(transport, &plan).await?;
    let peer_id = identified.peer.peer_id;
    let peer_name = identified.peer.display_name.clone();
    let negotiated = identified.negotiated().clone();
    who.set(Some(peer_id));
    let greeted =
        ctx.registry
            .borrow_mut()
            .greeted(peer_id, &identified.peer.resumable, &plan.resumable);
    if let cursio_core::registry::Greeted::Join(session) = greeted {
        hold.joined.set(true);
        ctx.registry
            .borrow_mut()
            .set_state(peer_id, PeerState::Online);
        tracing::info!(peer = %peer_name, "the computer called back; resuming the session");
        return cursio_core::link::join(identified, &plan, &session).await;
    }
    let shape = cursio_core::registry::LinkShape {
        dialled: matches!(role, Role::Initiator),
        path,
    };
    let claimed = ctx
        .registry
        .borrow_mut()
        .claim_link(peer_id, shape, hold.superseded.clone());
    match claimed {
        Some(token) => hold.token.set(Some(token)),
        None => {
            tracing::info!(peer = %peer_name, ?shape, "already linked to this computer the way both sides keep; closing this second link");
            return Ok(());
        }
    }
    if ctx.registry.borrow().is_known(&peer_id) {
        ctx.registry
            .borrow_mut()
            .set_state(peer_id, PeerState::Online);
        tracing::info!(peer = %peer_name, "peer online");
    } else if ctx.pairing.is_open() {
        tracing::info!(peer = %peer_name, "unknown machine; pairing it");
    } else {
        tracing::warn!(peer = %peer_name, "untrusted machine; not admitted");
    }

    let (link, paired_now) = ctx.pairing.secure(identified, &plan).await?;
    if paired_now {
        // The desktop waits for the next restart or for the peer to call; a
        // phone the user just paired should show and dial the machine at once.
        ctx.registry
            .borrow_mut()
            .admit(peer_id, peer_name.clone(), addr);
        ctx.registry
            .borrow_mut()
            .set_state(peer_id, PeerState::Online);
        tracing::info!(peer = %peer_name, "paired");
    }
    ctx.registry
        .borrow_mut()
        .hold_session(peer_id, link.resume.clone());
    *hold.session.borrow_mut() = Some(link.resume.clone());
    let peer = link.peer.clone();
    let (skipped, inapplicable) = cursio_core::control::link_skips(&negotiated, peer.device_kind);
    ctx.control.link_up(
        peer.peer_id,
        &peer.display_name,
        LinkView {
            position: peer.position,
            device_kind: peer.device_kind,
            status: peer.status,
            modules: negotiated.agreed.iter().map(|id| id.to_string()).collect(),
            skipped,
            inapplicable,
            since: std::time::SystemTime::now(),
            path,
            path_since: None,
        },
    );
    publish(ctx);

    // A phone never takes the cursor, so `CursorSetup` leaves this side
    // dormant for the life of the link, and a dormant session forwards its
    // capture channel to the peer: the touchpad is just a writer on it.
    //
    // Offered to the touchpad only when the peer has a cursor to move. Another
    // phone has none — it injects nothing — so listing it made a target whose
    // strokes went nowhere.
    let (input_tx, mut input_rx) = cursio_core::session::input_channel();
    if link.cursor.peer_takes_cursor {
        lock(&ctx.shared.inputs).insert(peer.peer_id, (peer.display_name.clone(), input_tx));
    }
    let mut status_cursor = ctx.status.borrow().cursor();
    let result = cursio_core::session::run_session_live(
        link.transport,
        link.cursor,
        Some(&mut input_rx),
        |_event| {},
        None,
        |_ownership| {},
        link.modules,
        || ctx.status.borrow_mut().poll(&mut status_cursor),
        None,
        Some(peer.peer_id),
    )
    .await;
    lock(&ctx.shared.inputs).remove(&peer.peer_id);
    match result {
        Ok(()) | Err(cursio_core::session::SessionError::PeerLeft) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Open Bluetooth links to paired computers the network has not reached for
/// a while; see [`crate::bt`].
async fn dial_bluetooth(ctx: &Rc<NodeCtx>) {
    // A phone's radio clashes with nothing the core knows of, so only the
    // person's own "off" holds it back.
    if ctx.control.running().bluetooth.mesh == cursio_core::config::BluetoothMesh::Off {
        return;
    }
    let now = std::time::Instant::now();
    let offline: Vec<PeerId> = ctx
        .registry
        .borrow()
        .all()
        .filter(|peer| !matches!(peer.state, PeerState::Online | PeerState::Connecting))
        .map(|peer| peer.peer_id)
        .collect();
    let gone_for = {
        let mut since = ctx.unreachable_since.borrow_mut();
        since.retain(|id, _| offline.contains(id));
        for id in &offline {
            since.entry(*id).or_insert(now);
        }
        since.values().map(|at| now.duration_since(*at)).max()
    };
    let standby = ctx.standby() && ctx.registry.borrow().wants_bluetooth_standby();
    if gone_for.is_none() && !standby {
        return;
    }
    let paired = tokio::task::spawn_blocking(crate::bt::paired_computers)
        .await
        .unwrap_or_default();
    let due = ctx
        .bluetooth
        .borrow_mut()
        .due(&paired, gone_for, standby, now);
    for device in due {
        let address = device.address.clone();
        let opened = tokio::task::spawn_blocking(move || crate::bt::connect(&address)).await;
        let fd = match opened {
            Ok(Ok(fd)) => fd,
            Ok(Err(why)) => {
                tracing::debug!(device = %device.name, %why, "no mesh over bluetooth to this device");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "the bluetooth dial did not finish");
                continue;
            }
        };
        let transport = match cursio_core::bluetooth_link::transport(fd) {
            Ok(transport) => transport,
            Err(e) => {
                tracing::warn!(error = %e, "cannot use the bluetooth link");
                continue;
            }
        };
        tracing::info!(device = %device.name, "reached a computer over bluetooth");
        ctx.bluetooth.borrow_mut().set_linked(&device.address, true);
        spawn_bluetooth_session(transport, Rc::clone(ctx), device.address);
    }
}

/// A session over Bluetooth that ends itself once the network reaches the
/// same computer again, so the ordinary dial takes over at network speed.
fn spawn_bluetooth_session<T: Transport + 'static>(
    transport: T,
    ctx: Rc<NodeCtx>,
    address: String,
) {
    tokio::task::spawn_local(async move {
        let who = std::cell::Cell::new(None);
        let hold = LinkHold::default();
        // The session this connection starts moves to the network when the
        // computer calls back there; the Bluetooth path is then let go of,
        // not the session.
        let ended = tokio::select! {
            result = run_link(transport, &ctx, Role::Initiator, None, &who, &hold) => result,
            () = leave_bluetooth_once_the_network_carries(&hold, &address, &ctx) => Ok(()),
            () = hold.superseded.notified() => {
                tracing::info!("the network link to this computer is up; leaving bluetooth");
                Ok(())
            }
        };
        if let Err(e) = &ended {
            tracing::warn!(error = %e, "bluetooth session ended with error");
        }
        ctx.bluetooth.borrow_mut().set_linked(&address, false);
        end_link(&ctx, who.get(), &hold);
        publish(&ctx);
    });
}

/// Let the Bluetooth path go once a network path carries the session, which
/// happens when either side dials over the network again and the connection
/// joins it. Never resolves: the session runs on in `run_link`. Kept as a
/// standby instead when the user asked for one.
async fn leave_bluetooth_once_the_network_carries(hold: &LinkHold, address: &str, ctx: &NodeCtx) {
    let mut released = false;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if released {
            continue;
        }
        let Some(session) = hold.session.borrow().clone() else {
            continue;
        };
        let paths = session.paths();
        if paths.network && paths.bluetooth && !ctx.standby() {
            tracing::info!("the network carries the session again; leaving bluetooth");
            session.release(
                cursio_transport::resume::PathKind::Bluetooth,
                "the network carries the session",
            );
            ctx.bluetooth.borrow_mut().set_linked(address, false);
            released = true;
        }
    }
}

/// Hand the control state what the mesh loop knows.
fn publish(ctx: &NodeCtx) {
    ctx.control
        .publish_mesh(&ctx.registry, ctx.status.borrow().current());
}

/// Record a sighting, and keep the notification's list current.
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
