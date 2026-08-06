//! Bringing a link up: find the peer, prove who we are, encrypt, agree on modules.
//!
//! Everything between "there is a socket" and "a session can start" lives here.
//! It is the same sequence for every kind of node — a desktop sharing its
//! cursor, a phone acting as a touchpad, a headless box serving files — because
//! none of those steps depend on what the machine can *do*:
//!
//! 1. [`Hello`](cursio_protocol::Hello), so each side knows who the other is
//!    and what it can run, before spending a password on it.
//! 2. Pairing on a first meeting, or the shared key recovered from the trust
//!    store on every meeting after.
//! 3. A Noise channel keyed by that secret.
//! 4. Module negotiation, and building the ones both sides agreed on.
//! 5. Deciding who holds the shared cursor ([`CursorSetup`]).
//!
//! ## Why this is a library and not part of the binary
//!
//! It was the middle of the CLI's `run_pairing_and_session`, wrapped around
//! things only a desktop has: an input injector, a capture handle, a cursor to
//! hide. A phone has none of those and needs all five steps above, so leaving
//! them there would mean a second implementation of pairing and Noise setup —
//! the two places where a subtle divergence is least affordable.
//!
//! What stays with the caller is what differs between nodes: what to do with
//! inbound input, whether there is anything to capture, and what to show the
//! user. [`Link::run_as_input_source`] is the one arrangement general enough to
//! belong here, because it is the whole of what a touchpad does.

use std::sync::mpsc;
use std::time::Duration;

use cursio_protocol::{InputEvent, ModuleAdvert, ModuleId, PeerId};
use cursio_transport::discovery::{self, DiscoveredPeer};
use cursio_transport::pairing::{self, Role, TrustStore};
use cursio_transport::{EncryptedTransport, Transport};

use crate::module::{ModuleRegistry, SessionCtx, SessionModule};
use crate::session::{self, CursorSetup, PeerHello, SessionError};
use crate::NodeIdentity;

/// Version of the input vocabulary this build speaks.
///
/// Input is not a [`SessionModule`] — its ownership and grab logic is woven
/// into the session loop itself — but it is still negotiated like one, so a
/// peer that cannot inject is not sent events it will drop.
pub const INPUT_VERSION: u16 = 1;

/// What a node needs to know to bring a link up.
///
/// A struct rather than a positional argument list: every caller sets most of these the
/// same way, and the ones that differ are the interesting ones.
pub struct Handshake<'a> {
    pub identity: &'a NodeIdentity,
    /// Used only on a first meeting; ignored once the peer is trusted.
    pub password: &'a str,
    /// Which half of the pairing and Noise handshakes this side runs. A
    /// property of the connection — whoever dialled — not a hierarchy.
    pub role: Role,
    pub registry: &'a ModuleRegistry,
    /// Where this machine stands in the user's layout.
    pub position: cursio_protocol::Position,
    /// What this machine can carry for the mesh, and what it wants from it.
    pub service: cursio_protocol::ServiceOffer,
    /// Whether this machine has input to send the peer.
    ///
    /// Follow the *capture*, not a command-line flag: capture is one
    /// machine-wide resource, and a session that could not borrow it genuinely
    /// has nothing to send, whatever the flag said. A phone sets this without
    /// any capture at all — its input comes from a touch surface.
    pub sends_input: bool,
}

/// A link whose peer has said hello but which is not yet encrypted.
///
/// The pause exists because callers need to act here and nowhere else. For an
/// *inbound* connection this is the first moment the peer is known at all — one
/// that dials us is an anonymous socket until it says hello — and the mesh
/// marks it online before the slower half of the setup runs.
///
/// Nothing confidential has crossed the wire yet, which is fine: the Hello
/// carries only what the peer chose to publish about itself.
pub struct Identified<T> {
    pub transport: T,
    pub peer: PeerHello,
    negotiated: cursio_protocol::Negotiated,
    /// Which edge borders this peer, worked out from the two positions as soon
    /// as it greeted us. Carried across the split rather than recomputed: the
    /// second half no longer has the peer's position to hand, and deriving the
    /// same answer twice is how the two halves start disagreeing.
    peer_edge: Option<cursio_protocol::Edge>,
}

