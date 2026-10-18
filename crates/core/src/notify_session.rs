//! The notification module on one link.
//!
//! One engine plays whichever parts this machine has. A phone posts its
//! notifications through [`hub`](crate::notify_hub::hub) and acts on what the
//! peer did with them ([`NotifyActor`]); a desktop shows what the peer posts
//! ([`NotifyDisplay`]) and tells it whether the user is here. Both at once is
//! fine and costs nothing: a machine with nothing to post never posts.
//!
//! A module rather than node state like `NodeStatus`: a notification carries
//! what someone wrote to the user, which the user must be able to keep on the
//! phone by switching this off, and a phone without notification access has
//! nothing to send.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cursio_protocol::notify::{NotificationKey, NotifyMsg};
use cursio_protocol::{Message, ModuleId, PeerId};
use cursio_transport::{TransportError, TransportTx};
use tokio::sync::mpsc;

use cursio_protocol::notify::{Notification, NotifyKind};

use crate::module::SessionModule;
use crate::notify_dedup::{dedup, Decision};
use crate::notify_hub::{hub, CallRoles, DisplayEvent, NotifyActor, NotifyDisplay};

/// Version of [`NotifyMsg`]; peers run the module only on an exact match.
/// 2 added [`NotifyMsg::Ring`]: a version-1 phone would have dropped it as
/// undecodable, so the two never agree and the ring is refused here, by name.
/// 3 added [`NotifyMsg::CallButtons`], for a messenger's call.
pub const VERSION: u16 = 3;

/// How often a desktop re-reads whether the user is here. The idle time is a
/// session-bus call, cheap but not free, and a notification that lands on the
/// screen the user left two seconds ago is still where they will find it.
const PRESENCE_EVERY: Duration = Duration::from_secs(2);

pub struct NotifyEngine {
    peer: PeerId,
    peer_name: String,
    display: Option<Arc<dyn NotifyDisplay>>,
    actor: Option<Arc<dyn NotifyActor>>,
    /// What this machine sends the peer outside a link's own traffic: a
    /// phone's posts, a desktop's ring.
    posts: (mpsc::UnboundedReceiver<NotifyMsg>, u64),
    events_tx: mpsc::UnboundedSender<DisplayEvent>,
    events_rx: mpsc::UnboundedReceiver<DisplayEvent>,
    here: Option<bool>,
    /// What this desktop last told a phone about being in use right now; see
    /// [`NotifyMsg::Activity`]. Only a phone is told: it alone decides where
    /// its notifications go.
    active: Option<bool>,
    peer_is_phone: bool,
    presence_at: Instant,
    shown: HashSet<NotificationKey>,
    /// Copies of an app known to have a client here, waiting to see whether
    /// that client shows them first: (copy, when it arrived, until when).
    held: Vec<(Notification, Instant, Instant)>,
    /// Copies already shown, taken down again if a client's twin turns up
    /// before the moment given. See [`crate::notify_dedup`].
    watched: Vec<(Notification, Instant, Instant)>,
    queued: Vec<NotifyMsg>,
}

impl NotifyEngine {
    pub fn new(
        peer: PeerId,
        peer_name: String,
        peer_is_phone: bool,
        display: Option<Arc<dyn NotifyDisplay>>,
        actor: Option<Arc<dyn NotifyActor>>,
    ) -> Self {
        let posts = hub().register(peer, &peer_name, peer_is_phone);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        NotifyEngine {
            peer,
            peer_name,
            display,
            actor,
            posts,
            events_tx,
            events_rx,
            here: None,
            active: None,
            peer_is_phone,
            presence_at: Instant::now(),
            shown: HashSet::new(),
            held: Vec::new(),
            watched: Vec::new(),
            queued: Vec::new(),
        }
    }

