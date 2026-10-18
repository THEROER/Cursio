//! This computer as a paired phone's Bluetooth headset, with the voice going
//! wherever Cursio sends sound.
//!
//! ## Why Cursio is the headset itself
//!
//! Android does not let an ordinary app record or inject a call's audio, so a
//! call's voice cannot cross the mesh the way the rest of the phone's sound
//! does. The one route the phone itself offers is the hands-free profile
//! (HFP): a paired headset gets the call's audio over SCO and controls the
//! call with AT commands. Phone Link and GSConnect both stop there and let the
//! operating system's Bluetooth stack become a headset *device* — which makes
//! the computer switch its speakers and microphone to that headset for the
//! length of the call.
//!
//! Here Cursio plays the hands-free unit itself: it registers the HF profile
//! with BlueZ, holds the RFCOMM link and the SCO socket, and hands the PCM to
//! its own audio path. The computer keeps the output and microphone it was
//! using (or the ones chosen in Cursio, on this machine or another), and no
//! device appears or disappears on the desktop. PipeWire keeps the audio
//! gateway role, so headsets paired to the computer still work through it.
//!
//! ## Layout
//!
//! [`at`] parses what the phone sends and [`hands_free`] runs the protocol
//! without a socket, so both are tested on recorded dialogues. [`msbc`] frames
//! wideband voice on a transparent link, and [`resample`] converts between the
//! link's 8 or 16 kHz mono and the 48 kHz stereo the audio path runs at. The sockets and D-Bus live in the platform half, which on
//! anything but Linux is an error saying so.
//!
//! ## The mesh's own channel
//!
//! [`MeshChannel`] is the odd one out: no Bluetooth profile's protocol at all,
//! just an RFCOMM socket under Cursio's [`MESH_UUID`] that a paired phone
//! opens and the mesh runs over unchanged, as it does over TCP.

pub mod a2dp;
pub mod at;
pub mod hands_free;
pub mod msbc;
pub mod resample;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{paired_phones, HandsFreeService, MediaSinkService, MeshChannel};

/// The Bluetooth address of a remote device, as BlueZ prints it.
pub type Address = String;

/// A phone paired with this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    /// What BlueZ calls it (its alias).
    pub name: String,
    pub address: Address,
    pub connected: bool,
}

#[cfg(not(target_os = "linux"))]
pub fn paired_phones() -> Result<Vec<PairedDevice>, BluetoothError> {
    Err(BluetoothError::Unsupported)
}

/// The service a phone opens to reach the mesh over Bluetooth. Fixed, and the
/// same in the Android app (`BtLink.kt`): it is how the phone finds this
/// machine's channel among everything else the computer offers.
pub const MESH_UUID: &str = "8a3f6c2e-0b4d-4e8a-9c1f-43757273696f";

#[derive(Debug, thiserror::Error)]
pub enum BluetoothError {
    #[error("no system bus: {0}")]
    Bus(String),
    #[error("bluez refused the hands-free profile: {0}; PipeWire holds that role until `cursio bluetooth install` hands it over (or oFono does, if it runs)")]
    Register(String),
    #[error("bluez refused the mesh's bluetooth channel: {0}")]
    RegisterChannel(String),
    #[error("bluetooth socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("no hands-free link to {0}")]
    NotConnected(String),
    #[error("bluetooth hands-free is not implemented on this platform")]
    Unsupported,
}

/// What the user asked of one phone's calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfCommand {
    Answer,
    /// Decline a ringing call or end the one that is up.
    HangUp,
    Dial(String),
    /// Keep the microphone off the call, sending silence instead.
    Mute(bool),
    /// Move the call's audio to the phone (`false`) or back here (`true`).
    AudioHere(bool),
}

/// Where a call's voice is played and recorded on this side.
///
/// Implemented by the node, which owns the audio path: this crate only
/// carries PCM between the SCO socket and it.
pub trait CallAudio: Send {
    /// Play link PCM (mono, at the link's rate).
    fn play(&mut self, pcm: &[i16]);
    /// Fill `out` with `samples` of microphone PCM at the link's rate; short
    /// of recorded sound, with silence.
    fn record(&mut self, samples: usize, out: &mut Vec<i16>);
}

/// Where a phone's music (A2DP) plays on this side.
pub trait MediaAudio: Send {
    /// Interleaved 16-bit PCM at `rate`, `channels` wide, as decoded.
    fn play(&mut self, pcm: &[i16], rate: u32, channels: u8);
}

/// Opens [`MediaAudio`] when a phone starts playing to this computer.
pub trait MediaAudioFactory: Send + Sync {
    /// `phone` is the Bluetooth address, `name` what BlueZ calls the device.
    fn open(&self, phone: &str, name: &str) -> Result<Box<dyn MediaAudio>, String>;
}

/// Opens [`CallAudio`] when a call's voice arrives.
pub trait CallAudioFactory: Send + Sync {
    /// `link_rate` is 8000 for CVSD, 16000 for mSBC.
    fn open(&self, phone: &str, link_rate: u32) -> Result<Box<dyn CallAudio>, String>;
}

/// What happened on one phone's hands-free link. `phone` is its Bluetooth
/// address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// The service level connection is up; `name` is what BlueZ calls it.
    Connected {
        phone: Address,
        name: String,
    },
    Disconnected {
        phone: Address,
    },
    Calls {
        phone: Address,
        calls: Vec<cursio_protocol::calls::Call>,
    },
    Refused {
        phone: Address,
        reason: String,
    },
    /// The call's voice is (or is no longer) flowing through this machine.
    Audio {
        phone: Address,
        here: bool,
    },
}

#[cfg(not(target_os = "linux"))]
pub struct HandsFreeService;

#[cfg(not(target_os = "linux"))]
impl HandsFreeService {
    pub fn start(
        _audio: std::sync::Arc<dyn CallAudioFactory>,
    ) -> Result<(Self, std::sync::mpsc::Receiver<LinkEvent>), BluetoothError> {
        Err(BluetoothError::Unsupported)
    }

    pub fn command(&self, phone: &str, _command: HfCommand) -> Result<(), BluetoothError> {
        Err(BluetoothError::NotConnected(phone.to_string()))
    }
}

#[cfg(not(target_os = "linux"))]
pub struct MediaSinkService;

#[cfg(not(target_os = "linux"))]
impl MediaSinkService {
    pub fn start(_audio: std::sync::Arc<dyn MediaAudioFactory>) -> Result<Self, BluetoothError> {
        Err(BluetoothError::Unsupported)
    }
}
