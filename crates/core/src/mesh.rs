//! The mesh supervisor: one node, many peers, no hub.
//!
//! Every node does the same two things at once — **listen** for peers dialling
//! in, and **dial** the peers it knows that are currently down. There is no
//! server role: `serve` and `connect` were two halves of the same behaviour, and
//! a mesh node simply does both, forever.
//!
//! That is what makes the failure story work. Each node holds its own
//! [`PeerRegistry`] seeded from the trust store, so the set of machines it knows
//! outlives any of them being switched off. When one machine dies the others
//! notice (the session's liveness timeout), mark it `Offline`, and keep talking
//! to each other; when it comes back, whichever node sees it first reconnects.
//! Nothing was forgotten, and nothing was waiting on the machine that died.
//!
//! ## Threading
//!
//! Sessions are **not `Send`**: the input path shares its injector and cursor
//! control between closures with `Rc`, deliberately, because they only ever run
//! on one task. So peer sessions are spawned with `spawn_local` onto a
//! [`LocalSet`] rather than `tokio::spawn`. The registry is shared as
//! `Rc<RefCell<..>>` for the same reason — single-threaded, never contended.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use cursio_protocol::PeerId;
use cursio_transport::pairing::TrustStore;

use crate::registry::{PeerRegistry, PeerState};

/// How often the supervisor looks for peers it should be dialling.
///
/// Discovery sightings arrive asynchronously, so this only bounds how quickly a
/// newly-seen peer is dialled; it is not a busy-poll of the network.
const DIAL_SWEEP_INTERVAL: Duration = Duration::from_secs(2);

/// Shared, single-threaded view of who we know and who is up.
pub type SharedRegistry = Rc<RefCell<PeerRegistry>>;

/// Build the registry every mesh node starts from: the durable set of peers we
/// have paired with.
pub fn registry_from_trust(local: PeerId) -> Result<SharedRegistry, anyhow::Error> {
    let store = TrustStore::load()?;
    let registry = PeerRegistry::from_trust_store(&store, local);
    tracing::info!(known = registry.all().count(), "loaded known peers");
    Ok(Rc::new(RefCell::new(registry)))
}

/// A peer the supervisor decided to connect to.
#[derive(Debug, Clone)]
pub struct DialTarget {
    pub peer_id: PeerId,
    pub display_name: String,
    pub addr: SocketAddr,
}

/// Decide who to dial on this sweep.
///
/// Filters the registry's candidates through the dial-race rule so two nodes
/// that come up together don't each open a session to the other — see
/// [`crate::registry::should_dial`].
///
/// Peers chosen here are moved to `Connecting` immediately, so a slow dial isn't
/// started twice by the next sweep.
pub fn next_dial_targets(local_id: PeerId, registry: &SharedRegistry) -> Vec<DialTarget> {
    next_dial_targets_at(local_id, registry, Instant::now())
}

/// How long a node leaves the dial to the peer [`should_dial`] picked before
/// placing it itself.
///
/// The rule assumes the two hear each other. They need not: a phone sharing
/// its connection hears the laptop on its hotspot, while its own announcements
/// never reach that laptop (measured on HyperOS: multicast from the hotspot's
/// owner to its clients is dropped, the other way passes). The laptop then
/// keeps dialling the address it remembered, and the phone keeps waiting for
/// it — nobody calls. So a node that has *heard* the peer in this run and
/// still sees it down after this long dials anyway. Several sweeps long, so a
/// peer that can dial has done so first and the pair does not open two links.
///
/// [`should_dial`]: crate::registry::should_dial
const YIELD_GRACE: Duration = Duration::from_secs(10);

fn next_dial_targets_at(
    local_id: PeerId,
    registry: &SharedRegistry,
    now: Instant,
) -> Vec<DialTarget> {
    let mut reg = registry.borrow_mut();
    let candidates = reg.dial_candidates();
    let mut targets = Vec::new();
    for peer in candidates {
        let Some(addr) = peer.addr else { continue };
        if !crate::registry::should_dial(local_id, peer.peer_id) {
            let since = reg.yielding(peer.peer_id, now).unwrap_or(now);
            let heard = peer.seen_at.is_some();
            if !heard || now.duration_since(since) < YIELD_GRACE {
                tracing::trace!(peer = %peer.display_name, "yielding dial to the peer");
                continue;
            }
            tracing::info!(
                peer = %peer.display_name,
                %addr,
                "the peer has not called although it is up; calling it instead"
            );
        }
        reg.set_state(peer.peer_id, PeerState::Connecting);
        targets.push(DialTarget {
            peer_id: peer.peer_id,
            display_name: peer.display_name,
            addr,
        });
    }
    targets
}

