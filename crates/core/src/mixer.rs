//! The listener's own volume for each peer stream playing on this machine.
//!
//! The owner of a stream already has a knob for it (`AudioGain`, driven from
//! the owner's side); this is the listener's, for a machine whose own mixer
//! cannot reach a peer's stream by name — a phone, where every stream Cursio
//! plays is one app to the system and the volume rocker moves all of them.
//! The two are multiplied, so neither overrides the other.
//!
//! Plain data with no platform dependency, so the control socket can carry it
//! on a build without the audio module: every link's audio engine fills it,
//! and whatever draws the UI reads and sets it through
//! [`Command::SetStreamVolume`](crate::control::Command::SetStreamVolume).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// Shared by every link's engine and the control socket.
#[derive(Clone, Default)]
pub struct LocalMixer(Arc<Mutex<MixerState>>);

#[derive(Default)]
struct MixerState {
    streams: BTreeMap<u32, MixerStream>,
    generation: u64,
}

/// One stream playing here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MixerStream {
    pub id: u32,
    pub peer: String,
    pub name: String,
    /// Linear, 1.0 = as sent.
    pub volume: f32,
    pub muted: bool,
}

/// Loudest a listener may set a stream: half again over what was sent, enough
/// for a quiet source without inviting clipping.
pub const MAX_LOCAL_VOLUME: f32 = 1.5;

impl LocalMixer {
    fn state(&self) -> MutexGuard<'_, MixerState> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn streams(&self) -> Vec<MixerStream> {
        self.state().streams.values().cloned().collect()
    }

    /// Set one stream's volume and mute. False if no such stream plays here.
    pub fn set(&self, id: u32, volume: f32, muted: bool) -> bool {
        let mut state = self.state();
        let Some(stream) = state.streams.get_mut(&id) else {
            return false;
        };
        stream.volume = volume.clamp(0.0, MAX_LOCAL_VOLUME);
        stream.muted = muted;
        state.generation += 1;
        true
    }

    /// A stream started playing. A playback reopened on another device keeps
    /// the volume it had.
    pub fn add(&self, id: u32, peer: &str, name: &str) {
        self.state()
            .streams
            .entry(id)
            .or_insert_with(|| MixerStream {
                id,
                peer: peer.to_owned(),
                name: name.to_owned(),
                volume: 1.0,
                muted: false,
            });
    }

    pub fn remove(&self, id: u32) {
        self.state().streams.remove(&id);
    }

    /// The factor this stream's audio is scaled by here.
    pub fn level(&self, id: u32) -> f32 {
        self.state()
            .streams
            .get(&id)
            .map_or(1.0, |s| if s.muted { 0.0 } else { s.volume })
    }

    /// Bumped by every [`set`](Self::set), so an engine applies a change once.
    pub fn generation(&self) -> u64 {
        self.state().generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_mixer_scales_mutes_and_refuses_unknown_streams() {
        let mixer = LocalMixer::default();
        assert!(!mixer.set(7, 0.5, false), "nothing plays as stream 7 yet");
        mixer.add(7, "Nocturne", "Firefox");
        assert_eq!(mixer.level(7), 1.0);
        let before = mixer.generation();
        assert!(mixer.set(7, 0.5, false));
        assert_eq!(mixer.level(7), 0.5);
        assert_ne!(
            mixer.generation(),
            before,
            "a change must be seen by engines"
        );
        assert!(mixer.set(7, 0.5, true));
        assert_eq!(mixer.level(7), 0.0);
        assert!(mixer.set(7, 9.0, false));
        assert_eq!(mixer.level(7), MAX_LOCAL_VOLUME);
    }

    #[test]
    fn a_reopened_playback_keeps_its_local_volume() {
        let mixer = LocalMixer::default();
        mixer.add(7, "Nocturne", "Firefox");
        mixer.set(7, 0.3, false);
        mixer.add(7, "Nocturne", "Firefox");
        assert_eq!(mixer.level(7), 0.3);
        mixer.remove(7);
        assert_eq!(mixer.level(7), 1.0);
        assert!(mixer.streams().is_empty());
    }
}
