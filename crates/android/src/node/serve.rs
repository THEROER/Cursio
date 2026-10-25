//! The node's own loop: listening, announcing, browsing, and dialling the
//! machines that are down — and the strangers a pairing window lets in.

use crate::node::bluetooth::dial_bluetooth;
use crate::node::link::spawn_session;
use crate::node::lock;
use crate::node::Node;
use crate::node::NodeCtx;
use crate::node::Shared;
use cursio_core::audio_gate::MicGate;
use cursio_core::audio_session::AudioFactory;
use cursio_core::control::ModuleSwitch;
use cursio_core::control::NodeState;
use cursio_core::live::Shared as LiveShared;
use cursio_core::mesh::SharedRegistry;
use cursio_core::module::ModuleRegistry;
use cursio_core::node_status::NodeStatusMonitor;
use cursio_protocol::PeerId;
use cursio_transport::discovery;
use cursio_transport::pairing::{Role, TrustStore};
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Listen, announce, dial, and run a session per link until asked to stop.
pub(super) async fn serve(node: Node) -> anyhow::Result<()> {
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

    let modules = phone_modules(&control, mic_gate);
    for advert in modules.advertise() {
        tracing::info!(module = %advert.id, version = advert.version, "module available");
    }
    // Input too: the touchpad drives a peer's cursor through the links.
    control.set_available(ModuleSwitch::available(&modules.available(), true));

    let self_id = identity.peer_id;
    let mut seen_rx = spawn_browse(self_id)?;

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
                sweep_once(&ctx, self_id, &mut strangers).await;
            }
        }
    }
}

/// What a phone runs on a link, and the platform halves the hubs reach the
/// phone through.
fn phone_modules(control: &NodeState, mic_gate: LiveShared<MicGate>) -> ModuleRegistry {
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
    modules
}

/// Discovery blocks its thread, so it gets one of its own rather than the
/// only thread this runtime has. What it sees arrives on the channel.
fn spawn_browse(
    self_id: PeerId,
) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<discovery::DiscoveredPeer>> {
    let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let browse_config = cursio_core::link::discovery_config();
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
    Ok(seen_rx)
}

/// One sweep: publish what the mesh knows, call the strangers an open
/// pairing window lets in, and dial every known machine that is down.
async fn sweep_once(ctx: &Rc<NodeCtx>, self_id: PeerId, strangers: &mut HashMap<PeerId, Stranger>) {
    publish(ctx);
    strangers.retain(|id, _| !ctx.registry.borrow().is_known(id));
    if ctx.pairing.is_open() {
        let now = std::time::Instant::now();
        for stranger in strangers.values_mut() {
            if stranger
                .called
                .is_some_and(|at| now.duration_since(at) < STRANGER_RECALL)
            {
                continue;
            }
            stranger.called = Some(now);
            match TcpTransport::connect(stranger.addr).await {
                Ok(t) => {
                    tracing::info!(peer = %stranger.name, addr = %stranger.addr, "dialled an unpaired machine to pair with it");
                    let addr = Some(stranger.addr);
                    spawn_session(t, Rc::clone(ctx), Role::Initiator, None, addr);
                }
                Err(e) => {
                    tracing::warn!(peer = %stranger.name, error = %e, "could not reach the machine to pair with")
                }
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
                spawn_session(
                    t,
                    Rc::clone(ctx),
                    Role::Initiator,
                    Some(target.peer_id),
                    addr,
                );
            }
            Err(e) => {
                tracing::debug!(peer = %target.display_name, error = %e, "dial failed; will retry");
                ctx.registry.borrow_mut().mark_offline(target.peer_id);
            }
        }
    }
    dial_bluetooth(ctx).await;
}

/// A machine on the LAN this phone has not paired with.
pub(super) struct Stranger {
    name: String,
    addr: SocketAddr,
    /// When it was last called during the open window.
    called: Option<std::time::Instant>,
}

/// How long a stranger that did not pair is left before being called again
/// while the window stays open: long enough not to hammer a machine that is
/// not pairing, short enough that one that opens its window is reached soon.
pub(super) const STRANGER_RECALL: std::time::Duration = std::time::Duration::from_secs(10);

/// How often the loop wakes to notice a stop request.
///
/// Short enough that stopping feels immediate to a user closing the app, long
/// enough to be free: this wakes a parked thread, and on a phone that is a
/// radio and a CPU that would rather stay asleep.
pub(super) const STOP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Hand the control state what the mesh loop knows.
pub(super) fn publish(ctx: &NodeCtx) {
    ctx.control
        .publish_mesh(&ctx.registry, ctx.status.borrow().current());
}

/// Record a sighting, and keep the notification's list current.
pub(super) fn note_seen(
    registry: &SharedRegistry,
    shared: &Shared,
    peer: &discovery::DiscoveredPeer,
) {
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
