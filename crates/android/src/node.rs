//! The node itself: one thread, one runtime, one mesh.
//!
//! Kotlin gets a handle it can start, stop and ask about. Everything the handle
//! wraps runs on a thread this module owns, because the session layer is
//! `!Send` by design (see `mesh.rs`) and a JVM thread is the wrong place to put
//! a Tokio runtime anyway — Android reuses and parks those, and a runtime
//! parked mid-session is a link the peer sees as dead.
//!
//! ## What this is, and what it is not yet
//!
//! Listening, announcing on the LAN, learning peers and holding the known-peer
//! set — the half of the mesh that makes a phone *appear* to its peers. Dialling
//! and running full sessions is the next step and needs the pairing UI to exist
//! first, since a phone cannot prompt for a password from a background thread.
//!
//! The node deliberately shares `cursio-core`'s registry and discovery rather
//! than reimplementing either: this file is plumbing for one platform, and any
//! logic that ends up here is logic that will drift from what the desktop does.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cursio_core::mesh::SharedRegistry;
use cursio_core::NodeIdentity;
use cursio_transport::discovery;
use cursio_transport::tcp::TcpListenerHandle;

/// What the node is currently able to say about itself.
///
/// Kept behind a mutex and updated from the node thread, because the caller is
/// a notification refresh on an arbitrary JVM thread and must never block on
/// the session loop.
#[derive(Debug, Default)]
struct Shared {
    /// The address the listener actually bound, once it has.
    bound: Mutex<Option<SocketAddr>>,
    /// Peers seen on the LAN, by display name. Names rather than ids because
    /// the only consumer is a line of text a person reads.
    seen: Mutex<Vec<String>>,
    /// Cleared to ask the node thread to wind down.
    running: AtomicBool,
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
    /// The bound address appears in [`status_line`](Self::status_line) when it
    /// is real.
    pub fn start(display_name: String, port: u16) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared::default());
        shared.running.store(true, Ordering::SeqCst);

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
        let bound = self.shared.bound.lock().expect("bound lock");
        let seen = self.shared.seen.lock().expect("seen lock");
        match (*bound, seen.len()) {
            (None, _) => "starting".to_string(),
            (Some(addr), 0) => format!("listening on {addr}"),
            (Some(addr), 1) => format!("listening on {addr} · 1 peer: {}", seen[0]),
            (Some(addr), n) => format!("listening on {addr} · {n} peers: {}", seen.join(", ")),
        }
    }
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

    // Sessions share their injector and cursor via `Rc` deliberately, so they
    // are spawned onto a `LocalSet` rather than the multi-thread scheduler.
    let local = tokio::task::LocalSet::new();
    if let Err(e) = local.block_on(&runtime, serve(Arc::clone(&shared), display_name, port)) {
        tracing::error!(error = %e, "node stopped with an error");
    }
}

