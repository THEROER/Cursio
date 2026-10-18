//! The media module on one link.
//!
//! Symmetric: every machine both has players and may show the peer's. Which
//! half runs is the factory's decision — a phone shows a computer's players
//! (that is what its island is for) but not another phone's, which it could
//! only drive from a second phone in the other hand.
//!
//! The backend is touched off this thread: an MPRIS call or a MediaController
//! is another process's answer, and a slow one must not hold this link's
//! other messages behind it.

use std::sync::Arc;

use async_trait::async_trait;
use cursio_protocol::media::{MediaCommand, MediaMsg, PlayerId, MAX_ART_BYTES};
use cursio_protocol::{DeviceKind, Message, ModuleAdvert, ModuleId, ModuleRole, PeerId};
use cursio_transport::{TransportError, TransportTx};
use tokio::sync::mpsc;

use crate::media_hub::{hub, MediaBackend, Outgoing};
use crate::module::SessionModule;

/// Version of [`MediaMsg`]; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

pub struct MediaEngine {
    peer: PeerId,
    peer_name: String,
    backend: Option<Arc<dyn MediaBackend>>,
    /// The local generation last sent; `0` sends at start.
    told: u64,
    outbox: Option<(mpsc::UnboundedReceiver<Outgoing>, u64)>,
    commands: Vec<(PlayerId, MediaCommand)>,
    art_wanted: Vec<(PlayerId, u64)>,
    /// A finished backend call, and the answer to send if it has one.
    done_tx: mpsc::UnboundedSender<Option<MediaMsg>>,
    done_rx: mpsc::UnboundedReceiver<Option<MediaMsg>>,
    in_flight: usize,
}

impl MediaEngine {
    pub fn new(
        peer: PeerId,
        peer_name: String,
        backend: Option<Arc<dyn MediaBackend>>,
        shows: bool,
    ) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let outbox = Some(hub().register(peer, peer_name.clone(), shows));
        MediaEngine {
            peer,
            peer_name,
            backend,
            told: 0,
            outbox,
            commands: Vec::new(),
            art_wanted: Vec::new(),
            done_tx,
            done_rx,
            in_flight: 0,
        }
    }

    fn on_message(&mut self, msg: MediaMsg) {
        match msg {
            MediaMsg::Players(players) => hub().set_remote(self.peer, players),
            MediaMsg::Command { player, command } => self.commands.push((player, command)),
            MediaMsg::ArtRequest { player, art } => self.art_wanted.push((player, art)),
            MediaMsg::Art { art, mime, bytes } => {
                if !bytes.is_empty() {
                    hub().store_art(&self.peer, art, mime, bytes);
                }
            }
        }
    }
}

async fn send(tx: &mut dyn TransportTx, msg: &MediaMsg) -> Result<(), TransportError> {
    let message = Message::module(ModuleId::MEDIA, MediaMsg::PRIORITY, msg)?;
    tx.send(&message).await
}

#[async_trait]
impl SessionModule for MediaEngine {
    fn name(&self) -> &'static str {
        "media"
    }

    fn id(&self) -> ModuleId {
        ModuleId::MEDIA
    }

    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        tracing::info!(peer = %self.peer_name, "sharing what plays");
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if let Some(backend) = &self.backend {
            let generation = hub().generation();
            if generation != self.told {
                self.told = generation;
                let backend = backend.clone();
                let players = tokio::task::spawn_blocking(move || backend.players())
                    .await
                    .unwrap_or_default();
                send(tx, &MediaMsg::Players(players)).await?;
            }
            for (player, command) in self.commands.drain(..) {
                let backend = backend.clone();
                let done = self.done_tx.clone();
                self.in_flight += 1;
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = backend.command(&player, command) {
                        tracing::debug!(%player, error = %e, "a peer's button press failed here");
                    }
                    // Nothing to send back: the player's new state reaches the
                    // peer with the next list.
                    let _ = done.send(None);
                });
            }
            for (player, art) in self.art_wanted.drain(..) {
                let backend = backend.clone();
                let done = self.done_tx.clone();
                self.in_flight += 1;
                tokio::task::spawn_blocking(move || {
                    let (mime, bytes) = backend
                        .art(&player, art)
                        .filter(|(_, b)| b.len() <= MAX_ART_BYTES)
                        .unwrap_or_default();
                    let _ = done.send(Some(MediaMsg::Art { art, mime, bytes }));
                });
            }
        } else {
            self.commands.clear();
            self.art_wanted.clear();
        }
        while let Ok(msg) = self.done_rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            if let Some(msg) = msg {
                send(tx, &msg).await?;
            }
        }
        if let Some((outbox, _)) = &mut self.outbox {
            while let Ok(out) = outbox.try_recv() {
                let msg = match out {
                    Outgoing::Command { player, command } => MediaMsg::Command { player, command },
                    Outgoing::ArtRequest { player, art } => MediaMsg::ArtRequest { player, art },
                };
                send(tx, &msg).await?;
            }
        }
        Ok(())
    }

    fn has_pending_work(&self) -> bool {
        self.in_flight > 0 || !self.commands.is_empty() || !self.art_wanted.is_empty()
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<MediaMsg>(ModuleId::MEDIA) {
            Ok(Some(msg)) => {
                self.on_message(msg);
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable media message from peer");
                true
            }
        }
    }
}

