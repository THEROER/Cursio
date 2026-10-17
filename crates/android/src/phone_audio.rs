//! The phone's sound settings that must outlive the app's window.
//!
//! The service starts the node after a reboot with no window open, and a send
//! volume the window applied on opening would be full scale until then — the
//! volume that hurt in the first place. So it is kept on disk beside the
//! config and applied in [`crate::init`], before anything can be routed.
//!
//! The chosen device is not kept here: Android asks the user's consent for
//! every playback capture, so a route cannot be resumed by itself after a
//! restart, only while the capture that carries it is still running — which
//! the audio backend already does by the device's name.

use std::path::PathBuf;

use cursio_audio::android_backend;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    volume: f32,
}

fn path() -> Option<PathBuf> {
    cursio_protocol::paths::config_dir().map(|dir| dir.join("phone_audio.json"))
}

/// Apply what was saved, if anything was.
pub(crate) fn restore() {
    let Some(path) = path() else {
        return;
    };
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
            Ok(saved) => android_backend::set_playback_volume(saved.volume),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "unreadable phone audio settings; sending at full volume")
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read phone audio settings; sending at full volume")
        }
    }
}

/// How loud this phone's sound reaches a peer, from 0 to 1, kept for next time.
pub fn set_playback_volume(volume: f32) -> anyhow::Result<()> {
    android_backend::set_playback_volume(volume);
    let path = path().ok_or_else(|| anyhow::anyhow!("no config directory; call init first"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let saved = Saved {
        volume: android_backend::playback_volume(),
    };
    std::fs::write(&path, serde_json::to_vec(&saved)?)?;
    Ok(())
}

pub use android_backend::{awaited_playback, playback_volume};
