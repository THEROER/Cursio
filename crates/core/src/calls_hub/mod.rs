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
//! **Where the voice goes** follows where the call was taken, as on Phone
//! Link: answered or dialled from this computer, it comes here; answered on
//! the phone, it stays there — Android otherwise routes it to any connected
//! hands-free unit, which put a call the user picked up in their hand on the
//! desk speakers. That is steered once, as the call starts ([`VoicePolicy`]);
//! a move made afterwards, here or on the phone, is the user's and stands.
//!
//! **A messenger's call** (WhatsApp, Telegram) is no telephony call: neither
//! the calls module nor `+CLCC` lists it. It reaches a computer as a phone
//! notification, which the notify module hands here
//! ([`CallsHub::messenger_posted`]) together with which of its buttons answer,
//! decline and end it. It is shown among the phone's calls, in the same
//! window; acting on it presses that button on the phone. Its voice, when the
//! phone has a hands-free link, arrives as Android's "virtual call" and is
//! steered like any other.
//!
//! "Taken here" includes a peer this computer's mouse drives: answered in the
//! call window on the screen beside it, the call reaches the phone over the
//! mesh and this computer only sees it go up. So a call that goes up while
//! the user's hand was on this desk just now ([`crate::presence::at_desk`])
//! counts as taken here too; one picked up with the desk untouched was taken
//! on the phone.
//!
//! **Which machine plays it** is the call audio's business
//! (`call_audio.rs`): this computer or the one the user is at. A computer
//! playing a voice relayed to it learns of it from its audio module
//! ([`CallsHub::relayed_voice`]) and shows the call with the voice here.
//!
//! [`CallsEngine`]: crate::calls_session::CallsEngine

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use cursio_protocol::calls::{Call, CallId, CallsMsg};
use cursio_protocol::PeerId;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::calls_hub::desk::Desk;
use crate::calls_hub::desk::HandsFreePhone;
use crate::calls_hub::desk::MeshPhone;
use crate::calls_hub::desk::PhoneSide;
use crate::config::CallsConfig;

mod desk;
mod messenger;
mod setup;
mod voice;

pub use setup::{AudioDevice, CallSetup, PairedPhone, ServiceState};

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
    /// The machine the voice plays on when this computer carries it but sends
    /// it on to the one the user is at. `None` with `audio_here` is this one.
    #[serde(default)]
    pub voice_on: Option<String>,
    /// Reached over Bluetooth hands-free, so the voice can come here.
    pub hands_free: bool,
    /// Why this phone cannot report calls, when it said.
    pub unavailable: Option<String>,
}

#[derive(Default)]
pub struct CallsHub {
    phone: Mutex<PhoneSide>,
    desk: Mutex<Desk>,
    settings: Mutex<CallsConfig>,
    /// `None` where this build carries no Bluetooth calls at all.
    setup: Mutex<Option<CallSetup>>,
    at_desk: DeskProbe,
}

/// Whether the user's hand was on this desk just now; a fake in tests, where
/// the real answer is whatever the developer's own keyboard says.
struct DeskProbe(fn() -> bool);

impl Default for DeskProbe {
    fn default() -> Self {
        DeskProbe(crate::presence::at_desk)
    }
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

    /// Take the user's `[calls]` settings, at start and whenever they change.
    pub fn configure(&self, settings: CallsConfig) {
        *lock(&self.settings) = settings;
    }

    pub fn settings(&self) -> CallsConfig {
        lock(&self.settings).clone()
    }

    pub fn set_setup(&self, setup: CallSetup) {
        *lock(&self.setup) = Some(setup);
    }

    pub fn setup(&self) -> Option<CallSetup> {
        lock(&self.setup).clone()
    }

    /// What BlueZ calls the phone at `address`, while its hands-free link is
    /// up.
    pub fn hands_free_name(&self, address: &str) -> Option<String> {
        lock(&self.desk)
            .hands_free
            .get(address)
            .map(|p| p.name.clone())
    }

    /// A peer (`from`) started or stopped playing `phone`'s call voice here.
    pub fn relayed_voice(&self, from: &str, phone: &str, on: bool) {
        {
            let mut desk = lock(&self.desk);
            desk.relayed
                .retain(|(p, f)| !(p.eq_ignore_ascii_case(phone) && f == from));
            if on {
                desk.relayed.push((phone.to_string(), from.to_string()));
            }
        }
        self.changed();
    }

