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

use crate::module::SessionModule;
use crate::notify_hub::{hub, DisplayEvent, NotifyActor, NotifyDisplay};

/// Version of [`NotifyMsg`]; peers run the module only on an exact match.
/// 2 added [`NotifyMsg::Ring`]: a version-1 phone would have dropped it as
/// undecodable, so the two never agree and the ring is refused here, by name.
pub const VERSION: u16 = 2;

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
    presence_at: Instant,
    shown: HashSet<NotificationKey>,
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
            presence_at: Instant::now(),
            shown: HashSet::new(),
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
    }

    fn on_message(&mut self, msg: NotifyMsg) {
        match msg {
            NotifyMsg::Presence { here } => {
                if self.actor.is_some() {
                    hub().set_here(self.peer, here);
                }
            }
            NotifyMsg::Posted(notification) => {
                if let Some(display) = &self.display {
                    tracing::debug!(peer = %self.peer_name, app = %notification.app_name, "showing a notification");
                    display.show(
                        self.peer,
                        &self.peer_name,
                        &notification,
                        self.events_tx.clone(),
                    );
                    self.shown.insert(notification.key);
                }
            }
            NotifyMsg::Removed { key } => {
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
