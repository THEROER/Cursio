//! The mesh node: listen, discover, dial the trusted peers that are down,
//! and keep doing so for the life of the process.

use crate::input::make_persistent_capture;
use crate::input::SharedCapture;
use crate::peer_session::spawn_peer_session;
use crate::process::bind_mesh_port;
use cursio_core::node_status::NodeStatusMonitor;
use cursio_core::pairing_window::PairingWindow;
use cursio_core::NodeIdentity;
use cursio_protocol::{PeerId, ServiceOffer};
use cursio_transport::discovery;
use cursio_transport::pairing::{Role, TrustStore};
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use std::net::SocketAddr;

/// Run as a mesh node: listen and dial trusted peers simultaneously, forever.
///
/// This is *the* way to run Cursio — every node accepts and every node dials,
/// which is what "no server" means in practice. Peers come from the trust store,
/// so the set survives any of them being off; discovery only supplies their
/// current addresses.
///
/// Sessions are spawned on a [`tokio::task::LocalSet`], not `tokio::spawn`: the
/// input path shares its injector/cursor between closures via `Rc` and so isn't
/// `Send`. Keeping sessions single-threaded lets that stay exactly as it is.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_mesh(
    identity: NodeIdentity,
    port: u16,
    send_input: bool,
    live: cursio_core::live::LiveConfig,
    modules: cursio_core::module::ModuleRegistry,
    pairing: PairingWindow,
    offer: ServiceOffer,
    control: cursio_core::control::NodeState,
) -> anyhow::Result<()> {
    let NodeBase {
        listener,
        _announcement,
        capture,
        status,
    } = start_node(&identity, port, send_input).await?;

    // One registry for the whole process, shared by every session. Sessions are
    // single-threaded by design, so `Rc` is the right sharing primitive here.
    let modules = std::rc::Rc::new(modules);
    for advert in modules.advertise() {
        tracing::info!(module = %advert.id, version = advert.version, "module available");
    }

    let registry = cursio_core::mesh::registry_from_trust(identity.peer_id)?;
    for peer in registry.borrow().all() {
        tracing::info!(peer = %peer.display_name, id = %peer.peer_id, "known peer");
    }

    // Discovery runs on its own thread (the mdns callback is blocking) and
    // reports sightings over a channel. A sighting only ever *refreshes* a
    // trusted peer's address — an unknown machine announcing itself is ignored,
    // so joining the mesh stays an explicit, password-gated act.
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let self_id = identity.peer_id;
    std::thread::Builder::new()
        .name("cursio-mesh-discovery".into())
        .spawn(move || {
            let config = cursio_core::link::discovery_config();
            if let Err(e) = discovery::browse_with(self_id, &config, move |peer| {
                let _ = seen_tx.send(peer);
            }) {
                tracing::warn!(error = %e, "discovery ended");
            }
        })?;

    // Machines seen announcing themselves that this node does not trust yet.
    // Only ever filled while `--pair` holds a window open; emptied by the sweep
    // that dials them, so a machine that refuses is retried on the next
    // sighting rather than hammered every two seconds.
    let mut strangers: std::collections::HashMap<PeerId, (String, SocketAddr)> =
        std::collections::HashMap::new();
    if let Some(addr) = pairing.addr() {
        // An address given on the command line stands in for a sighting, for
        // the LAN where mDNS is blocked. The id is not known until the
        // handshake, so a nil one is used purely to key the map — `should_dial`
        // would otherwise decide the call, and a machine the user named
        // explicitly is one they want called.
        strangers.insert(PeerId::nil(), ("the machine you named".into(), addr));
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mut sweep = tokio::time::interval(cursio_core::mesh::sweep_interval());
            // Read once: the card does not change while the node runs.
            let radio_conflict = std::rc::Rc::new(cursio_core::bluetooth_link::radio_conflict());
            let decide = {
                let control = control.clone();
                let radio_conflict = radio_conflict.clone();
                std::rc::Rc::new(move || {
                    cursio_core::bluetooth_link::decide(
                        control.running().bluetooth.mesh,
                        radio_conflict.as_deref(),
                    )
                })
            };
            let mut decided = decide();
            cursio_core::bluetooth_link::report(&decided);
            let mut bluetooth = if decided.is_on() {
                cursio_core::bluetooth_link::listen()
            } else {
                tracing::info!(?decided, "the mesh does not use bluetooth on this machine");
                cursio_core::bluetooth_link::Listener::idle()
            };
            let mut listening = decided.is_on();
            let open_bluetooth = {
                let identity = identity.clone();
                let registry = registry.clone();
                let pairing = pairing.clone();
                let control = control.clone();
                let capture = capture.clone();
                let status = status.clone();
                let live = live.clone();
                let modules = modules.clone();
                std::rc::Rc::new(
                    move |transport: cursio_core::bluetooth_link::Link, device: String, role: Role| {
                        spawn_peer_session(
                            transport,
                            identity.clone(),
                            role,
                            None,
                            registry.clone(),
                            pairing.clone(),
                            control.clone(),
                            capture.clone(),
                            status.clone(),
                            live.clone(),
                            modules.clone(),
                            offer,
                            Some(device),
                            None,
                        );
                    },
                )
            };
            #[cfg(any(target_os = "linux", windows))]
            tokio::task::spawn_local(dial_over_bluetooth(
                registry.clone(),
                control.clone(),
                decide.clone(),
                open_bluetooth.clone(),
            ));
            loop {
                tokio::select! {
                    // A paired device off the network opened the mesh's
                    // Bluetooth channel. Same handshake, same session: only
                    // the path differs.
                    Some((device, transport)) = bluetooth.accept() => {
                        // A listener stays registered until the node stops,
                        // so turning Bluetooth off is honoured here.
                        if !decide().is_on() {
                            tracing::debug!(%device, "bluetooth is off for the mesh; closing a connection");
                            continue;
                        }
                        tracing::info!(%device, "peer dialled in over bluetooth");
                        open_bluetooth(transport, device, Role::Responder);
                    }

                    // A peer dialled us. We don't know who until the handshake,
                    // so the session itself reports the identity back.
                    accepted = listener.accept() => {
                        let transport = match accepted {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(error = %e, "accept failed");
                                continue;
                            }
                        };
                        tracing::info!(peer = %transport.peer_addr(), "peer dialled in");
                        let network = Some(transport.peer_addr().ip());
                        spawn_peer_session(
                            transport,
                            identity.clone(),
                            Role::Responder,
                            None,
                            registry.clone(),
                            pairing.clone(),
                            control.clone(),
                            capture.clone(),
                            status.clone(),
                            live.clone(),
                            modules.clone(),
                            offer,
                            None,
                            network,
                        );
                    }

                    // Discovery saw someone. Refresh their address if trusted.
                    Some(peer) = seen_rx.recv() => {
                        control.saw(peer.peer_id, &peer.display_name, peer.addr);
                        let known = registry.borrow_mut().note_seen(
                            peer.peer_id,
                            &peer.display_name,
                            peer.addr,
                        );
                        if known {
                            tracing::debug!(peer = %peer.display_name, addr = %peer.addr, "trusted peer seen");
                        } else if pairing.is_open() {
                            // The one window in which a stranger is interesting.
                            // Outside it an unknown machine announcing itself is
                            // ignored, which is what keeps joining the mesh an
                            // explicit, password-gated act.
                            if strangers.insert(peer.peer_id, (peer.display_name.clone(), peer.addr)).is_none() {
                                tracing::info!(
                                    peer = %peer.display_name,
                                    addr = %peer.addr,
                                    "unpaired machine found; will offer it the pairing password"
                                );
                            }
                        } else {
                            tracing::trace!(peer = %peer.display_name, "ignoring untrusted peer");
                        }
                    }

                    // Time to dial whoever is known, offline, and ours to call.
                    _ = sweep.tick() => {
                        let now_decided = decide();
                        if now_decided != decided {
                            cursio_core::bluetooth_link::report(&now_decided);
                            if now_decided.is_on() && !listening {
                                bluetooth = cursio_core::bluetooth_link::listen();
                                listening = true;
                            }
                            decided = now_decided;
                        }
                        // Before the dial below and the publish, so a machine
                        // the user just removed is neither called again nor
                        // shown for one more sweep.
                        for peer in live.current().forgotten {
                            if let Some(gone) = registry.borrow_mut().forget(&peer) {
                                tracing::info!(peer = %gone.display_name, "no longer dialling a removed machine");
                            }
                        }
                        control.publish_mesh(&registry, status.borrow().current());
                        // A machine that is not trusted yet, while the window is
                        // open. This is what `join` used to be: the mesh only
                        // ever dials the trust store, so somebody has to make
                        // the first call to a stranger. Both sides run the same
                        // command and `should_dial` settles which one calls —
                        // the same rule that stops two trusted nodes dialling
                        // each other at once.
                        if pairing.is_open() {
                            for (peer_id, (name, addr)) in std::mem::take(&mut strangers) {
                                // A nil id is the `--addr` placeholder: the user
                                // named this machine, so this side calls it
                                // rather than waiting to be called.
                                if peer_id != PeerId::nil()
                                    && !cursio_core::registry::should_dial(self_id, peer_id)
                                {
                                    tracing::debug!(peer = %name, "letting the other machine call us");
                                    continue;
                                }
                                let transport = match TcpTransport::connect(addr).await {
                                    Ok(t) => t,
                                    Err(e) => {
                                        tracing::warn!(peer = %name, %addr, error = %e, "could not reach the machine to pair with");
                                        continue;
                                    }
                                };
                                tracing::info!(peer = %name, %addr, "dialled an unpaired machine to pair with it");
                                spawn_peer_session(
                                    transport,
                                    identity.clone(),
                                    Role::Initiator,
                                    None,
                                    registry.clone(),
                                    pairing.clone(),
                                    control.clone(),
                                    capture.clone(),
                                    status.clone(),
                                    live.clone(),
                                    modules.clone(),
                                    offer,
                                    None,
                                    Some(addr.ip()),
                                );
                            }
                        }
                        let targets = cursio_core::mesh::next_dial_targets(self_id, &registry);
                        if targets.is_empty() {
                            continue;
                        }
                        // Off the loop: a peer that has gone holds its dial for
                        // the connect timeout, and a Bluetooth call that peer
                        // placed meanwhile waited that long to be accepted —
                        // 17 s, measured, every sweep's worth end to end. The
                        // targets are `Connecting` now, so no sweep dials them
                        // twice.
                        let identity = identity.clone();
                        let registry = registry.clone();
                        let pairing = pairing.clone();
                        let control = control.clone();
                        let capture = capture.clone();
                        let status = status.clone();
                        let live = live.clone();
                        let modules = modules.clone();
                        tokio::task::spawn_local(async move {
                        for (target, dialled) in cursio_core::mesh::dial(targets).await {
                            let transport = match dialled {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::debug!(
                                        peer = %target.display_name,
                                        error = %e,
                                        "dial failed; will retry"
                                    );
                                    // Back to Offline so the next sweep retries.
                                    registry.borrow_mut().mark_offline(target.peer_id);
                                    continue;
                                }
                            };
                            tracing::info!(peer = %target.display_name, addr = %target.addr, "dialled peer");
                            // Cache the address that just worked so we can redial
                            // this peer after a restart even if mDNS never
                            // announces it. Best-effort: a failed write must not
                            // stop the session.
                            if let Ok(mut store) = TrustStore::load() {
                                if let Err(e) = store.remember_addr(&target.peer_id, target.addr) {
                                    tracing::debug!(error = %e, "could not cache peer address");
                                }
                            }
                            spawn_peer_session(
                                transport,
                                identity.clone(),
                                Role::Initiator,
                                Some(target.peer_id),
                                registry.clone(),
                                pairing.clone(),
                                control.clone(),
                                capture.clone(),
                                status.clone(),
                                live.clone(),
                                modules.clone(),
                                offer,
                                None,
                                Some(target.addr.ip()),
                            );
                        }
                        });
                    }
                }
            }
        })
        .await
}

