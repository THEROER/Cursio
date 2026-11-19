//! A messenger's call on the phone — Telegram, WhatsApp — shown beside its
//! telephony calls and answered by pressing the notification's buttons.

use crate::calls_hub::lock;
use crate::calls_hub::CallAction;
use crate::calls_hub::CallsHub;
use crate::notify_hub::{CallRoles, DisplayEvent};
use cursio_protocol::calls::{Call, CallDirection, CallId, CallState};
use cursio_protocol::notify::Notification;
use tokio::sync::mpsc;

/// A messenger's call, from a phone's notification.
pub(super) struct MessengerCall {
    /// The phone's name, which places the call with its other calls.
    pub(super) phone: String,
    key: String,
    /// Shown as the call's name: who, and through which app.
    label: String,
    id: CallId,
    roles: CallRoles,
    /// The notify module's way back to the phone: a button pressed here.
    events: mpsc::UnboundedSender<DisplayEvent>,
}

impl MessengerCall {
    /// Ringing while the phone offers a way to answer it.
    fn ringing(&self) -> bool {
        self.roles.answer.is_some()
    }

    pub(super) fn as_call(&self) -> Call {
        Call {
            id: self.id,
            direction: CallDirection::Incoming,
            state: if self.ringing() {
                CallState::Ringing
            } else {
                CallState::Active
            },
            number: None,
            name: Some(self.label.clone()),
        }
    }

    fn press(&self, button: Option<u8>) -> Result<(), String> {
        let action = button.ok_or_else(|| format!("{} offers no button for that", self.label))?;
        self.events
            .send(DisplayEvent::Invoked {
                key: self.key.clone(),
                action,
                reply: None,
            })
            .map_err(|_| format!("the link to {} just closed", self.phone))
    }
}

/// Ids for messenger calls, apart from the phone's own (small, from `+CLCC`
/// or the call monitor).
pub(super) const MESSENGER_IDS: CallId = 0x4000_0000;

impl CallsHub {
    /// A phone posted (or updated) a messenger's call. `events` is how a
    /// button pressed here reaches the phone.
    pub fn messenger_posted(
        &self,
        phone: &str,
        notification: &Notification,
        events: mpsc::UnboundedSender<DisplayEvent>,
    ) {
        let label = match (notification.title.trim(), notification.app_name.trim()) {
            ("", app) => app.to_string(),
            (who, "") => who.to_string(),
            (who, app) => format!("{who} · {app}"),
        };
        {
            let mut desk = lock(&self.desk);
            match desk
                .messenger
                .iter_mut()
                .find(|c| c.phone == phone && c.key == notification.key)
            {
                Some(call) => {
                    call.label = label;
                    call.events = events;
                }
                None => {
                    let id = MESSENGER_IDS + desk.messenger.len() as CallId;
                    desk.messenger.push(MessengerCall {
                        phone: phone.to_string(),
                        key: notification.key.clone(),
                        label,
                        id,
                        roles: CallRoles::default(),
                        events,
                    });
                }
            }
        }
        self.changed();
    }

    /// Which buttons of a messenger's call answer, decline and end it.
    pub fn messenger_buttons(&self, phone: &str, key: &str, roles: CallRoles) {
        {
            let mut desk = lock(&self.desk);
            let Some(call) = desk
                .messenger
                .iter_mut()
                .find(|c| c.phone == phone && c.key == key)
            else {
                return;
            };
            call.roles = roles;
        }
        self.changed();
    }

    /// The phone took a messenger's call notification down: the call ended,
    /// or became something else.
    pub fn messenger_removed(&self, phone: &str, key: &str) {
        let removed = {
            let mut desk = lock(&self.desk);
            let before = desk.messenger.len();
            desk.messenger
                .retain(|c| !(c.phone == phone && c.key == key));
            desk.messenger.len() != before
        };
        if removed {
            self.changed();
        }
    }

    /// Act on a messenger's call of `phone` when the action is for one: an
    /// answer or decline while one rings and no telephony call does, an end
    /// while one is up. `None` when the action is for a telephony call.
    pub(super) fn messenger_act(
        &self,
        phone: &str,
        action: &CallAction,
    ) -> Option<Result<(), String>> {
        let desk = lock(&self.desk);
        let theirs = || {
            desk.messenger
                .iter()
                .filter(|c| c.phone.eq_ignore_ascii_case(phone))
        };
        let phones = desk.phones();
        let telephony: Vec<&Call> = phones
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(phone))
            .flat_map(|p| p.calls.iter())
            .filter(|c| c.id < MESSENGER_IDS)
            .collect();
        let telephony_ringing = telephony.iter().any(|c| c.state.is_incoming_offer());
        let telephony_up = telephony.iter().any(|c| !c.state.is_incoming_offer());
        let result = match action {
            CallAction::Answer | CallAction::Decline if !telephony_ringing => {
                let call = theirs().find(|c| c.ringing())?;
                let button = match action {
                    CallAction::Answer => call.roles.answer,
                    _ => call.roles.decline.or(call.roles.hang_up),
                };
                call.press(button)
            }
            CallAction::HangUp if !telephony_up => {
                let call = theirs().find(|c| !c.ringing())?;
                call.press(call.roles.hang_up.or(call.roles.decline))
            }
            _ => return None,
        };
        Some(result)
    }
}
