//! Peers' players offered here as MPRIS players of their own, so the desktop's
//! media controls drive them like any local one.

use crate::media_hub::{hub, MediaDisplay, RemotePlayer};
use crate::media_mpris::OURS;
use crate::media_mpris::PATH;
use cursio_protocol::media::{MediaCommand, PlaybackState};
use cursio_protocol::PeerId;
use std::collections::HashMap;
use std::sync::{mpsc, Mutex};
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

/// Peers' players, each a bus name of its own while it lasts.
#[derive(Default)]
pub struct MprisDisplay {
    servers: Mutex<HashMap<(PeerId, String), mpsc::Sender<RemotePlayer>>>,
    next: std::sync::atomic::AtomicU32,
}

impl MediaDisplay for MprisDisplay {
    fn show(&self, peer: PeerId, peer_name: &str, players: &[RemotePlayer]) {
        let mut servers = self.servers.lock().unwrap_or_else(|p| p.into_inner());
        // Dropping a sender ends its thread, and the bus name goes with it.
        servers.retain(|(p, id), _| *p != peer || players.iter().any(|r| &r.player.id == id));
        for remote in players {
            let key = (peer, remote.player.id.clone());
            if let Some(tx) = servers.get(&key) {
                if tx.send(remote.clone()).is_ok() {
                    continue;
                }
            }
            let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = format!("{OURS}{}_{n}", bus_safe(peer_name));
            let (tx, rx) = mpsc::channel();
            let first = remote.clone();
            let peer_name = peer_name.to_string();
            let spawned = std::thread::Builder::new()
                .name("mpris-peer".into())
                .spawn(move || serve(name, peer, peer_name, first, rx));
            match spawned {
                Ok(_) => {
                    servers.insert(key, tx);
                }
                Err(e) => tracing::warn!(error = %e, "cannot offer a peer's player here"),
            }
        }
    }
}

pub(super) fn bus_safe(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if s.starts_with(|c: char| c.is_ascii_alphabetic()) {
        s
    } else {
        format!("p{s}")
    }
}

/// Where a peer's cover is kept for the desktop to read by `file://` URL.
pub(super) fn art_file(art: u64, data: &(String, Vec<u8>)) -> Option<String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)?
        .join("cursio/media-art");
    std::fs::create_dir_all(&dir).ok()?;
    let ext = if data.0 == "image/png" { "png" } else { "jpg" };
    let path = dir.join(format!("{art:016x}.{ext}"));
    if !path.exists() {
        std::fs::write(&path, &data.1).ok()?;
    }
    Some(format!("file://{}", path.display()))
}

pub(super) struct RootIface {
    identity: String,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl RootIface {
    fn raise(&self) {}
    fn quit(&self) {}
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> String {
        self.identity.clone()
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

pub(super) struct PlayerIface {
    peer: PeerId,
    remote: RemotePlayer,
}

impl PlayerIface {
    fn press(&self, command: MediaCommand) {
        if let Err(e) = hub().command(&self.peer, &self.remote.player.id, command) {
            tracing::debug!(error = %e, "a button for a peer's player went nowhere");
        }
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl PlayerIface {
    fn play(&self) {
        self.press(MediaCommand::Play)
    }
    fn pause(&self) {
        self.press(MediaCommand::Pause)
    }
    fn play_pause(&self) {
        self.press(MediaCommand::PlayPause)
    }
    fn stop(&self) {
        self.press(MediaCommand::Pause)
    }
    fn next(&self) {
        self.press(MediaCommand::Next)
    }
    fn previous(&self) {
        self.press(MediaCommand::Previous)
    }
    fn seek(&self, offset: i64) {
        let now = self.remote.position_ms().unwrap_or(0) as i64;
        let to = (now + offset / 1_000).max(0) as u64;
        self.press(MediaCommand::Seek { position_ms: to })
    }
    fn set_position(&self, _track_id: ObjectPath<'_>, position: i64) {
        self.press(MediaCommand::Seek {
            position_ms: (position / 1_000).max(0) as u64,
        })
    }
    #[zbus(property)]
    fn playback_status(&self) -> String {
        match self.remote.player.state {
            PlaybackState::Playing => "Playing",
            PlaybackState::Paused => "Paused",
            PlaybackState::Stopped => "Stopped",
        }
        .into()
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let p = &self.remote.player;
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        let mut put = |k: &str, v: Value<'_>| {
            if let Ok(v) = OwnedValue::try_from(v) {
                m.insert(k.to_string(), v);
            }
        };
        let track = format!(
            "/dev/cursio/track/{:016x}",
            crate::media_hub::art_hash(&p.title)
        );
        if let Ok(path) = ObjectPath::try_from(track.as_str()) {
            put("mpris:trackid", Value::from(path));
        }
        put("xesam:title", Value::from(p.title.as_str()));
        if !p.artist.is_empty() {
            put("xesam:artist", Value::from(vec![p.artist.as_str()]));
        }
        if !p.album.is_empty() {
            put("xesam:album", Value::from(p.album.as_str()));
        }
        if let Some(d) = p.duration_ms {
            put("mpris:length", Value::from((d * 1_000) as i64));
        }
        if let (Some(art), Some(data)) = (p.art, &self.remote.art) {
            if let Some(url) = art_file(art, data) {
                put("mpris:artUrl", Value::from(url));
            }
        }
        m
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.remote.position_ms().unwrap_or(0) as i64 * 1_000
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        f64::from(self.remote.player.volume.unwrap_or(1.0))
    }
    #[zbus(property)]
    fn set_volume(&mut self, volume: f64) {
        self.press(MediaCommand::SetVolume {
            volume: volume as f32,
        })
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.remote.player.can_next
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.remote.player.can_previous
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.remote.player.can_play
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.remote.player.can_pause
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.remote.player.can_seek
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

/// One peer player's bus name, alive until its sender is dropped.
pub(super) fn serve(
    name: String,
    peer: PeerId,
    peer_name: String,
    first: RemotePlayer,
    updates: mpsc::Receiver<RemotePlayer>,
) {
    let identity = format!("{} on {peer_name}", first.player.app);
    let built = zbus::blocking::connection::Builder::session()
        .and_then(|b| b.name(name.as_str()))
        .and_then(|b| b.serve_at(PATH, RootIface { identity }))
        .and_then(|b| {
            b.serve_at(
                PATH,
                PlayerIface {
                    peer,
                    remote: first,
                },
            )
        })
        .and_then(|b| b.build());
    let conn = match built {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(error = %e, %name, "cannot offer a peer's player here");
            return;
        }
    };
    tracing::debug!(%name, "offering a peer's player");
    let server = conn.object_server();
    while let Ok(remote) = updates.recv() {
        let Ok(iface) = server.interface::<_, PlayerIface>(PATH) else {
            break;
        };
        let emitter = iface.signal_emitter().clone();
        let mut player = iface.get_mut();
        player.remote = remote;
        let emitted = zbus::block_on(async {
            player.playback_status_changed(&emitter).await?;
            player.metadata_changed(&emitter).await?;
            player.can_go_next_changed(&emitter).await?;
            player.can_go_previous_changed(&emitter).await?;
            player.can_play_changed(&emitter).await?;
            player.can_pause_changed(&emitter).await?;
            player.can_seek_changed(&emitter).await?;
            player.volume_changed(&emitter).await
        });
        if let Err(e) = emitted {
            tracing::debug!(error = %e, "could not announce a peer player's change");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_name_becomes_a_valid_bus_name_element() {
        assert_eq!(bus_safe("POCO X7 Pro"), "POCO_X7_Pro");
        assert_eq!(bus_safe("7th"), "p7th");
    }
}
