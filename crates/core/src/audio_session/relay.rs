//! Sound passed on rather than played: an application's stream carried
//! through to another peer, and the tunnels that outlive a node restarting.

use crate::audio_session::module::send_control;
use crate::audio_session::AudioEngine;
use cursio_protocol::AudioMsg;
use cursio_transport::{TransportError, TransportTx};

/// Grace before an app's capture tap is released after its node leaves an
/// Intent Sink.
///
/// The mixer moving a stream from one sink to another unlinks then relinks
/// within a frame; without a grace period that would drop and rebuild the tap
/// (and its audio) on every re-route. A short delay lets a relink cancel it.
///
/// This releases only the tap. The Remote Stream it feeds stays open — see
/// [`AudioEngine::app_routes`] — so a pause costs nothing beyond the tap.
pub(super) const ROUTED_APP_GRACE: std::time::Duration = std::time::Duration::from_millis(800);

/// How long an app's tunnel is held open after its node disappears.
///
/// Distinguishes "paused, seeking, or reopening its audio client" from "quit".
/// Well beyond any of the former — a Minecraft world load or a Simple Voice Chat
/// reconnect replaces the node within seconds — while still releasing the stream
/// of an app the user actually closed, rather than holding it for the session.
pub(super) const TUNNEL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a node must have lived before its departure counts as a pause
/// rather than the app discarding a node it never really used.
///
/// An app holding several nodes at once made every departure look like a
/// leftover, because *some* other node was always live — so the tunnel was
/// closed immediately and [`AudioEngine::app_routes`] lost its entry. Measured
/// over a day of real use, that path fired 72 times and the reuse it exists to
/// enable ("app returned; re-pointed its tap") fired **zero** times: Chromium
/// keeps a node per tab, so pausing a video always raced against a sibling.
///
/// Age separates the two cases the `still_live` test could not. A node the app
/// opens and drops while starting up (Minecraft, browsers probing devices) is
/// gone within a second or two; a node that actually played audio has lived far
/// longer by the time the user pauses it. Below this, the tunnel is a leftover
/// and closing it is right — that is what keeps a dead stream from answering
/// the peer's demand in place of the live one. Above it, the app is pausing and
/// the tunnel must survive.
pub(super) const NODE_SETTLED_AGE: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a departed node's tunnel is a startup leftover, to be closed at once,
/// rather than an app pausing, whose tunnel must survive.
///
/// A leftover is silent but still advertised, and the peer picks by
/// advertisement order — so leaving one up had it answer demand in place of the
/// node actually producing sound. Being outlived by a sibling is not enough to
/// call it one, though: an app with a node per tab always has a sibling, which
/// made every pause look like a leftover. Age is the part that separates them.
pub(super) fn is_startup_leftover(still_live: bool, age: std::time::Duration) -> bool {
    still_live && age < NODE_SETTLED_AGE
}

