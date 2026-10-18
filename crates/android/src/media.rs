//! This phone's players, and the computers' players on this phone.
//!
//! `MediaBridge` (Kotlin) reads the phone's active media sessions — the
//! notification listener is what lets an app see them — and pushes them down
//! through `nativeMediaChanged` as JSON. A computer's button press comes back
//! up into `CursioNative.onMediaCommand`.
//!
//! The other way, each computer's players go up to `onMediaRemote` and become
//! a media session and a media-style notification of this app's, which is
//! what HyperOS lifts into its island. Their buttons come back through
//! `nativeMediaRemoteCommand`, and "Disconnect" through `nativeMediaDismiss`.

use std::sync::{Arc, Mutex};

use cursio_core::media_hub::{hub, MediaBackend, MediaDisplay, RemotePlayer};
use cursio_protocol::media::{MediaCommand, PlaybackState, Player};
use cursio_protocol::PeerId;
use jni::objects::{JByteArray, JObject, JValue};
use serde::{Deserialize, Serialize};

static PLAYERS: Mutex<Vec<Player>> = Mutex::new(Vec::new());

/// One player as `MediaBridge` describes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    id: String,
    app: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    album: String,
    state: String,
    duration_ms: Option<i64>,
    position_ms: Option<i64>,
    #[serde(default)]
    can_play: bool,
    #[serde(default)]
    can_pause: bool,
    #[serde(default)]
    can_next: bool,
    #[serde(default)]
    can_previous: bool,
    #[serde(default)]
    can_seek: bool,
    /// Decimal, as a string: a `u64` does not survive a JSON number.
    art: Option<String>,
}

pub fn players(json: &str) -> Result<Vec<Player>, serde_json::Error> {
    let sessions: Vec<Session> = serde_json::from_str(json)?;
    Ok(sessions
        .into_iter()
        .map(|s| Player {
            id: s.id,
            app: s.app,
            title: s.title,
            artist: s.artist,
            album: s.album,
            state: match s.state.as_str() {
                "playing" => PlaybackState::Playing,
                "paused" => PlaybackState::Paused,
                _ => PlaybackState::Stopped,
            },
            duration_ms: s.duration_ms.filter(|d| *d > 0).map(|d| d as u64),
            position_ms: s.position_ms.filter(|p| *p >= 0).map(|p| p as u64),
            volume: None,
            can_play: s.can_play,
            can_pause: s.can_pause,
            can_next: s.can_next,
            can_previous: s.can_previous,
            can_seek: s.can_seek,
            art: s.art.and_then(|a| a.parse().ok()),
        })
        .collect())
}

/// The phone's sessions changed.
pub fn changed(json: &str) {
    match players(json) {
        Ok(list) => {
            *PLAYERS.lock().unwrap_or_else(|p| p.into_inner()) = list;
            hub().local_changed();
        }
        Err(e) => tracing::warn!(error = %e, "unreadable player list from the phone"),
    }
}

pub struct AndroidBackend;

impl AndroidBackend {
    pub fn shared() -> Arc<dyn MediaBackend> {
        Arc::new(AndroidBackend)
    }
}

/// A command as `MediaBridge` and `MediaNotifier` name it, with its number.
pub fn command_name(command: MediaCommand) -> (&'static str, i64) {
    match command {
        MediaCommand::Play => ("play", 0),
        MediaCommand::Pause => ("pause", 0),
        MediaCommand::PlayPause => ("playPause", 0),
        MediaCommand::Next => ("next", 0),
        MediaCommand::Previous => ("previous", 0),
        MediaCommand::Seek { position_ms } => ("seek", position_ms as i64),
        MediaCommand::SetVolume { volume } => ("volume", (volume * 1000.0) as i64),
    }
}

pub fn command_from(name: &str, arg: i64) -> Option<MediaCommand> {
    Some(match name {
        "play" => MediaCommand::Play,
        "pause" => MediaCommand::Pause,
        "playPause" => MediaCommand::PlayPause,
        "next" => MediaCommand::Next,
        "previous" => MediaCommand::Previous,
        "seek" => MediaCommand::Seek {
            position_ms: arg.max(0) as u64,
        },
        "volume" => MediaCommand::SetVolume {
            volume: arg as f32 / 1000.0,
        },
        _ => return None,
    })
}

