//! A phone's calls as notifications on this computer, with the buttons that
//! act on them.
//!
//! The same [`NotifyDisplay`] that shows a phone's notifications shows its
//! calls, so a call looks, rings (urgent, until it is over) and is dismissed
//! like every other notification here. One notification per call, replaced in
//! place as the call moves from ringing to talking; a button press goes back
//! through [`hub`] by whichever route reaches the phone.
//!
//! On the machine the user is at, a call gets a window instead: the app's
//! call page, small and on top, as Phone Link does — a ringing phone wants
//! more than a banner that slides away. The node asks for it by starting the
//! app's stable copy with `--page call`; a running app takes that over its
//! single-instance hand-off, so this works whether or not it is open. The
//! notification stays the fallback for a machine with no installed app, and
//! for every machine the user is not at, where a window would pop up over
//! nobody.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use cursio_protocol::calls::{Call, CallState};
use cursio_protocol::notify::{Notification, NotifyAction, NotifyKind};
use cursio_protocol::PeerId;
use tokio::sync::mpsc;

use crate::calls_hub::{hub, CallAction, PhoneCalls};
use crate::notify_hub::{DisplayEvent, NotifyDisplay};

/// Calls are shown under a peer id of their own, so their keys never meet a
/// phone's notification keys.
const CALLS_PEER: PeerId = PeerId::from_u128(0xCA11);

/// The buttons a call shows, in order, and what each does.
fn buttons(phone: &PhoneCalls, call: &Call, muted: bool) -> Vec<(&'static str, CallAction)> {
    match call.state {
        CallState::Ringing | CallState::Waiting => vec![
            ("Answer", CallAction::Answer),
            ("Decline", CallAction::Decline),
        ],
        _ => {
            let mut b = vec![
                ("Hang up", CallAction::HangUp),
                if muted {
                    ("Unmute", CallAction::Mute { muted: false })
                } else {
                    ("Mute", CallAction::Mute { muted: true })
                },
            ];
            if phone.hands_free {
                b.push(if phone.audio_here {
                    ("Use phone", CallAction::AudioHere { here: false })
                } else {
                    ("Use computer", CallAction::AudioHere { here: true })
                });
            }
            b
        }
    }
}

fn describe(phone: &PhoneCalls, call: &Call) -> String {
    let what = match call.state {
        CallState::Ringing => "Incoming call",
        CallState::Waiting => "Call waiting",
        CallState::Dialing | CallState::Alerting => "Calling…",
        CallState::Active => "On a call",
        CallState::Held => "On hold",
    };
    let route = match (call.state, phone.hands_free, phone.audio_here) {
        (CallState::Active, _, true) => " · voice on this computer",
        (CallState::Active, true, false) => " · voice on the phone",
        (CallState::Ringing, false, _) => " · answer here, talk on the phone",
        _ => "",
    };
    format!("{what} on {}{route}", phone.name)
}

struct State {
    /// Shown keys, with the phone and buttons behind each.
    shown: HashMap<String, (String, Vec<CallAction>)>,
    muted: HashSet<String>,
    /// Calls the app's window is showing, which get no notification.
    windowed: HashSet<String>,
}

/// Bring up the app's call window; `false` when there is no app to start.
fn open_call_window() -> bool {
    let Some(app) = crate::update::layout::stable_executable("cursio-app") else {
        return false;
    };
    match std::process::Command::new(&app)
        .args(["--page", "call"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // A running app takes the request and this copy exits at once;
            // reaping it keeps that exit from lingering as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(e) => {
            tracing::warn!(app = %app.display(), error = %e, "cannot open the call window");
            false
        }
    }
}

