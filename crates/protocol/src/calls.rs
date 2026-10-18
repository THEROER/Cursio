//! A phone's calls, answered, placed and ended from a computer.
//!
//! Two routes reach the same calls, and they differ in where the voice goes:
//!
//! - **Bluetooth hands-free (HFP)**, when the computer is paired with the
//!   phone. The computer *is* the phone's headset: the voice crosses the
//!   Bluetooth link and Cursio plays and records it on whatever this machine
//!   is already using. Control is AT commands on that link, and needs no
//!   permission on the phone at all.
//! - **This module**, over the mesh. Android gives an ordinary app no way to
//!   record or inject a call's audio — `VOICE_CALL` capture is reserved to
//!   system apps — so over Wi-Fi the voice stays on the phone (or its own
//!   headset). What crosses is the state of every call and what the user did:
//!   answer, decline, hang up, dial, mute.
//!
//! A desktop that has both uses Bluetooth for a phone it holds a hands-free
//! link to and this module otherwise. The phone always reports through this
//! module, so a machine without Bluetooth still sees who is calling.
//!
//! ## State, not events
//!
//! [`CallsMsg::Calls`] carries every call the phone has right now, and is
//! resent whole on every change. A call that ended is simply absent. Events
//! ("ringing", "answered") would need the receiver to replay them in order and
//! never miss one; a list is right the moment it arrives, including the first
//! one after a link comes up mid-call.

use serde::{Deserialize, Serialize};

/// The phone's number for one call, stable while that call lasts.
pub type CallId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

/// Where one call is. The names are the ones `AT+CLCC` and Android's
/// `Call.STATE_*` share, so neither route needs a table to translate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallState {
    /// Incoming, not yet answered.
    Ringing,
    /// Incoming while another call is active: answering puts that one on hold.
    Waiting,
    /// Outgoing, the network has not reached the other phone yet.
    Dialing,
    /// Outgoing, the other phone is ringing.
    Alerting,
    Active,
    Held,
}

impl CallState {
    /// Still waiting for the user to pick it up.
    pub fn is_incoming_offer(&self) -> bool {
        matches!(self, CallState::Ringing | CallState::Waiting)
    }
}

/// One call, as the phone has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub id: CallId,
    pub direction: CallDirection,
    pub state: CallState,
    /// `None` when the caller withheld it.
    pub number: Option<String>,
    /// The name the phone's contacts give the number, when they do.
    pub name: Option<String>,
}

impl Call {
    /// What to call the other party on a screen: the contact, else the
    /// number, else that it was withheld.
    pub fn who(&self) -> &str {
        self.name
            .as_deref()
            .or(self.number.as_deref())
            .unwrap_or("Unknown caller")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallsMsg {
    /// Phone -> desktop, when the link comes up: whether the phone can report
    /// and act on calls, and the name it goes by over Bluetooth.
    ///
    /// The name is how a desktop recognises the device a hands-free link is to
    /// as this peer: Android hides its own Bluetooth address from apps, and
    /// the name is what BlueZ reports for the paired device.
    Status {
        available: bool,
        /// Why not, in words, when `available` is false: what to grant.
        reason: Option<String>,
        bluetooth_name: Option<String>,
    },
    /// Phone -> desktop: every call there is now, resent whole on change.
    Calls { calls: Vec<Call> },
    /// Desktop -> phone.
    Answer { id: CallId },
    /// Desktop -> phone: refuse a call still ringing.
    Decline { id: CallId },
    /// Desktop -> phone: end a call that is up.
    HangUp { id: CallId },
    /// Desktop -> phone: place a call.
    Dial { number: String },
    /// Desktop -> phone: mute or unmute the phone's microphone.
    Mute { muted: bool },
    /// Phone -> desktop: a request could not be carried out, and why.
    /// Silence would be indistinguishable from a request that never arrived.
    Refused { reason: String },
}

impl CallsMsg {
    /// Control: a ringing call is the one thing on the channel a person is
    /// waiting on, and it is a few bytes.
    pub const PRIORITY: crate::Priority = crate::Priority::Control;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_prefers_the_contact_then_the_number() {
        let mut call = Call {
            id: 1,
            direction: CallDirection::Incoming,
            state: CallState::Ringing,
            number: Some("+380501234567".into()),
            name: Some("Mom".into()),
        };
        assert_eq!(call.who(), "Mom");
        call.name = None;
        assert_eq!(call.who(), "+380501234567");
        call.number = None;
        assert_eq!(call.who(), "Unknown caller");
    }

    #[test]
    fn a_call_list_survives_the_wire() {
        let msg = CallsMsg::Calls {
            calls: vec![Call {
                id: 7,
                direction: CallDirection::Outgoing,
                state: CallState::Alerting,
                number: Some("112".into()),
                name: None,
            }],
        };
        let bytes = bincode::serialize(&msg).unwrap();
        assert_eq!(bincode::deserialize::<CallsMsg>(&bytes).unwrap(), msg);
    }
}
