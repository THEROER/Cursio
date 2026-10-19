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

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Instant;

use cursio_protocol::{PeerId, SessionTag};
use cursio_transport::pairing::TrustStore;
use cursio_transport::resume::ResumeHandle;

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

/// How one link to a peer was made: which side called, and over what.
///
/// Two links to one peer happen whenever both sides call at once — the side
/// [`should_dial`] tells to wait calls anyway after a grace, and a phone hands
/// over from Bluetooth to the network — and each half of the pair sees both.
/// Both halves must then keep the *same* one, which they can decide alone
/// only from what both know about each link: see [`LinkShape::beats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkShape {
    /// This node placed the call.
    pub dialled: bool,
    pub path: crate::control::LinkPath,
}

impl LinkShape {
    /// Whether a new link of this shape should replace a `held` one to
    /// `remote`, from `local`'s side.
    ///
    /// The network beats Bluetooth, which is only kept for when the network is
    /// not there. Then the call [`should_dial`] says the lower id places —
    /// the remote side sees the same link with `dialled` the other way round
    /// and `should_dial` the other way round, so it agrees. Two links of one
    /// shape are one side calling twice; the newer wins, since the older is
    /// most likely a socket that died without either side noticing yet.
    pub fn beats(self, held: LinkShape, local: Option<PeerId>, remote: PeerId) -> bool {
        use crate::control::LinkPath;
        if self.path != held.path {
            return self.path == LinkPath::Network;
        }
        match local {
            Some(local) if self.dialled != held.dialled => {
                self.dialled == should_dial(local, remote)
            }
            _ => true,
        }
    }
}

/// The link a peer is reached by, and how to end it if a better one arrives.
#[derive(Debug)]
struct HeldLink {
    token: u64,
    shape: LinkShape,
    superseded: std::rc::Rc<tokio::sync::Notify>,
}

/// Every peer this node knows, keyed by its stable id.
#[derive(Debug, Default)]
pub struct PeerRegistry {
    peers: HashMap<PeerId, KnownPeer>,
    /// This node's own id, so it can never end up as a peer of itself.
    local: Option<PeerId>,
    /// The one link each peer is reached by. Every session runs the whole
    /// module set, so a second one to a peer showed its window twice, opened
    /// its microphone twice and, ending, marked a peer offline whose other
    /// link was still up — which had the mesh dial it yet again.
    links: HashMap<PeerId, HeldLink>,
    next_link: u64,
    /// The session each peer is reached by, held open while its connection
    /// is down so a new one can join it.
    sessions: HashMap<PeerId, HeldSession>,
    /// Which peer answered on each Bluetooth address. Learned from the
    /// greeting, since nothing else ties a radio to a mesh identity: the
    /// dialler calls an address only when its own peer is missing, not
    /// whenever any peer is.
    bluetooth: HashMap<String, PeerId>,
    /// Peers whose Bluetooth path this node placed. The side that dialled is
    /// the one that lets it go once the network carries the session again;
    /// a phone that keeps a standby open must not have it closed from here.
    bluetooth_dialled: HashSet<PeerId>,
}

/// A running session, as the registry holds it.
struct HeldSession(ResumeHandle);

impl std::fmt::Debug for HeldSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldSession")
            .field("paths", &self.0.paths())
            .finish()
    }
}

/// What a greeting means for a peer's session.
pub enum Greeted {
    /// Both ends hold this session: the connection joins it.
    Join(ResumeHandle),
    /// No session both ends hold: the connection starts one.
    Fresh,
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
            ..Default::default()
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

    /// Take `peer`'s one link for a session that has just learnt who it talks
    /// to, answering the token to [`release_link`](Self::release_link) it
    /// with — or `None` when the link already held is the better one and this
    /// session should end.
    ///
    /// Replacing a held link wakes its `superseded`, which that session waits
    /// on alongside its own run.
    pub fn claim_link(
        &mut self,
        peer: PeerId,
        shape: LinkShape,
        superseded: std::rc::Rc<tokio::sync::Notify>,
    ) -> Option<u64> {
        if let Some(held) = self.links.get(&peer) {
            if !shape.beats(held.shape, self.local, peer) {
                return None;
            }
            held.superseded.notify_one();
        }
        self.next_link += 1;
        let token = self.next_link;
        self.links.insert(
            peer,
            HeldLink {
                token,
                shape,
                superseded,
            },
        );
        Some(token)
    }

