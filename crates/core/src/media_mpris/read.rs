//! Reading this desktop's players off the session bus, with their covers.

use crate::media_mpris::OURS;
use crate::media_mpris::PATH;
use crate::media_mpris::PLAYER_IFACE;
use crate::media_mpris::PREFIX;
use cursio_protocol::media::{PlaybackState, Player, MAX_ART_BYTES};
use std::collections::HashMap;
use std::sync::Mutex;
use zbus::zvariant::{OwnedValue, Value};

/// What the poll read, with what commands need besides the list.
#[derive(Default)]
pub(super) struct Snapshot {
    pub(super) players: Vec<Player>,
    pub(super) track_ids: HashMap<String, String>,
    pub(super) art_urls: HashMap<u64, String>,
}

pub(super) fn str_of(v: Option<&OwnedValue>) -> Option<String> {
    match &**v? {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        _ => None,
    }
}

pub(super) fn bool_of(v: Option<&OwnedValue>) -> bool {
    matches!(v.map(|v| &**v), Some(Value::Bool(true)))
}

pub(super) fn i64_of(v: Option<&OwnedValue>) -> Option<i64> {
    match &**v? {
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        Value::I32(n) => Some(i64::from(*n)),
        Value::U32(n) => Some(i64::from(*n)),
        _ => None,
    }
}

pub(super) fn strings_of(v: Option<&OwnedValue>) -> Vec<String> {
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

pub(super) fn get_all(
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

pub(super) fn read_players(bus: &zbus::blocking::Connection) -> Snapshot {
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
            .map(|identity| clean_identity(&identity))
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

pub(super) fn percent_decode(s: &str) -> String {
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

/// A player's name as a person reads it. A snap's Firefox reports itself as
/// "Mozilla firefox_firefox": the snap's name glued on after an underscore,
/// repeating a word already there. That repeat goes; anything else stays.
pub(super) fn clean_identity(identity: &str) -> String {
    match identity.rsplit_once('_') {
        Some((name, suffix))
            if !suffix.is_empty()
                && name
                    .split_whitespace()
                    .any(|word| word.eq_ignore_ascii_case(suffix)) =>
        {
            name.to_string()
        }
        _ => identity.to_string(),
    }
}

/// Whether `name` is BlueZ's `mpris-proxy` standing in for a Bluetooth
/// device's player — a phone or a computer playing through this one. That
/// device is not this machine: a peer offers its own players itself, and
/// sending one on made Lightmoon's Spotify reach the phones a second time, as
/// "LIGHTMOON · Nocturne" and without its cover.
pub(super) fn bluetooth_proxy(bus: &zbus::blocking::Connection, name: &str) -> bool {
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
pub(super) fn remote_art(hash: u64, url: &str) -> Option<(String, Vec<u8>)> {
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

    #[test]
    fn a_snaps_name_glued_onto_a_players_identity_is_dropped() {
        assert_eq!(clean_identity("Mozilla firefox_firefox"), "Mozilla firefox");
        assert_eq!(clean_identity("Spotify"), "Spotify");
        assert_eq!(clean_identity("my_player"), "my_player");
    }

    #[test]
    fn a_file_url_is_percent_decoded() {
        assert_eq!(percent_decode("/tmp/a%20b.png"), "/tmp/a b.png");
    }
}