impl AudioEngine {
    /// Tear down one per-app Remote Stream and tell the peer.
    ///
    /// Shared by the orphan sweep above and [`Self::reap_idle_tunnels`], which
    /// differ only in *why* the stream is finished, not in what has to be
    /// cleaned up.
    /// Offer the peer every relay made for it since the last pump, and close
    /// the streams of relays whose producer let go.
    pub(super) async fn pump_relays(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        let Some(peer) = self.peer else {
            return Ok(());
        };
        for (relay, pcm) in crate::audio_relay::unclaimed(peer) {
            let id = self.id_base + 500 + self.next_app_slot;
            self.next_app_slot += 1;
            tracing::info!(id, label = %relay.label, "sending sound made here to the peer");
            let meta = cursio_protocol::AudioStreamMeta {
                id,
                role: cursio_protocol::StreamRole::Playback,
                app_name: Some(relay.label.clone()),
                device: None,
                format: cursio_protocol::AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                    sample_kind: cursio_protocol::SampleKind::F32,
                },
                codec: cursio_protocol::AudioCodec::Opus,
            };
            send_control(tx, AudioMsg::StreamAdvertise(meta)).await?;
            self.relays.insert(id, (relay, pcm));
        }
        let ended: Vec<u32> = self
            .relays
            .iter()
            .filter(|(_, (relay, _))| relay.closed())
            .map(|(id, _)| *id)
            .collect();
        for id in ended {
            self.relays.remove(&id);
            self.close_app_stream(tx, id, "its producer stopped")
                .await?;
        }
        Ok(())
    }

    /// Whether a relay is listening to the peer's microphone.
    pub(super) fn relay_wants_mic(&self) -> bool {
        self.relays.values().any(|(relay, _)| relay.wants_mic)
    }

    pub(super) async fn close_app_stream(
        &mut self,
        tx: &mut dyn TransportTx,
        stream_id: u32,
        why: &str,
    ) -> Result<(), TransportError> {
        // Drop this stream's sender-side encoder and rate-control state, so a
        // later app starting on this id begins clean at the default bitrate
        // rather than inheriting a throttled encoder.
        self.encoders.remove(&stream_id);
        self.pending_packets.remove(&stream_id);
        self.gates.remove(&stream_id);
        self.unacked.remove(&stream_id);
        self.sent_since_adapt.remove(&stream_id);
        self.caught_up_streak.remove(&stream_id);
        self.acked_seq.remove(&stream_id);
        self.stream_bitrate.remove(&stream_id);
        self.last_adapt.remove(&stream_id);
        self.routed_app_route.remove(&stream_id);
        self.routing_dirty = true;
        tracing::info!(id = stream_id, why, "closing Remote Stream");
        send_control(
            tx,
            AudioMsg::StreamClose {
                id: stream_id,
                reason: cursio_protocol::AudioStreamCloseReason::ConsumerClosed,
            },
        )
        .await
    }

    /// Close tunnels whose app has been gone long enough to be closed, not paused.
    ///
    /// Keeping a tunnel open across a pause is the point (see the reaper above),
    /// but an app that was quit is never coming back, and its stream id, encoder
    /// and the peer's playback node would otherwise be held for the life of the
    /// session. [`TUNNEL_IDLE_TIMEOUT`] is the line between the two: long enough
    /// that no pause, seek or client restart crosses it, short enough that
    /// quitting an app frees its stream promptly.
    pub(super) async fn reap_idle_tunnels(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        let now = std::time::Instant::now();
        let dead: Vec<String> = self
            .tunnel_idle_since
            .iter()
            .filter(|(_, &since)| now.duration_since(since) >= TUNNEL_IDLE_TIMEOUT)
            .map(|(app, _)| app.clone())
            .collect();
        for app in dead {
            self.tunnel_idle_since.remove(&app);
            let Some((stream_id, _)) = self.app_routes.remove(&app) else {
                continue;
            };
            self.close_app_stream(tx, stream_id, "the app stayed away")
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_session::test_support::advertised;
    use crate::audio_session::test_support::closed;
    use crate::audio_session::test_support::engine;
    use crate::audio_session::test_support::RecordingTx;
    use cursio_protocol::PeerId;

    /// An app that replaces its PipeWire node keeps its stream id.
    ///
    /// Pausing, seeking, or reopening the audio client swaps the node out. When
    /// the route was keyed on the node id, the replacement looked like a brand
    /// new app: the old tunnel closed and a second stream opened — or, for
    /// `PipeWire ALSA [java]` (Minecraft with Simple Voice Chat), nothing
    /// reopened at all and the app stayed silent. Keyed by name, the returning
    /// node inherits the stream that is already up.
    #[test]
    fn an_app_returning_on_a_new_node_keeps_its_stream() {
        let mut e = engine();
        let device = ("vm".to_string(), "Voicemeeter".to_string());

        // Routed once, on node 584.
        e.app_routes.insert("java".into(), (85533, device.clone()));
        e.routed_app_names.insert(584, "java".into());

        // The node goes away; the tunnel is held, not closed.
        e.routed_app_names.remove(&584);
        e.tunnel_idle_since
            .insert("java".into(), std::time::Instant::now());
        assert!(
            e.app_routes.contains_key("java"),
            "the tunnel must survive the node disappearing"
        );

        // It comes back as node 285 — same app, so the same stream id.
        let resumed = e.app_routes.get("java").map(|(id, _)| *id);
        assert_eq!(
            resumed,
            Some(85533),
            "the returning app must reuse its stream, not open a second one"
        );
    }

    /// An app holding several nodes at once must not have them collapsed onto
    /// one stream.
    ///
    /// Firefox opens a node per playing tab, delivered as separate Joined
    /// events. Reusing the app's stream id for the second live node re-pointed
    /// the tunnel at the newcomer and orphaned the tap that was actually
    /// producing sound, so the browser went silent until it happened to
    /// recreate its nodes in a luckier order.
    #[test]
    fn concurrent_nodes_of_one_app_keep_separate_streams() {
        let mut e = engine();
        let device = ("vm".to_string(), "Voicemeeter".to_string());

        // First node of the app takes the name -> stream mapping.
        e.routed_app_names.insert(100, "Firefox".into());
        e.app_routes
            .entry("Firefox".to_string())
            .or_insert_with(|| (85501, device.clone()));

        // A SECOND node of the same app shows up while the first is still live.
        let already_live = e
            .routed_app_names
            .iter()
            .any(|(node, name)| name == "Firefox" && *node != 200);
        assert!(already_live, "the first node must count as live");

        // So it must NOT reuse the first node's stream...
        e.routed_app_names.insert(200, "Firefox".into());
        e.app_routes
            .entry("Firefox".to_string())
            .or_insert_with(|| (85502, device.clone()));

        // ...and the original mapping must survive untouched.
        assert_eq!(
            e.app_routes.get("Firefox").map(|(id, _)| *id),
            Some(85501),
            "a second node must not steal the first node's stream"
        );
    }

    /// A node an app drops while keeping others must not leave its tunnel
    /// behind.
    ///
    /// Minecraft opens two nodes at startup and drops one a second later. The
    /// dead one's tunnel used to be kept "for its return" — but the app had not
    /// gone anywhere, and the peer, choosing between two advertised streams for
    /// the same app, reported `listening=true` for the dead one. The live node's
    /// audio then went nowhere: Minecraft silent after a restart.
    #[test]
    fn a_dropped_node_does_not_leave_a_tunnel_competing_for_the_peer() {
        let mut e = engine();
        let device = ("vm".to_string(), "Voicemeeter".to_string());

        // Two nodes of one app, each with its own stream.
        e.routed_app_names.insert(500, "java".into());
        e.routed_app_names.insert(429, "java".into());
        e.app_routes.insert("java".into(), (85505, device.clone()));

        // Node 429 goes away while 500 keeps playing.
        e.routed_app_names.remove(&429);
        let still_live = e.routed_app_names.values().any(|n| n == "java");
        assert!(
            still_live,
            "the surviving node must be recognised, so 429's tunnel is an orphan"
        );

        // An orphan must NOT start the idle clock — that path is for an app
        // that has genuinely gone quiet.
        assert!(
            !e.tunnel_idle_since.contains_key("java"),
            "an orphaned tunnel is closed outright, not left to expire"
        );

        // And the surviving node keeps its route.
        assert_eq!(
            e.app_routes.get("java").map(|(id, _)| *id),
            Some(85505),
            "closing the orphan must not strip the live node's route"
        );
    }

    /// The idle clock starts only when the app's LAST node goes away.
    #[test]
    fn a_tunnel_stays_while_any_node_of_the_app_lives() {
        let mut e = engine();
        e.routed_app_names.insert(100, "Firefox".into());
        e.routed_app_names.insert(200, "Firefox".into());

        // One node leaves; another is still playing.
        e.routed_app_names.remove(&100);
        let still_live = e.routed_app_names.values().any(|n| n == "Firefox");
        assert!(
            still_live,
            "the tunnel must not start expiring while a node still plays"
        );

        // The last one leaves: now it may expire.
        e.routed_app_names.remove(&200);
        assert!(!e.routed_app_names.values().any(|n| n == "Firefox"));
    }

    /// A node that played for a while and then stopped is a pause, not an
    /// orphan — even while another node of the same app is live.
    ///
    /// Chromium keeps a node per tab, so a paused video was always outlived by
    /// a sibling. Judging by that alone, every pause was read as a startup
    /// leftover: the tunnel closed, `app_routes` lost the app, and the sound did
    /// not come back until something forced a brand new node (seeking did,
    /// un-pausing did not). Over a day of real use the reuse path this exists to
    /// enable never ran once.
    #[test]
    fn a_settled_node_pausing_is_not_treated_as_a_startup_leftover() {
        let long_enough = NODE_SETTLED_AGE + std::time::Duration::from_secs(1);
        assert!(
            !is_startup_leftover(true, long_enough),
            "a node that played for a while is pausing; its tunnel must survive"
        );

        // The case the age test must not give away: Minecraft opens two nodes at
        // startup and drops one immediately. That one never settled, and leaving
        // it advertised had the peer answer demand for a silent stream.
        assert!(
            is_startup_leftover(true, std::time::Duration::from_millis(900)),
            "a node dropped seconds after opening is a leftover and must close"
        );

        // With no sibling the tunnel is the app's only one, so it is held for a
        // return however briefly the node lived.
        assert!(
            !is_startup_leftover(false, std::time::Duration::from_millis(10)),
            "an app's only tunnel is never an orphan"
        );
    }

    #[tokio::test]
    async fn sound_relayed_to_the_peer_goes_as_an_app_stream_and_ends_with_its_producer() {
        let peer = PeerId::new_v4();
        let mut e = engine().relaying_to(peer);
        let relay = crate::audio_relay::open(peer, "Call · POCO X7 Pro", true);
        let mut tx = RecordingTx::default();
        e.pump_relays(&mut tx).await.unwrap();
        let ads = advertised(&tx);
        assert_eq!(ads.len(), 1);
        assert_eq!(ads[0].app_name.as_deref(), Some("Call · POCO X7 Pro"));
        assert!(ads[0].device.is_none());

        drop(relay);
        let mut tx = RecordingTx::default();
        e.pump_relays(&mut tx).await.unwrap();
        assert_eq!(closed(&tx), vec![ads[0].id]);
    }

    #[tokio::test]
    async fn the_peers_microphone_stays_wanted_while_a_relay_hears_it() {
        let peer = PeerId::new_v4();
        let mut e = engine().relaying_to(peer);
        e.peer_streams.insert(476800);
        e.peer_mic = Some(476800);
        e.failed_playbacks.insert(476800);
        let _relay = crate::audio_relay::open(peer, "Call · POCO X7 Pro", true);
        let mut tx = RecordingTx::default();
        e.pump_relays(&mut tx).await.unwrap();
        e.pump_demand(&mut tx).await.unwrap();
        assert_eq!(tx.demands(), vec![(476800, true)]);
    }

    /// The tunnel is kept for a pause, but not forever: an app that was quit
    /// must eventually release its stream.
    #[test]
    fn a_pause_is_kept_but_a_quit_is_collected() {
        assert!(
            TUNNEL_IDLE_TIMEOUT > ROUTED_APP_GRACE,
            "the tunnel must outlive the tap it feeds"
        );
        assert!(
            TUNNEL_IDLE_TIMEOUT >= std::time::Duration::from_secs(10),
            "must comfortably outlast a world load or a voice-chat reconnect"
        );
        assert!(
            TUNNEL_IDLE_TIMEOUT <= std::time::Duration::from_secs(300),
            "a quit app must not hold its stream for the session"
        );
    }
}
