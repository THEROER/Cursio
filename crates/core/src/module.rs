//! Session modules: subsystems that share the one encrypted channel.
//!
//! A session used to be *either* input *or* audio, because each path split the
//! transport and ran its own loop — and only one owner can hold the single
//! Noise channel. Everything past input needs to run *alongside* input, not
//! instead of it.
//!
//! A [`SessionModule`] is a subsystem that speaks its own [`Message`] variants
//! over the shared channel without owning it. Each one runs on a thread of its
//! own ([`crate::module_host`]), sending through its own handle onto the
//! link's priority queue and receiving only the messages that name it
//! ([`Message::module_id`]), which the reader routes straight to it.
//!
//! They used to share the session loop, lent a `tx` once per turn and offered
//! every inbound message in turn. That loop also carries the shared cursor, so
//! any module that waited on anything froze the mouse — the trait's methods
//! were always meant to return promptly, and "meant to" did not survive a
//! PipeWire thread join or a disk read under a lock. On its own thread a slow
//! module delays only itself.
//!
//! Input stays hard-wired in `run_session`: its ownership/grab/edge logic is
//! ordering-sensitive and `!Send`, and is the one thing that loop is for now.

use async_trait::async_trait;
use cursio_protocol::{Message, ModuleAdvert, ModuleId, Negotiated};
use cursio_transport::{TransportError, TransportTx};

/// A subsystem multiplexed onto the session's encrypted channel.
///
/// Implementors hold their own state and never own the transport; `tx` is the
/// module's own handle onto the link, lent for each call by the host running
/// it. Every method is called on that one thread, in order, so a module needs
/// no locking against itself.
#[async_trait]
pub trait SessionModule: Send {
    /// Human-readable name, for logs and the thread's name.
    fn name(&self) -> &'static str;

    /// Which module this is: the key the reader routes inbound messages by.
    /// Must match the id its factory advertises.
    fn id(&self) -> ModuleId;

    /// Announce ourselves to the peer once the session is up. Called first,
    /// before anything is pumped or handled. An error here ends the session.
    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError>;

    /// Flush pending outbound work. Called at the module's own pace: every few
    /// milliseconds while it has traffic, more slowly once it is quiet, and
    /// straight after it handles inbound messages.
    ///
    /// Send a bounded amount per call and return: while this runs, this
    /// module's inbound messages wait. A slow pump no longer holds up input or
    /// any other module, but it still holds up its own replies.
    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError>;

    /// Whether this module has work outstanding that an answer will complete.
    ///
    /// A module drops to an idle pace once it has *sent* nothing for a short
    /// while, which is the wrong test for a module that spends its time
    /// waiting: a mount serving a download sends little and owes much, so it
    /// was judged idle, slept 50 ms, and delivered the next answer a tick
    /// late. Measured against Steam that was 111 ms per write on a link with a
    /// 10 ms round trip, and 391 trips into the idle pace during a single
    /// transfer.
    ///
    /// Answering `true` keeps the module at its fast pace. Modules that have
    /// nothing pending — the common case, and the whole point of the idle pace
    /// — leave this alone.
    fn has_pending_work(&self) -> bool {
        false
    }

    /// Handle one message from the peer. Only messages whose
    /// [`Message::module_id`] is this module's arrive here; return `false` for
    /// one of those this build does not recognise, which is logged.
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

/// Which modules the user has switched on, readable from every link at once.
///
/// Separate from the factories because it changes while they do not: a
/// factory says whether a module *can* run here, a switch whether the user
/// *wants* it to, and the app flips switches on a running node. A module with
/// no entry is on — the service module has no switch at all.
///
/// Shared by `Arc` because the control socket writes it from its own task and
/// links read it wherever they are being brought up; it holds a handful of
/// booleans and is never held across an await.
#[derive(Clone, Default)]
pub struct ModuleSwitches {
    on: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<ModuleId, bool>>>,
}

impl ModuleSwitches {
    pub fn is_on(&self, id: ModuleId) -> bool {
        self.read().get(&id).copied().unwrap_or(true)
    }

    /// Answers whether that changed anything.
    pub fn set(&self, id: ModuleId, on: bool) -> bool {
        let mut map = self.on.write().unwrap_or_else(|p| p.into_inner());
        map.insert(id, on) != Some(on)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, std::collections::HashMap<ModuleId, bool>> {
        self.on.read().unwrap_or_else(|p| p.into_inner())
    }
}

/// Everything this node could run, whether or not any given peer wants it.
///
/// Built once per process. `advertise` is what goes in `Hello`; `build_agreed`
/// turns a negotiation result into the modules for one session.
#[derive(Default)]
pub struct ModuleRegistry {
    factories: Vec<Box<dyn ModuleFactory>>,
    switches: ModuleSwitches,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a factory. Order is preserved, and it is the order modules are
    /// started in; inbound messages are routed by id, not by order.
    pub fn with(mut self, factory: Box<dyn ModuleFactory>) -> Self {
        self.factories.push(factory);
        self
    }

    /// Read the user's switches from `switches` rather than a private set, so
    /// whoever holds the same handle turns modules on and off.
    pub fn with_switches(mut self, switches: ModuleSwitches) -> Self {
        self.switches = switches;
        self
    }

    pub fn switches(&self) -> &ModuleSwitches {
        &self.switches
    }

    /// What to advertise in `Hello`: every module available here that the
    /// user has switched on.
    ///
    /// A switched-off factory is not even asked: asking can cost something,
    /// such as the one-time privilege prompt USB sharing raises.
    pub fn advertise(&self) -> Vec<ModuleAdvert> {
        self.factories
            .iter()
            .filter(|f| self.switches.is_on(f.id()))
            .filter_map(|f| f.advert())
            .collect()
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
        self.build(&negotiated.agreed, ctx)
    }

    /// Build the modules named in `ids`, in registration order.
    ///
    /// What a running link calls when renegotiation agrees a module it was
    /// not running; [`Self::build_agreed`] is the same for a link coming up.
    pub fn build(&self, ids: &[ModuleId], ctx: &SessionCtx) -> Vec<Box<dyn SessionModule>> {
        let mut built = Vec::new();
        for factory in &self.factories {
            if !ids.contains(&factory.id()) {
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
