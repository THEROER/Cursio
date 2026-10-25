//! This Windows machine's players, read from the system media controls.
//!
//! Every player that shows in Windows' own volume flyout — Spotify, a browser
//! tab, the Media Player — publishes a session to
//! `GlobalSystemMediaTransportControlsSessionManager`, with its track, state,
//! timeline, buttons and cover. That is the one place to read them all from,
//! as MPRIS is on Linux, and it answers a desktop app without any permission.
//!
//! Only this machine's players are read: peers' players are not shown here,
//! since Windows offers no way for an app to publish a session for a player
//! that is not itself. The module is advertised as a source for that reason.
//!
//! The WinRT calls block on their own operations (`.get()`), so all of it runs
//! on a thread of its own and on the module threads asking for a button or a
//! cover — never on the runtime. `CoIncrementMTAUsage` puts every thread of
//! the process in the multithreaded apartment those calls need.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_protocol::media::{MediaCommand, PlaybackState, Player, MAX_ART_BYTES};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;

use crate::media_hub::{art_hash, hub, CallPause, MediaBackend};

pub struct SmtcBackend {
    manager: Manager,
    players: Mutex<Vec<Player>>,
}

/// Start reading this machine's players; false when Windows has no media
/// session manager to read them from.
pub fn start() -> bool {
    if let Err(e) = unsafe { windows::Win32::System::Com::CoIncrementMTAUsage() } {
        tracing::warn!(error = %e, "cannot join the COM apartment the media controls need");
        return false;
    }
    let manager = match Manager::RequestAsync().and_then(|op| op.get()) {
        Ok(manager) => manager,
        Err(e) => {
            tracing::warn!(error = %e, "Windows' media session manager is not available; players are not shared");
            return false;
        }
    };
    let backend = Arc::new(SmtcBackend {
        manager,
        players: Mutex::new(Vec::new()),
    });
    hub().set_backend(backend.clone());
    let spawned = std::thread::Builder::new()
        .name("smtc-watch".into())
        .spawn(move || {
            let mut pause = CallPause::default();
            loop {
                backend.poll();
                pause.tick(backend.as_ref());
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot watch this machine's players");
        return false;
    }
    true
}

impl SmtcBackend {
    fn sessions(&self) -> Vec<Session> {
        self.manager
            .GetSessions()
            .map(|list| list.into_iter().collect())
            .unwrap_or_default()
    }

    fn session(&self, player: &str) -> Option<Session> {
        self.sessions().into_iter().find(|s| {
            s.SourceAppUserModelId()
                .is_ok_and(|id| id == player)
        })
    }

    fn poll(&self) {
        let read: Vec<Player> = self.sessions().iter().filter_map(read_player).collect();
        let mut players = self.players.lock().unwrap_or_else(|p| p.into_inner());
        let changed = crate::media_hub::players_changed(&players, &read);
        *players = read;
        drop(players);
        if changed {
            hub().local_changed();
        }
    }
}

fn read_player(session: &Session) -> Option<Player> {
    let id = session.SourceAppUserModelId().ok()?.to_string();
    let props = session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.get())
        .ok()?;
    let info = session.GetPlaybackInfo().ok()?;
    let controls = info.Controls().ok()?;
    let timeline = session.GetTimelineProperties().ok();
    let state = match info.PlaybackStatus().ok()? {
        Status::Playing => PlaybackState::Playing,
        Status::Paused => PlaybackState::Paused,
        _ => PlaybackState::Stopped,
    };
    let title = props.Title().map(|t| t.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|t| t.to_string()).unwrap_or_default();
    let album = props
        .AlbumTitle()
        .map(|t| t.to_string())
        .unwrap_or_default();
    // A TimeSpan counts 100 ns ticks.
    let ms = |ticks: i64| u64::try_from(ticks / 10_000).ok();
    let (duration_ms, position_ms) = match &timeline {
        Some(t) => {
            let start = t.StartTime().map(|s| s.Duration).unwrap_or(0);
            let end = t.EndTime().map(|e| e.Duration).unwrap_or(0);
            let position = t.Position().map(|p| p.Duration).ok();
            (
                ms(end - start).filter(|d| *d > 0),
                position.and_then(|p| ms(p - start)),
            )
        }
        None => (None, None),
    };
    // The cover is named by the track: Windows hands out a fresh stream
    // reference on every read, which says nothing about whether it changed.
    let art = props
        .Thumbnail()
        .is_ok()
        .then(|| art_hash(&format!("{id}\u{0}{title}\u{0}{artist}\u{0}{album}")));
    let app = app_name(&id);
    Some(Player {
        volume: app_volume(&app),
        app,
        id,
        title,
        artist,
        album,
        state,
        duration_ms,
        position_ms,
        can_play: controls.IsPlayEnabled().unwrap_or(false),
        can_pause: controls.IsPauseEnabled().unwrap_or(false),
        can_next: controls.IsNextEnabled().unwrap_or(false),
        can_previous: controls.IsPreviousEnabled().unwrap_or(false),
        can_seek: controls.IsPlaybackPositionEnabled().unwrap_or(false),
        art,
    })
}

/// A player's volume is its app's own in the Windows mixer: the media
/// controls carry none, and Spotify's in-app slider is not reachable from
/// outside it. The two multiply, so turning this one is heard the same.
#[cfg(feature = "audio")]
fn app_volume(app: &str) -> Option<f32> {
    cursio_audio::app_volume(app)
}

#[cfg(not(feature = "audio"))]
fn app_volume(_app: &str) -> Option<f32> {
    None
}

#[cfg(feature = "audio")]
fn set_app_volume(app: &str, volume: f32) -> Result<(), String> {
    cursio_audio::set_app_volume(app, volume).map_err(|e| e.to_string())
}

#[cfg(not(feature = "audio"))]
fn set_app_volume(_app: &str, _volume: f32) -> Result<(), String> {
    Err("this build has no audio backend to set a player's volume with".into())
}

/// A player's name from its app id: `Spotify.exe` is Spotify, and a packaged
/// app's `Publisher.Name_hash!App` is its last part.
fn app_name(id: &str) -> String {
    let name = id.rsplit('!').next().unwrap_or(id);
    let name = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name);
    let name = name.rsplit('\\').next().unwrap_or(name);
    if name.eq_ignore_ascii_case("app") {
        // `SpotifyAB.SpotifyMusic_zpdnekdrzrea0!App`: the package says it.
        let package = id.split('!').next().unwrap_or(id);
        let package = package.split('_').next().unwrap_or(package);
        return package.rsplit('.').next().unwrap_or(package).to_string();
    }
    name.to_string()
}

