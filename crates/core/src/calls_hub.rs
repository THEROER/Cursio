//! Where a phone's calls meet this machine, whichever route they took.
//!
//! **On a phone** the call monitor reports here once ([`CallsHub::set_phone`])
//! and every link's [`CallsEngine`] forwards it; requests from computers come
//! back through the [`CallActor`].
//!
//! **On a computer** a phone's calls arrive by up to two routes: the mesh
//! module from the phone, and a Bluetooth hands-free link to it (see
//! `cursio_bluetooth`). They describe the same calls, so a phone is shown
//! once: by its hands-free link while one is up — that is the route the voice
//! takes, and its state comes from the phone's own call list — and by the mesh
//! otherwise. The two are matched by name, because Android hides its own
//! Bluetooth address from apps and the name is what both sides can see.
//!
//! [`CallsEngine`]: crate::calls_session::CallsEngine

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use cursio_protocol::calls::{Call, CallId, CallsMsg};
use cursio_protocol::PeerId;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// What a computer's user asked of a phone's calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CallAction {
    /// Answer the call that is ringing.
    Answer,
    /// Refuse the call that is ringing.
    Decline,
    /// End the call that is up.
    HangUp,
    Dial {
        number: String,
    },
    Mute {
        muted: bool,
    },
    /// Move the voice to this computer (`true`) or back to the phone.
    /// Bluetooth only: over the mesh the voice never leaves the phone.
    AudioHere {
        here: bool,
    },
}

/// Carries out on the phone what a computer asked for.
pub trait CallActor: Send + Sync {
    fn act(&self, msg: &CallsMsg) -> Result<(), String>;
}

/// The Bluetooth hands-free side of a computer, as the hub drives it.
pub trait HandsFreeControl: Send + Sync {
    fn act(&self, phone: &str, action: &CallAction) -> Result<(), String>;
}

/// Told every phone's calls whenever any of them change.
pub type OnChange = Arc<dyn Fn(&[PhoneCalls]) + Send + Sync>;

/// One phone as a computer sees it, for the control socket and the menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneCalls {
    pub name: String,
    pub calls: Vec<Call>,
    /// The voice of the call that is up is playing on this computer.
    pub audio_here: bool,
    /// Reached over Bluetooth hands-free, so the voice can come here.
    pub hands_free: bool,
    /// Why this phone cannot report calls, when it said.
    pub unavailable: Option<String>,
}

#[derive(Default)]
struct PhoneSide {
    status: Option<CallsMsg>,
    calls: Vec<Call>,
    links: Vec<(u64, mpsc::UnboundedSender<CallsMsg>)>,
}

struct MeshPhone {
    token: u64,
    name: String,
    bluetooth_name: Option<String>,
    unavailable: Option<String>,
    calls: Vec<Call>,
    tx: mpsc::UnboundedSender<CallsMsg>,
}

#[derive(Default)]
struct HandsFreePhone {
    name: String,
    calls: Vec<Call>,
    audio_here: bool,
}

#[derive(Default)]
struct Desk {
    mesh: HashMap<PeerId, MeshPhone>,
    hands_free: HashMap<String, HandsFreePhone>,
    control: Option<Arc<dyn HandsFreeControl>>,
    on_change: Option<OnChange>,
}