    /// Where this computer sends the voice of a hands-free phone's call:
    /// `Some(machine)` when it plays on another one, `None` when here.
    pub fn voice_sent_to(&self, address: &str, machine: Option<String>) {
        let changed = lock(&self.desk)
            .hands_free
            .get_mut(address)
            .is_some_and(|phone| {
                let changed = phone.voice_on != machine;
                phone.voice_on = machine;
                changed
            });
        if changed {
            self.changed();
        }
    }

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
        let now = Instant::now();
        let steering = self.settings().answer_moves_voice;
        let steer = {
            let mut desk = lock(&self.desk);
            desk.hands_free.get_mut(address).and_then(|phone| {
                phone.voice.calls(&calls, now, steering, self.at_desk.0);
                phone.calls = calls;
                phone.voice.correction(phone.audio_here, now)
            })
        };
        self.steer_voice(address, steer);
        self.changed();
    }

    pub fn hands_free_audio(&self, address: &str, here: bool) {
        let steer = {
            let mut desk = lock(&self.desk);
            desk.hands_free.get_mut(address).and_then(|phone| {
                phone.audio_here = here;
                phone.voice.correction(here, Instant::now())
            })
        };
        self.steer_voice(address, steer);
        self.changed();
    }

    /// Ask the phone to put the voice where the call was taken.
    fn steer_voice(&self, address: &str, here: Option<bool>) {
        let Some(here) = here else { return };
        let control = lock(&self.desk).control.clone();
        if let Some(control) = control {
            if let Err(e) = control.act(address, &CallAction::AudioHere { here }) {
                tracing::warn!(phone = %address, here, error = %e, "could not move a call's voice");
            }
        }
    }

    /// Every phone this computer can see calls of.
    pub fn phones(&self) -> Vec<PhoneCalls> {
        lock(&self.desk).phones()
    }

    /// Carry out `action` on the phone called `name` (its peer name, its
    /// Bluetooth name, or its peer id), by hands-free when it is reached that
    /// way and over the mesh otherwise.
    pub fn act(&self, name: &str, action: CallAction) -> Result<(), String> {
        if let Some(result) = self.messenger_act(name, &action) {
            if result.is_ok() && action == CallAction::Answer {
                // Its voice arrives as a virtual call on the hands-free link,
                // if there is one: answered here, it belongs here.
                let mut desk = lock(&self.desk);
                if let Some(phone) = desk
                    .hands_free
                    .values_mut()
                    .find(|p| p.name.eq_ignore_ascii_case(name))
                {
                    phone.voice.taken_here(Instant::now());
                }
            }
            return result;
        }
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
            let now = Instant::now();
            let steering = self.settings().answer_moves_voice;
            {
                let mut desk = lock(&self.desk);
                if let Some(phone) = desk.hands_free.get_mut(&address) {
                    match action {
                        CallAction::Answer | CallAction::Dial { .. } if steering => {
                            phone.voice.taken_here(now)
                        }
                        CallAction::Answer | CallAction::Dial { .. } => phone.voice.moved_by_user(),
                        CallAction::AudioHere { .. } => phone.voice.moved_by_user(),
                        _ => {}
                    }
                }
            }
            control.act(&address, &action)?;
            // Answering opens the call on the phone's side; the voice comes
            // here with it rather than waiting for the next update to ask.
            if action == CallAction::Answer {
                let steer = lock(&self.desk)
                    .hands_free
                    .get_mut(&address)
                    .and_then(|phone| phone.voice.correction(phone.audio_here, now));
                self.steer_voice(&address, steer);
            }
            return Ok(());
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
    use crate::calls_hub::voice::VoicePolicy;
    use crate::calls_hub::voice::STEER_FOR;
    use crate::notify_hub::CallRoles;
    use crate::notify_hub::DisplayEvent;
    use cursio_protocol::calls::{CallDirection, CallState};
    use cursio_protocol::notify::Notification;

    #[test]
    fn an_older_nodes_call_setup_reads_as_one_that_can_be_a_headset() {
        let old: CallSetup = serde_json::from_str(r#"{"roles_taken":true}"#).unwrap();
        assert!(old.headset);
        let windows = CallSetup {
            headset: false,
            ..CallSetup::default()
        };
        let back: CallSetup =
            serde_json::from_str(&serde_json::to_string(&windows).unwrap()).unwrap();
        assert!(!back.headset);
    }

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
            recorder.0.lock().unwrap().first(),
            Some(&("9C:9E:D5:78:FE:C4".to_string(), CallAction::Answer))
        );
    }

    const PHONE: &str = "9C:9E:D5:78:FE:C4";

    fn hands_free_hub() -> (CallsHub, Arc<Recorder>) {
        let hub = CallsHub {
            at_desk: DeskProbe(|| false),
            ..CallsHub::default()
        };
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        hub.set_hands_free_control(recorder.clone());
        hub.hands_free_connected(PHONE, "POCO X7 Pro");
        (hub, recorder)
    }

    fn asked(recorder: &Recorder) -> Vec<CallAction> {
        recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, a)| a.clone())
            .collect()
    }

    #[test]
    fn answering_here_brings_the_voice_to_this_computer() {
        let (hub, recorder) = hands_free_hub();
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Ringing)]);
        hub.act("POCO X7 Pro", CallAction::Answer).unwrap();
        assert_eq!(
            asked(&recorder),
            [CallAction::Answer, CallAction::AudioHere { here: true }]
        );
        // The phone's updates while it opens the audio link ask nothing more.
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Active)]);
        hub.hands_free_audio(PHONE, false);
        assert_eq!(asked(&recorder).len(), 2);
    }

    #[test]
    fn a_call_answered_on_the_phone_keeps_its_voice_there() {
        let (hub, recorder) = hands_free_hub();
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Ringing)]);
        // A ringing call's in-band ring may come here; that is not steered.
        hub.hands_free_audio(PHONE, true);
        assert!(asked(&recorder).is_empty());
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Active)]);
        assert_eq!(asked(&recorder), [CallAction::AudioHere { here: false }]);
    }

    #[test]
    fn dialling_here_brings_the_voice_once_the_call_is_placed() {
        let (hub, recorder) = hands_free_hub();
        hub.act(
            "POCO X7 Pro",
            CallAction::Dial {
                number: "+380501234567".into(),
            },
        )
        .unwrap();
        hub.hands_free_calls(PHONE, vec![call(2, CallState::Dialing)]);
        assert_eq!(
            asked(&recorder),
            [
                CallAction::Dial {
                    number: "+380501234567".into()
                },
                CallAction::AudioHere { here: true }
            ]
        );
    }

    #[test]
    fn a_move_the_user_made_is_not_undone() {
        let (hub, recorder) = hands_free_hub();
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Active)]);
        hub.act("POCO X7 Pro", CallAction::AudioHere { here: true })
            .unwrap();
        hub.hands_free_audio(PHONE, true);
        assert_eq!(asked(&recorder), [CallAction::AudioHere { here: true }]);
    }

    #[test]
    fn the_voice_is_left_alone_once_the_call_has_settled() {
        let mut policy = VoicePolicy::default();
        let start = Instant::now();
        policy.calls(&[call(1, CallState::Active)], start, true, || false);
        assert_eq!(policy.correction(true, start), Some(false));
        let mut later = VoicePolicy::default();
        later.calls(&[call(1, CallState::Active)], start, true, || false);
        assert_eq!(later.correction(true, start + STEER_FOR), None);
    }

    #[test]
    fn a_new_call_is_steered_afresh_after_the_last_one_ended() {
        let mut policy = VoicePolicy::default();
        let now = Instant::now();
        policy.taken_here(now);
        policy.calls(&[], now, true, || false);
        policy.calls(&[call(2, CallState::Active)], now, true, || false);
        assert_eq!(policy.correction(true, now), Some(false));
    }

    #[test]
    fn a_call_answered_on_the_screen_this_desk_drives_brings_the_voice_here() {
        let hub = CallsHub {
            at_desk: DeskProbe(|| true),
            ..CallsHub::default()
        };
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        hub.set_hands_free_control(recorder.clone());
        hub.hands_free_connected(PHONE, "POCO X7 Pro");
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Ringing)]);
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Active)]);
        assert_eq!(asked(&recorder), [CallAction::AudioHere { here: true }]);
    }

    #[test]
    fn with_answering_left_to_the_phone_nothing_moves_the_voice() {
        let (hub, recorder) = hands_free_hub();
        hub.configure(CallsConfig {
            answer_moves_voice: false,
            ..CallsConfig::default()
        });
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Ringing)]);
        hub.act("POCO X7 Pro", CallAction::Answer).unwrap();
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Active)]);
        hub.hands_free_audio(PHONE, false);
        assert_eq!(asked(&recorder), [CallAction::Answer]);
    }

    #[test]
    fn a_voice_relayed_here_shows_on_the_phone_it_belongs_to() {
        let hub = CallsHub::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let peer = PeerId::from_u128(1);
        hub.register_mesh_phone(peer, "POCO X7 Pro".into(), tx);
        hub.from_mesh_phone(
            &peer,
            CallsMsg::Calls {
                calls: vec![call(1, CallState::Active)],
            },
        );
        assert!(!hub.phones()[0].audio_here);
        hub.relayed_voice("Nocturne", "poco x7 pro", true);
        assert!(hub.phones()[0].audio_here);
        hub.relayed_voice("Nocturne", "POCO X7 Pro", false);
        assert!(!hub.phones()[0].audio_here);
    }

    #[test]
    fn the_machine_a_voice_is_sent_to_is_shown() {
        let (hub, _) = hands_free_hub();
        hub.hands_free_audio(PHONE, true);
        hub.voice_sent_to(PHONE, Some("Lightmoon".into()));
        let phone = &hub.phones()[0];
        assert!(phone.audio_here);
        assert_eq!(phone.voice_on.as_deref(), Some("Lightmoon"));
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

    fn telegram_call(key: &str) -> Notification {
        Notification {
            key: key.into(),
            app_id: "org.telegram.messenger".into(),
            app_name: "Telegram".into(),
            title: "Anaya".into(),
            text: "Incoming call".into(),
            icon: None,
            when_ms: 0,
            kind: cursio_protocol::notify::NotifyKind::Call,
            actions: Vec::new(),
        }
    }

    fn ringing_roles() -> CallRoles {
        CallRoles {
            answer: Some(1),
            decline: Some(0),
            hang_up: None,
        }
    }

    #[test]
    fn a_messengers_call_is_shown_among_the_phones_calls_by_who_and_app() {
        let hub = CallsHub::default();
        let (events, _rx) = mpsc::unbounded_channel();
        hub.messenger_posted("POCO X7 Pro", &telegram_call("k"), events);
        hub.messenger_buttons("POCO X7 Pro", "k", ringing_roles());
        let phones = hub.phones();
        assert_eq!(phones.len(), 1);
        let shown = &phones[0].calls[0];
        assert_eq!(shown.state, CallState::Ringing);
        assert_eq!(shown.name.as_deref(), Some("Anaya · Telegram"));
        hub.messenger_removed("POCO X7 Pro", "k");
        assert!(hub.phones().is_empty());
    }

    #[test]
    fn answering_a_messengers_call_presses_its_answer_button_on_the_phone() {
        let (hub, recorder) = hands_free_hub();
        let (events, mut rx) = mpsc::unbounded_channel();
        hub.messenger_posted("POCO X7 Pro", &telegram_call("k"), events);
        hub.messenger_buttons("POCO X7 Pro", "k", ringing_roles());
        hub.act("POCO X7 Pro", CallAction::Answer).unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DisplayEvent::Invoked {
                key: "k".into(),
                action: 1,
                reply: None
            }
        );
        assert!(asked(&recorder).is_empty(), "nothing goes to hands-free");
        // Its voice then arrives as a virtual call, and is kept here.
        hub.hands_free_calls(
            PHONE,
            vec![Call {
                id: 1,
                direction: CallDirection::Incoming,
                state: CallState::Active,
                number: None,
                name: None,
            }],
        );
        assert_eq!(asked(&recorder), [CallAction::AudioHere { here: true }]);
    }

    #[test]
    fn the_virtual_call_of_a_messenger_is_shown_once_under_its_own_name() {
        let (hub, _recorder) = hands_free_hub();
        let (events, _rx) = mpsc::unbounded_channel();
        hub.messenger_posted("POCO X7 Pro", &telegram_call("k"), events);
        hub.hands_free_calls(
            PHONE,
            vec![Call {
                id: 1,
                direction: CallDirection::Incoming,
                state: CallState::Active,
                number: None,
                name: None,
            }],
        );
        let calls = &hub.phones()[0].calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name.as_deref(), Some("Anaya · Telegram"));
    }

    #[test]
    fn a_telephony_call_ringing_takes_the_answer_rather_than_a_messengers() {
        let (hub, recorder) = hands_free_hub();
        let (events, mut rx) = mpsc::unbounded_channel();
        hub.messenger_posted("POCO X7 Pro", &telegram_call("k"), events);
        hub.messenger_buttons("POCO X7 Pro", "k", ringing_roles());
        hub.hands_free_calls(PHONE, vec![call(1, CallState::Ringing)]);
        hub.act("POCO X7 Pro", CallAction::Answer).unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(asked(&recorder)[0], CallAction::Answer);
    }
}