impl MediaBackend for SmtcBackend {
    fn players(&self) -> Vec<Player> {
        self.players
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn command(&self, player: &str, command: MediaCommand) -> Result<(), String> {
        let session = self
            .session(player)
            .ok_or_else(|| format!("no player {player} on this machine"))?;
        let done = match command {
            MediaCommand::Play => session.TryPlayAsync(),
            MediaCommand::Pause => session.TryPauseAsync(),
            MediaCommand::PlayPause => session.TryTogglePlayPauseAsync(),
            MediaCommand::Next => session.TrySkipNextAsync(),
            MediaCommand::Previous => session.TrySkipPreviousAsync(),
            MediaCommand::Seek { position_ms } => {
                session.TryChangePlaybackPositionAsync(position_ms as i64 * 10_000)
            }
            MediaCommand::SetVolume { volume } => {
                set_app_volume(&app_name(player), volume)?;
                hub().local_changed();
                return Ok(());
            }
        }
        .and_then(|op| op.get())
        .map_err(|e| e.to_string())?;
        hub().local_changed();
        if done {
            Ok(())
        } else {
            Err(format!("{} refused it", app_name(player)))
        }
    }

    fn art(&self, player: &str, _art: u64) -> Option<(String, Vec<u8>)> {
        let session = self.session(player)?;
        let props = session
            .TryGetMediaPropertiesAsync()
            .and_then(|op| op.get())
            .ok()?;
        let stream = props
            .Thumbnail()
            .ok()?
            .OpenReadAsync()
            .and_then(|op| op.get())
            .ok()?;
        let size = stream.Size().ok()?;
        if size as usize > MAX_ART_BYTES {
            tracing::debug!(player, size, "cover too large to send");
            return None;
        }
        let mime = stream
            .ContentType()
            .map(|t| t.to_string())
            .ok()
            .filter(|t| t.starts_with("image/"))
            .unwrap_or_else(|| "image/jpeg".into());
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?;
        reader.LoadAsync(size as u32).and_then(|op| op.get()).ok()?;
        let mut bytes = vec![0u8; size as usize];
        reader.ReadBytes(&mut bytes).ok()?;
        Some((mime, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn players_are_named_after_their_app_not_its_id() {
        assert_eq!(app_name("Spotify.exe"), "Spotify");
        assert_eq!(
            app_name("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"),
            "Spotify"
        );
        assert_eq!(
            app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "Microsoft.ZuneMusic"
        );
        assert_eq!(app_name("Some.Package_abc!App"), "Package");
    }
}
