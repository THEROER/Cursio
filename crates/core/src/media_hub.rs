//! Where this machine's players meet the mesh, and peers' players are shown.
//!
//! One per process, like the other hubs: the players are the machine's, not a
//! link's, so a backend reads them once and every link sends the same list.
//! A link's engine only carries lists and button presses; what is played here
//! comes from a [`MediaBackend`] and what a peer plays goes to a
//! [`MediaDisplay`] — MPRIS players on a Linux desktop, a media notification
//! on a phone.
//!
//! ## Hidden until the next track
//!
//! A phone's "Disconnect" on a computer's player hides that player rather than
//! stopping anything: a video left paused in a browser tab otherwise sits in
//! the island for the rest of the day. It comes back when the title changes —
//! the next thing that plays there is worth showing again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use cursio_protocol::media::{MediaCommand, Player, PlayerId};
use cursio_protocol::PeerId;
use tokio::sync::mpsc;

/// This machine's players.
pub trait MediaBackend: Send + Sync {
    fn players(&self) -> Vec<Player>;
    fn command(&self, player: &str, command: MediaCommand) -> Result<(), String>;
    /// The cover `art` names, as `(mime, bytes)`.
    fn art(&self, player: &str, art: u64) -> Option<(String, Vec<u8>)>;
}

/// Where peers' players are shown.
pub trait MediaDisplay: Send + Sync {
    /// Everything `peer` plays that is to be shown now; empty when nothing is.
    fn show(&self, peer: PeerId, peer_name: &str, players: &[RemotePlayer]);
}

/// A peer's player, with what is needed to show it.
#[derive(Debug, Clone)]
pub struct RemotePlayer {
    pub player: Player,
    /// When the list carrying it arrived, to advance its position from.
    pub received: Instant,
    /// Its cover, once fetched.
    pub art: Option<Arc<(String, Vec<u8>)>>,
}

impl RemotePlayer {
    /// Where the track is now, advanced from when the list was sent.
    pub fn position_ms(&self) -> Option<u64> {
        let at = self.player.position_ms?;
        let moved = match self.player.state {
            cursio_protocol::media::PlaybackState::Playing => {
                self.received.elapsed().as_millis() as u64
            }
            _ => 0,
        };
        let now = at.saturating_add(moved);
        Some(self.player.duration_ms.map_or(now, |d| now.min(d)))
    }
}

/// What a link carries out on the peer's behalf.
pub enum Outgoing {
    Command {
        player: PlayerId,
        command: MediaCommand,
    },
    ArtRequest {
        player: PlayerId,
        art: u64,
    },
}

struct Remote {
    name: String,
    players: Vec<RemotePlayer>,
    /// Only a link that shows a peer's players keeps them here.
    shown: bool,
}

#[derive(Default)]
struct Inner {
    backend: Option<Arc<dyn MediaBackend>>,
    display: Option<Arc<dyn MediaDisplay>>,
    remotes: HashMap<PeerId, Remote>,
    links: HashMap<PeerId, (u64, mpsc::UnboundedSender<Outgoing>)>,
    art: HashMap<u64, Arc<(String, Vec<u8>)>>,
    /// Art already asked for, so a list resent before the answer arrives does
    /// not ask again.
    asked: HashMap<u64, Instant>,
    dismissed: HashMap<(PeerId, PlayerId), String>,
    next_token: u64,
}

pub struct MediaHub {
    inner: Mutex<Inner>,
    /// Bumped when this machine's own players change; links compare it.
    generation: std::sync::atomic::AtomicU64,
}