/// Show phones' calls through `display` from now on.
pub fn start(display: Arc<dyn NotifyDisplay>) {
    let state = Arc::new(Mutex::new(State {
        shown: HashMap::new(),
        muted: HashSet::new(),
        windowed: HashSet::new(),
    }));
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<DisplayEvent>();

    let shown = state.clone();
    hub().on_change(Arc::new(move |phones: &[PhoneCalls]| {
        let mut state = shown.lock().unwrap_or_else(|p| p.into_inner());
        let mut live = HashSet::new();
        for phone in phones {
            if phone.calls.is_empty() {
                state.muted.remove(&phone.name);
            }
            let muted = state.muted.contains(&phone.name);
            for call in &phone.calls {
                let key = format!("call:{}:{}", phone.name, call.id);
                let buttons = buttons(phone, call, muted);
                let notification = Notification {
                    key: key.clone(),
                    app_id: "dev.cursio.calls".into(),
                    app_name: "Phone".into(),
                    title: call.who().to_string(),
                    text: describe(phone, call),
                    icon: None,
                    when_ms: 0,
                    kind: NotifyKind::Call,
                    actions: buttons
                        .iter()
                        .map(|(title, _)| NotifyAction {
                            title: (*title).into(),
                            reply: false,
                        })
                        .collect(),
                };
                let actions: Vec<CallAction> = buttons.into_iter().map(|(_, a)| a).collect();
                let new_call = !state.shown.contains_key(&key);
                if new_call && crate::presence::user_here() && open_call_window() {
                    state.windowed.insert(key.clone());
                }
                let unchanged = state
                    .shown
                    .get(&key)
                    .is_some_and(|(_, shown)| *shown == actions);
                if !unchanged && !state.windowed.contains(&key) {
                    display.show(CALLS_PEER, &phone.name, &notification, events_tx.clone());
                }
                state
                    .shown
                    .insert(key.clone(), (phone.name.clone(), actions));
                live.insert(key);
            }
        }
        let gone: Vec<String> = state
            .shown
            .keys()
            .filter(|k| !live.contains(*k))
            .cloned()
            .collect();
        for key in gone {
            state.shown.remove(&key);
            if !state.windowed.remove(&key) {
                display.close(CALLS_PEER, &key);
            }
        }
    }));

    let spawned = std::thread::Builder::new()
        .name("calls-display".into())
        .spawn(move || {
            while let Some(event) = events_rx.blocking_recv() {
                let DisplayEvent::Invoked { key, action, .. } = event else {
                    // Closing a call's notification is not declining it: the
                    // phone keeps ringing, and the call is still there.
                    continue;
                };
                let target = {
                    let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                    let target = state.shown.get(&key).and_then(|(phone, actions)| {
                        actions
                            .get(action as usize)
                            .map(|a| (phone.clone(), a.clone()))
                    });
                    if let Some((phone, CallAction::Mute { muted })) = &target {
                        if *muted {
                            state.muted.insert(phone.clone());
                        } else {
                            state.muted.remove(phone);
                        }
                        state.shown.remove(&key);
                    }
                    target
                };
                if let Some((phone, call_action)) = target {
                    if let Err(e) = hub().act(&phone, call_action) {
                        tracing::warn!(%phone, error = %e, "a call button did nothing");
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "call buttons will not work");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::calls::CallDirection;

    fn phone(hands_free: bool, audio_here: bool) -> PhoneCalls {
        PhoneCalls {
            name: "POCO X7 Pro".into(),
            calls: Vec::new(),
            audio_here,
            hands_free,
            unavailable: None,
        }
    }

    fn call(state: CallState) -> Call {
        Call {
            id: 1,
            direction: CallDirection::Incoming,
            state,
            number: None,
            name: None,
        }
    }

    #[test]
    fn a_ringing_call_offers_answer_and_decline() {
        let b = buttons(&phone(false, false), &call(CallState::Ringing), false);
        assert_eq!(
            b.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            ["Answer", "Decline"]
        );
    }

    #[test]
    fn moving_the_voice_is_offered_only_over_bluetooth() {
        let mesh = buttons(&phone(false, false), &call(CallState::Active), false);
        assert!(!mesh
            .iter()
            .any(|(_, a)| matches!(a, CallAction::AudioHere { .. })));
        let bt = buttons(&phone(true, true), &call(CallState::Active), true);
        assert_eq!(
            bt.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            ["Hang up", "Unmute", "Use phone"]
        );
    }
}