/// Serve the control socket for this node.
///
/// A second node for the same user is not refused here: two processes on one
/// machine are an existing, deliberate setup (a test node on another port),
/// and the first one keeps answering for the machine. The second says so.
pub(crate) fn start_control(state: cursio_core::control::NodeState) {
    let endpoint = match cursio_core::control::transport::endpoint() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::warn!(%error, "no control socket; the app and `cursio status` cannot reach this node");
            return;
        }
    };
    tokio::spawn(async move {
        if let Err(error) = cursio_core::control::transport::serve(endpoint, state).await {
            tracing::warn!(%error, "control socket stopped; the app and `cursio status` cannot reach this node");
        }
    });
}

/// Call paired computers over Bluetooth when the network stops reaching them,
/// and let go of a Bluetooth path once it reaches them again; see
/// `cursio_core::bluetooth_link`.
///
/// Its own task, because a call holds the radio for seconds and the mesh
/// loop must keep accepting meanwhile. One call at a time, which is also all
/// one radio does well.
#[cfg(any(target_os = "linux", windows))]
pub(crate) async fn dial_over_bluetooth(
    registry: cursio_core::mesh::SharedRegistry,
    control: cursio_core::control::NodeState,
    decide: std::rc::Rc<dyn Fn() -> cursio_core::bluetooth_link::Decision>,
    open: std::rc::Rc<dyn Fn(cursio_core::bluetooth_link::Link, String, Role)>,
) {
    use cursio_core::bluetooth_link::{self as bt, Known};
    use std::time::Instant;

    let mut dialer = bt::Dialer::default();
    let mut missing_since: std::collections::HashMap<cursio_protocol::PeerId, Instant> =
        std::collections::HashMap::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tick.tick().await;
        if !decide().is_on() {
            continue;
        }
        let standby = control.running().bluetooth.standby;
        if !standby {
            for session in registry.borrow_mut().bluetooth_paths_to_release() {
                tracing::info!("the network carries the session again; leaving bluetooth");
                session.release(
                    cursio_transport::resume::PathKind::Bluetooth,
                    "the network carries the session",
                );
            }
        }

        let now = Instant::now();
        let anyone_missing = {
            let reg = registry.borrow();
            let peers: Vec<_> = reg.all().map(|peer| peer.peer_id).collect();
            missing_since.retain(|id, _| peers.contains(id) && !reg.network_reaches(id));
            for id in peers {
                if !reg.network_reaches(&id) {
                    missing_since.entry(id).or_insert(now);
                }
            }
            missing_since
                .values()
                .any(|since| now.duration_since(*since) >= bt::NETWORK_GRACE)
        };
        if !anyone_missing && !standby {
            continue;
        }

        let paired = bt::paired_computers().await;
        dialer.observe(&paired);
        let wanted = {
            let reg = registry.borrow();
            for device in &paired {
                if let Some(peer) = reg.bluetooth_peer(&device.address) {
                    dialer.set_linked(&device.address, reg.bluetooth_reaches(&peer));
                }
            }
            bt::select(
                &paired,
                |address| {
                    let peer = reg.bluetooth_peer(address);
                    Known {
                        peer,
                        bluetooth_up: peer.is_some_and(|p| reg.bluetooth_reaches(&p)),
                        gone_for: peer
                            .and_then(|p| missing_since.get(&p))
                            .map(|since| now.duration_since(*since)),
                    }
                },
                anyone_missing,
                standby,
            )
        };
        for device in dialer.pace(&wanted, now) {
            match bt::dial(device.address.clone()).await {
                Ok(link) => {
                    // Linked or not is read from the session on the next
                    // turn, once the peer has greeted; a handshake that
                    // fails leaves the address to the ordinary retry.
                    tracing::info!(device = %device.name, "reached a computer over bluetooth");
                    open(link, device.address, Role::Initiator);
                }
                Err(e) => {
                    let known_peer = registry.borrow().bluetooth_peer(&device.address).is_some();
                    let retry = dialer.failed(&device.address, e.refused, known_peer);
                    tracing::debug!(device = %device.name, reason = %e.reason,
                        known_peer, retry_in_secs = retry.as_secs(),
                        "no mesh over bluetooth to this device");
                }
            }
        }
    }
}

