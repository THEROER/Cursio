//! The set of machines this node knows about, and which of them are reachable
//! right now.
//!
//! This is the "don't forget the world when one machine dies" piece. The
//! *known* set is durable — it comes from the trust store, so a peer that has
//! been paired stays known across reboots, power cuts, and its own absence.
//! Only [`PeerState`] is transient: a node going away changes its state to
//! `Offline`, it never removes it. The one removal is the user's own
//! [`PeerRegistry::forget`], which goes with dropping the peer from the trust
//! store — a machine that is merely away and one the user sent away must not
//! look alike, or a reboot would erase the mesh.
//!
//! Nothing here is a hub. Every node runs the same registry over the same
//! durable set, so "who is up" is a local observation each node makes for
//! itself, not something a coordinator hands down.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use cursio_protocol::PeerId;
use cursio_transport::pairing::TrustStore;

/// What we currently believe about one peer.
#[derive(Debug, Clone)]
pub enum PeerState {
    /// A session is live right now.
    Online,
    /// Not connected. `since` is when we last saw it, if we ever did in this
    /// process — `None` means "known from the trust store but not seen yet".
    Offline { since: Option<Instant> },
    /// A connection attempt is in flight, so the supervisor doesn't start a
    /// second one for the same peer.
    Connecting,
}

/// One known machine.
#[derive(Debug, Clone)]
pub struct KnownPeer {
    pub peer_id: PeerId,
    pub display_name: String,
    /// Where we last saw it. Learned from discovery and refreshed on every
    /// sighting, because a rebooted machine can come back on a new address.
    pub addr: Option<SocketAddr>,
    pub state: PeerState,
    /// When discovery last reported it in this process. `None` for a peer
    /// known only from the trust store.
    pub seen_at: Option<Instant>,
    /// Since when this node has been leaving the dial to the peer
    /// ([`should_dial`]) while the peer stayed down. See
    /// [`crate::mesh::next_dial_targets`].
    pub yielding_since: Option<Instant>,
}

impl KnownPeer {
    /// Is this peer a candidate to dial right now?
    fn dialable(&self) -> bool {
        matches!(self.state, PeerState::Offline { .. }) && self.addr.is_some()
    }
}

/// Every peer this node knows, keyed by its stable id.
#[derive(Debug, Default)]
pub struct PeerRegistry {
    peers: HashMap<PeerId, KnownPeer>,
    /// This node's own id, so it can never end up as a peer of itself.
    local: Option<PeerId>,
}

impl PeerRegistry {
    /// Seed the registry from the trust store: everything we have ever paired
    /// with is known, whether or not it is currently reachable.
    ///
    /// Peers start `Offline` with no address — discovery fills addresses in as
    /// it sees them. That ordering matters: a stale address from a previous run
    /// would have us dialling a machine that has since moved.
    ///
    /// `local` is this node's own id, and any entry matching it is skipped: a
    /// trust store really can contain the node itself (loopback testing pairs a
    /// machine with itself), which was harmless when sessions were 1:1 but would
    /// have a mesh node endlessly dialling itself.
    pub fn from_trust_store(store: &TrustStore, local: PeerId) -> Self {
        let peers = store
            .peers()
            .filter(|t| t.peer_id != local)
            .map(|t| {
                (
                    t.peer_id,
                    KnownPeer {
                        peer_id: t.peer_id,
                        display_name: t.display_name.clone(),
                        // Seed from the trust store's cached address so a peer is
                        // dialable at startup even before mDNS announces it (or
                        // when mDNS never does). A live sighting overrides this via
                        // `note_seen`; until then this is our best known address.
                        addr: t.last_addr,
                        state: PeerState::Offline { since: None },
                        seen_at: None,
                        yielding_since: None,
                    },
                )
            })
            .collect();
        Self {
            peers,
            local: Some(local),
        }
    }

    /// Is this id us? Used to keep the node out of its own peer set.
    fn is_self(&self, id: PeerId) -> bool {
        self.local == Some(id)
    }

