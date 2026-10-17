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
//! user — a phone's touchpad, for one, is only a writer on its session's
//! capture channel.

use cursio_protocol::{ModuleAdvert, ModuleId};
use cursio_transport::discovery;
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
    /// This machine's power and network state as the link comes up. Read once
    /// here rather than per link: a node brings several links up at the same
    /// moment, and re-probing sysfs for each would report the same answer.
    pub status: cursio_protocol::NodeStatus,
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
    T: Transport + 'static,
{
    let advertised = advertise(plan.registry, plan.sends_input);
    let peer = match session::handshake(
        &mut transport,
        plan.identity,
        session::local_device_kind(),
        &advertised,
        plan.position,
        plan.service,
        plan.status,
    )
    .await
    {
        Ok(peer) => peer,
        Err(SessionError::UpdateOnly(preamble)) => {
            return Err(carry_updates_only(transport, plan, preamble).await)
        }
        Err(e) => return Err(e.into()),
    };
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
        // Empty for a peer that reports nothing, which is what an older build
        // and a platform with no backend both look like.
        status = %peer.status,
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

/// A peer on another protocol version: no session, but the update lane, so
/// whichever of the two is behind can still be handed a build.
///
/// Encrypted with the pair's stored key like any link — the lane carries
/// signed builds, so the signature and not the channel is what vouches for
/// them, but a stranger on the network still learns nothing and can offer
/// nothing. Always ends in an error: the caller's link is over either way,
/// and the mesh dials again later, by which time one of the two may have
/// switched.
async fn carry_updates_only<T>(
    transport: T,
    plan: &Handshake<'_>,
    preamble: cursio_protocol::update::Preamble,
) -> anyhow::Error
where
    T: Transport + 'static,
{
    let mismatch = format!(
        "peer {} runs build {} speaking protocol {}, this build speaks {}",
        preamble.peer_id,
        preamble.build,
        preamble.protocol_version,
        cursio_protocol::PROTOCOL_VERSION
    );
    let Some(agent) = crate::update::agent::global() else {
        return anyhow::anyhow!("{mismatch}; updates are not running on this node");
    };
    let trusted = match TrustStore::load() {
        Ok(store) => store.get(&preamble.peer_id).cloned(),
        Err(e) => return anyhow::anyhow!("{mismatch}; cannot read the trust store: {e}"),
    };
    let Some(trusted) = trusted else {
        return anyhow::anyhow!("{mismatch}; it is not paired with this machine");
    };
    let key = match trusted.shared_key() {
        Ok(key) => key,
        Err(e) => return anyhow::anyhow!("{mismatch}; its stored key is unreadable: {e}"),
    };
    tracing::info!(peer = %trusted.display_name, %mismatch, "carrying only updates on this link");
    let secure = match EncryptedTransport::establish(transport, &key, plan.role).await {
        Ok(secure) => secure,
        Err(e) => return anyhow::anyhow!("{mismatch}; the encrypted channel failed: {e}"),
    };
    match crate::update::lane::run_alone(secure, agent, trusted.display_name.clone()).await {
        Ok(()) => anyhow::anyhow!("{mismatch}; only updates crossed"),
        Err(e) => anyhow::anyhow!("{mismatch}; the update lane failed: {e}"),
    }
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
    let lan = discovery::lan_interface_addrs();
    if lan.is_empty() {
        tracing::info!("no LAN interface yet; discovery starts using one as soon as it appears");
    } else {
        tracing::info!(addrs = ?lan, "discovery on the LAN interfaces");
    }
    discovery::DiscoveryConfig::lan()
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