impl Drop for MediaEngine {
    fn drop(&mut self) {
        if let Some((_, token)) = &self.outbox {
            hub().unregister(&self.peer, *token);
        }
    }
}

/// Builds [`MediaEngine`].
pub struct MediaFactory {
    /// Whether this machine reads its players; the hub's backend answers.
    pub plays: bool,
    /// Whether peers' players are shown here.
    pub shows: bool,
    /// A phone shows only computers' players.
    pub shows_only_computers: bool,
}

impl crate::module::ModuleFactory for MediaFactory {
    fn id(&self) -> ModuleId {
        ModuleId::MEDIA
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        let role = match (self.plays, self.shows) {
            (true, true) => ModuleRole::Both,
            (true, false) => ModuleRole::Source,
            (false, true) => ModuleRole::Sink,
            (false, false) => return None,
        };
        Some(ModuleAdvert::new(ModuleId::MEDIA, VERSION, role))
    }

    fn supported(&self) -> bool {
        self.plays || self.shows
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        let shows =
            self.shows && !(self.shows_only_computers && ctx.peer.device_kind == DeviceKind::Phone);
        let backend = if self.plays { hub().backend() } else { None };
        Ok(Box::new(MediaEngine::new(
            ctx.peer.peer_id,
            ctx.peer_name().to_string(),
            backend,
            shows,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::media::Player;
    use std::sync::Mutex;

    struct Fake(Mutex<Vec<(String, MediaCommand)>>);
    impl MediaBackend for Fake {
        fn players(&self) -> Vec<Player> {
            Vec::new()
        }
        fn command(&self, player: &str, command: MediaCommand) -> Result<(), String> {
            self.0.lock().unwrap().push((player.into(), command));
            Ok(())
        }
        fn art(&self, _: &str, art: u64) -> Option<(String, Vec<u8>)> {
            Some(("image/jpeg".into(), vec![art as u8; 4]))
        }
    }

    #[derive(Default)]
    struct Sent(Vec<MediaMsg>);
    #[async_trait]
    impl TransportTx for Sent {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            if let Ok(Some(m)) = msg.decode_module::<MediaMsg>(ModuleId::MEDIA) {
                self.0.push(m);
            }
            Ok(())
        }
    }

    async fn settle(engine: &mut MediaEngine, tx: &mut Sent) {
        for _ in 0..50 {
            engine.pump(tx).await.unwrap();
            if !engine.has_pending_work() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_peer_s_button_press_reaches_this_machine_s_player() {
        let fake = Arc::new(Fake(Mutex::new(Vec::new())));
        let mut engine = MediaEngine::new(
            PeerId::from_bytes([7; 16]),
            "Peer".into(),
            Some(fake.clone()),
            false,
        );
        let mut tx = Sent::default();
        engine.on_message(MediaMsg::Command {
            player: "spotify".into(),
            command: MediaCommand::Next,
        });
        settle(&mut engine, &mut tx).await;
        assert_eq!(
            *fake.0.lock().unwrap(),
            vec![("spotify".to_string(), MediaCommand::Next)]
        );
        assert!(matches!(tx.0.first(), Some(MediaMsg::Players(_))));
    }

    #[tokio::test]
    async fn asked_for_art_comes_back_with_its_hash() {
        let fake = Arc::new(Fake(Mutex::new(Vec::new())));
        let mut engine = MediaEngine::new(
            PeerId::from_bytes([8; 16]),
            "Peer".into(),
            Some(fake),
            false,
        );
        let mut tx = Sent::default();
        engine.on_message(MediaMsg::ArtRequest {
            player: "p".into(),
            art: 5,
        });
        settle(&mut engine, &mut tx).await;
        assert!(tx
            .0
            .iter()
            .any(|m| matches!(m, MediaMsg::Art { art: 5, bytes, .. } if bytes.len() == 4)));
    }
}
