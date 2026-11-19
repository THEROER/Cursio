//! What this computer offers a phone's calls: the hands-free and speaker
//! services, the phones paired over Bluetooth, and the devices a call can use.

use serde::{Deserialize, Serialize};

/// This computer as a phone's Bluetooth headset and speaker, for the
/// settings: whether it can be, and with what.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallSetup {
    /// This machine can be a phone's headset and speaker at all (Linux, so
    /// far). Without it the rows below about roles, services and pairing say
    /// nothing true, while where a call's voice plays still matters: another
    /// computer's call can be sent here. Absent from older nodes, which were
    /// all ones that could.
    #[serde(default = "headset_default")]
    pub headset: bool,
    /// PipeWire has left the phone roles to Cursio (`cursio bluetooth
    /// install`); without it neither service can register.
    pub roles_taken: bool,
    /// The hands-free unit: calls' voice through this computer.
    pub hands_free: ServiceState,
    /// The A2DP sink: the phone's sound through this computer.
    pub speaker: ServiceState,
    /// Phones paired with this computer over Bluetooth.
    pub phones: Vec<PairedPhone>,
    /// What a call can play on and record from here.
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
}

pub(super) fn headset_default() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServiceState {
    #[default]
    Starting,
    Running,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedPhone {
    pub name: String,
    pub address: String,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    /// What `[calls] output`/`input` holds to pick it.
    pub id: String,
    pub name: String,
}
