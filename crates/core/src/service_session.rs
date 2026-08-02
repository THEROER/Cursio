//! Carrying "I cannot do this, will you?" between machines.
//!
//! The policy half of [`cursio_protocol::service`]. A duty that must live on
//! exactly one machine — hosting the AirPlay receiver, so far — cannot be
//! placed by each node ranking itself against the others, because ranking
//! answers "who should" and the question is "who *can*". A Mac is reachable,
//! willing, and still unable to hold port 7000, because Apple's own receiver
//! has it. Ranking hands it the duty; nothing happens; the mesh has no
//! receiver and no line in any log says why.
//!
//! So the machine that cannot serve says so, out loud, and a peer that can
//! answers. What arrives here is the *decision* — this node is now hosting
//! something the user started elsewhere — which the rest of the process reads
//! through a [`ServiceState`] handle rather than by being called back.
//!
//! ## Why a shared handle and not a channel
//!
//! The AirPlay receiver is bound before any session exists and outlives all of
//! them; several peer sessions run at once, each with its own module instance.
//! A handle every instance writes and the receiver reads is the only shape
//! where "somebody asked me to host this" survives the session that carried
//! the request going away.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cursio_protocol::{Message, ModuleId, PeerId, Priority, ServiceId, ServiceMsg};
use cursio_transport::{TransportError, TransportTx};

use crate::module::SessionModule;

/// Every duty this build knows how to ask about or carry.
///
/// One list, walked by both the asking and the forgetting paths, so a duty
/// added later cannot be wired into one and missed by the other.
const KNOWN_SERVICES: &[ServiceId] = &[ServiceId::AIRPLAY];

/// Version of this module's wire vocabulary ([`ServiceMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

/// What this machine has been asked to carry, and what it has asked for.
///
/// Shared between every peer session and whatever actually performs the duty.
/// Cheap to clone; the lock is held only across a set insertion.
#[derive(Clone, Default)]
pub struct ServiceState {
    inner: Arc<Mutex<Inner>>,
}

/// Which peer is carrying a duty, and where it answers.
#[derive(Clone, Copy)]
struct HostedBy {
    peer: PeerId,
    /// Where the duty itself is reachable — a receiver binds its own port, not
    /// the mesh link's. `None` when the peer accepted a request but has not
    /// said where yet.
    addr: Option<std::net::SocketAddr>,
}

#[derive(Default)]
struct Inner {
    /// Duties a peer has asked this machine to take on.
    asked_of_us: HashSet<ServiceId>,
    /// Duties this machine has asked a peer to take on, so a second session
    /// does not repeat a request already answered.
    asked_by_us: HashSet<ServiceId>,
    /// Duties some peer has confirmed it is actually running: which peer, and
    /// where it answers.
    ///
    /// The peer id is not decoration. A session ending has to forget what *that*
    /// peer was carrying and nothing else — with three machines, B losing its
    /// link to C would otherwise make B forget that A holds the receiver, and B
    /// would announce a second one.
    hosted_elsewhere: std::collections::HashMap<ServiceId, HostedBy>,
    /// Duties any peer has answered a query about, either way.
    ///
    /// Distinct from `hosted_elsewhere` being empty, which means both "no peer
    /// hosts this" and "nobody has said yet" — and those need opposite
    /// behaviour from whatever is deciding whether to advertise. Acting on the
    /// second as though it were the first is how a node that correctly stood
    /// down at startup put its announcement straight back a second later.
    answered: HashSet<ServiceId>,
    /// Duties this machine is carrying, and the address they answer on — what
    /// a peer's [`ServiceMsg::Query`] is answered with.
    hosted_here: std::collections::HashMap<ServiceId, std::net::SocketAddr>,
}