/// Everything a node needs before it can talk to anyone: a bound listener, an
/// mDNS announcement, and local input capture.
///
/// Shared by every entry point so the sequence exists once. It used to be
/// copy-pasted between `serve` and `mesh`, which is exactly how the capture
/// lifetime bug got fixed in one place and not the other.
pub(crate) struct NodeBase {
    listener: TcpListenerHandle,
    /// Kept alive purely for its `Drop`: dropping it withdraws the mDNS record.
    _announcement: discovery::Announcement,
    capture: SharedCapture,
    status: SharedNodeStatus,
}

/// This machine's power/network state, shared between concurrent sessions.
///
/// One monitor per machine, not per link: the readings are machine-wide, and a
/// node bringing up four links would otherwise probe sysfs four times to learn
/// the same number — and could report four different answers if a poll landed
/// mid-change. `Rc<RefCell<_>>` for the same reason as [`SharedCapture`]:
/// sessions are `!Send` and never leave this thread.
pub(crate) type SharedNodeStatus = std::rc::Rc<std::cell::RefCell<NodeStatusMonitor>>;

/// Bind, announce on the LAN, and bring up input capture.
pub(crate) async fn start_node(
    identity: &NodeIdentity,
    port: u16,
    send_input: bool,
) -> anyhow::Result<NodeBase> {
    let listener = bind_mesh_port(port).await?;
    let bound = listener.local_addr();
    tracing::info!(%bound, "listening for peers");

    // Being reachable is not the same as being bound. On Windows the firewall
    // decides, and its own prompt opens the wrong protocol here — see
    // `cursio_core::firewall`. Done with the port that was actually bound, so a
    // node started on a different one opens that.
    cursio_core::firewall::allow_mesh_port(bound.port());

    // A machine short of memory looks, from inside Cursio, like a network
    // fault in every module at once; this says which it is.
    cursio_core::pressure::watch();

    // Announce ourselves so other nodes can find us. Pin mDNS to the real LAN
    // interface so VPN/virtual interfaces don't break it.
    let announcement = cursio_core::link::announce(identity, bound.port())?;

    // Capture comes up ONCE here, before any session: the portal permission is
    // asked a single time and survives peers coming and going. See
    // [`PersistentCapture`].
    //
    // It is started without edges on purpose. Which edge borders a peer is a
    // per-link answer derived from the two `--at` positions, and no peer has
    // said hello yet; the hub arms them as links come up, through
    // [`cursio_input::set_watched_edges`].
    let capture: SharedCapture = std::rc::Rc::new(make_persistent_capture(send_input));

    // Read once here rather than per link, for the same reason capture is
    // built once: the answer is machine-wide. The first reading rides every
    // greeting; changes are announced from the session loop.
    let status: SharedNodeStatus =
        std::rc::Rc::new(std::cell::RefCell::new(NodeStatusMonitor::new()));
    tracing::info!(status = %status.borrow().current(), "local node status");

    Ok(NodeBase {
        listener,
        _announcement: announcement,
        capture,
        status,
    })
}
