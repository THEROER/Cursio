//! Session modules: subsystems that share the one encrypted channel.
//!
//! A session used to be *either* input *or* audio, because each path split the
//! transport and ran its own loop — and only one owner can hold the single
//! Noise channel. Everything past input (audio today, window transfer next)
//! needs to run *alongside* input, not instead of it.
//!
//! A [`SessionModule`] is a subsystem that speaks its own [`Message`] variants
//! over the shared channel without owning it. [`crate::session::run_session`]
//! keeps ownership of the transport and lends each module a `tx` at three
//! points: once at startup, once per loop iteration to flush outbound work, and
//! whenever an inbound message isn't one the session handles itself.
//!
//! Multiplexing is by message *type*: modules are asked in turn whether a
//! message is theirs, and the first to claim it wins. Input stays hard-wired in
//! `run_session` — its ownership/grab/edge logic is deeply intertwined with the
//! loop's state — so modules cover the subsystems layered on top of it.

use async_trait::async_trait;
use cursio_protocol::{Message, ModuleAdvert, ModuleId, Negotiated};
use cursio_transport::{TransportError, TransportTx};

/// A subsystem multiplexed onto the session's encrypted channel.
///
/// Implementors hold their own state and never own the transport; the session
/// lends `tx` for the duration of each call.
#[async_trait]
pub trait SessionModule: Send {
    /// Human-readable name, for logs.
    fn name(&self) -> &'static str;

    /// Announce ourselves to the peer once the session is up. Called before the
    /// session loop starts. An error here aborts the session.
    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError>;

    /// Flush pending outbound work. Called once per loop iteration, *before*
    /// the session's `select!`.
    ///
    /// Must return promptly and must not drain its sources dry: the session
    /// can't reach its inbound branch while a module is pumping, so a module
    /// that keeps sending while data keeps arriving starves the peer's
    /// messages entirely. Send a bounded amount per call and return.
    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError>;

    /// Handle one message from the peer. Return `true` if it belonged to this
    /// module, which stops it being offered to other modules.
    fn handle_message(&mut self, msg: &Message) -> bool;

    /// Tear down cleanly when the session ends. Best-effort: the link may
    /// already be dead, so implementors should not treat send errors as fatal.
    async fn shutdown(&mut self, _tx: &mut dyn TransportTx) {}
}

/// What a module needs to know about the session it is being built for.
///
/// Constructors used to take these one at a time, threaded by hand through
/// `run_pairing_and_session`'s argument list — which is why that list grew to
/// thirteen. Passing one context instead means a new module that needs the
/// peer's name, or its device kind, adds no parameter to anything.
///
/// Borrowed, not owned: a context lives only for the duration of the build.
pub struct SessionCtx<'a> {
    /// This node's stable identity.
    pub local: &'a crate::NodeIdentity,
    /// Who we're talking to, and what they said they can do.
    pub peer: &'a crate::session::PeerHello,
    /// Which of this machine's screen edges borders the peer, if configured.
    ///
    /// The window module's drag trigger needs it — it compares the pointer
    /// against that edge itself, independently of who owns the cursor.
    pub peer_edge: Option<cursio_protocol::Edge>,
}

impl SessionCtx<'_> {
    /// The peer's display name — the single most common thing a module wants,
    /// for its logs.
    pub fn peer_name(&self) -> &str {
        &self.peer.display_name
    }
}

/// Builds one module, and knows whether it can run here at all.
///
/// The indirection exists so `main` stops naming every module. A node assembles
/// a `Vec<Box<dyn ModuleFactory>>` once at startup; each session then asks the
/// factories whose ids were negotiated to build, and nothing in the binary needs
/// a `#[cfg]` block or an `if` per module.
///
/// The three methods split cleanly by *when* they are answered: `id` always,
/// `advert` once at startup (before any peer is known), and `build` per session
/// (once the peer is known and negotiation has run).
/// `Send + Sync` because a registry is consulted while a link is being brought
/// up, and that happens across await points on whatever thread the runtime
/// chooses. Factories hold configuration, not live state, so this costs
/// nothing to satisfy.
pub trait ModuleFactory: Send + Sync {
    /// Which module this builds. Must match `advert`'s id.
    fn id(&self) -> ModuleId;

    /// How to advertise this module in `Hello`, or `None` when it can't run on
    /// this machine at all.
    ///
    /// `None` covers both "not configured" (the flag is off) and "no backend
    /// here" (window transfer outside Linux/Windows). Both mean the same thing
    /// on the wire — we don't have it — so the distinction stays local. This is
    /// where a module's `#[cfg]` belongs, rather than in `main`.
    fn advert(&self) -> Option<ModuleAdvert>;

    /// Construct the module for one session.
    ///
    /// Called only when negotiation agreed on this module's id, so an
    /// implementation may assume the peer has it too. An error here is not
    /// fatal to the session — the caller logs it and carries on without this
    /// module, because input and audio are still worth having when clipboard
    /// fails to start (rule 6 in `docs/platform-design.md`).
    fn build(&self, ctx: &SessionCtx) -> anyhow::Result<Box<dyn SessionModule>>;
}

/// Everything this node could run, whether or not any given peer wants it.
///
/// Built once per process. `advertise` is what goes in `Hello`; `build_agreed`
/// turns a negotiation result into the modules for one session.
#[derive(Default)]
pub struct ModuleRegistry {
    factories: Vec<Box<dyn ModuleFactory>>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a factory. Order is preserved, and it decides the order modules
    /// are offered inbound messages in — first claim wins, so a module that
    /// claims broadly should be registered last.
    pub fn with(mut self, factory: Box<dyn ModuleFactory>) -> Self {
        self.factories.push(factory);
        self
    }

    /// What to advertise in `Hello`: every module actually available here.
    pub fn advertise(&self) -> Vec<ModuleAdvert> {
        self.factories.iter().filter_map(|f| f.advert()).collect()
    }

    /// Build the modules for one session from what negotiation agreed on.
    ///
    /// A factory that fails is skipped with a log line rather than failing the
    /// session: losing clipboard should not cost you the cursor.
    pub fn build_agreed(
        &self,
        negotiated: &Negotiated,
        ctx: &SessionCtx,
    ) -> Vec<Box<dyn SessionModule>> {
        let mut built = Vec::new();
        for factory in &self.factories {
            if !negotiated.is_agreed(factory.id()) {
                continue;
            }
            match factory.build(ctx) {
                Ok(module) => {
                    tracing::info!(module = %factory.id(), "module built");
                    built.push(module);
                }
                Err(e) => {
                    tracing::error!(
                        module = %factory.id(),
                        error = %e,
                        "module failed to start; continuing without it"
                    );
                }
            }
        }
        built
    }
}