impl Desk {
    /// Every phone once, its hands-free link standing in for its mesh link.
    fn phones(&self) -> Vec<PhoneCalls> {
        let mut out: Vec<PhoneCalls> = self
            .hands_free
            .values()
            .map(|p| PhoneCalls {
                name: p.name.clone(),
                calls: p.calls.clone(),
                audio_here: p.audio_here,
                hands_free: true,
                unavailable: None,
            })
            .collect();
        for phone in self.mesh.values() {
            if self.hands_free_of(phone).is_none() {
                out.push(PhoneCalls {
                    name: phone.name.clone(),
                    calls: phone.calls.clone(),
                    audio_here: false,
                    hands_free: false,
                    unavailable: phone.unavailable.clone(),
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn hands_free_of(&self, phone: &MeshPhone) -> Option<&String> {
        let names = [Some(&phone.name), phone.bluetooth_name.as_ref()];
        self.hands_free.iter().find_map(|(address, hf)| {
            names
                .iter()
                .flatten()
                .any(|n| n.eq_ignore_ascii_case(&hf.name))
                .then_some(address)
        })
    }
}

#[derive(Default)]
pub struct CallsHub {
    phone: Mutex<PhoneSide>,
    desk: Mutex<Desk>,
}

/// The process's one hub.
pub fn hub() -> &'static CallsHub {
    static HUB: OnceLock<CallsHub> = OnceLock::new();
    HUB.get_or_init(CallsHub::default)
}

impl CallsHub {
    // ---- phone side ----

    /// The phone's call monitor: whether it can report, and every call now.
    pub fn set_phone(&self, status: CallsMsg, calls: Vec<Call>) {
        let mut phone = lock(&self.phone);
        let status_changed = phone.status.as_ref() != Some(&status);
        let calls_changed = phone.calls != calls;
        phone.status = Some(status.clone());
        phone.calls = calls.clone();
        phone.links.retain(|(_, tx)| {
            (!status_changed || tx.send(status.clone()).is_ok())
                && (!calls_changed
                    || tx
                        .send(CallsMsg::Calls {
                            calls: calls.clone(),
                        })
                        .is_ok())
        });
    }

    /// Whether this phone reports calls itself, so the dialer's own call
    /// notification need not travel too: the computer shows the call with
    /// buttons that act on it either way, and two of them would ring twice.
    pub fn reports_calls(&self) -> bool {
        matches!(
            lock(&self.phone).status,
            Some(CallsMsg::Status {
                available: true,
                ..
            })
        )
    }

    /// A link to a computer is up: what to tell it now, and a channel for
    /// what changes later.
    pub fn register_phone_link(&self) -> (Vec<CallsMsg>, mpsc::UnboundedReceiver<CallsMsg>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let token: u64 = rand::random();
        let mut phone = lock(&self.phone);
        let mut now = Vec::new();
        if let Some(status) = &phone.status {
            now.push(status.clone());
            now.push(CallsMsg::Calls {
                calls: phone.calls.clone(),
            });
        }
        phone.links.push((token, tx));
        (now, rx, token)
    }

    pub fn unregister_phone_link(&self, token: u64) {
        lock(&self.phone).links.retain(|(t, _)| *t != token);
    }

    // ---- computer side ----

    /// Show phones' calls somewhere (the notification server) as they change.
    pub fn on_change(&self, f: OnChange) {
        lock(&self.desk).on_change = Some(f);
    }

    pub fn set_hands_free_control(&self, control: Arc<dyn HandsFreeControl>) {
        lock(&self.desk).control = Some(control);
    }

    pub fn register_mesh_phone(
        &self,
        peer: PeerId,
        name: String,
        tx: mpsc::UnboundedSender<CallsMsg>,
    ) -> u64 {
        let token: u64 = rand::random();
        lock(&self.desk).mesh.insert(
            peer,
            MeshPhone {
                token,
                name,
                bluetooth_name: None,
                unavailable: None,
                calls: Vec::new(),
                tx,
            },
        );
        token
    }

    pub fn unregister_mesh_phone(&self, peer: &PeerId, token: u64) {
        let changed = {
            let mut desk = lock(&self.desk);
            if desk.mesh.get(peer).is_some_and(|p| p.token == token) {
                desk.mesh.remove(peer);
                true
            } else {
                false
            }
        };
        if changed {
            self.changed();
        }
    }

    /// What a phone said over the mesh.
    pub fn from_mesh_phone(&self, peer: &PeerId, msg: CallsMsg) {
        {
            let mut desk = lock(&self.desk);
            let Some(phone) = desk.mesh.get_mut(peer) else {
                return;
            };
            match msg {
                CallsMsg::Status {
                    available,
                    reason,
                    bluetooth_name,
                } => {
                    phone.unavailable = (!available)
                        .then(|| reason.unwrap_or_else(|| "calls are off on the phone".into()));
                    phone.bluetooth_name = bluetooth_name;
                }
                CallsMsg::Calls { calls } => phone.calls = calls,
                CallsMsg::Refused { reason } => {
                    tracing::warn!(phone = %phone.name, %reason, "the phone refused a call request");
                    return;
                }
                other => {
                    tracing::debug!(?other, "a phone sent a computer's request; ignored");
                    return;
                }
            }
        }
        self.changed();
    }

    pub fn hands_free_connected(&self, address: &str, name: &str) {
        lock(&self.desk).hands_free.insert(
            address.into(),
            HandsFreePhone {
                name: name.into(),
                ..Default::default()
            },
        );
        self.changed();
    }

    pub fn hands_free_gone(&self, address: &str) {
        lock(&self.desk).hands_free.remove(address);
        self.changed();
    }

    pub fn hands_free_calls(&self, address: &str, calls: Vec<Call>) {
        if let Some(phone) = lock(&self.desk).hands_free.get_mut(address) {
            phone.calls = calls;
        }
        self.changed();
    }

    pub fn hands_free_audio(&self, address: &str, here: bool) {
        if let Some(phone) = lock(&self.desk).hands_free.get_mut(address) {
            phone.audio_here = here;
        }
        self.changed();
    }

    /// Every phone this computer can see calls of.
    pub fn phones(&self) -> Vec<PhoneCalls> {
        lock(&self.desk).phones()
    }

    /// Carry out `action` on the phone called `name` (its peer name, its
    /// Bluetooth name, or its peer id), by hands-free when it is reached that
    /// way and over the mesh otherwise.
    pub fn act(&self, name: &str, action: CallAction) -> Result<(), String> {
        let desk = lock(&self.desk);
        let hands_free = desk
            .hands_free
            .iter()
            .find(|(address, p)| p.name.eq_ignore_ascii_case(name) || *address == name)
            .map(|(address, _)| address.clone())
            .or_else(|| {
                desk.mesh
                    .iter()
                    .find(|(id, p)| id.to_string() == name || p.name.eq_ignore_ascii_case(name))
                    .and_then(|(_, p)| desk.hands_free_of(p).cloned())
            });
        if let (Some(address), Some(control)) = (hands_free, desk.control.clone()) {
            drop(desk);
            return control.act(&address, &action);
        }
        let Some(phone) = desk
            .mesh
            .iter()
            .find(|(id, p)| id.to_string() == name || p.name.eq_ignore_ascii_case(name))
            .map(|(_, p)| p)
        else {
            let names: Vec<String> = desk.phones().into_iter().map(|p| p.name).collect();
            return Err(format!(
                "no phone called {name} is connected; phones: {}",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            ));
        };
        if let Some(reason) = &phone.unavailable {
            return Err(format!("{} cannot do calls: {reason}", phone.name));
        }
        let msg = mesh_request(&phone.calls, action)?;
        phone
            .tx
            .send(msg)
            .map_err(|_| format!("the link to {} just closed", phone.name))
    }

    fn changed(&self) {
        let (phones, f) = {
            let desk = lock(&self.desk);
            (desk.phones(), desk.on_change.clone())
        };
        if let Some(f) = f {
            f(&phones);
        }
    }
}

/// Turn an action into the request the phone acts on, naming the call it
/// concerns: the ringing one to answer or decline, the one up to end.
fn mesh_request(calls: &[Call], action: CallAction) -> Result<CallsMsg, String> {
    let ringing = || {
        calls
            .iter()
            .find(|c| c.state.is_incoming_offer())
            .map(|c| c.id)
            .ok_or_else(|| "no call is ringing".to_string())
    };
    let current = || -> Result<CallId, String> {
        calls
            .iter()
            .find(|c| !c.state.is_incoming_offer())
            .or(calls.first())
            .map(|c| c.id)
            .ok_or_else(|| "there is no call to end".to_string())
    };
    Ok(match action {
        CallAction::Answer => CallsMsg::Answer { id: ringing()? },
        CallAction::Decline => CallsMsg::Decline { id: ringing()? },
        CallAction::HangUp => CallsMsg::HangUp { id: current()? },
        CallAction::Dial { number } => CallsMsg::Dial { number },
        CallAction::Mute { muted } => CallsMsg::Mute { muted },
        CallAction::AudioHere { .. } => {
            return Err(
                "the voice of a call reaches a computer only over bluetooth; pair the phone with this computer"
                    .into(),
            )
        }
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::calls::{CallDirection, CallState};

    fn call(id: CallId, state: CallState) -> Call {
        Call {
            id,
            direction: CallDirection::Incoming,
            state,
            number: Some("+380501234567".into()),
            name: None,
        }
    }

    struct Recorder(Mutex<Vec<(String, CallAction)>>);

    impl HandsFreeControl for Recorder {
        fn act(&self, phone: &str, action: &CallAction) -> Result<(), String> {
            self.0.lock().unwrap().push((phone.into(), action.clone()));
            Ok(())
        }
    }

    #[test]
    fn answering_over_the_mesh_names_the_ringing_call() {
        let hub = CallsHub::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let peer = PeerId::from_u128(1);
        hub.register_mesh_phone(peer, "POCO X7 Pro".into(), tx);
        hub.from_mesh_phone(
            &peer,
            CallsMsg::Calls {
                calls: vec![call(3, CallState::Active), call(4, CallState::Waiting)],
            },
        );
        hub.act("poco x7 pro", CallAction::Answer).unwrap();
        assert_eq!(rx.try_recv().unwrap(), CallsMsg::Answer { id: 4 });
        hub.act("POCO X7 Pro", CallAction::HangUp).unwrap();
        assert_eq!(rx.try_recv().unwrap(), CallsMsg::HangUp { id: 3 });
    }

    #[test]
    fn a_phone_with_a_hands_free_link_is_shown_once_and_driven_over_bluetooth() {
        let hub = CallsHub::default();
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        hub.set_hands_free_control(recorder.clone());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let peer = PeerId::from_u128(1);
        hub.register_mesh_phone(peer, "POCO X7 Pro".into(), tx);
        hub.from_mesh_phone(
            &peer,
            CallsMsg::Status {
                available: true,
                reason: None,
                bluetooth_name: Some("POCO X7 Pro".into()),
            },
        );
        hub.hands_free_connected("9C:9E:D5:78:FE:C4", "POCO X7 Pro");
        let phones = hub.phones();
        assert_eq!(phones.len(), 1);
        assert!(phones[0].hands_free);
        hub.act("POCO X7 Pro", CallAction::Answer).unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(
            recorder.0.lock().unwrap().as_slice(),
            &[("9C:9E:D5:78:FE:C4".to_string(), CallAction::Answer)]
        );
    }

    #[test]
    fn the_voice_cannot_be_asked_over_the_mesh() {
        assert!(mesh_request(&[], CallAction::AudioHere { here: true }).is_err());
        assert!(mesh_request(&[], CallAction::Answer).is_err());
    }

    #[test]
    fn a_link_that_comes_up_mid_call_hears_the_calls_at_once() {
        let hub = CallsHub::default();
        let status = CallsMsg::Status {
            available: true,
            reason: None,
            bluetooth_name: None,
        };
        hub.set_phone(status.clone(), vec![call(1, CallState::Active)]);
        let (now, mut rx, _) = hub.register_phone_link();
        assert_eq!(
            now,
            vec![
                status.clone(),
                CallsMsg::Calls {
                    calls: vec![call(1, CallState::Active)]
                }
            ]
        );
        hub.set_phone(status, Vec::new());
        assert_eq!(
            rx.try_recv().unwrap(),
            CallsMsg::Calls { calls: Vec::new() }
        );
        assert!(hub.reports_calls());
    }
}