impl ServiceState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that this machine cannot carry `service`, so every session that
    /// comes up asks its peer about it.
    ///
    /// Set once, by whatever failed. A session that connects later still sees
    /// it, which is what makes the request survive being made before any peer
    /// was reachable.
    pub fn needs_host(&self, service: ServiceId) {
        self.lock().asked_by_us.insert(service);
    }

    /// Whether this machine is looking for somewhere to put `service`.
    pub fn is_looking_for_host(&self, service: ServiceId) -> bool {
        self.lock().asked_by_us.contains(&service)
    }

    /// Whether a peer has confirmed it is running `service`.
    pub fn is_hosted_elsewhere(&self, service: ServiceId) -> bool {
        self.lock().hosted_elsewhere.contains_key(&service)
    }

    /// Forget what `peer` was carrying, because its link has gone.
    ///
    /// Scoped to that peer: another one may still be hosting the same duty,
    /// and dropping its answer too would have this machine announce a second
    /// receiver over the top of a working one.
    pub fn forget_peer(&self, peer: PeerId) {
        self.lock()
            .hosted_elsewhere
            .retain(|_, hosted| hosted.peer != peer);
    }

    /// Whether any peer has answered a query about `service` yet, either way.
    ///
    /// "Nobody hosts it" and "nobody has spoken" are the same absence in
    /// [`Self::is_hosted_elsewhere`], and they call for opposite decisions.
    pub fn has_been_answered(&self, service: ServiceId) -> bool {
        self.lock().answered.contains(&service)
    }

    /// Where a peer said it is running `service`, if it said.
    pub fn host_address(&self, service: ServiceId) -> Option<std::net::SocketAddr> {
        self.lock()
            .hosted_elsewhere
            .get(&service)
            .and_then(|hosted| hosted.addr)
    }

    /// Record that this machine is carrying `service`, reachable at `addr`.
    ///
    /// Set by whatever actually started it, so a peer's query is answered from
    /// what is running rather than from what was intended.
    pub fn now_hosting(&self, service: ServiceId, addr: std::net::SocketAddr) {
        self.lock().hosted_here.insert(service, addr);
    }

    /// Whether this machine is carrying `service`.
    pub fn is_hosted_here(&self, service: ServiceId) -> bool {
        self.lock().hosted_here.contains_key(&service)
    }

    /// Whether a peer has asked this machine to take `service` on.
    pub fn was_asked_to_host(&self, service: ServiceId) -> bool {
        self.lock().asked_of_us.contains(&service)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock here would mean a panic while holding it, which none
        // of these three-line critical sections can do. Recovering keeps a
        // panic in one session from disabling service placement everywhere.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Decides what this machine answers when a peer asks it to host something.
///
/// A trait rather than a closure because the answer is genuinely per-duty and
/// involves real resources: hosting AirPlay means binding port 7000, and the
/// only honest way to answer is to try.
pub trait ServiceHost: Send {
    /// Take `service` on, or explain why not.
    ///
    /// Called at most once per duty per session. The reason is the peer's
    /// words for what it could not do, carried so a machine that suddenly
    /// starts hosting something can say in its own log why.
    fn accept(&mut self, service: ServiceId, reason: &str) -> Result<(), String>;
}

/// A host that refuses everything, naming the duty.
///
/// The default for a node with nothing to offer. Refusing is not a failure
/// mode: it is how the asking machine learns to keep looking rather than
/// waiting on a peer that silently ignored it.
pub struct RefuseAll;

impl ServiceHost for RefuseAll {
    fn accept(&mut self, service: ServiceId, _reason: &str) -> Result<(), String> {
        Err(format!("this machine does not host {service}"))
    }
}

/// The session module that exchanges [`ServiceMsg`]s.
pub struct ServiceEngine {
    peer_name: String,
    /// Which peer this session talks to, so what it said can be forgotten on
    /// its own when the link goes — and only what *it* said.
    peer_id: PeerId,
    /// Whether this peer wins a tie when both machines carry the same duty.
    ///
    /// Lowest peer id, which both sides compute identically from ids they have
    /// already exchanged — so no message is needed to settle it and the two
    /// answers cannot disagree. See [`ServiceEngine::yield_to`].
    peer_outranks_us: bool,
    state: ServiceState,
    host: Box<dyn ServiceHost>,
    /// Queued for the next pump, because `handle_message` is synchronous while
    /// sending is not.
    outbound: Vec<ServiceMsg>,
    /// Requests already sent on this link, so a pump does not repeat them.
    requested: HashSet<ServiceId>,
    /// Whether the opening round of queries has gone out on this link.
    queried: bool,
    /// Why this machine cannot carry each duty it is asking about.
    reasons: Arc<Mutex<std::collections::HashMap<ServiceId, String>>>,
}

impl ServiceEngine {
    pub fn new(
        peer_name: String,
        peer_id: PeerId,
        peer_outranks_us: bool,
        state: ServiceState,
        host: Box<dyn ServiceHost>,
        reasons: Arc<Mutex<std::collections::HashMap<ServiceId, String>>>,
    ) -> Self {
        Self {
            peer_name,
            peer_id,
            peer_outranks_us,
            state,
            host,
            outbound: Vec::new(),
            requested: HashSet::new(),
            queried: false,
            reasons,
        }
    }

    /// Whether a peer's "I am hosting this" should make this machine stand down.
    ///
    /// Only interesting when *both* are hosting, which is the ordinary opening
    /// of a mesh: each node binds its port before any peer has said hello,
    /// because the alternative is a receiver that appears seconds late. Both
    /// then answer a [`ServiceMsg::Query`] truthfully with `hosting: true`.
    ///
    /// Without a tie-break each believed the other had it and *both* withdrew,
    /// leaving an iPhone with no `Cursio` at all while two machines sat holding
    /// port 7000. Yielding to the lower peer id settles it in one exchange with
    /// no extra message: the two sides compute opposite answers from the same
    /// pair of ids, so exactly one stands down.
    fn yield_to(&self, service: ServiceId) -> bool {
        !self.state.is_hosted_here(service) || self.peer_outranks_us
    }

    /// Answer one request, by actually trying to take the duty on.
    fn answer(&mut self, service: ServiceId, reason: String) -> ServiceMsg {
        if !service.is_known() {
            // Refused rather than accepted-and-ignored: an older build that
            // swallowed an unknown duty would leave the requester believing it
            // had found a host.
            return ServiceMsg::Refused {
                service,
                reason: format!("this build does not know {service}"),
            };
        }
        self.state.lock().asked_of_us.insert(service);
        match self.host.accept(service, &reason) {
            Ok(()) => {
                tracing::info!(
                    peer = %self.peer_name,
                    %service,
                    peer_reason = %reason,
                    "hosting a service for a peer that could not"
                );
                ServiceMsg::Accepted { service }
            }
            Err(why) => {
                tracing::info!(peer = %self.peer_name, %service, reason = %why, "declined to host");
                ServiceMsg::Refused {
                    service,
                    reason: cursio_protocol::service::truncate_reason(&why),
                }
            }
        }
    }
}

#[async_trait]
impl SessionModule for ServiceEngine {
    fn name(&self) -> &'static str {
        "service"
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.pump(tx).await
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // Ask before deciding anything. A machine that works out on its own
        // whether to host is a machine that reaches the same answer as its
        // peer, and the user is shown two receivers called Cursio.
        if !self.queried {
            self.queried = true;
            for &service in KNOWN_SERVICES {
                self.outbound.push(ServiceMsg::Query { service });
            }
        }

        // Ask about anything this machine has found it cannot do. Checked every
        // pump rather than once at startup, because the thing that fails — a
        // port bind — may fail long after the session came up.
        for &service in KNOWN_SERVICES {
            if !self.state.is_looking_for_host(service)
                || self.state.is_hosted_elsewhere(service)
                || !self.requested.insert(service)
            {
                continue;
            }
            let reason = self
                .reasons
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&service)
                .cloned()
                .unwrap_or_else(|| "this machine cannot host it".to_string());
            tracing::info!(peer = %self.peer_name, %service, %reason, "asking a peer to host");
            self.outbound.push(ServiceMsg::Request {
                service,
                reason: cursio_protocol::service::truncate_reason(&reason),
            });
        }

        for msg in std::mem::take(&mut self.outbound) {
            // Control priority: these are a handful of tiny frames that decide
            // whether a subsystem exists at all, and waiting behind a file
            // transfer for that is the difference between a receiver appearing
            // now and appearing in a minute.
            tx.send(&Message::module(
                ModuleId::SERVICE,
                Priority::Control,
                &msg,
            )?)
            .await?;
        }
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        let decoded: ServiceMsg = match msg.decode_module(ModuleId::SERVICE) {
            Ok(Some(decoded)) => decoded,
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!(peer = %self.peer_name, %error, "undecodable service message");
                return true;
            }
        };
        match decoded {
            ServiceMsg::Query { service } => {
                let addr = self.state.lock().hosted_here.get(&service).copied();
                self.outbound.push(ServiceMsg::Status {
                    service,
                    hosting: addr.is_some(),
                    addr,
                });
            }
            ServiceMsg::Status {
                service,
                hosting,
                addr,
            } => {
                // Recorded before the tie-break, because the fact worth
                // remembering is that a peer spoke at all — even when what it
                // said does not change anything here.
                self.state.lock().answered.insert(service);
                if hosting && !self.yield_to(service) {
                    // Both of us have it and we win the tie. Say so rather than
                    // logging nothing: from the peer's side this is the moment
                    // it stands down, and the two lines belong together when
                    // someone reads the pair of logs afterwards.
                    tracing::info!(
                        peer = %self.peer_name,
                        %service,
                        "peer hosts this too, but this machine keeps it (lower id)"
                    );
                } else if hosting {
                    tracing::info!(peer = %self.peer_name, %service, ?addr, "peer already hosts this");
                    self.state.lock().hosted_elsewhere.insert(
                        service,
                        HostedBy {
                            peer: self.peer_id,
                            addr,
                        },
                    );
                } else {
                    // Removed rather than left: an answer of "not me" from the
                    // peer that used to host it is how this machine learns the
                    // duty is free again.
                    self.state.lock().hosted_elsewhere.remove(&service);
                }
            }
            ServiceMsg::Request { service, reason } => {
                let answer = self.answer(service, reason);
                self.outbound.push(answer);
            }
            ServiceMsg::Accepted { service } => {
                tracing::info!(peer = %self.peer_name, %service, "peer is hosting this for us");
                self.state.lock().hosted_elsewhere.insert(
                    service,
                    HostedBy {
                        peer: self.peer_id,
                        addr: None,
                    },
                );
            }
            ServiceMsg::Refused { service, reason } => {
                // Not an error: the whole point of asking several peers is that
                // some of them cannot either. Left in `asked_by_us`, so the next
                // peer to connect is asked in turn.
                tracing::info!(peer = %self.peer_name, %service, %reason, "peer cannot host it");
            }
            ServiceMsg::Released { service } => {
                tracing::info!(peer = %self.peer_name, %service, "peer gave up hosting it");
                self.state.lock().hosted_elsewhere.remove(&service);
                // Ask again on the next pump: whoever wanted it still does.
                self.requested.remove(&service);
            }
        }
        true
    }

    async fn shutdown(&mut self, _tx: &mut dyn TransportTx) {
        // What *this* peer was hosting stops being true the moment its link
        // goes, whether or not it said so. Leaving it set would keep this
        // machine believing a receiver exists somewhere it can no longer reach
        // — which is exactly what has to be noticed for the duty to move to
        // another machine.
        self.state.forget_peer(self.peer_id);
    }
}

