//! Sound on the mesh: the audio module on one link, the microphone gate, the
//! streams a machine relays to a peer, and with `calls` a phone's call voice
//! played and recorded here over Bluetooth.
//!
//! Policy over `cursio-audio`'s mechanism, split out of `cursio-core` (which
//! re-exports every module under its old path, behind its `audio` and
//! `calls` features). See `docs/audio-design.md` and `docs/calls-design.md`.

/// Which captured chunks an outbound audio stream bothers to send.
pub mod audio_gate;
/// Sound made on this machine — a call's voice, a phone's music — sent to a
/// peer as one of its application streams.
pub mod audio_relay;
pub mod audio_session;
/// Handing a paired phone's headset and speaker roles from PipeWire to Cursio.
#[cfg(all(target_os = "linux", feature = "calls"))]
pub mod bluetooth_setup;
/// A phone's call voice, played and recorded on this computer's audio.
#[cfg(feature = "calls")]
pub mod call_audio;

#[cfg(test)]
use cursio_core_base::test_support;
use cursio_core_base::{config, health, live, mixer, module};
use cursio_hubs::calls_hub;
#[cfg(feature = "calls")]
use cursio_hubs::presence;