    /// A session to `peer` ended. Answers whether the peer is now unlinked —
    /// whether this session, ending, is what takes it offline.
    ///
    /// `token` is what [`claim_link`](Self::claim_link) gave it; `None` for a
    /// session that never held the link (it ended before learning who it
    /// talked to, or lost to the one held).
    pub fn release_link(&mut self, peer: PeerId, token: Option<u64>) -> bool {
        match (self.links.get(&peer).map(|held| held.token), token) {
            (Some(held), Some(mine)) if held == mine => {
                self.links.remove(&peer);
                true
            }
            (Some(_), _) => false,
            (None, _) => true,
        }
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
            .filter(|p| {
                p.dialable()
                    || (p.addr.is_some()
                        && self.needs_network_path(&p.peer_id)
                        && !matches!(p.state, PeerState::Connecting))
            })
            .cloned()
            .collect()
    }

    /// A session to `peer` is alive but has no network path: dial one, so it
    /// resumes over the network rather than waiting out its hold or staying
    /// on Bluetooth.
    fn needs_network_path(&self, peer: &PeerId) -> bool {
        self.sessions
            .get(peer)
            .is_some_and(|s| !s.0.is_ended() && !s.0.paths().network)
    }

    /// A session runs over the network with no Bluetooth standby beside it.
    pub fn wants_bluetooth_standby(&self) -> bool {
        self.sessions.values().any(|s| {
            let paths = s.0.paths();
            !s.0.is_ended() && paths.network && !paths.bluetooth
        })
    }

    /// Whether `peer`'s session has a network path now.
    pub fn network_reaches(&self, peer: &PeerId) -> bool {
        self.sessions
            .get(peer)
            .is_some_and(|s| !s.0.is_ended() && s.0.paths().network)
    }

    /// Whether `peer`'s session has a Bluetooth path now.
    pub fn bluetooth_reaches(&self, peer: &PeerId) -> bool {
        self.sessions
            .get(peer)
            .is_some_and(|s| !s.0.is_ended() && s.0.paths().bluetooth)
    }

    /// `peer` greeted over the Bluetooth device at `address`; `dialled` when
    /// this node placed that call.
    pub fn note_bluetooth(&mut self, address: &str, peer: PeerId, dialled: bool) {
        self.bluetooth.insert(address.to_string(), peer);
        if dialled {
            self.bluetooth_dialled.insert(peer);
        }
    }

    /// The peer that answered on `address`, once one has.
    pub fn bluetooth_peer(&self, address: &str) -> Option<PeerId> {
        self.bluetooth.get(address).copied()
    }

    /// The sessions whose Bluetooth path this node placed and the network now
    /// carries as well: those paths are to be let go of. Forgets a placed
    /// path that is gone anyway.
    pub fn bluetooth_paths_to_release(&mut self) -> Vec<ResumeHandle> {
        let mut release = Vec::new();
        let sessions = &self.sessions;
        self.bluetooth_dialled.retain(|peer| {
            let Some(session) = sessions.get(peer).filter(|s| !s.0.is_ended()) else {
                return false;
            };
            let paths = session.0.paths();
            // Kept while the session lives even with no Bluetooth path: the
            // greeting is noted before the path it arrived on joins.
            if paths.network && paths.bluetooth {
                release.push(session.0.clone());
                return false;
            }
            true
        });
        release
    }

    /// Hold `handle` as `peer`'s session.
    pub fn hold_session(&mut self, peer: PeerId, handle: ResumeHandle) {
        self.sessions.insert(peer, HeldSession(handle));
    }

    /// `peer`'s session tagged `tag` ended; forget it unless a newer one
    /// replaced it already.
    pub fn session_ended(&mut self, peer: PeerId, tag: SessionTag) {
        if self.sessions.get(&peer).is_some_and(|s| s.0.tag() == tag) {
            self.sessions.remove(&peer);
        }
    }

    /// The sessions this node can resume, for its greeting.
    pub fn resumable_tags(&self) -> Vec<SessionTag> {
        self.sessions
            .values()
            .filter(|s| !s.0.is_ended())
            .map(|s| s.0.tag())
            .collect()
    }

