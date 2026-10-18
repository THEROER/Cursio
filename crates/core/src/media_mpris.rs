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

use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use cursio_protocol::media::{MediaCommand, PlaybackState, Player, MAX_ART_BYTES};
use cursio_protocol::PeerId;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use crate::media_hub::{hub, CallPause, MediaBackend, MediaDisplay, RemotePlayer};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const OURS: &str = "org.mpris.MediaPlayer2.cursio_";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

/// What the poll read, with what commands need besides the list.
#[derive(Default)]
struct Snapshot {
    players: Vec<Player>,
    track_ids: HashMap<String, String>,
    art_urls: HashMap<u64, String>,
}

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


fn str_of(v: Option<&OwnedValue>) -> Option<String> {
    match &**v? {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        _ => None,
    }
}

fn bool_of(v: Option<&OwnedValue>) -> bool {
    matches!(v.map(|v| &**v), Some(Value::Bool(true)))
}

fn i64_of(v: Option<&OwnedValue>) -> Option<i64> {
    match &**v? {
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        Value::I32(n) => Some(i64::from(*n)),
        Value::U32(n) => Some(i64::from(*n)),
        _ => None,
    }
}

fn strings_of(v: Option<&OwnedValue>) -> Vec<String> {
    match v.map(|v| &**v) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| match v {
                Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        Some(Value::Str(s)) => vec![s.to_string()],
        _ => Vec::new(),
    }
}

fn get_all(
    bus: &zbus::blocking::Connection,
    name: &str,
    iface: &str,
) -> Option<HashMap<String, OwnedValue>> {
    let reply = bus
        .call_method(
            Some(name),
            PATH,
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &(iface,),
        )
        .ok()?;
    reply.body().deserialize().ok()
}

fn read_players(bus: &zbus::blocking::Connection) -> Snapshot {
    let mut snap = Snapshot::default();
    let names: Vec<String> = bus
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
        )
        .ok()
        .and_then(|r| r.body().deserialize().ok())
        .unwrap_or_default();
    for name in names
        .into_iter()
        .filter(|n| n.starts_with(PREFIX) && !n.starts_with(OURS))
    {
        if bluetooth_proxy(bus, &name) {
            continue;
        }
        let Some(props) = get_all(bus, &name, PLAYER_IFACE) else {
            continue;
        };
        let app = get_all(bus, &name, "org.mpris.MediaPlayer2")
            .and_then(|p| str_of(p.get("Identity")))
            .unwrap_or_else(|| name.trim_start_matches(PREFIX).to_string());
        let meta: HashMap<String, OwnedValue> = props
            .get("Metadata")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| HashMap::try_from(v).ok())
            .unwrap_or_default();
        let state = match str_of(props.get("PlaybackStatus")).as_deref() {
            Some("Playing") => PlaybackState::Playing,
            Some("Paused") => PlaybackState::Paused,
            _ => PlaybackState::Stopped,
        };
        let art_url = str_of(meta.get("mpris:artUrl")).filter(|u| {
            u.starts_with("file://") || u.starts_with("https://") || u.starts_with("http://")
        });
        let art = art_url.as_deref().map(crate::media_hub::art_hash);
        if let (Some(hash), Some(url)) = (art, art_url) {
            snap.art_urls.insert(hash, url);
        }
        if let Some(track) = str_of(meta.get("mpris:trackid")) {
            snap.track_ids.insert(name.clone(), track);
        }
        snap.players.push(Player {
            id: name.clone(),
            app,
            title: str_of(meta.get("xesam:title")).unwrap_or_default(),
            artist: strings_of(meta.get("xesam:artist")).join(", "),
            album: str_of(meta.get("xesam:album")).unwrap_or_default(),
            state,
            duration_ms: i64_of(meta.get("mpris:length"))
                .filter(|l| *l > 0)
                .map(|l| l as u64 / 1_000),
            position_ms: i64_of(props.get("Position"))
                .filter(|p| *p >= 0)
                .map(|p| p as u64 / 1_000),
            volume: match props.get("Volume").map(|v| &**v) {
                Some(Value::F64(v)) => Some(*v as f32),
                _ => None,
            },
            can_play: bool_of(props.get("CanPlay")),
            can_pause: bool_of(props.get("CanPause")),
            can_next: bool_of(props.get("CanGoNext")),
            can_previous: bool_of(props.get("CanGoPrevious")),
            can_seek: bool_of(props.get("CanSeek")),
            art,
        });
    }
    snap
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

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

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

fn bus_safe(name: &str) -> String {
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
fn art_file(art: u64, data: &(String, Vec<u8>)) -> Option<String> {
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

struct RootIface {
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

struct PlayerIface {
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
        let track = format!("/dev/cursio/track/{:016x}", crate::media_hub::art_hash(&p.title));
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
fn serve(
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

/// Whether `name` is BlueZ's `mpris-proxy` standing in for a Bluetooth
/// device's player — a phone or a computer playing through this one. That
/// device is not this machine: a peer offers its own players itself, and
/// sending one on made Lightmoon's Spotify reach the phones a second time, as
/// "LIGHTMOON · Nocturne" and without its cover.
fn bluetooth_proxy(bus: &zbus::blocking::Connection, name: &str) -> bool {
    let pid: Option<u32> = bus
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(name,),
        )
        .ok()
        .and_then(|r| r.body().deserialize().ok());
    pid.and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok())
        .is_some_and(|comm| comm.trim() == "mpris-proxy")
}

/// A cover the player names by web address, as Spotify does: fetched here
/// and sent like a local one, so the phone needs no network of its own and
/// the mesh carries only its own messages. The last few are kept, since every
/// link's media module asks for the same cover.
fn remote_art(hash: u64, url: &str) -> Option<(String, Vec<u8>)> {
    use gio::prelude::*;
    static CACHE: Mutex<Vec<(u64, String, Vec<u8>)>> = Mutex::new(Vec::new());
    const KEPT: usize = 8;
    {
        let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, mime, bytes)) = cache.iter().find(|(h, ..)| *h == hash) {
            return Some((mime.clone(), bytes.clone()));
        }
    }
    let (bytes, _) = gio::File::for_uri(url)
        .load_contents(gio::Cancellable::NONE)
        .map_err(|e| tracing::debug!(%url, error = %e, "cannot fetch a player's cover"))
        .ok()?;
    if bytes.len() > MAX_ART_BYTES {
        tracing::debug!(%url, size = bytes.len(), "cover too large to send");
        return None;
    }
    let mime = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else {
        "image/jpeg"
    };
    let bytes = bytes.to_vec();
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() >= KEPT {
        cache.remove(0);
    }
    cache.push((hash, mime.into(), bytes.clone()));
    Some((mime.into(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_hub::players_changed as changed;

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

    #[test]
    fn a_peer_name_becomes_a_valid_bus_name_element() {
        assert_eq!(bus_safe("POCO X7 Pro"), "POCO_X7_Pro");
        assert_eq!(bus_safe("7th"), "p7th");
    }

    #[test]
    fn a_file_url_is_percent_decoded() {
        assert_eq!(percent_decode("/tmp/a%20b.png"), "/tmp/a b.png");
    }
}