    /// Record a sighting from discovery.
    ///
    /// Only updates peers we already trust — an untrusted machine announcing
    /// itself on the LAN is not admitted to the mesh here. Pairing is the only
    /// way in, and it is explicit (`connect --password`).
    pub fn note_seen(&mut self, peer_id: PeerId, display_name: &str, addr: SocketAddr) -> bool {
        if self.is_self(peer_id) {
            return false;
        }
        match self.peers.get_mut(&peer_id) {
            Some(peer) => {
                // Always refresh the address: this is how a peer that came back
                // on a different IP becomes reachable again.
                peer.addr = Some(addr);
                peer.seen_at = Some(Instant::now());
                if peer.display_name != display_name {
                    peer.display_name = display_name.to_string();
                }
                true
            }
            None => false,
        }
    }

    /// Add (or refresh) a peer we have just paired with, so a brand-new peer
    /// joins the mesh without waiting for a restart.
    pub fn admit(&mut self, peer_id: PeerId, display_name: String, addr: Option<SocketAddr>) {
        if self.is_self(peer_id) {
            tracing::debug!("refusing to admit this node as its own peer");
            return;
        }
        let entry = self.peers.entry(peer_id).or_insert_with(|| KnownPeer {
            peer_id,
            display_name: display_name.clone(),
            addr,
            state: PeerState::Offline { since: None },
            seen_at: None,
            yielding_since: None,
        });
        entry.display_name = display_name;
        if addr.is_some() {
            entry.addr = addr;
        }
    }

    pub fn set_state(&mut self, peer_id: PeerId, state: PeerState) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            if matches!(state, PeerState::Online) {
                peer.yielding_since = None;
            }
            peer.state = state;
        }
    }

    /// Note that this node is leaving the dial to `peer_id`, and answer since
    /// when it has been.
    pub fn yielding(&mut self, peer_id: PeerId, now: Instant) -> Option<Instant> {
        let peer = self.peers.get_mut(&peer_id)?;
        Some(*peer.yielding_since.get_or_insert(now))
    }

    /// Mark a peer offline as of now.
    pub fn mark_offline(&mut self, peer_id: PeerId) {
        self.set_state(
            peer_id,
            PeerState::Offline {
                since: Some(Instant::now()),
            },
        );
    }

    /// Drop a peer the user removed from the mesh, so it is neither dialled
    /// nor shown. Not for a peer that went away — see [`Self::mark_offline`].
    pub fn forget(&mut self, peer_id: &PeerId) -> Option<KnownPeer> {
        self.peers.remove(peer_id)
    }

    pub fn get(&self, peer_id: &PeerId) -> Option<&KnownPeer> {
        self.peers.get(peer_id)
    }

    pub fn is_known(&self, peer_id: &PeerId) -> bool {
        self.peers.contains_key(peer_id)
    }

    /// Peers we should try to reach: known, currently offline, and with a
    /// known address.
    pub fn dial_candidates(&self) -> Vec<KnownPeer> {
        self.peers
            .values()
            .filter(|p| p.dialable())
            .cloned()
            .collect()
    }

    pub fn all(&self) -> impl Iterator<Item = &KnownPeer> {
        self.peers.values()
    }

    pub fn online_count(&self) -> usize {
        self.peers
            .values()
            .filter(|p| matches!(p.state, PeerState::Online))
            .count()
    }
}