impl MediaBackend for AndroidBackend {
    fn players(&self) -> Vec<Player> {
        PLAYERS.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn command(&self, player: &str, command: MediaCommand) -> Result<(), String> {
        let (name, arg) = command_name(command);
        let refused = crate::jvm::with_native(|env, class| {
            let player = JObject::from(env.new_string(player)?);
            let name = JObject::from(env.new_string(name)?);
            let answer = env
                .call_static_method(
                    class,
                    "onMediaCommand",
                    "(Ljava/lang/String;Ljava/lang/String;J)Ljava/lang/String;",
                    &[
                        JValue::Object(&player),
                        JValue::Object(&name),
                        JValue::Long(arg),
                    ],
                )?
                .l()?;
            if answer.is_null() {
                Ok(None)
            } else {
                Ok(Some(String::from(env.get_string(&answer.into())?)))
            }
        })?;
        refused.map_or(Ok(()), Err)
    }

    fn art(&self, player: &str, _art: u64) -> Option<(String, Vec<u8>)> {
        let bytes = crate::jvm::with_native(|env, class| {
            let player = JObject::from(env.new_string(player)?);
            let array = env
                .call_static_method(
                    class,
                    "onMediaArt",
                    "(Ljava/lang/String;)[B",
                    &[JValue::Object(&player)],
                )?
                .l()?;
            if array.is_null() {
                return Ok(None);
            }
            Ok(Some(env.convert_byte_array(JByteArray::from(array))?))
        })
        .ok()??;
        Some(("image/jpeg".into(), bytes))
    }
}

/// A computer's player, as `MediaNotifier` reads it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Shown<'a> {
    id: &'a str,
    app: &'a str,
    title: &'a str,
    artist: &'a str,
    album: &'a str,
    playing: bool,
    duration_ms: Option<u64>,
    position_ms: Option<u64>,
    can_play: bool,
    can_pause: bool,
    can_next: bool,
    can_previous: bool,
    can_seek: bool,
    art: Option<String>,
    /// 0.0–1.0, when the computer can set this player's volume.
    volume: Option<f32>,
}

pub fn shown_json(players: &[RemotePlayer]) -> String {
    let shown: Vec<Shown> = players
        .iter()
        .map(|r| Shown {
            id: &r.player.id,
            app: &r.player.app,
            title: &r.player.title,
            artist: &r.player.artist,
            album: &r.player.album,
            playing: r.player.state == PlaybackState::Playing,
            duration_ms: r.player.duration_ms,
            position_ms: r.position_ms(),
            can_play: r.player.can_play,
            can_pause: r.player.can_pause,
            can_next: r.player.can_next,
            can_previous: r.player.can_previous,
            can_seek: r.player.can_seek,
            // Only once fetched: the notifier asks for the bytes by this.
            art: r.art.as_ref().and(r.player.art).map(|a| a.to_string()),
            volume: r.player.volume,
        })
        .collect();
    serde_json::to_string(&shown).unwrap_or_else(|_| "[]".into())
}

pub struct AndroidDisplay;

impl AndroidDisplay {
    pub fn shared() -> Arc<dyn MediaDisplay> {
        Arc::new(AndroidDisplay)
    }
}

impl MediaDisplay for AndroidDisplay {
    fn show(&self, peer: PeerId, peer_name: &str, players: &[RemotePlayer]) {
        let json = shown_json(players);
        let shown = crate::jvm::with_native(|env, class| {
            let peer = JObject::from(env.new_string(peer.to_string())?);
            let name = JObject::from(env.new_string(peer_name)?);
            let json = JObject::from(env.new_string(json)?);
            env.call_static_method(
                class,
                "onMediaRemote",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                &[
                    JValue::Object(&peer),
                    JValue::Object(&name),
                    JValue::Object(&json),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = shown {
            tracing::warn!(error = %e, "could not show a computer's player");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_list_from_the_bridge_reads_into_players() {
        let json = r#"[{"id":"com.spotify#1","app":"Spotify","title":"T","artist":"A",
            "state":"playing","durationMs":180000,"positionMs":-1,"canPlay":true,
            "canPause":true,"canNext":true,"art":"18446744073709551615"}]"#;
        let list = players(json).unwrap();
        assert_eq!(list[0].state, PlaybackState::Playing);
        assert_eq!(list[0].position_ms, None);
        assert_eq!(list[0].art, Some(u64::MAX));
        assert!(!list[0].can_seek);
    }

    #[test]
    fn a_command_survives_the_trip_through_its_name() {
        for c in [
            MediaCommand::PlayPause,
            MediaCommand::Seek { position_ms: 5_000 },
            MediaCommand::Next,
        ] {
            let (name, arg) = command_name(c);
            assert_eq!(command_from(name, arg), Some(c));
        }
    }
}