    fn check_presence(&mut self, now: Instant) {
        if self.display.is_none() || (self.here.is_some() && now < self.presence_at) {
            return;
        }
        self.presence_at = now + PRESENCE_EVERY;
        let here = crate::presence::user_here();
        if self.here != Some(here) {
            tracing::debug!(peer = %self.peer_name, here, "telling the peer whether the user is here");
            self.here = Some(here);
            self.queued.push(NotifyMsg::Presence { here });
        }
        if !self.peer_is_phone {
            return;
        }
        if let Some((active, idle)) = crate::presence::activity() {
            if self.active != Some(active) {
                self.active = Some(active);
                self.queued.push(NotifyMsg::Activity {
                    active,
                    idle_ms: u64::try_from(idle.as_millis()).unwrap_or(u64::MAX),
                });
            }
        }
    }

    fn on_message(&mut self, msg: NotifyMsg) {
        match msg {
            NotifyMsg::Presence { here } => {
                if self.actor.is_some() {
                    hub().set_here(self.peer, here);
                }
            }
            NotifyMsg::Activity { active, idle_ms } => {
                if self.actor.is_some() {
                    hub().set_activity(self.peer, active, Duration::from_millis(idle_ms));
                }
            }
            NotifyMsg::Posted(notification) => self.posted(notification),
            NotifyMsg::CallButtons {
                key,
                answer,
                decline,
                hang_up,
            } => {
                if self.display.is_some() {
                    let roles = CallRoles {
                        answer,
                        decline,
                        hang_up,
                    };
                    crate::calls_hub::hub().messenger_buttons(&self.peer_name, &key, roles);
                }
            }
            NotifyMsg::Removed { key } => {
                if self.display.is_some() {
                    crate::calls_hub::hub().messenger_removed(&self.peer_name, &key);
                }
                self.held.retain(|(n, _, _)| n.key != key);
                self.watched.retain(|(n, _, _)| n.key != key);
                if let Some(display) = &self.display {
                    if self.shown.remove(&key) {
                        display.close(self.peer, &key);
                    }
                }
                if let Some(actor) = &self.actor {
                    actor.dismiss(&key);
                }
            }
            NotifyMsg::Invoke { key, action, reply } => {
                if let Some(actor) = &self.actor {
                    actor.invoke(&key, action, reply.as_deref());
                }
            }
            NotifyMsg::Ring { on } => match &self.actor {
                Some(actor) => {
                    tracing::info!(peer = %self.peer_name, on, "asked to ring");
                    actor.ring(on);
                }
                None => {
                    tracing::warn!(peer = %self.peer_name, "asked to ring, but this machine has no ringer")
                }
            },
        }
    }

    fn posted(&mut self, notification: Notification) {
        if self.display.is_none() {
            return;
        }
        // A call the phone forwards as a notification is a messenger's: it is
        // shown in the call window with the phone's other calls, not as a
        // notification beside them.
        if notification.kind == NotifyKind::Call {
            tracing::debug!(peer = %self.peer_name, app = %notification.app_name, "a messenger's call");
            crate::calls_hub::hub().messenger_posted(
                &self.peer_name,
                &notification,
                self.events_tx.clone(),
            );
            return;
        }
        // An update of one already on screen replaces it in place; whatever
        // was decided about it the first time still stands.
        if self.shown.contains(&notification.key) {
            self.show(&notification);
            return;
        }
        self.held.retain(|(n, _, _)| n.key != notification.key);
        let now = Instant::now();
        match dedup().decide(&notification, now) {
            Decision::Duplicate => {
                tracing::debug!(peer = %self.peer_name, app = %notification.app_name, "a client here already showed this; not showing the phone's copy");
            }
            Decision::Show => self.show(&notification),
            Decision::ShowWatching(until) => {
                self.show(&notification);
                self.watched.push((notification, now, until));
            }
            Decision::Hold(until) => self.held.push((notification, now, until)),
        }
    }

    fn show(&mut self, notification: &Notification) {
        let Some(display) = &self.display else { return };
        tracing::debug!(peer = %self.peer_name, app = %notification.app_name, "showing a notification");
        display.show(
            self.peer,
            &self.peer_name,
            notification,
            self.events_tx.clone(),
        );
        self.shown.insert(notification.key.clone());
    }