/// Should *we* be the one to dial, when both nodes want to connect to each
/// other at the same time?
///
/// Two nodes that both come up and see each other will each start dialling, and
/// without a rule they end up with two redundant sessions (or a race that drops
/// both). The node with the lower id keeps its outbound attempt; the higher one
/// yields and lets the inbound connection arrive. Both compute this from the
/// same two stable ids, so they always agree — no negotiation, no coordinator.
///
/// The same lower-id-wins convention decides initial cursor ownership, so the
/// two rules can't disagree about which node is "first".
pub fn should_dial(local: PeerId, remote: PeerId) -> bool {
    local < remote
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u128) -> PeerId {
        PeerId::from_u128(n)
    }

    /// Exactly one side of any pair dials — otherwise simultaneous startup
    /// gives duplicate sessions.
    #[test]
    fn dial_race_has_exactly_one_winner() {
        let (a, b) = (peer(1), peer(2));
        assert!(should_dial(a, b));
        assert!(!should_dial(b, a));
    }

    /// A peer that goes offline must stay *known* — that is the whole point:
    /// the laptop dying must not make the PC forget it exists.
    #[test]
    fn offline_peer_is_still_known() {
        let mut reg = PeerRegistry::default();
        let id = peer(7);
        reg.admit(id, "laptop".into(), Some("127.0.0.1:9000".parse().unwrap()));
        reg.set_state(id, PeerState::Online);

        reg.mark_offline(id);

        assert!(reg.is_known(&id), "peer was forgotten when it went offline");
        assert_eq!(reg.online_count(), 0);
        assert!(
            reg.get(&id).unwrap().addr.is_some(),
            "address was discarded"
        );
    }

    /// A rebooted peer often returns on a different address; the registry must
    /// take the new one or we keep dialling into the void.
    #[test]
    fn sighting_refreshes_a_stale_address() {
        let mut reg = PeerRegistry::default();
        let id = peer(3);
        reg.admit(id, "pc".into(), Some("192.168.1.10:9000".parse().unwrap()));

        let moved: SocketAddr = "192.168.1.55:9000".parse().unwrap();
        assert!(reg.note_seen(id, "pc", moved));

        assert_eq!(reg.get(&id).unwrap().addr, Some(moved));
    }

    /// A trust store can legitimately contain this very node (loopback testing
    /// pairs a machine with itself). Harmless when sessions were 1:1, but a mesh
    /// node would dial itself forever — caught on the first real run.
    #[test]
    fn a_node_is_never_its_own_peer() {
        let me = peer(42);
        let mut reg = PeerRegistry {
            peers: Default::default(),
            local: Some(me),
        };

        reg.admit(me, "myself".into(), Some("127.0.0.1:9000".parse().unwrap()));
        assert!(!reg.is_known(&me), "the node admitted itself as a peer");

        assert!(!reg.note_seen(me, "myself", "127.0.0.1:9000".parse().unwrap()));
        assert!(
            reg.dial_candidates().is_empty(),
            "the node would dial itself"
        );
    }

    #[test]
    fn a_forgotten_peer_is_neither_known_nor_dialled_nor_readmitted_by_a_sighting() {
        let mut reg = PeerRegistry::default();
        let id = peer(5);
        let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        reg.admit(id, "laptop".into(), Some(addr));

        assert_eq!(
            reg.forget(&id).map(|p| p.display_name),
            Some("laptop".into())
        );

        assert!(!reg.is_known(&id));
        assert!(reg.dial_candidates().is_empty());
        assert!(!reg.note_seen(id, "laptop", addr));
    }

    /// Untrusted machines announcing themselves must not join the mesh.
    #[test]
    fn unknown_peers_are_not_admitted_by_discovery() {
        let mut reg = PeerRegistry::default();
        let seen = reg.note_seen(peer(99), "stranger", "10.0.0.1:9000".parse().unwrap());
        assert!(!seen, "an untrusted peer was admitted by a mere sighting");
        assert!(!reg.is_known(&peer(99)));
    }

    /// Only offline peers with a known address are dialled — never one that is
    /// already connected or still being connected to.
    #[test]
    fn dial_candidates_exclude_connected_and_addressless() {
        let mut reg = PeerRegistry::default();
        let (online, connecting, no_addr, ready) = (peer(1), peer(2), peer(3), peer(4));
        let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();

        reg.admit(online, "a".into(), Some(addr));
        reg.set_state(online, PeerState::Online);
        reg.admit(connecting, "b".into(), Some(addr));
        reg.set_state(connecting, PeerState::Connecting);
        reg.admit(no_addr, "c".into(), None);
        reg.admit(ready, "d".into(), Some(addr));

        let ids: Vec<_> = reg
            .dial_candidates()
            .into_iter()
            .map(|p| p.peer_id)
            .collect();
        assert_eq!(ids, vec![ready]);
    }
}