/// Builds [`ServiceEngine`]. Always available: asking is cheap, and a node with
/// nothing to offer still needs to be able to *ask*.
pub struct ServiceFactory {
    pub state: ServiceState,
    /// Why this machine cannot carry each duty, filled in by whatever failed.
    pub reasons: Arc<Mutex<std::collections::HashMap<ServiceId, String>>>,
    /// What answers a peer's request. Boxed per session by this closure, since
    /// a host is `!Sync` and each session needs its own.
    pub make_host: Box<dyn Fn() -> Box<dyn ServiceHost> + Send + Sync>,
}

impl crate::module::ModuleFactory for ServiceFactory {
    fn id(&self) -> ModuleId {
        ModuleId::SERVICE
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        Some(cursio_protocol::ModuleAdvert::symmetric(
            ModuleId::SERVICE,
            VERSION,
        ))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(ServiceEngine::new(
            ctx.peer_name().to_string(),
            ctx.peer.peer_id,
            // Lowest id wins a tie. Computed here, from the greeting, because
            // it is a property of the pair rather than of either machine.
            ctx.peer.peer_id < ctx.local.peer_id,
            self.state.clone(),
            (self.make_host)(),
            self.reasons.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that records what it was asked and answers as told.
    struct ScriptedHost {
        accept: bool,
        seen: Seen,
    }

    impl ServiceHost for ScriptedHost {
        fn accept(&mut self, service: ServiceId, reason: &str) -> Result<(), String> {
            self.seen
                .lock()
                .unwrap()
                .push((service, reason.to_string()));
            if self.accept {
                Ok(())
            } else {
                Err("port is busy here too".into())
            }
        }
    }

    /// What the scripted host recorded, shared with the test that built it.
    type Seen = Arc<Mutex<Vec<(ServiceId, String)>>>;

    fn engine(accept: bool) -> (ServiceEngine, ServiceState, Seen) {
        engine_ranked(accept, false)
    }

    /// `peer_outranks_us` decides who keeps a duty both machines are carrying.
    fn engine_ranked(accept: bool, peer_outranks_us: bool) -> (ServiceEngine, ServiceState, Seen) {
        engine_for(accept, peer_outranks_us, PeerId::new_v4())
    }

    /// A session with a named peer, for the cases where *which* peer said
    /// something is the point.
    fn engine_for(
        accept: bool,
        peer_outranks_us: bool,
        peer_id: PeerId,
    ) -> (ServiceEngine, ServiceState, Seen) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = ServiceState::new();
        let engine = ServiceEngine::new(
            "peer".into(),
            peer_id,
            peer_outranks_us,
            state.clone(),
            Box::new(ScriptedHost {
                accept,
                seen: seen.clone(),
            }),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        (engine, state, seen)
    }

    fn request(reason: &str) -> Message {
        Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Request {
                service: ServiceId::AIRPLAY,
                reason: reason.into(),
            },
        )
        .unwrap()
    }

    /// The case the whole module exists for: a Mac that cannot bind 7000 asks,
    /// and the machine that can answers by actually taking it.
    #[test]
    fn a_request_is_answered_by_trying_to_host_it() {
        let (mut engine, state, seen) = engine(true);
        assert!(engine.handle_message(&request("port 7000 is in use")));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[(ServiceId::AIRPLAY, "port 7000 is in use".to_string())]
        );
        assert!(state.was_asked_to_host(ServiceId::AIRPLAY));
        assert_eq!(
            engine.outbound,
            vec![ServiceMsg::Accepted {
                service: ServiceId::AIRPLAY
            }]
        );
    }

    /// A peer that also cannot must say so. Silence is indistinguishable from a
    /// message that never arrived, and the asker would wait forever.
    #[test]
    fn a_peer_that_cannot_host_refuses_with_a_reason() {
        let (mut engine, _state, _seen) = engine(false);
        engine.handle_message(&request("port 7000 is in use"));
        let ServiceMsg::Refused { service, reason } = &engine.outbound[0] else {
            panic!("expected a refusal, got {:?}", engine.outbound);
        };
        assert_eq!(*service, ServiceId::AIRPLAY);
        assert!(
            reason.contains("busy"),
            "the reason should say why: {reason}"
        );
    }

    /// An older build must not accept a duty it does not implement, or the
    /// asker stops looking for a host that will never appear.
    #[test]
    fn an_unknown_duty_is_refused_without_consulting_the_host() {
        let (mut engine, _state, seen) = engine(true);
        let msg = Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Request {
                service: ServiceId(999),
                reason: "whatever".into(),
            },
        )
        .unwrap();
        engine.handle_message(&msg);
        assert!(
            seen.lock().unwrap().is_empty(),
            "the host must not be asked"
        );
        assert!(matches!(engine.outbound[0], ServiceMsg::Refused { .. }));
    }

