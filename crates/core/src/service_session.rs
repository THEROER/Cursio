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
use cursio_protocol::{Message, ModuleId, Priority, ServiceId, ServiceMsg};
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

#[derive(Default)]
struct Inner {
    /// Duties a peer has asked this machine to take on.
    asked_of_us: HashSet<ServiceId>,
    /// Duties this machine has asked a peer to take on, so a second session
    /// does not repeat a request already answered.
    asked_by_us: HashSet<ServiceId>,
    /// Duties some peer has confirmed it is actually running.
    hosted_elsewhere: HashSet<ServiceId>,
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
        self.lock().hosted_elsewhere.contains(&service)
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
    state: ServiceState,
    host: Box<dyn ServiceHost>,
    /// Queued for the next pump, because `handle_message` is synchronous while
    /// sending is not.
    outbound: Vec<ServiceMsg>,
    /// Requests already sent on this link, so a pump does not repeat them.
    requested: HashSet<ServiceId>,
    /// Why this machine cannot carry each duty it is asking about.
    reasons: Arc<Mutex<std::collections::HashMap<ServiceId, String>>>,
}

impl ServiceEngine {
    pub fn new(
        peer_name: String,
        state: ServiceState,
        host: Box<dyn ServiceHost>,
        reasons: Arc<Mutex<std::collections::HashMap<ServiceId, String>>>,
    ) -> Self {
        Self {
            peer_name,
            state,
            host,
            outbound: Vec::new(),
            requested: HashSet::new(),
            reasons,
        }
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
            ServiceMsg::Request { service, reason } => {
                let answer = self.answer(service, reason);
                self.outbound.push(answer);
            }
            ServiceMsg::Accepted { service } => {
                tracing::info!(peer = %self.peer_name, %service, "peer is hosting this for us");
                self.state.lock().hosted_elsewhere.insert(service);
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
        // What this peer was hosting stops being true the moment the link goes,
        // whether or not it said so. Leaving the flag set would keep this
        // machine believing a receiver exists somewhere it can no longer reach.
        for service in KNOWN_SERVICES {
            self.state.lock().hosted_elsewhere.remove(service);
        }
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
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = ServiceState::new();
        let engine = ServiceEngine::new(
            "peer".into(),
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
    /// otherwise leaves this machine pointing at a receiver it cannot reach.
    #[tokio::test]
    async fn losing_the_link_forgets_that_the_peer_was_hosting() {
        let (mut engine, state, _seen) = engine(true);
        state.lock().hosted_elsewhere.insert(ServiceId::AIRPLAY);
        let mut tx = NullTx;
        engine.shutdown(&mut tx).await;
        assert!(!state.is_hosted_elsewhere(ServiceId::AIRPLAY));
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
