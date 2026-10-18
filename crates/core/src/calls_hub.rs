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
//! [`CallsEngine`]: crate::calls_session::CallsEngine

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    voice: VoicePolicy,
}

/// How long after a call starts this computer keeps putting its voice where
/// the call was taken. The phone opens and closes the audio link on its own
/// for a moment around answering; past this window a move is the user's.
const STEER_FOR: Duration = Duration::from_secs(8);

/// Where the voice of a hands-free phone's call should be, decided once as
/// the call starts.
#[derive(Debug, Default)]
struct VoicePolicy {
    /// Where the voice belongs and until when this computer steers it there.
    /// `None` until the call is taken somewhere.
    steer: Option<(bool, Instant)>,
    /// The last move asked of the phone, so a stream of call updates while
    /// the phone gets round to it does not ask again for each.
    asked: Option<bool>,
}

impl VoicePolicy {
    /// Answered or dialled from this computer: the voice comes here.
    fn taken_here(&mut self, now: Instant) {
        self.steer = Some((true, now + STEER_FOR));
        self.asked = None;
    }

    /// The user moved the voice: theirs from now on, nothing is steered.
    fn moved_by_user(&mut self) {
        self.steer = Some((false, Instant::now()));
        self.asked = None;
    }

    /// The phone's calls changed. A call that is up, or being placed, without
    /// having been taken here was taken on the phone, and its voice stays
    /// there; with no call left everything starts over.
    fn calls(&mut self, calls: &[Call], now: Instant) {
        if calls.is_empty() {
            *self = VoicePolicy::default();
            return;
        }
        let started = calls.iter().any(|c| !c.state.is_incoming_offer());
        if started && self.steer.is_none() {
            self.steer = Some((false, now + STEER_FOR));
        }
    }

    /// The move to ask of the phone now, if the voice is not where it
    /// belongs and this computer is still steering it.
    fn correction(&mut self, audio_here: bool, now: Instant) -> Option<bool> {
        let (want, until) = self.steer?;
        if now >= until || audio_here == want || self.asked == Some(want) {
            return None;
        }
        self.asked = Some(want);
        Some(want)
    }
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
        let now = Instant::now();
        let steer = {
            let mut desk = lock(&self.desk);
            desk.hands_free.get_mut(address).and_then(|phone| {
                phone.voice.calls(&calls, now);
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
            {
                let mut desk = lock(&self.desk);
                if let Some(phone) = desk.hands_free.get_mut(&address) {
                    match action {
                        CallAction::Answer | CallAction::Dial { .. } => phone.voice.taken_here(now),
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
            recorder.0.lock().unwrap().first(),
            Some(&("9C:9E:D5:78:FE:C4".to_string(), CallAction::Answer))
        );
    }

    const PHONE: &str = "9C:9E:D5:78:FE:C4";

    fn hands_free_hub() -> (CallsHub, Arc<Recorder>) {
        let hub = CallsHub::default();
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
        policy.calls(&[call(1, CallState::Active)], start);
        assert_eq!(policy.correction(true, start), Some(false));
        let mut later = VoicePolicy::default();
        later.calls(&[call(1, CallState::Active)], start);
        assert_eq!(later.correction(true, start + STEER_FOR), None);
    }

    #[test]
    fn a_new_call_is_steered_afresh_after_the_last_one_ended() {
        let mut policy = VoicePolicy::default();
        let now = Instant::now();
        policy.taken_here(now);
        policy.calls(&[], now);
        policy.calls(&[call(2, CallState::Active)], now);
        assert_eq!(policy.correction(true, now), Some(false));
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