pub fn hub() -> &'static MediaHub {
    static HUB: OnceLock<MediaHub> = OnceLock::new();
    HUB.get_or_init(|| MediaHub {
        inner: Mutex::new(Inner::default()),
        generation: std::sync::atomic::AtomicU64::new(1),
    })
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl MediaHub {
    pub fn set_backend(&self, backend: Arc<dyn MediaBackend>) {
        lock(&self.inner).backend = Some(backend);
        self.local_changed();
    }

    pub fn set_display(&self, display: Arc<dyn MediaDisplay>) {
        lock(&self.inner).display = Some(display);
    }

    pub fn backend(&self) -> Option<Arc<dyn MediaBackend>> {
        lock(&self.inner).backend.clone()
    }

    /// This machine's players changed; every link sends the new list.
    pub fn local_changed(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn local_players(&self) -> Vec<Player> {
        self.backend().map(|b| b.players()).unwrap_or_default()
    }

    pub(crate) fn register(
        &self,
        peer: PeerId,
        name: String,
        shown: bool,
    ) -> (mpsc::UnboundedReceiver<Outgoing>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = lock(&self.inner);
        inner.next_token += 1;
        let token = inner.next_token;
        inner.links.insert(peer, (token, tx));
        inner.remotes.insert(
            peer,
            Remote {
                name,
                players: Vec::new(),
                shown,
            },
        );
        (rx, token)
    }

    pub(crate) fn unregister(&self, peer: &PeerId, token: u64) {
        let mut inner = lock(&self.inner);
        if inner.links.get(peer).is_some_and(|(t, _)| *t == token) {
            inner.links.remove(peer);
            if let Some(remote) = inner.remotes.remove(peer) {
                if remote.shown {
                    if let Some(display) = inner.display.clone() {
                        drop(inner);
                        display.show(*peer, &remote.name, &[]);
                    }
                }
            }
        }
    }

    /// A peer sent its list.
    pub(crate) fn set_remote(&self, peer: PeerId, players: Vec<Player>) {
        let now = Instant::now();
        let mut inner = lock(&self.inner);
        let mut wanted = Vec::new();
        let Some(remote) = inner.remotes.get(&peer) else {
            return;
        };
        if !remote.shown {
            return;
        }
        let list: Vec<RemotePlayer> = players
            .into_iter()
            .map(|player| RemotePlayer {
                art: player.art.and_then(|a| inner.art.get(&a).cloned()),
                received: now,
                player,
            })
            .collect();
        for p in &list {
            if let (Some(art), None) = (p.player.art, &p.art) {
                let stale = inner
                    .asked
                    .get(&art)
                    .is_none_or(|at| at.elapsed().as_secs() > 30);
                if stale {
                    wanted.push((p.player.id.clone(), art));
                }
            }
        }
        for (_, art) in &wanted {
            inner.asked.insert(*art, now);
        }
        // A new title brings a dismissed player back.
        inner.dismissed.retain(|(p, id), title| {
            *p != peer
                || list
                    .iter()
                    .any(|r| &r.player.id == id && &r.player.title == title)
        });
        if let Some(remote) = inner.remotes.get_mut(&peer) {
            remote.players = list;
        }
        if let Some((_, tx)) = inner.links.get(&peer) {
            for (player, art) in wanted {
                let _ = tx.send(Outgoing::ArtRequest { player, art });
            }
        }
        drop(inner);
        self.redisplay(&peer);
    }

    /// A cover arrived.
    pub(crate) fn store_art(&self, peer: &PeerId, art: u64, mime: String, bytes: Vec<u8>) {
        let mut inner = lock(&self.inner);
        let art_ref = Arc::new((mime, bytes));
        inner.art.insert(art, art_ref.clone());
        // Covers come and go with tracks; a few hundred is a long evening.
        if inner.art.len() > 256 {
            let keep: Vec<u64> = inner
                .remotes
                .values()
                .flat_map(|r| r.players.iter().filter_map(|p| p.player.art))
                .collect();
            inner.art.retain(|k, _| keep.contains(k));
        }
        if let Some(remote) = inner.remotes.get_mut(peer) {
            for p in &mut remote.players {
                if p.player.art == Some(art) {
                    p.art = Some(art_ref.clone());
                }
            }
        }
        drop(inner);
        self.redisplay(peer);
    }

    fn redisplay(&self, peer: &PeerId) {
        let inner = lock(&self.inner);
        let Some(display) = inner.display.clone() else {
            return;
        };
        let Some(remote) = inner.remotes.get(peer) else {
            return;
        };
        let shown: Vec<RemotePlayer> = remote
            .players
            .iter()
            .filter(|p| !inner.dismissed.contains_key(&(*peer, p.player.id.clone())))
            .cloned()
            .collect();
        let name = remote.name.clone();
        drop(inner);
        display.show(*peer, &name, &shown);
    }

    /// Press a button on a peer's player.
    pub fn command(
        &self,
        peer: &PeerId,
        player: &str,
        command: MediaCommand,
    ) -> Result<(), String> {
        let inner = lock(&self.inner);
        let (_, tx) = inner
            .links
            .get(peer)
            .ok_or_else(|| "that machine is not connected".to_string())?;
        tx.send(Outgoing::Command {
            player: player.to_string(),
            command,
        })
        .map_err(|_| "that machine's link is closing".to_string())
    }

    /// Hide a peer's player until it plays something else.
    pub fn dismiss(&self, peer: &PeerId, player: &str) {
        let mut inner = lock(&self.inner);
        let title = inner
            .remotes
            .get(peer)
            .and_then(|r| r.players.iter().find(|p| p.player.id == player))
            .map(|p| p.player.title.clone());
        if let Some(title) = title {
            inner.dismissed.insert((*peer, player.to_string()), title);
        }
        drop(inner);
        self.redisplay(peer);
    }

    /// A peer's cover, already fetched.
    pub fn art(&self, art: u64) -> Option<Arc<(String, Vec<u8>)>> {
        lock(&self.inner).art.get(&art).cloned()
    }
}

/// Pauses what plays here while a phone has a call, and resumes it after —
/// what KDE Connect does. Kept per process: the backend's poll calls
/// [`CallPause::tick`] about once a second.
#[derive(Default)]
pub struct CallPause {
    paused: Vec<PlayerId>,
}

impl CallPause {
    pub fn tick(&mut self, backend: &dyn MediaBackend) {
        let calling = crate::calls_hub::hub()
            .phones()
            .iter()
            .any(|p| !p.calls.is_empty());
        self.step(calling, crate::presence::user_here(), backend);
    }

    fn step(&mut self, calling: bool, here: bool, backend: &dyn MediaBackend) {
        use cursio_protocol::media::PlaybackState;
        if calling {
            if !here {
                return;
            }
            for player in backend.players() {
                if player.state == PlaybackState::Playing
                    && player.can_pause
                    && !self.paused.contains(&player.id)
                    && backend.command(&player.id, MediaCommand::Pause).is_ok()
                {
                    tracing::info!(app = %player.app, "paused for a call");
                    self.paused.push(player.id);
                }
            }
        } else {
            for id in self.paused.drain(..) {
                if let Err(e) = backend.command(&id, MediaCommand::Play) {
                    tracing::debug!(player = %id, error = %e, "could not resume after a call");
                }
            }
        }
    }
}

/// Whether two lists differ in anything a remote control shows. A position
/// counts only when it jumped: a second's drift is a track playing.
pub(crate) fn players_changed(old: &[Player], new: &[Player]) -> bool {
    if old.len() != new.len() {
        return true;
    }
    old.iter().zip(new).any(|(a, b)| {
        let mut a2 = a.clone();
        let mut b2 = b.clone();
        a2.position_ms = None;
        b2.position_ms = None;
        if a2 != b2 {
            return true;
        }
        match (a.position_ms, b.position_ms) {
            (Some(x), Some(y)) => {
                let expected = if a.state == cursio_protocol::media::PlaybackState::Playing {
                    x + 1_000
                } else {
                    x
                };
                expected.abs_diff(y) > 3_000
            }
            _ => false,
        }
    })
}

/// The id a cover is asked for by: a hash of whatever names it on this machine.
pub(crate) fn art_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::media::PlaybackState;

    pub(crate) fn player(id: &str, title: &str, state: PlaybackState) -> Player {
        Player {
            id: id.into(),
            app: "App".into(),
            title: title.into(),
            artist: String::new(),
            album: String::new(),
            state,
            duration_ms: Some(100_000),
            position_ms: Some(1_000),
            volume: None,
            can_play: true,
            can_pause: true,
            can_next: true,
            can_previous: true,
            can_seek: true,
            art: None,
        }
    }

    struct Fake {
        players: Mutex<Vec<Player>>,
        commands: Mutex<Vec<(String, MediaCommand)>>,
    }

    impl MediaBackend for Fake {
        fn players(&self) -> Vec<Player> {
            self.players.lock().unwrap().clone()
        }
        fn command(&self, player: &str, command: MediaCommand) -> Result<(), String> {
            self.commands.lock().unwrap().push((player.into(), command));
            Ok(())
        }
        fn art(&self, _: &str, _: u64) -> Option<(String, Vec<u8>)> {
            None
        }
    }

    #[test]
    fn a_call_pauses_what_plays_here_and_resumes_it_after() {
        let fake = Fake {
            players: Mutex::new(vec![
                player("a", "x", PlaybackState::Playing),
                player("b", "y", PlaybackState::Paused),
            ]),
            commands: Mutex::new(Vec::new()),
        };
        let mut pause = CallPause::default();
        pause.step(true, true, &fake);
        pause.step(true, true, &fake);
        pause.step(false, true, &fake);
        assert_eq!(
            *fake.commands.lock().unwrap(),
            vec![
                ("a".to_string(), MediaCommand::Pause),
                ("a".to_string(), MediaCommand::Play)
            ]
        );
    }

    #[test]
    fn a_call_leaves_a_machine_the_user_is_not_at_alone() {
        let fake = Fake {
            players: Mutex::new(vec![player("a", "x", PlaybackState::Playing)]),
            commands: Mutex::new(Vec::new()),
        };
        let mut pause = CallPause::default();
        pause.step(true, false, &fake);
        pause.step(false, false, &fake);
        assert!(fake.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn a_playing_position_moves_on_from_when_the_list_was_sent() {
        let p = RemotePlayer {
            player: player("a", "x", PlaybackState::Playing),
            received: Instant::now() - std::time::Duration::from_secs(2),
            art: None,
        };
        let at = p.position_ms().unwrap();
        assert!((3_000..3_500).contains(&at), "{at}");
        let paused = RemotePlayer {
            player: player("a", "x", PlaybackState::Paused),
            ..p
        };
        assert_eq!(paused.position_ms(), Some(1_000));
    }

    struct Seen(Mutex<Vec<Vec<String>>>);
    impl MediaDisplay for Seen {
        fn show(&self, _: PeerId, _: &str, players: &[RemotePlayer]) {
            self.0
                .lock()
                .unwrap()
                .push(players.iter().map(|p| p.player.title.clone()).collect());
        }
    }

    #[test]
    fn a_dismissed_player_comes_back_with_its_next_track() {
        let hub = MediaHub {
            inner: Mutex::new(Inner::default()),
            generation: std::sync::atomic::AtomicU64::new(1),
        };
        let seen = Arc::new(Seen(Mutex::new(Vec::new())));
        hub.set_display(seen.clone());
        let peer = PeerId::from_bytes([3; 16]);
        let (_rx, _token) = hub.register(peer, "Nocturne".into(), true);
        hub.set_remote(peer, vec![player("a", "one", PlaybackState::Playing)]);
        hub.dismiss(&peer, "a");
        hub.set_remote(peer, vec![player("a", "one", PlaybackState::Paused)]);
        hub.set_remote(peer, vec![player("a", "two", PlaybackState::Playing)]);
        let seen = seen.0.lock().unwrap();
        assert_eq!(
            *seen,
            vec![
                vec!["one".to_string()],
                vec![],
                vec![],
                vec!["two".to_string()]
            ]
        );
    }

    #[test]
    fn a_cover_is_asked_for_once_while_the_answer_is_on_its_way() {
        let hub = MediaHub {
            inner: Mutex::new(Inner::default()),
            generation: std::sync::atomic::AtomicU64::new(1),
        };
        let peer = PeerId::from_bytes([4; 16]);
        let (mut rx, _) = hub.register(peer, "Nocturne".into(), true);
        let mut p = player("a", "one", PlaybackState::Playing);
        p.art = Some(9);
        hub.set_remote(peer, vec![p.clone()]);
        hub.set_remote(peer, vec![p]);
        let mut asked = 0;
        while let Ok(out) = rx.try_recv() {
            if matches!(out, Outgoing::ArtRequest { art: 9, .. }) {
                asked += 1;
            }
        }
        assert_eq!(asked, 1);
    }
}
