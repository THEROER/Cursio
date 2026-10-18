//! The phone's sound settings that must outlive the app's window.
//!
//! The service starts the node after a reboot with no window open, and a send
//! volume the window applied on opening would be full scale until then — the
//! volume that hurt in the first place. So it is kept on disk beside the
//! config and applied in [`crate::init`], before anything can be routed.
//!
//! The chosen device is kept too, but only as a *remembered* choice, never
//! handed back to the backend as a route. Android asks the user's consent for
//! every playback capture and the consent dies with the process, so a route
//! resumed by itself after a restart would open a stream with nothing in it.
//! While the process lives the backend already follows the device by name
//! across its peer going away; what this adds is the answer to "where did the
//! sound go before Cursio restarted", which the window turns into one consent
//! prompt instead of the user picking the device again from the list.

use std::path::PathBuf;
use std::sync::Mutex;

use cursio_audio::android_backend::{self, PlaybackTarget};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    volume: f32,
    /// The chosen device by name, `None` when the sound plays on the phone.
    #[serde(default)]
    device: Option<String>,
}

/// The device chosen before this process started, until the user either
/// resumes it or chooses anything else.
static REMEMBERED: Mutex<Option<String>> = Mutex::new(None);

fn remembered() -> std::sync::MutexGuard<'static, Option<String>> {
    REMEMBERED.lock().unwrap_or_else(|p| p.into_inner())
}

fn path() -> Option<PathBuf> {
    cursio_protocol::paths::config_dir().map(|dir| dir.join("phone_audio.json"))
}

fn load() -> Option<Saved> {
    let path = path()?;
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
            Ok(saved) => Some(saved),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "unreadable phone audio settings; sending at full volume to no device");
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read phone audio settings; sending at full volume to no device");
            None
        }
    }
}

fn save(saved: &Saved) -> anyhow::Result<()> {
    let path = path().ok_or_else(|| anyhow::anyhow!("no config directory; call init first"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_vec(saved)?)?;
    Ok(())
}

/// Apply what was saved, if anything was.
pub(crate) fn restore() {
    let Some(saved) = load() else {
        return;
    };
    android_backend::set_playback_volume(saved.volume);
    if let Some(device) = &saved.device {
        tracing::info!(
            device,
            "remembering where this phone's sound went before the restart"
        );
    }
    *remembered() = saved.device;
}

/// How loud this phone's sound reaches a peer, from 0 to 1, kept for next time.
pub fn set_playback_volume(volume: f32) -> anyhow::Result<()> {
    android_backend::set_playback_volume(volume);
    let mut saved = load().unwrap_or_default();
    saved.volume = android_backend::playback_volume();
    save(&saved)
}

/// Send this phone's playback to one device, or to none, kept for next time.
///
/// The name is written only after the backend accepted the route, so a target
/// that vanished meanwhile does not become the device remembered.
pub fn route_playback(target: Option<u64>) -> anyhow::Result<()> {
    let device = match target {
        Some(key) => android_backend::playback_targets()
            .into_iter()
            .find(|t| t.key == key)
            .map(|t| t.name),
        None => None,
    };
    android_backend::route_playback(target)?;
    *remembered() = None;
    let mut saved = load().unwrap_or_else(|| Saved {
        volume: android_backend::playback_volume(),
        device: None,
    });
    saved.device = device;
    save(&saved)
}

/// The device chosen before this process started, while nothing is routed:
/// what the window offers to resume, by name whether or not it is connected.
pub fn remembered_playback() -> Option<String> {
    let routed = android_backend::playback_targets().iter().any(|t| t.routed);
    if routed {
        return None;
    }
    remembered().clone()
}

/// The connected target the remembered device names, if it is connected now.
pub fn resumable_playback() -> Option<PlaybackTarget> {
    let name = remembered_playback()?;
    android_backend::playback_targets()
        .into_iter()
        .find(|t| t.name == name)
}

pub use android_backend::{awaited_playback, playback_volume};
