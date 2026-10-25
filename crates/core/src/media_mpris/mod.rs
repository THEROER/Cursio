//! This desktop's players read over MPRIS, and peers' players offered as
//! MPRIS players of their own.
//!
//! ## Reading: a poll, once a second
//!
//! MPRIS announces changes with `PropertiesChanged`, one subscription per
//! player and one more for players appearing. A poll of `GetAll` on each
//! player is a handful of calls a second and needs no bookkeeping of who
//! appeared; the position is left out of the comparison (it moves while a
//! track plays) except when it jumps, which is a seek.
//!
//! ## Showing: one bus name per peer player
//!
//! MPRIS fixes the object path, so one connection can own only one player.
//! Each peer player gets a connection of its own on a thread of its own, named
//! `org.mpris.MediaPlayer2.cursio_<peer>_<n>`; GNOME's media widget then shows
//! it like any local player, and its buttons come back here. Our own names are
//! skipped when reading, or a peer's player would be sent straight back.
//!
//! All of it runs outside the Tokio runtime: zbus's blocking calls run their
//! future on the calling thread (see `session_bus`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_protocol::media::{MediaCommand, Player, MAX_ART_BYTES};
use zbus::zvariant::{ObjectPath, Value};

use crate::media_hub::{hub, CallPause, MediaBackend};
use crate::media_mpris::read::percent_decode;
use crate::media_mpris::read::read_players;
use crate::media_mpris::read::remote_art;
use crate::media_mpris::read::Snapshot;

mod read;
mod serve;

pub use serve::MprisDisplay;

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const OURS: &str = "org.mpris.MediaPlayer2.cursio_";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

pub struct MprisBackend {
    snapshot: Mutex<Snapshot>,
}

/// Start reading this desktop's players and showing peers' as MPRIS players.
pub fn start() {
    let backend = Arc::new(MprisBackend {
        snapshot: Mutex::new(Snapshot::default()),
    });
    hub().set_backend(backend.clone());
    hub().set_display(Arc::new(MprisDisplay::default()));
    let spawned = std::thread::Builder::new()
        .name("mpris-watch".into())
        .spawn(move || {
            let mut pause = CallPause::default();
            loop {
                backend.poll();
                pause.tick(backend.as_ref());
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot watch this desktop's players");
    }
}

impl MprisBackend {
    fn poll(&self) {
        let Some(read) = crate::session_bus::with_bus(read_players) else {
            return;
        };
        let mut snap = self.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        if crate::media_hub::players_changed(&snap.players, &read.players) {
            *snap = read;
            drop(snap);
            hub().local_changed();
        } else {
            // Keep the fresh positions without telling anyone: they ride along
            // with the next list.
            *snap = read;
        }
    }
}

impl MediaBackend for MprisBackend {
    fn players(&self) -> Vec<Player> {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .players
            .clone()
    }

    fn command(&self, player: &str, command: MediaCommand) -> Result<(), String> {
        if !player.starts_with(PREFIX) || player.starts_with(OURS) {
            return Err(format!("{player} is not a player here"));
        }
        let track = self
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .track_ids
            .get(player)
            .cloned();
        let player = player.to_string();
        let done = crate::session_bus::with_bus(move |bus| -> Result<(), String> {
            let call = |method: &str| {
                bus.call_method(Some(player.as_str()), PATH, Some(PLAYER_IFACE), method, &())
                    .map(|_| ())
            };
            let result = match command {
                MediaCommand::Play => call("Play"),
                MediaCommand::Pause => call("Pause"),
                MediaCommand::PlayPause => call("PlayPause"),
                MediaCommand::Next => call("Next"),
                MediaCommand::Previous => call("Previous"),
                MediaCommand::Seek { position_ms } => {
                    let track = track.ok_or("the player names no track to seek in")?;
                    let path = ObjectPath::try_from(track.as_str()).map_err(|e| e.to_string())?;
                    bus.call_method(
                        Some(player.as_str()),
                        PATH,
                        Some(PLAYER_IFACE),
                        "SetPosition",
                        &(path, (position_ms * 1_000) as i64),
                    )
                    .map(|_| ())
                }
                MediaCommand::SetVolume { volume } => bus
                    .call_method(
                        Some(player.as_str()),
                        PATH,
                        Some("org.freedesktop.DBus.Properties"),
                        "Set",
                        &(
                            PLAYER_IFACE,
                            "Volume",
                            Value::from(f64::from(volume.clamp(0.0, 1.0))),
                        ),
                    )
                    .map(|_| ()),
            };
            result.map_err(|e| e.to_string())
        });
        let result = done.unwrap_or_else(|| Err("no session bus".into()));
        hub().local_changed();
        result
    }

    fn art(&self, _player: &str, art: u64) -> Option<(String, Vec<u8>)> {
        let url = self
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .art_urls
            .get(&art)
            .cloned()?;
        if !url.starts_with("file://") {
            return remote_art(art, &url);
        }
        let path = percent_decode(url.trim_start_matches("file://"));
        let meta = std::fs::metadata(&path).ok()?;
        if meta.len() as usize > MAX_ART_BYTES {
            tracing::debug!(%path, size = meta.len(), "cover too large to send");
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let mime = if path.ends_with(".png") {
            "image/png"
        } else {
            "image/jpeg"
        };
        Some((mime.into(), bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_hub::players_changed as changed;
    use cursio_protocol::media::PlaybackState;

    fn player(state: PlaybackState, position_ms: u64) -> Player {
        Player {
            id: "org.mpris.MediaPlayer2.x".into(),
            app: "X".into(),
            title: "t".into(),
            artist: String::new(),
            album: String::new(),
            state,
            duration_ms: Some(200_000),
            position_ms: Some(position_ms),
            volume: None,
            can_play: true,
            can_pause: true,
            can_next: false,
            can_previous: false,
            can_seek: true,
            art: None,
        }
    }

    #[test]
    fn a_track_playing_on_is_not_a_change() {
        let a = [player(PlaybackState::Playing, 10_000)];
        let b = [player(PlaybackState::Playing, 11_000)];
        assert!(!changed(&a, &b));
    }

    #[test]
    fn a_seek_is_a_change() {
        let a = [player(PlaybackState::Playing, 10_000)];
        let b = [player(PlaybackState::Playing, 60_000)];
        assert!(changed(&a, &b));
    }

    #[test]
    fn a_pause_is_a_change() {
        let a = [player(PlaybackState::Playing, 10_000)];
        let b = [player(PlaybackState::Paused, 10_000)];
        assert!(changed(&a, &b));
    }
}