/// How long to wait between dial sweeps.
pub fn sweep_interval() -> Duration {
    DIAL_SWEEP_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::PeerRegistry;

    fn shared(reg: PeerRegistry) -> SharedRegistry {
        Rc::new(RefCell::new(reg))
    }

    /// The node with the lower id dials; the other waits. Otherwise two nodes
    /// starting together open two sessions to each other.
    #[test]
    fn only_the_lower_id_dials() {
        let (low, high) = (PeerId::from_u128(1), PeerId::from_u128(2));
        let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();

        let mut reg = PeerRegistry::default();
        reg.admit(high, "high".into(), Some(addr));
        let reg = shared(reg);
        assert_eq!(next_dial_targets(low, &reg).len(), 1, "low id should dial");

        let mut reg2 = PeerRegistry::default();
        reg2.admit(low, "low".into(), Some(addr));
        let reg2 = shared(reg2);
        assert!(
            next_dial_targets(high, &reg2).is_empty(),
            "high id should wait for the inbound connection"
        );
    }

    /// The node that waits calls after all once it has heard the peer and the
    /// peer has not called — the hotspot that hears its client but is never
    /// heard by it.
    #[test]
    fn a_waiting_node_that_hears_the_peer_calls_after_the_grace() {
        let (low, high) = (PeerId::from_u128(1), PeerId::from_u128(2));
        let addr: SocketAddr = "10.83.118.56:40711".parse().unwrap();
        let mut reg = PeerRegistry::default();
        reg.admit(low, "low".into(), None);
        reg.note_seen(low, "low", addr);
        let reg = shared(reg);

        let start = Instant::now();
        assert!(next_dial_targets_at(high, &reg, start).is_empty());
        assert!(next_dial_targets_at(high, &reg, start + YIELD_GRACE / 2).is_empty());
        let late = next_dial_targets_at(high, &reg, start + YIELD_GRACE);
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].addr, addr);
    }

    /// An address remembered from an earlier run is not reason enough: both
    /// sides would call one another's stale addresses.
    #[test]
    fn a_waiting_node_that_never_heard_the_peer_keeps_waiting() {
        let (low, high) = (PeerId::from_u128(1), PeerId::from_u128(2));
        let mut reg = PeerRegistry::default();
        reg.admit(
            low,
            "low".into(),
            Some("192.168.1.18:41217".parse().unwrap()),
        );
        let reg = shared(reg);
        let start = Instant::now();
        assert!(next_dial_targets_at(high, &reg, start).is_empty());
        assert!(next_dial_targets_at(high, &reg, start + YIELD_GRACE * 3).is_empty());
    }

    /// A dial in flight must not be started again by the next sweep.
    #[test]
    fn a_peer_is_not_dialled_twice() {
        let local = PeerId::from_u128(1);
        let remote = PeerId::from_u128(9);
        let mut reg = PeerRegistry::default();
        reg.admit(
            remote,
            "peer".into(),
            Some("127.0.0.1:9000".parse().unwrap()),
        );
        let reg = shared(reg);

        assert_eq!(next_dial_targets(local, &reg).len(), 1);
        assert!(
            next_dial_targets(local, &reg).is_empty(),
            "the same peer was dialled again while a dial was in flight"
        );
    }

    /// After a peer drops, it becomes dialable again — that is the reconnect.
    #[test]
    fn a_dropped_peer_becomes_dialable_again() {
        let local = PeerId::from_u128(1);
        let remote = PeerId::from_u128(9);
        let mut reg = PeerRegistry::default();
        reg.admit(
            remote,
            "peer".into(),
            Some("127.0.0.1:9000".parse().unwrap()),
        );
        let reg = shared(reg);

        assert_eq!(next_dial_targets(local, &reg).len(), 1);
        reg.borrow_mut().mark_offline(remote);
        assert_eq!(
            next_dial_targets(local, &reg).len(),
            1,
            "a peer that dropped should be retried"
        );
    }
}
