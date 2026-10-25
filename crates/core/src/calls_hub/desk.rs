//! The computer's view of every phone: one entry per phone, by its hands-free
//! link while one is up and by the mesh otherwise.

use crate::calls_hub::messenger::MessengerCall;
use crate::calls_hub::voice::VoicePolicy;
use crate::calls_hub::HandsFreeControl;
use crate::calls_hub::OnChange;
use crate::calls_hub::PhoneCalls;
use cursio_protocol::calls::{Call, CallsMsg};
use cursio_protocol::PeerId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Default)]
pub(super) struct PhoneSide {
    pub(super) status: Option<CallsMsg>,
    pub(super) calls: Vec<Call>,
    pub(super) links: Vec<(u64, mpsc::UnboundedSender<CallsMsg>)>,
}

pub(super) struct MeshPhone {
    pub(super) token: u64,
    pub(super) name: String,
    pub(super) bluetooth_name: Option<String>,
    pub(super) unavailable: Option<String>,
    pub(super) calls: Vec<Call>,
    pub(super) tx: mpsc::UnboundedSender<CallsMsg>,
}

#[derive(Default)]
pub(super) struct HandsFreePhone {
    pub(super) name: String,
    pub(super) calls: Vec<Call>,
    pub(super) audio_here: bool,
    pub(super) voice_on: Option<String>,
    pub(super) voice: VoicePolicy,
}

#[derive(Default)]
pub(super) struct Desk {
    pub(super) mesh: HashMap<PeerId, MeshPhone>,
    pub(super) hands_free: HashMap<String, HandsFreePhone>,
    pub(super) messenger: Vec<MessengerCall>,
    pub(super) control: Option<Arc<dyn HandsFreeControl>>,
    pub(super) on_change: Option<OnChange>,
    /// Phones whose call voice a peer relays to play here: the phone's name
    /// as that peer knows it, and the peers relaying it.
    pub(super) relayed: Vec<(String, String)>,
}

impl Desk {
    /// Every phone once, its hands-free link standing in for its mesh link.
    pub(super) fn phones(&self) -> Vec<PhoneCalls> {
        let mut out: Vec<PhoneCalls> = self
            .hands_free
            .values()
            .map(|p| PhoneCalls {
                name: p.name.clone(),
                calls: p.calls.clone(),
                audio_here: p.audio_here,
                voice_on: p.voice_on.clone(),
                hands_free: true,
                unavailable: None,
            })
            .collect();
        for phone in self.mesh.values() {
            if self.hands_free_of(phone).is_none() {
                let names = [Some(&phone.name), phone.bluetooth_name.as_ref()];
                let relayed_here = self.relayed.iter().any(|(relayed, _)| {
                    names
                        .iter()
                        .flatten()
                        .any(|n| n.eq_ignore_ascii_case(relayed))
                });
                out.push(PhoneCalls {
                    name: phone.name.clone(),
                    calls: phone.calls.clone(),
                    audio_here: relayed_here && !phone.calls.is_empty(),
                    voice_on: None,
                    hands_free: false,
                    unavailable: phone.unavailable.clone(),
                });
            }
        }
        for call in &self.messenger {
            let at = match out
                .iter()
                .position(|p| p.name.eq_ignore_ascii_case(&call.phone))
            {
                Some(at) => at,
                None => {
                    out.push(PhoneCalls {
                        name: call.phone.clone(),
                        calls: Vec::new(),
                        audio_here: false,
                        voice_on: None,
                        hands_free: false,
                        unavailable: None,
                    });
                    out.len() - 1
                }
            };
            let phone = &mut out[at];
            // The hands-free link sees the same call as a "virtual call" with
            // no number or name; the messenger's own says who it is.
            phone
                .calls
                .retain(|c| c.number.is_some() || c.name.is_some());
            phone.calls.push(call.as_call());
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub(super) fn hands_free_of(&self, phone: &MeshPhone) -> Option<&String> {
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