    #[test]
    fn an_acceptance_records_that_somebody_else_is_hosting_it() {
        let (mut engine, state, _seen) = engine(true);
        assert!(!state.is_hosted_elsewhere(ServiceId::AIRPLAY));
        let msg = Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Accepted {
                service: ServiceId::AIRPLAY,
            },
        )
        .unwrap();
        engine.handle_message(&msg);
        assert!(state.is_hosted_elsewhere(ServiceId::AIRPLAY));
    }

    /// A link going down takes the host with it, said so or not. Believing
    /// otherwise leaves this machine pointing at a receiver it cannot reach —
    /// and is what has to be noticed for the duty to move somewhere else.
    #[tokio::test]
    async fn losing_the_link_forgets_that_the_peer_was_hosting() {
        let host = PeerId::new_v4();
        let (mut engine, state, _seen) = engine_for(true, false, host);
        state.lock().hosted_elsewhere.insert(
            ServiceId::AIRPLAY,
            HostedBy {
                peer: host,
                addr: None,
            },
        );
        let mut tx = NullTx;
        engine.shutdown(&mut tx).await;
        assert!(!state.is_hosted_elsewhere(ServiceId::AIRPLAY));
    }

    /// The whole reason the duty can move: the machine holding it goes away,
    /// and the one that stood down for it takes it back.
    ///
    /// Each step is what the announcer actually reads — `is_hosted_elsewhere`
    /// to decide whether to withdraw, `has_been_answered` to tell "nobody
    /// hosts it" from "nobody has said".
    #[tokio::test]
    async fn a_host_going_away_frees_the_duty_for_whoever_is_left() {
        let host = PeerId::new_v4();
        let (mut engine, state, _seen) = engine_for(true, true, host);
        let here: std::net::SocketAddr = "192.168.1.35:7000".parse().unwrap();
        state.now_hosting(ServiceId::AIRPLAY, here);

        // Both machines bound; the peer outranks us, so we yield to it.
        let peer_hosts = Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Status {
                service: ServiceId::AIRPLAY,
                hosting: true,
                addr: Some("192.168.1.140:7000".parse().unwrap()),
            },
        )
        .unwrap();
        assert!(engine.handle_message(&peer_hosts));
        assert!(state.is_hosted_elsewhere(ServiceId::AIRPLAY));

        // The host's link drops. Nothing announces the duty now, and this
        // machine knows the difference between that and never having asked.
        let mut tx = NullTx;
        engine.shutdown(&mut tx).await;
        assert!(!state.is_hosted_elsewhere(ServiceId::AIRPLAY));
        assert!(state.has_been_answered(ServiceId::AIRPLAY));
        assert!(
            state.is_hosted_here(ServiceId::AIRPLAY),
            "the receiver stayed bound while it was stood down, so it can \
             simply announce again"
        );
    }

    /// With three machines, B keeps a link to both A and C. A holds the
    /// receiver; C going away must not make B forget that, or B announces a
    /// second `Cursio` over a working one.
    #[tokio::test]
    async fn losing_one_link_does_not_forget_what_another_peer_hosts() {
        let hosting_peer = PeerId::new_v4();
        let other_peer = PeerId::new_v4();
        let (mut to_other, state, _seen) = engine_for(true, false, other_peer);
        state.lock().hosted_elsewhere.insert(
            ServiceId::AIRPLAY,
            HostedBy {
                peer: hosting_peer,
                addr: Some("192.168.1.5:7000".parse().unwrap()),
            },
        );

        let mut tx = NullTx;
        to_other.shutdown(&mut tx).await;
        assert!(
            state.is_hosted_elsewhere(ServiceId::AIRPLAY),
            "a link to a peer that hosts nothing must not clear the real host"
        );
    }

    /// The exchange the user described: ask first, answer with where.
    #[test]
    fn a_query_is_answered_with_where_the_duty_is_running() {
        let (mut engine, state, _seen) = engine(true);
        let here: std::net::SocketAddr = "192.168.1.35:7000".parse().unwrap();
        state.now_hosting(ServiceId::AIRPLAY, here);

        let query = Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Query {
                service: ServiceId::AIRPLAY,
            },
        )
        .unwrap();
        assert!(engine.handle_message(&query));
        assert_eq!(
            engine.outbound,
            vec![ServiceMsg::Status {
                service: ServiceId::AIRPLAY,
                hosting: true,
                addr: Some(here),
            }]
        );
    }

    /// Both machines bind before either has said hello, so both answer a query
    /// with `hosting: true`. Without a tie-break each believed the other had
    /// it and *both* withdrew — an iPhone was offered no `Cursio` at all while
    /// two machines held port 7000.
    #[test]
    fn two_machines_hosting_the_same_duty_do_not_both_stand_down() {
        let here: std::net::SocketAddr = "192.168.1.35:7000".parse().unwrap();
        let theirs: std::net::SocketAddr = "192.168.1.140:7000".parse().unwrap();
        let peer_says_yes = Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Status {
                service: ServiceId::AIRPLAY,
                hosting: true,
                addr: Some(theirs),
            },
        )
        .unwrap();

        // The higher-id side yields, which is the side that must.
        let (mut engine, state, _seen) = engine_ranked(true, true);
        state.now_hosting(ServiceId::AIRPLAY, here);
        assert!(engine.handle_message(&peer_says_yes));
        assert!(
            state.is_hosted_elsewhere(ServiceId::AIRPLAY),
            "the outranked machine must stand down"
        );

        // The lower-id side keeps it, from the very same message.
        let (mut engine, state, _seen) = engine_ranked(true, false);
        state.now_hosting(ServiceId::AIRPLAY, here);
        assert!(engine.handle_message(&peer_says_yes));
        assert!(
            !state.is_hosted_elsewhere(ServiceId::AIRPLAY),
            "the winning machine must keep its announcement"
        );
    }

    /// The tie-break applies only to a tie. A machine hosting nothing believes
    /// its peer whichever way the ids fall — otherwise the lower-id node would
    /// ignore the only answer it ever gets.
    #[test]
    fn a_machine_hosting_nothing_believes_the_peer_whatever_its_id() {
        for peer_outranks_us in [true, false] {
            let (mut engine, state, _seen) = engine_ranked(true, peer_outranks_us);
            let status = Message::module(
                ModuleId::SERVICE,
                Priority::Control,
                &ServiceMsg::Status {
                    service: ServiceId::AIRPLAY,
                    hosting: true,
                    addr: None,
                },
            )
            .unwrap();
            assert!(engine.handle_message(&status));
            assert!(
                state.is_hosted_elsewhere(ServiceId::AIRPLAY),
                "peer_outranks_us={peer_outranks_us}"
            );
        }
    }

    /// "Nobody hosts it" and "nobody has spoken" must not look alike, or a node
    /// that correctly declined to announce puts its record back a second later
    /// and the picker shows two.
    #[test]
    fn silence_is_distinguishable_from_a_peer_saying_no() {
        let (mut engine, state, _seen) = engine(true);
        assert!(
            !state.has_been_answered(ServiceId::AIRPLAY),
            "nothing has been said yet"
        );
        assert!(!state.is_hosted_elsewhere(ServiceId::AIRPLAY));

        let says_no = Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Status {
                service: ServiceId::AIRPLAY,
                hosting: false,
                addr: None,
            },
        )
        .unwrap();
        assert!(engine.handle_message(&says_no));
        assert!(state.has_been_answered(ServiceId::AIRPLAY));
        assert!(!state.is_hosted_elsewhere(ServiceId::AIRPLAY));
    }

    /// A machine carrying nothing must say so plainly, or the asker cannot tell
    /// "no" from a message that never arrived.
    #[test]
    fn a_machine_hosting_nothing_answers_that_it_is_not() {
        let (mut engine, _state, _seen) = engine(true);
        let query = Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Query {
                service: ServiceId::AIRPLAY,
            },
        )
        .unwrap();
        engine.handle_message(&query);
        assert_eq!(
            engine.outbound,
            vec![ServiceMsg::Status {
                service: ServiceId::AIRPLAY,
                hosting: false,
                addr: None,
            }]
        );
    }

    /// An answer of "yes, here" is what stops this machine starting a second
    /// receiver — and the address is what lets anything be pointed at it.
    #[test]
    fn a_peers_answer_records_both_that_and_where() {
        let (mut engine, state, _seen) = engine(true);
        let there: std::net::SocketAddr = "192.168.1.140:7000".parse().unwrap();
        let status = Message::module(
            ModuleId::SERVICE,
            Priority::Control,
            &ServiceMsg::Status {
                service: ServiceId::AIRPLAY,
                hosting: true,
                addr: Some(there),
            },
        )
        .unwrap();
        engine.handle_message(&status);
        assert!(state.is_hosted_elsewhere(ServiceId::AIRPLAY));
        assert_eq!(state.host_address(ServiceId::AIRPLAY), Some(there));
    }

    /// The peer that used to host it saying "not me" is how this machine learns
    /// the duty is free again.
    #[tokio::test]
    async fn the_opening_round_asks_every_known_duty() {
        let (mut engine, _state, _seen) = engine(true);
        let mut tx = NullTx;
        engine.pump(&mut tx).await.unwrap();
        // Queued and sent in one pump, so nothing is left to assert on
        // `outbound`; asking twice is what the flag prevents.
        engine.pump(&mut tx).await.unwrap();
        assert!(engine.queried);
    }

    #[test]
    fn a_message_for_another_module_is_left_alone() {
        let (mut engine, _state, _seen) = engine(true);
        let msg = Message::module(ModuleId::CLIPBOARD, Priority::Bulk, &1_u8).unwrap();
        assert!(!engine.handle_message(&msg));
    }

    struct NullTx;

    #[async_trait]
    impl TransportTx for NullTx {
        async fn send(&mut self, _msg: &Message) -> Result<(), TransportError> {
            Ok(())
        }
    }
}
