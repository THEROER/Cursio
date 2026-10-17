//! Where a phone's notifications enter the mesh, and where a desktop's answers
//! leave it.
//!
//! The phone's notification listener lives outside any link — Android calls it
//! whenever something is posted — while what to send where depends on every
//! link at once: a notification goes only to the desktops whose user is there
//! (`cursio_protocol::notify`). So each link's [`NotifyEngine`] registers here
//! and reports its peer's presence, and the listener posts here once.
//!
//! The two platform halves are traits, as with sharing: a desktop shows
//! notifications through a [`NotifyDisplay`], a phone acts on what was done
//! with them through a [`NotifyActor`].
//!
//! [`NotifyEngine`]: crate::notify_session::NotifyEngine

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use cursio_protocol::notify::{Notification, NotificationKey, NotifyMsg};
use cursio_protocol::PeerId;
use tokio::sync::mpsc;

/// What the user did with a notification shown here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayEvent {
    /// Closed it without acting.
    Closed { key: NotificationKey },
    /// Pressed a button, with the text typed if it takes a reply.
    Invoked {
        key: NotificationKey,
        action: u8,
        reply: Option<String>,
    },
}

/// Shows a peer's notifications on this machine.
pub trait NotifyDisplay: Send + Sync {
    /// Show `notification` from `peer`, or replace the one shown under its key.
    /// What the user then does with it goes to `events`.
    fn show(
        &self,
        peer: PeerId,
        peer_name: &str,
        notification: &Notification,
        events: mpsc::UnboundedSender<DisplayEvent>,
    );

    /// Take down what is shown under `key` for `peer`, if anything is. Not
    /// reported back as [`DisplayEvent::Closed`]: the peer is the one that
    /// asked.
    fn close(&self, peer: PeerId, key: &str);
}

/// Acts, on the phone, on what a desktop's user did with a notification.
pub trait NotifyActor: Send + Sync {
    fn invoke(&self, key: &str, action: u8, reply: Option<&str>);
    fn dismiss(&self, key: &str);
}

struct Link {
    token: u64,
    tx: mpsc::UnboundedSender<NotifyMsg>,
    /// What the peer last said; `false` until it says, so a desktop is not
    /// lit up before it has answered.
    here: bool,
}

/// This machine's notifications, on their way to its links.
#[derive(Default)]
pub struct NotifyHub {
    links: Mutex<HashMap<PeerId, Link>>,
}

/// The process's one hub.
pub fn hub() -> &'static NotifyHub {
    static HUB: OnceLock<NotifyHub> = OnceLock::new();
    HUB.get_or_init(NotifyHub::default)
}

impl NotifyHub {
    /// A link to `peer` is up: what this machine posts for it arrives on the
    /// returned channel. The token unregisters exactly this registration, so a
    /// link that replaced an older one is not removed by the older one's end.
    pub fn register(&self, peer: PeerId) -> (mpsc::UnboundedReceiver<NotifyMsg>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let token: u64 = rand::random();
        self.lock().insert(
            peer,
            Link {
                token,
                tx,
                here: false,
            },
        );
        (rx, token)
    }

    pub fn unregister(&self, peer: &PeerId, token: u64) {
        let mut links = self.lock();
        if links.get(peer).is_some_and(|l| l.token == token) {
            links.remove(peer);
        }
    }

    pub fn set_here(&self, peer: PeerId, here: bool) {
        if let Some(link) = self.lock().get_mut(&peer) {
            link.here = here;
        }
    }

    /// Send a notification posted here to the desktops whose user is at them,
    /// or to all of them when none is.
    pub fn post(&self, notification: Notification) {
        let links = self.lock();
        let present: Vec<&Link> = links.values().filter(|l| l.here).collect();
        let targets: Vec<&Link> = if present.is_empty() {
            links.values().collect()
        } else {
            present
        };
        for link in targets {
            let _ = link.tx.send(NotifyMsg::Posted(notification.clone()));
        }
    }

    /// A notification is gone here. Told to every link, not only those it was
    /// sent to: the user may have moved since, and closing what is not shown
    /// costs nothing.
    pub fn remove(&self, key: &str) {
        for link in self.lock().values() {
            let _ = link.tx.send(NotifyMsg::Removed { key: key.into() });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, Link>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::notify::NotifyKind;

    fn notification(key: &str) -> Notification {
        Notification {
            key: key.into(),
            app_id: "org.telegram.messenger".into(),
            app_name: "Telegram".into(),
            title: "Alice".into(),
            text: "are you coming?".into(),
            icon: None,
            when_ms: 0,
            kind: NotifyKind::Message,
            actions: Vec::new(),
        }
    }

    fn posted(rx: &mut mpsc::UnboundedReceiver<NotifyMsg>) -> Vec<NotifyMsg> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn a_notification_goes_only_where_the_user_is() {
        let hub = NotifyHub::default();
        let (desk, laptop) = (PeerId::from_u128(1), PeerId::from_u128(2));
        let (mut at_desk, _) = hub.register(desk);
        let (mut at_laptop, _) = hub.register(laptop);
        hub.set_here(laptop, true);
        hub.post(notification("k1"));
        assert!(posted(&mut at_desk).is_empty());
        assert_eq!(
            posted(&mut at_laptop),
            vec![NotifyMsg::Posted(notification("k1"))]
        );
    }

    #[test]
    fn with_nobody_at_any_machine_every_machine_keeps_it() {
        let hub = NotifyHub::default();
        let (mut a, _) = hub.register(PeerId::from_u128(1));
        let (mut b, _) = hub.register(PeerId::from_u128(2));
        hub.post(notification("k1"));
        assert_eq!(posted(&mut a).len(), 1);
        assert_eq!(posted(&mut b).len(), 1);
    }

    #[test]
    fn an_older_links_end_does_not_unregister_its_replacement() {
        let hub = NotifyHub::default();
        let peer = PeerId::from_u128(1);
        let (_old, old_token) = hub.register(peer);
        let (mut new, _) = hub.register(peer);
        hub.unregister(&peer, old_token);
        hub.remove("k1");
        assert_eq!(
            posted(&mut new),
            vec![NotifyMsg::Removed { key: "k1".into() }]
        );
    }
}