impl<T> Identified<T> {
    /// The modules both sides agreed on, and why the rest were skipped.
    pub fn negotiated(&self) -> &cursio_protocol::Negotiated {
        &self.negotiated
    }
}

/// A link that is authenticated, encrypted, and ready for a session.
pub struct Link {
    pub transport: Box<dyn Transport>,
    pub peer: PeerHello,
    /// The modules both sides agreed on, already built.
    pub modules: Vec<Box<dyn SessionModule>>,
    /// Who holds the shared cursor, and whether the peer can hold it at all.
    pub cursor: CursorSetup,
    /// Which of this machine's edges borders this peer, derived from the two
    /// positions. The caller points capture at it: the answer is only knowable
    /// once the peer has said hello, which is after capture exists.
    pub peer_edge: Option<cursio_protocol::Edge>,
}

/// Say hello and work out which modules can run.
///
/// Split from the rest so a caller can act on the peer's identity before the
/// slower half — see [`Identified`]. [`establish`] runs both halves for callers
/// with nothing to do in between.
pub async fn identify<T>(mut transport: T, plan: &Handshake<'_>) -> anyhow::Result<Identified<T>>
where
    T: Transport,
{
    let advertised = advertise(plan.registry, plan.sends_input);
    let peer = session::handshake(
        &mut transport,
        plan.identity,
        session::local_device_kind(),
        &advertised,
        plan.position,
        plan.service,
    )
    .await?;
    // Derived per link, not per process: four machines in a row each border a
    // different neighbour, and a single machine-wide setting can only ever
    // state one of those answers.
    let peer_edge = plan.position.edge_toward(peer.position);
    tracing::info!(
        peer = %peer.display_name,
        id = %peer.peer_id,
        kind = ?peer.device_kind,
        position = ?peer.position,
        ?peer_edge,
        "handshake ok"
    );

    // Which modules actually run here. A module enabled on one side only is a
    // log line now, not a silent half-configuration.
    let negotiated = peer.negotiate(&advertised);
    for id in &negotiated.agreed {
        tracing::info!(module = %id, "module negotiated");
    }
    for (id, why) in &negotiated.skipped {
        tracing::warn!(module = %id, reason = %why, "module not running on this link");
    }

    Ok(Identified {
        transport,
        peer,
        negotiated,
        peer_edge,
    })
}

/// Pair if this is a first meeting, encrypt, and build the agreed modules.
///
/// Takes the identified link by value because pairing and the Noise handshake
/// both consume the transport: what comes back is the encrypted channel, and
/// the plaintext one no longer exists to be used by mistake.
pub async fn secure<T>(link: Identified<T>, plan: &Handshake<'_>) -> anyhow::Result<Link>
where
    T: Transport + 'static,
{
    let Identified {
        mut transport,
        peer,
        negotiated,
        peer_edge,
    } = link;

    // Pairing runs over the plaintext transport, which is safe: SPAKE2 never
    // sends the password itself, only a value derived from it.
    let mut store = TrustStore::load()?;
    let shared_key = match store.get(&peer.peer_id) {
        Some(trusted) => {
            tracing::info!("peer already trusted; skipping pairing");
            trusted.shared_key()?
        }
        None => {
            if plan.password.is_empty() {
                anyhow::bail!(
                    "peer {} is not trusted; run `cursio mesh --pair` on both machines",
                    peer.peer_id
                );
            }
            tracing::info!("new peer; running pairing");
            let paired = pairing::run_pairing(
                &mut transport,
                plan.password,
                plan.role,
                (peer.peer_id, peer.display_name.clone()),
            )
            .await?;
            let key = paired.shared_key.clone();
            store.add(&paired)?;
            tracing::info!("paired and trust stored ✓");
            key
        }
    };

    tracing::info!("establishing encrypted channel (Noise)");
    let secure = EncryptedTransport::establish(transport, &shared_key, plan.role).await?;
    tracing::info!("encrypted channel up ({})", secure.kind());

    // Built from what was *negotiated*, not from local flags alone: a module the
    // peer lacks would otherwise sit here advertising into a void.
    let ctx = SessionCtx {
        local: plan.identity,
        peer: &peer,
        peer_edge,
    };
    let modules = plan.registry.build_agreed(&negotiated, &ctx);

    let cursor = CursorSetup::negotiate(
        session::local_device_kind(),
        plan.identity.peer_id,
        peer.device_kind,
        peer.peer_id,
    );

    Ok(Link {
        peer_edge,
        transport: Box::new(secure),
        peer,
        modules,
        cursor,
    })
}

/// Bring a link all the way up, for callers with nothing to do in between.
pub async fn establish<T>(transport: T, plan: Handshake<'_>) -> anyhow::Result<Link>
where
    T: Transport + 'static,
{
    let identified = identify(transport, &plan).await?;
    secure(identified, &plan).await
}

impl Link {
    /// Run this link as a device that only *produces* input — a phone acting as
    /// a touchpad, and anything else with no screen for the cursor to enter.
    ///
    /// The whole arrangement follows from staying dormant, which
    /// [`CursorSetup::negotiate`] guarantees for such a device: a dormant
    /// session forwards everything on its capture channel to the peer, so
    /// feeding `input` from a touch surface *is* the touchpad. There is nothing
    /// to inject here and no ownership to hand around, so both of those
    /// callbacks are empty rather than absent — the session still reports state
    /// changes, they simply have nothing to change.
    pub async fn run_as_input_source(
        self,
        input: &mut tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    ) -> Result<(), SessionError> {
        debug_assert!(
            !self.cursor.start_active,
            "an input source must stay dormant, or its events act locally instead of \
             reaching the peer"
        );
        session::run_session(
            self.transport,
            self.cursor,
            Some(input),
            |_| {},
            None,
            |_| {},
            self.modules,
        )
        .await
    }
}

/// What this node offers on a link.
///
/// Input is appended by hand because it is not a registry module; everything
/// else comes from the factories, each of which knows whether it can run on
/// this platform.
pub fn advertise(registry: &ModuleRegistry, sends_input: bool) -> Vec<ModuleAdvert> {
    let mut adverts = registry.advertise();
    if sends_input {
        adverts.push(ModuleAdvert::symmetric(ModuleId::INPUT, INPUT_VERSION));
    }
    adverts
}

/// Build the discovery config, pinning mDNS to the detected LAN interface when
/// one is found.
///
/// On hosts cluttered with VPNs (WireGuard, Tailscale, Hamachi) this keeps
/// multicast off interfaces that reject it. Falls back to the default
/// (auto-exclude known-virtual ranges) if no LAN address is detected.
pub fn discovery_config() -> discovery::DiscoveryConfig {
    match discovery::guess_lan_ipv4() {
        Some(addr) => {
            tracing::info!(%addr, "pinning discovery to LAN interface");
            discovery::DiscoveryConfig::pinned_to(addr)
        }
        None => {
            tracing::warn!("could not detect a LAN interface; using default discovery config");
            discovery::DiscoveryConfig::default()
        }
    }
}

/// Browse until one peer is found or the timeout elapses.
///
/// Browsing is blocking and lives on its own thread; the caller gets an async
/// wait for the first answer. Good enough for "connect me to whatever is
/// there", which is what a phone's first screen needs.
pub async fn discover_one(self_id: PeerId, timeout: Duration) -> Option<DiscoveredPeer> {
    let cfg = discovery_config();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = discovery::browse_with(self_id, &cfg, |peer| {
            let _ = tx.send(peer);
        });
    });
    rx.recv_timeout(timeout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node with input to send must say so, or the peer skips the input
    /// vocabulary and every event is dropped as unnegotiated.
    #[test]
    fn a_node_that_sends_input_advertises_it() {
        let registry = ModuleRegistry::default();
        let with = advertise(&registry, true);
        let without = advertise(&registry, false);
        assert!(with.iter().any(|a| a.id == ModuleId::INPUT));
        assert!(!without.iter().any(|a| a.id == ModuleId::INPUT));
    }
}