    /// Show held copies whose wait ran out, and take down shown ones whose
    /// twin arrived. The record is read first and released before anything
    /// reaches the notification server, which the watcher must not wait on.
    fn settle(&mut self, now: Instant) {
        if self.held.is_empty() && self.watched.is_empty() {
            return;
        }
        let mut show = Vec::new();
        let mut take_down = Vec::new();
        {
            let mut record = dedup();
            self.held.retain(|(n, since, until)| {
                if record.arrived(n, *since) {
                    false
                } else if now >= *until {
                    show.push(n.clone());
                    false
                } else {
                    true
                }
            });
            self.watched.retain(|(n, since, until)| {
                if record.arrived(n, *since) {
                    take_down.push(n.key.clone());
                    false
                } else {
                    now < *until
                }
            });
        }
        for n in show {
            self.show(&n);
        }
        if let Some(display) = &self.display {
            for key in take_down {
                tracing::debug!(peer = %self.peer_name, "a client here showed this too; taking the phone's copy down");
                // Closed here only: on the phone it is still unread.
                self.shown.remove(&key);
                display.close(self.peer, &key);
            }
        }
    }

    fn on_event(&mut self, event: DisplayEvent) {
        match event {
            DisplayEvent::Closed { key } => {
                self.shown.remove(&key);
                self.queued.push(NotifyMsg::Removed { key });
            }
            DisplayEvent::Invoked { key, action, reply } => {
                self.shown.remove(&key);
                self.queued.push(NotifyMsg::Invoke { key, action, reply });
            }
        }
    }
}

async fn send(tx: &mut dyn TransportTx, msg: &NotifyMsg) -> Result<(), TransportError> {
    let message = Message::module(ModuleId::NOTIFY, NotifyMsg::PRIORITY, msg)?;
    tx.send(&message).await
}

#[async_trait]
impl SessionModule for NotifyEngine {
    fn name(&self) -> &'static str {
        "notify"
    }

    fn id(&self) -> ModuleId {
        ModuleId::NOTIFY
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.check_presence(Instant::now());
        for msg in std::mem::take(&mut self.queued) {
            send(tx, &msg).await?;
        }
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.check_presence(Instant::now());
        self.settle(Instant::now());
        while let Ok(event) = self.events_rx.try_recv() {
            self.on_event(event);
        }
        while let Ok(msg) = self.posts.0.try_recv() {
            self.queued.push(msg);
        }
        for msg in std::mem::take(&mut self.queued) {
            send(tx, &msg).await?;
        }
        Ok(())
    }

    fn has_pending_work(&self) -> bool {
        // A held copy is due within seconds; the idle pace would show it late.
        !self.held.is_empty() || !self.watched.is_empty()
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<NotifyMsg>(ModuleId::NOTIFY) {
            Ok(Some(msg)) => {
                self.on_message(msg);
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable notification message from peer");
                true
            }
        }
    }
}

impl Drop for NotifyEngine {
    fn drop(&mut self) {
        hub().unregister(&self.peer, self.posts.1);
        // Its buttons answer through this link, which is gone.
        if let Some(display) = &self.display {
            for key in self.shown.drain() {
                display.close(self.peer, &key);
            }
        }
    }
}

/// Builds [`NotifyEngine`] with whichever halves this machine has.
pub struct NotifyFactory {
    pub display: Option<Arc<dyn NotifyDisplay>>,
    pub actor: Option<Arc<dyn NotifyActor>>,
    /// This machine can ask a phone to ring. Enough on its own to run the
    /// module: a desktop that cannot show a phone's notifications yet can
    /// still find it.
    pub rings: bool,
}

impl NotifyFactory {
    fn runs(&self) -> bool {
        self.display.is_some() || self.actor.is_some() || self.rings
    }
}

impl crate::module::ModuleFactory for NotifyFactory {
    fn id(&self) -> ModuleId {
        ModuleId::NOTIFY
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        self.runs()
            .then(|| cursio_protocol::ModuleAdvert::symmetric(ModuleId::NOTIFY, VERSION))
    }

    fn supported(&self) -> bool {
        self.runs()
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(NotifyEngine::new(
            ctx.peer.peer_id,
            ctx.peer_name().to_string(),
            matches!(
                ctx.peer.device_kind,
                cursio_protocol::DeviceKind::Phone | cursio_protocol::DeviceKind::Tablet
            ),
            self.display.clone(),
            self.actor.clone(),
        )))
    }
}