/// Listen, announce, and keep the known-peer set current until asked to stop.
async fn serve(shared: Arc<Shared>, display_name: String, port: u16) -> anyhow::Result<()> {
    let mut identity = NodeIdentity::load_or_create()?;
    // The phone's name is the user's, not the hostname: Android's hostname is
    // a meaningless `localhost` on most devices, and this is the label every
    // peer shows for this machine.
    if !display_name.is_empty() && identity.display_name != display_name {
        identity.display_name = display_name;
    }
    tracing::info!(
        id = %identity.peer_id,
        name = %identity.display_name,
        "node identity"
    );

    let listener = TcpListenerHandle::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    let bound = listener.local_addr();
    *shared.bound.lock().expect("bound lock") = Some(bound);
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

    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let self_id = identity.peer_id;
    let browse_config = config.clone();
    // Discovery blocks its thread, so it gets one of its own rather than a
    // Tokio worker — on a current-thread runtime it would otherwise be the
    // only thing that ever ran.
    std::thread::Builder::new()
        .name("cursio-browse".into())
        .spawn(move || {
            let _ = discovery::browse_with(self_id, &browse_config, |peer| {
                let _ = seen_tx.send(peer);
            });
        })
        .map_err(|e| anyhow::anyhow!("cannot spawn the discovery thread: {e}"))?;

    loop {
        if !shared.running.load(Ordering::SeqCst) {
            tracing::info!("stop requested");
            return Ok(());
        }

        tokio::select! {
            // Checked on a timer rather than only on an event, so a node with
            // no peers still notices it was asked to stop.
            _ = tokio::time::sleep(STOP_CHECK_INTERVAL) => {}

            Some(peer) = seen_rx.recv() => {
                note_seen(&registry, &shared, peer);
            }

            accepted = listener.accept() => {
                match accepted {
                    // Accepting and immediately dropping is honest about what
                    // this build does: a peer learns the phone is there and
                    // that the link went nowhere, rather than waiting out a
                    // handshake that was never going to come. Sessions arrive
                    // with the pairing UI.
                    Ok(transport) => {
                        tracing::info!(
                            addr = %transport.peer_addr(),
                            "peer dialled in; no session support yet"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
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

/// Record a sighting, and keep the notification's peer list current.
fn note_seen(registry: &SharedRegistry, shared: &Arc<Shared>, peer: discovery::DiscoveredPeer) {
    let known = registry
        .borrow_mut()
        .note_seen(peer.peer_id, &peer.display_name, peer.addr);
    if known {
        tracing::info!(peer = %peer.display_name, addr = %peer.addr, "known peer seen");
    } else {
        tracing::debug!(peer = %peer.display_name, addr = %peer.addr, "untrusted machine seen");
    }

    let mut seen = shared.seen.lock().expect("seen lock");
    if !seen.iter().any(|name| name == &peer.display_name) {
        seen.push(peer.display_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_with(bound: Option<SocketAddr>, seen: &[&str]) -> Arc<Shared> {
        let shared = Arc::new(Shared::default());
        *shared.bound.lock().expect("bound lock") = bound;
        *shared.seen.lock().expect("seen lock") = seen.iter().map(|s| (*s).to_string()).collect();
        shared
    }

    fn line(shared: Arc<Shared>) -> String {
        NodeHandle {
            shared,
            thread: None,
        }
        .status_line()
    }

    /// The notification is the only place a phone user sees whether this is
    /// working, so "no address yet" must read as starting rather than as a node
    /// that is up and sees nobody.
    #[test]
    fn an_unbound_node_reads_as_starting_not_as_alone() {
        assert_eq!(line(shared_with(None, &[])), "starting");
        // Even having seen peers: without a listener this node is not reachable.
        assert_eq!(line(shared_with(None, &["Lightmoon"])), "starting");
    }

    /// Bound with nobody around is the ordinary first state, and must say the
    /// address — it is what the user checks against the peer's logs.
    #[test]
    fn a_bound_node_names_the_address_it_listens_on() {
        let addr: SocketAddr = "192.168.1.7:46073".parse().expect("addr");
        assert_eq!(
            line(shared_with(Some(addr), &[])),
            "listening on 192.168.1.7:46073"
        );
    }

    /// Peer counts are what the user actually reads, so one peer is named
    /// singular rather than "1 peers".
    #[test]
    fn peers_are_counted_and_named() {
        let addr: SocketAddr = "192.168.1.7:46073".parse().expect("addr");
        assert_eq!(
            line(shared_with(Some(addr), &["Lightmoon"])),
            "listening on 192.168.1.7:46073 · 1 peer: Lightmoon"
        );
        assert_eq!(
            line(shared_with(Some(addr), &["Lightmoon", "Thero-PC"])),
            "listening on 192.168.1.7:46073 · 2 peers: Lightmoon, Thero-PC"
        );
    }

    /// mDNS re-announces the same peer repeatedly — every few seconds, and
    /// again on every network change. Without the check the notification would
    /// grow "Lightmoon, Lightmoon, Lightmoon" until it was unreadable.
    #[test]
    fn a_peer_seen_repeatedly_is_listed_once() {
        let shared = shared_with(Some("192.168.1.7:46073".parse().expect("addr")), &[]);
        let registry = cursio_core::mesh::registry_from_trust(cursio_protocol::PeerId::nil())
            .expect("registry");

        for _ in 0..3 {
            note_seen(
                &registry,
                &shared,
                discovery::DiscoveredPeer {
                    peer_id: cursio_protocol::PeerId::from_u128(1),
                    display_name: "Lightmoon".into(),
                    addr: "192.168.1.5:46073".parse().expect("addr"),
                },
            );
        }

        assert_eq!(shared.seen.lock().expect("seen lock").len(), 1);
    }
}