    /// Decide what a connection from `peer`, holding `peer_tags`, is for.
    ///
    /// Both ends run this on the same two greetings, so they agree. A
    /// session the peer no longer holds — it restarted, or gave up — is
    /// ended here when it has no path left; one still carried by a path is
    /// left to the duplicate rule, since the peer may simply have greeted
    /// before that session existed.
    pub fn greeted(&mut self, peer: PeerId, peer_tags: &[SessionTag]) -> Greeted {
        let Some(ours) = self.sessions.get(&peer).map(|s| s.0.clone()) else {
            return Greeted::Fresh;
        };
        if ours.is_ended() {
            self.sessions.remove(&peer);
            return Greeted::Fresh;
        }
        if peer_tags.contains(&ours.tag()) {
            return Greeted::Join(ours);
        }
        let paths = ours.paths();
        if !paths.network && !paths.bluetooth {
            ours.end("the peer no longer holds this session");
            self.sessions.remove(&peer);
            // Its link stops holding the peer now, so the new one is not
            // judged against a session that is over.
            if let Some(held) = self.links.remove(&peer) {
                held.superseded.notify_one();
            }
        }
        Greeted::Fresh
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

    /// A live session over a loopback connection, and the far end kept
    /// alive beside it.
    async fn held_session() -> (ResumeHandle, cursio_transport::resume::ResumableLink) {
        use cursio_transport::resume::{PathKind, ResumableLink};

        let (near, far, tag) = path_pair(PathKind::Network).await;
        let (_link, handle) = ResumableLink::new(near, tag);
        let (far_link, _) = ResumableLink::new(far, tag);
        (handle, far_link)
    }

    /// An encrypted loopback connection of `kind`, both ends as paths.
    async fn path_pair(
        kind: cursio_transport::resume::PathKind,
    ) -> (
        cursio_transport::resume::Path,
        cursio_transport::resume::Path,
        cursio_protocol::SessionTag,
    ) {
        use cursio_transport::pairing::Role;
        use cursio_transport::tcp::TcpTransport;
        use cursio_transport::EncryptedTransport;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (dialled, accepted) = tokio::join!(TcpTransport::connect(addr), listener.accept());
        let near = dialled.unwrap();
        let far = TcpTransport::from_stream(accepted.unwrap().0).unwrap();
        let key = b"both ends hold this";
        let (near, far) = tokio::join!(
            EncryptedTransport::establish(near, key, Role::Initiator),
            EncryptedTransport::establish(far, key, Role::Responder),
        );
        let (near, far) = (near.unwrap(), far.unwrap());
        let tag = near.session_tag();
        (near.into_path(kind), far.into_path(kind), tag)
    }

    #[tokio::test]
    async fn a_bluetooth_path_this_node_placed_is_let_go_once_the_network_carries_too() {
        use cursio_transport::resume::{PathKind, ResumableLink};

        let (near, far, tag) = path_pair(PathKind::Network).await;
        let (_near_link, session) = ResumableLink::new(near, tag);
        let (_far_link, far_session) = ResumableLink::new(far, tag);
        let mut reg = PeerRegistry::default();
        reg.hold_session(peer(2), session.clone());
        reg.note_bluetooth("04:68:74:98:F7:C6", peer(2), true);
        assert_eq!(reg.bluetooth_peer("04:68:74:98:F7:C6"), Some(peer(2)));
        assert!(
            reg.bluetooth_paths_to_release().is_empty(),
            "the Bluetooth path has not joined yet"
        );

        let (near, far, _) = path_pair(PathKind::Bluetooth).await;
        let (a, b) = tokio::join!(session.attach(near), far_session.attach(far));
        a.unwrap();
        b.unwrap();
        assert!(reg.network_reaches(&peer(2)) && reg.bluetooth_reaches(&peer(2)));
        assert_eq!(reg.bluetooth_paths_to_release().len(), 1);
        assert!(
            reg.bluetooth_paths_to_release().is_empty(),
            "released once, not on every turn"
        );
    }

    #[tokio::test]
    async fn a_bluetooth_path_the_peer_placed_is_left_to_the_peer() {
        use cursio_transport::resume::{PathKind, ResumableLink};

        let (near, far, tag) = path_pair(PathKind::Network).await;
        let (_near_link, session) = ResumableLink::new(near, tag);
        let (_far_link, far_session) = ResumableLink::new(far, tag);
        let mut reg = PeerRegistry::default();
        reg.hold_session(peer(2), session.clone());
        reg.note_bluetooth("04:68:74:98:F7:C6", peer(2), false);
        let (near, far, _) = path_pair(PathKind::Bluetooth).await;
        let (a, b) = tokio::join!(session.attach(near), far_session.attach(far));
        a.unwrap();
        b.unwrap();
        assert!(reg.bluetooth_paths_to_release().is_empty());
    }

    #[tokio::test]
    async fn a_connection_joins_the_session_both_greetings_hold() {
        let (session, _far) = held_session().await;
        let mut reg = PeerRegistry::default();
        reg.hold_session(peer(2), session.clone());
        assert_eq!(reg.resumable_tags(), vec![session.tag()]);
        assert!(matches!(
            reg.greeted(peer(2), &[session.tag()]),
            Greeted::Join(_)
        ));
    }

    #[tokio::test]
    async fn a_session_the_peer_lost_is_ended_once_it_has_no_path() {
        let (session, _far) = held_session().await;
        let mut reg = PeerRegistry::default();
        reg.hold_session(peer(2), session.clone());
        session.network_lost();
        assert!(matches!(reg.greeted(peer(2), &[]), Greeted::Fresh));
        assert!(session.is_ended(), "a session nobody can resume was kept");
        assert!(reg.resumable_tags().is_empty());
    }

    #[tokio::test]
    async fn a_session_still_carried_is_not_ended_by_a_greeting_that_crossed_it() {
        let (session, _far) = held_session().await;
        let mut reg = PeerRegistry::default();
        reg.hold_session(peer(2), session.clone());
        assert!(matches!(reg.greeted(peer(2), &[]), Greeted::Fresh));
        assert!(
            !session.is_ended(),
            "the peer may have greeted before the session existed"
        );
    }

    #[tokio::test]
    async fn a_session_with_no_network_path_is_dialled_back() {
        let (session, _far) = held_session().await;
        let mut reg = PeerRegistry::default();
        reg.admit(
            peer(2),
            "laptop".into(),
            Some("127.0.0.1:9000".parse().unwrap()),
        );
        reg.set_state(peer(2), PeerState::Online);
        reg.hold_session(peer(2), session.clone());
        assert!(reg.dial_candidates().is_empty());
        session.network_lost();
        assert_eq!(
            reg.dial_candidates().len(),
            1,
            "the session's path was not dialled back"
        );
    }

    fn shape(dialled: bool, path: crate::control::LinkPath) -> LinkShape {
        LinkShape { dialled, path }
    }

    /// Both ends of a doubled link must drop the same socket, or each keeps
    /// the one the other closed and the pair ends up with none.
    #[test]
    fn both_sides_of_a_dial_collision_keep_the_same_link() {
        use crate::control::LinkPath::Network;
        let (low, high) = (peer(1), peer(2));
        // The call `low` placed, as each side sees it, and the call `high` placed.
        let (low_call_at_low, low_call_at_high) = (shape(true, Network), shape(false, Network));
        let (high_call_at_low, high_call_at_high) = (shape(false, Network), shape(true, Network));

        assert!(low_call_at_low.beats(high_call_at_low, Some(low), high));
        assert!(!high_call_at_low.beats(low_call_at_low, Some(low), high));
        assert!(low_call_at_high.beats(high_call_at_high, Some(high), low));
        assert!(!high_call_at_high.beats(low_call_at_high, Some(high), low));
    }

    #[test]
    fn the_network_replaces_bluetooth_and_never_the_other_way() {
        use crate::control::LinkPath::{Bluetooth, Network};
        let (local, remote) = (peer(1), peer(2));
        assert!(shape(false, Network).beats(shape(true, Bluetooth), Some(local), remote));
        assert!(!shape(true, Bluetooth).beats(shape(false, Network), Some(local), remote));
    }

    #[test]
    fn a_better_link_wakes_the_held_one_whose_end_then_leaves_the_peer_linked() {
        use crate::control::LinkPath::Network;
        let (local, remote) = (peer(1), peer(2));
        let mut reg = PeerRegistry {
            local: Some(local),
            ..Default::default()
        };
        let first = std::rc::Rc::new(tokio::sync::Notify::new());
        let held = reg.claim_link(remote, shape(false, Network), first.clone());
        let better = reg.claim_link(
            remote,
            shape(true, Network),
            std::rc::Rc::new(tokio::sync::Notify::new()),
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), first.notified())
                .await
                .expect("the replaced link was not told to end");
        });
        assert!(
            !reg.release_link(remote, held),
            "the replaced link took the peer offline"
        );
        assert!(reg.release_link(remote, better));
    }

    #[test]
    fn a_link_losing_to_the_held_one_is_refused_and_leaves_the_peer_linked() {
        use crate::control::LinkPath::Network;
        let (local, remote) = (peer(1), peer(2));
        let mut reg = PeerRegistry {
            local: Some(local),
            ..Default::default()
        };
        let held = std::rc::Rc::new(tokio::sync::Notify::new());
        let token = reg.claim_link(remote, shape(true, Network), held);
        assert!(token.is_some());

        let refused = reg.claim_link(
            remote,
            shape(false, Network),
            std::rc::Rc::new(tokio::sync::Notify::new()),
        );
        assert_eq!(refused, None);
        assert!(!reg.release_link(remote, refused));
        assert!(reg.release_link(remote, token));
        assert!(
            reg.release_link(remote, None),
            "a peer with no link is unlinked"
        );
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
            local: Some(me),
            ..Default::default()
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
