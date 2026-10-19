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
use std::time::{Duration, Instant};

use crate::at_phone::DesktopActivity;

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

/// Which buttons of a call's notification answer, decline and end it, as
/// positions among its actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallRoles {
    pub answer: Option<u8>,
    pub decline: Option<u8>,
    pub hang_up: Option<u8>,
}

/// Acts, on the phone, on what a desktop's user did with a notification.
pub trait NotifyActor: Send + Sync {
    fn invoke(&self, key: &str, action: u8, reply: Option<&str>);
    fn dismiss(&self, key: &str);
    /// Ring loudly until found, or stop.
    fn ring(&self, on: bool);
}

struct Link {
    token: u64,
    tx: mpsc::UnboundedSender<NotifyMsg>,
    /// What the peer last said; `false` until it says, so a desktop is not
    /// lit up before it has answered.
    here: bool,
    /// What the peer last said about being in use right now, and when its
    /// last input was then; `None` from a peer too old to say.
    activity: Option<(bool, Instant)>,
    name: String,
    /// Only a phone rings; a desktop asked to would say nothing, and silence
    /// reads as "it is ringing somewhere I cannot hear".
    phone: bool,
    /// Who waits for this phone's answer to a ring.
    ring_waiting: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
}

/// How long a ring waits for the phone to say it rang.
const RING_ANSWER_WAIT: Duration = Duration::from_secs(5);

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
    pub fn register(
        &self,
        peer: PeerId,
        name: &str,
        phone: bool,
    ) -> (mpsc::UnboundedReceiver<NotifyMsg>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let token: u64 = rand::random();
        self.lock().insert(
            peer,
            Link {
                token,
                tx,
                here: false,
                activity: None,
                name: name.to_string(),
                phone,
                ring_waiting: None,
            },
        );
        (rx, token)
    }

    /// Ring `peer` (or stop it) and wait for the phone to say it did.
    ///
    /// "Ringing" used to be answered the moment the message was queued, so a
    /// phone that never acted on it read as one ringing out of earshot.
    pub async fn ring_confirmed(&self, peer: &str, on: bool) -> Result<String, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let name = self.ring_with(peer, on, Some(tx))?;
        match tokio::time::timeout(RING_ANSWER_WAIT, rx).await {
            Ok(Ok(Ok(()))) => Ok(name),
            Ok(Ok(Err(refused))) => Err(format!("{name} refused to ring: {refused}")),
            Ok(Err(_)) => Err(format!("the link to {name} closed before it answered")),
            Err(_) => Err(format!(
                "{name} did not answer within {} s: its Cursio may be older, or stuck — restarting the app on the phone brings it back",
                RING_ANSWER_WAIT.as_secs()
            )),
        }
    }

    /// The phone answered a ring.
    pub fn ring_answered(&self, peer: PeerId, on: bool, refused: Option<String>) {
        let waiting = self
            .lock()
            .get_mut(&peer)
            .and_then(|l| l.ring_waiting.take());
        match &refused {
            None => tracing::info!(%peer, on, "the phone confirmed the ring"),
            Some(why) => tracing::warn!(%peer, on, %why, "the phone refused to ring"),
        }
        if let Some(waiting) = waiting {
            let _ = waiting.send(refused.map_or(Ok(()), Err));
        }
    }

    /// Ring `peer` (an id or a name), or stop it ringing.
    ///
    /// A link registers only once the module was agreed, so a phone that is
    /// connected but missing here runs a Cursio too old to ring, or has
    /// notifications switched off — said rather than sent into nothing.
    pub fn ring(&self, peer: &str, on: bool) -> Result<String, String> {
        self.ring_with(peer, on, None)
    }

    fn ring_with(
        &self,
        peer: &str,
        on: bool,
        waiting: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    ) -> Result<String, String> {
        let mut links = self.lock();
        let id = links
            .iter()
            .find(|(_, l)| l.name.eq_ignore_ascii_case(peer))
            .or_else(|| links.iter().find(|(id, _)| id.to_string() == peer))
            .filter(|(_, l)| !l.tx.is_closed())
            .map(|(id, _)| *id)
            .ok_or_else(|| {
                let mut phones: Vec<&str> = links
                    .values()
                    .filter(|l| l.phone && !l.tx.is_closed())
                    .map(|l| l.name.as_str())
                    .collect();
                phones.sort_unstable();
                format!(
                    "{peer} cannot be rung: it is not connected, has notifications switched \
                     off, or runs an older Cursio; phones that can: {}",
                    if phones.is_empty() {
                        "none".to_string()
                    } else {
                        phones.join(", ")
                    }
                )
            })?;
        let Some(link) = links.get_mut(&id) else {
            return Err(format!("{peer} has just gone"));
        };
        if !link.phone {
            return Err(format!("{} is not a phone; only a phone rings", link.name));
        }
        link.tx
            .send(NotifyMsg::Ring { on })
            .map_err(|_| format!("the link to {} has just closed", link.name))?;
        if waiting.is_some() {
            link.ring_waiting = waiting;
        }
        Ok(link.name.clone())
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

    /// `peer` is in use right now (`active`), or its last input was `idle` ago.
    pub fn set_activity(&self, peer: PeerId, active: bool, idle: Duration) {
        if let Some(link) = self.lock().get_mut(&peer) {
            let last = Instant::now()
                .checked_sub(idle)
                .unwrap_or_else(Instant::now);
            link.activity = Some((active, last));
        }
    }

    /// The desktop the user was at last: the one with the latest input, else
    /// one saying the user is there. `None` while no desktop has said either.
    pub fn last_used_desktop(&self) -> Option<String> {
        let now = Instant::now();
        let links = self.lock();
        let desktops = || links.values().filter(|l| !l.phone && !l.tx.is_closed());
        desktops()
            .filter_map(|l| {
                let (active, last) = l.activity?;
                Some((if active { now } else { last }, l))
            })
            .max_by_key(|(last, _)| *last)
            .map(|(_, l)| l)
            .or_else(|| desktops().find(|l| l.here))
            .map(|l| l.name.clone())
    }

    /// What every desktop said about being in use, for [`crate::at_phone`].
    /// One in use right now had input "now": it keeps saying so only by not
    /// saying it stopped.
    fn desktop_activity(links: &HashMap<PeerId, Link>, now: Instant) -> Vec<DesktopActivity> {
        links
            .values()
            .filter(|l| !l.phone)
            .map(|l| DesktopActivity {
                last_input: l
                    .activity
                    .map(|(active, last)| if active { now } else { last }),
            })
            .collect()
    }

    /// Send a notification posted here to the desktops whose user is at them,
    /// or to all of them when none is — or to none, while the user has this
    /// phone in hand (see [`crate::at_phone`]).
    pub fn post(&self, notification: Notification) {
        self.send_to_present(&[NotifyMsg::Posted(notification)]);
    }

    /// Post a call. A telephony call — the dialer's own notification — is
    /// left to the calls module when it runs, which shows it with buttons of
    /// its own. Any other (a messenger's) goes out like a notification, with
    /// which of its buttons answer, decline and end it, so a desktop can show
    /// it in its call window.
    ///
    /// Every call used to be left to the calls module, which sees telephony
    /// only: with it running, a WhatsApp or Telegram call reached no desktop
    /// at all.
    pub fn post_call(&self, notification: Notification, telephony: bool, buttons: CallRoles) {
        if telephony && crate::calls_hub::hub().reports_calls() {
            tracing::info!(app = %notification.app_id, "a telephony call; the calls module shows it");
            return;
        }
        let key = notification.key.clone();
        let mut messages = vec![NotifyMsg::Posted(notification)];
        if buttons != CallRoles::default() {
            messages.push(NotifyMsg::CallButtons {
                key,
                answer: buttons.answer,
                decline: buttons.decline,
                hang_up: buttons.hang_up,
            });
        }
        self.send_to_present(&messages);
    }

    fn send_to_present(&self, messages: &[NotifyMsg]) {
        let links = self.lock();
        if crate::at_phone::user_at_phone(&Self::desktop_activity(&links, Instant::now())) {
            let app = messages.iter().find_map(|m| match m {
                NotifyMsg::Posted(n) => Some(n.app_id.as_str()),
                _ => None,
            });
            tracing::debug!(app = ?app, "the user has the phone in hand; keeping it here");
            return;
        }
        let present: Vec<&Link> = links.values().filter(|l| l.here).collect();
        let targets: Vec<&Link> = if present.is_empty() {
            links.values().collect()
        } else {
            present
        };
        tracing::info!(
            desktops = targets.len(),
            present = links.values().filter(|l| l.here).count(),
            "forwarding a notification"
        );
        for link in targets {
            for msg in messages {
                let _ = link.tx.send(msg.clone());
            }
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
        let (mut at_desk, _) = hub.register(desk, "desk", false);
        let (mut at_laptop, _) = hub.register(laptop, "laptop", false);
        hub.set_here(laptop, true);
        hub.post(notification("k1"));
        assert!(posted(&mut at_desk).is_empty());
        assert_eq!(
            posted(&mut at_laptop),
            vec![NotifyMsg::Posted(notification("k1"))]
        );
    }

    #[tokio::test]
    async fn a_ring_is_reported_only_once_the_phone_answers() {
        let hub = NotifyHub::default();
        let phone = PeerId::from_u128(7);
        let (mut to_phone, _) = hub.register(phone, "POCO", true);
        let (ringing, asked) = tokio::join!(hub.ring_confirmed("poco", true), async {
            let asked = to_phone.recv().await;
            hub.ring_answered(phone, true, None);
            asked
        });
        assert_eq!(asked, Some(NotifyMsg::Ring { on: true }));
        assert_eq!(ringing, Ok("POCO".to_string()));
    }

    #[tokio::test]
    async fn a_phone_refusing_to_ring_says_why() {
        let hub = NotifyHub::default();
        let phone = PeerId::from_u128(7);
        let (_to_phone, _) = hub.register(phone, "POCO", true);
        let (ringing, ()) = tokio::join!(hub.ring_confirmed("POCO", true), async {
            tokio::task::yield_now().await;
            hub.ring_answered(phone, true, Some("this machine has no ringer".into()));
        });
        assert!(ringing.unwrap_err().contains("no ringer"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_phone_that_never_answers_a_ring_is_not_reported_ringing() {
        let hub = NotifyHub::default();
        let (_to_phone, _) = hub.register(PeerId::from_u128(7), "POCO", true);
        let error = hub.ring_confirmed("POCO", true).await.unwrap_err();
        assert!(error.contains("did not answer"), "{error}");
    }

    #[test]
    fn the_last_used_desktop_is_the_one_with_the_latest_input() {
        let hub = NotifyHub::default();
        let (desk, laptop, phone) = (
            PeerId::from_u128(1),
            PeerId::from_u128(2),
            PeerId::from_u128(3),
        );
        let _a = hub.register(desk, "desk", false);
        let _b = hub.register(laptop, "laptop", false);
        let _c = hub.register(phone, "phone", true);
        assert_eq!(hub.last_used_desktop(), None);

        hub.set_here(desk, true);
        assert_eq!(hub.last_used_desktop().as_deref(), Some("desk"));

        hub.set_activity(desk, false, Duration::from_secs(600));
        hub.set_activity(laptop, false, Duration::from_secs(20));
        hub.set_activity(phone, true, Duration::ZERO);
        assert_eq!(hub.last_used_desktop().as_deref(), Some("laptop"));
    }

    #[test]
    fn with_nobody_at_any_machine_every_machine_keeps_it() {
        let hub = NotifyHub::default();
        let (mut a, _) = hub.register(PeerId::from_u128(1), "a", false);
        let (mut b, _) = hub.register(PeerId::from_u128(2), "b", false);
        hub.post(notification("k1"));
        assert_eq!(posted(&mut a).len(), 1);
        assert_eq!(posted(&mut b).len(), 1);
    }

    #[test]
    fn a_desktop_in_use_now_counts_as_input_this_moment() {
        let hub = NotifyHub::default();
        let desk = PeerId::from_u128(1);
        let laptop = PeerId::from_u128(2);
        let silent = PeerId::from_u128(3);
        let phone = PeerId::from_u128(4);
        let _a = hub.register(desk, "desk", false);
        let _b = hub.register(laptop, "laptop", false);
        let _c = hub.register(silent, "old", false);
        let _d = hub.register(phone, "phone", true);
        hub.set_activity(desk, true, Duration::from_secs(1));
        hub.set_activity(laptop, false, Duration::from_secs(60));
        let now = Instant::now() + Duration::from_secs(30);
        let mut seen: Vec<Option<Instant>> = NotifyHub::desktop_activity(&hub.lock(), now)
            .into_iter()
            .map(|d| d.last_input)
            .collect();
        seen.sort();
        assert_eq!(seen.len(), 3, "the phone's own link is not a desktop");
        assert_eq!(seen[0], None, "a desktop that never said stays unknown");
        assert!(seen[1].is_some_and(|at| at < now - Duration::from_secs(60)));
        assert_eq!(seen[2], Some(now));
    }

    #[test]
    fn a_messengers_call_goes_out_with_the_roles_of_its_buttons() {
        let hub = NotifyHub::default();
        let (mut desk, _) = hub.register(PeerId::from_u128(1), "desk", false);
        let mut call = notification("c1");
        call.kind = NotifyKind::Call;
        let roles = CallRoles {
            answer: Some(1),
            decline: Some(0),
            hang_up: None,
        };
        hub.post_call(call.clone(), false, roles);
        assert_eq!(
            posted(&mut desk),
            vec![
                NotifyMsg::Posted(call),
                NotifyMsg::CallButtons {
                    key: "c1".into(),
                    answer: Some(1),
                    decline: Some(0),
                    hang_up: None,
                },
            ]
        );
    }

    #[test]
    fn an_older_links_end_does_not_unregister_its_replacement() {
        let hub = NotifyHub::default();
        let peer = PeerId::from_u128(1);
        let (_old, old_token) = hub.register(peer, "phone", true);
        let (mut new, _) = hub.register(peer, "phone", true);
        hub.unregister(&peer, old_token);
        hub.remove("k1");
        assert_eq!(
            posted(&mut new),
            vec![NotifyMsg::Removed { key: "k1".into() }]
        );
    }

    #[test]
    fn a_phone_is_rung_by_its_name_in_any_case() {
        let hub = NotifyHub::default();
        let (mut phone, _) = hub.register(PeerId::from_u128(1), "POCO X7 Pro", true);
        assert_eq!(hub.ring("poco x7 pro", true).as_deref(), Ok("POCO X7 Pro"));
        assert_eq!(posted(&mut phone), vec![NotifyMsg::Ring { on: true }]);
    }

    #[test]
    fn a_phone_is_rung_by_its_id() {
        let hub = NotifyHub::default();
        let id = PeerId::from_u128(7);
        let (mut phone, _) = hub.register(id, "Redmi", true);
        assert!(hub.ring(&id.to_string(), false).is_ok());
        assert_eq!(posted(&mut phone), vec![NotifyMsg::Ring { on: false }]);
    }

    #[test]
    fn ringing_a_desktop_is_refused_rather_than_sent() {
        let hub = NotifyHub::default();
        let (mut desk, _) = hub.register(PeerId::from_u128(1), "Lightmoon", false);
        let err = hub.ring("Lightmoon", true).unwrap_err();
        assert!(err.contains("not a phone"), "{err}");
        assert!(posted(&mut desk).is_empty());
    }

    #[test]
    fn ringing_an_unknown_peer_names_the_phones_that_can_be_rung() {
        let hub = NotifyHub::default();
        let (_phone, _) = hub.register(PeerId::from_u128(1), "Redmi", true);
        let (_desk, _) = hub.register(PeerId::from_u128(2), "Lightmoon", false);
        let err = hub.ring("POCO", true).unwrap_err();
        assert!(err.ends_with("phones that can: Redmi"), "{err}");
    }
}
