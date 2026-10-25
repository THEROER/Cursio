//! The engine as a session module: starting, the pump, and every message
//! the peer sends it.

use crate::audio_gate::GateKind;
use crate::audio_session::ids::MIC_ID_OFFSET;
use crate::audio_session::send::drain_stale;
use crate::audio_session::send::record_sent;
use crate::audio_session::send::take_with_budget;
use crate::audio_session::send::BITRATE_START;
use crate::audio_session::send::MAX_CHUNKS_PER_PUMP;
use crate::audio_session::send::MAX_CHUNK_BACKLOG;
use crate::audio_session::AudioEngine;
use crate::module::SessionModule;
use async_trait::async_trait;
use cursio_protocol::{AudioMsg, Message, ModuleId};
use cursio_transport::{TransportError, TransportTx};

/// Put one audio *control* message on the wire, enveloped for this module.
///
/// Chunks do not go through here — they stay a first-class `Message::AudioChunk`
/// and are sent directly, because the per-chunk rate makes an extra serialize
/// the one cost the envelope cannot justify. See
/// [`AudioMsg`](cursio_protocol::AudioMsg).
pub(super) async fn send_control(
    tx: &mut dyn TransportTx,
    msg: AudioMsg,
) -> Result<(), TransportError> {
    let framed = Message::module(ModuleId::AUDIO, AudioMsg::PRIORITY, &msg)?;
    tx.send(&framed).await
}

#[async_trait]
impl SessionModule for AudioEngine {
    fn name(&self) -> &'static str {
        "audio"
    }

    fn id(&self) -> ModuleId {
        ModuleId::AUDIO
    }

    /// Advertise everything this machine can offer: one stream per output
    /// device, one per input device (microphone), and one per playing
    /// application. There is no global "everything" stream — only apps and
    /// devices cross the boundary (see docs/audio-design.md "Redesign: one
    /// system").
    ///
    /// The peer mirrors each advertisement, so *its* output list shows our
    /// devices individually ("Cursio: Realtek …") and each of our apps arrives as
    /// its own volume-controllable node.
    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let id_base = self.id_base;

        // Blocking, like the app scan in `pump` — but this runs once in `start`,
        // before the session loop exists, so there is nothing to stall.
        let devices = cursio_audio::list_output_devices().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not list output devices; none offered");
            Vec::new()
        });
        tracing::info!(count = devices.len(), "advertising local output devices");

        for (i, dev) in devices.iter().enumerate() {
            let id = id_base + 1 + i as u32;
            self.inbound_targets.insert(id, dev.id.clone());
            self.mixer_names.insert(
                id,
                (dev.description.clone(), crate::mixer::StreamKind::Device),
            );
            // We advertised it ⇒ we OWN this device's replicated state.
            self.owned_device_nodes.insert(dev.id.clone());
            let meta = cursio_protocol::AudioStreamMeta {
                id,
                role: cursio_protocol::StreamRole::Playback,
                app_name: None,
                device: Some((dev.id.clone(), dev.description.clone())),
                format: cursio_protocol::AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                    sample_kind: cursio_protocol::SampleKind::F32,
                },
                codec: cursio_protocol::AudioCodec::Opus,
            };
            tracing::info!(id, device = %dev.description, "advertising device");
            send_control(tx, AudioMsg::StreamAdvertise(meta)).await?;
        }

        // Start the device monitor: it observes our real devices' volume/mute
        // (owner side, broadcast in `pump`) and applies replicas to our Intent
        // Sinks (non-owner side). Best-effort — if it fails, device-state
        // replication is simply absent; audio streaming is unaffected.
        match cursio_audio::DeviceMonitor::start() {
            Ok(m) => {
                self.device_monitor = Some(m);
                tracing::info!(
                    "device-state monitor up — device volumes replicate across machines"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "no device-state monitor; device volumes won't replicate");
            }
        }

        // Microphones, offered as sources the peer can record from. Same shape
        // as the outputs above, but advertised with `StreamRole::Capture` — that
        // is the flag telling the peer to publish a virtual *microphone* instead
        // of a virtual speaker.
        let inputs = cursio_audio::list_input_devices().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not list input devices; no microphones offered");
            Vec::new()
        });
        tracing::info!(count = inputs.len(), "advertising local input devices");

        for (i, dev) in inputs.iter().enumerate() {
            let id = id_base + MIC_ID_OFFSET + i as u32;
            // Start recording it now: the peer only ever *pulls* audio, so the
            // capture has to exist for chunks to flow once it opens the mirror.
            let cap = match cursio_audio::Capture::mic_tap(id, Some(dev.id.clone())) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, device = %dev.description, "cannot record this input");
                    continue;
                }
            };
            let meta = cursio_protocol::AudioStreamMeta {
                id,
                role: cursio_protocol::StreamRole::Capture,
                app_name: None,
                device: Some((dev.id.clone(), dev.description.clone())),
                format: cursio_protocol::AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                    sample_kind: cursio_protocol::SampleKind::F32,
                },
                codec: cursio_protocol::AudioCodec::Opus,
            };
            tracing::info!(id, device = %dev.description, "advertising microphone");
            send_control(tx, AudioMsg::StreamAdvertise(meta)).await?;
            // Open from the start all the same: some peers' playbacks open only
            // on a stream's first chunk, and a peer that never heard one could
            // never say it is listening. The first "not listening" closes it.
            self.mic_captures.insert(id, cap);
            self.mic_devices.insert(id, dev.id.clone());
        }

        // NOTE: there is deliberately NO global "everything" stream. It used to
        // capture the whole default sink and ship it to the peer, but in the
        // unified "one system" model that double-ships every app (it already goes
        // per-app) and re-exports the peer's own device mirrors back to it — the
        // duplicate-audio and stray-mixer-row bug. Only apps and devices cross the
        // boundary now. See docs/audio-design.md "Redesign: one system".
        //
        // NOTE: we do NOT blindly tap every playing application. In the unified
        // model a per-app Remote Stream is created ONLY when the app is funnelled
        // into an Intent Sink (a peer device) — see `pump_intent_sinks`. Tapping
        // every app up-front (the old `scan_apps`) both duplicated audio (the app
        // also reached the peer through the default sink's Intent Sink) and filled
        // the peer's mixer with idle rows. Membership is the sole trigger now.

        // Send an initial RoutingState (empty for now) on the next pump, so a
        // reconnecting peer starts from a known, authoritative map rather than
        // stale routes from a prior session. It fills in as apps get funnelled.
        self.routing_dirty = true;

        tracing::info!("audio up — the peer's devices appear here as 'Cursio: <device>' outputs");
        Ok(())
    }

    /// Send whatever the local captures have produced since the last call.
    ///
    /// Takes at most a **few chunks per stream**, then returns. Draining each
    /// channel dry (`while let Ok(..)`) starves the caller's `select!`: with
    /// many live streams the audio never stops arriving, so the inbound branch
    /// is never reached and everything the peer sends is silently ignored (this
    /// really happened — the peer's advertisements never got read). The caller
    /// additionally polls `biased`, inbound first, for the same reason.
    /// Busy while anything here is being listened to. Silence is dropped
    /// before it is sent, so "sent nothing" said nothing about whether a
    /// stream was live: an engine left playing silence slowed to the idle
    /// pace, the sound that followed went stale before the next pump and was
    /// dropped in turn, and it never sent again — Nocturne's apps went silent
    /// in Lightmoon's VoiceMeeter.
    fn has_pending_work(&self) -> bool {
        let wanted = |id: &u32| !self.unwanted_streams.contains(id);
        self.mic_captures.keys().any(wanted)
            || self.routed_apps.values().any(|(id, _)| wanted(id))
            || !self.relays.is_empty()
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // Runs every pump, not just when audio moves: a stall is precisely the
        // case where the send path below never executes, so checking only there
        // would stay silent exactly when a warning is wanted.
        self.log_health();

        // Take delivery of anything a worker finished building. First, so a node
        // that came up since the last pump is usable for the rest of this one.
        self.collect_opened_intent_sinks();
        self.collect_opened_playbacks();

        let generation = self.mixer.generation();
        if generation != self.mixer_seen {
            self.mixer_seen = generation;
            let ids: Vec<u32> = self.playbacks.keys().copied().collect();
            for id in ids {
                self.apply_gain(id);
            }
        }

        // Turn Intent-Sink membership into per-app Remote Streams: apps funnelled
        // into a peer device's Intent Sink get tapped and shipped, tagged to render
        // into that device; apps that left are torn down after a grace period.
        self.pump_intent_sinks(tx).await?;
        self.pump_relays(tx).await?;

        // Tell the owner which of its streams anything here is actually playing,
        // so it can stay quiet on the rest. See `AudioMsg::StreamDemand`.
        self.pump_demand(tx).await?;

        let mut budget = MAX_CHUNKS_PER_PUMP;

        // Every live stream gets an equal share of the pump.
        //
        // The budget used to be one pot each stream drained `while budget > 0`,
        // so the first stream served took everything it had queued and the rest
        // shared the remainder. In ordinary use the pot is nowhere near
        // exhausted (`over_budget` stays at zero), so this is not about
        // capacity — it is about order: the iteration comes from a `HashMap`,
        // so *which* stream is served first varies, and a burst from one stream
        // delays the others by however long its backlog takes.
        //
        // At least one each, so a stream is never handed a share of zero and
        // silenced outright by arithmetic; the whole-pump ceiling still bounds
        // how long this blocks the session loop.
        let live_streams = self.routed_apps.len() + self.mic_captures.len() + self.relays.len();
        let share = (MAX_CHUNKS_PER_PUMP / live_streams.max(1)).max(1);

        // `routed_apps` holds `(stream_id, Capture)` — the per-app Remote Streams
        // born from Intent-Sink membership. Pump them alongside our mic taps.
        // (Device Intent Sinks carry no audio; they only observe membership.)
        let mic_kind = GateKind::Mic(self.mic_gate.get());
        let routed = self
            .routed_apps
            .iter_mut()
            .map(|(_, (sid, cap))| (&*sid, cap.chunks(), GateKind::Program));
        let mics = self
            .mic_captures
            .iter_mut()
            .map(|(id, cap)| (id, cap.chunks(), mic_kind));
        let relays = self
            .relays
            .iter_mut()
            .map(|(id, (_, pcm))| (id, pcm, GateKind::Program));
        for (id, chunks, gate_kind) in routed.chain(mics).chain(relays) {
            // Stop as soon as the budget is gone, leaving the rest queued for
            // the next pump rather than discarding it. Dropping audible chunks
            // to stay under budget is itself heard as broken audio — the budget
            // exists to bound how long one pump blocks the session loop (input
            // shares this channel), not to throw sound away.
            if budget == 0 {
                break;
            }
            // This stream's slice of what is left. Capped by the pot so the sum
            // of the slices can never exceed the whole-pump ceiling.
            let allotted = share.min(budget);
            let mut budget_for_stream = allotted;
            // Nobody on the far side is playing this stream: drain what the
            // capture produced and send none of it.
            //
            // The capture keeps running (so the stream resumes instantly when a
            // consumer appears, with no renegotiation) but its output goes
            // nowhere. This is what stops a peer mirroring 13 endpoints from
            // streaming all 13 into nodes nothing has opened.
            if self.unwanted_streams.contains(id) {
                if let Some(pending) = self.pending_packets.remove(id) {
                    self.unwanted_dropped += pending.len() as u64;
                }
                let drained = drain_stale(chunks, 0);
                if drained > 0 {
                    self.unwanted_dropped += drained as u64;
                }
                continue;
            }
            // Discard a stale backlog before encoding anything.
            //
            // A capture starts producing the moment it is created and its channel
            // is unbounded, so between creation (or any pump that ran out of
            // budget) and now, chunks pile up. Sending them is worse than useless:
            // they are already too old to be wanted, yet they still cost link
            // capacity — and since input rides this same channel, that cost is
            // felt as mouse lag for as long as the backlog takes to drain. This is
            // what made the first seconds after connecting stutter.
            //
            // Same trade as the playback jitter buffer: for live audio freshness
            // beats completeness, so keep the newest and drop what is behind.
            let dropped = drain_stale(chunks, MAX_CHUNK_BACKLOG);
            if dropped > 0 {
                self.stale_dropped += dropped as u64;
            }
            while budget_for_stream > 0 {
                if let Some(packet) = self
                    .pending_packets
                    .get_mut(id)
                    .and_then(|packets| take_with_budget(packets, &mut budget_for_stream))
                {
                    let seq = packet.seq;
                    tx.send(&Message::AudioChunk(packet)).await?;
                    record_sent(&mut self.unacked, &mut self.sent_since_adapt, *id, seq);
                    self.sent += 1;
                    continue;
                }

                let Ok(chunk) = chunks.try_recv() else {
                    break;
                };
                // A virtual sink produces chunks continuously whether or not
                // anything is playing into it, so most streams are pumping pure
                // silence. Sending it wastes the link and — because input rides
                // the same channel — is felt directly as mouse lag. Nothing is
                // lost by dropping it: silence reconstructs as silence, and the
                // receiver already fills gaps with silence on underrun. What
                // counts as silence differs for a microphone; see `audio_gate`.
                if !self
                    .gates
                    .entry(*id)
                    .or_default()
                    .admit(gate_kind, &chunk.data)
                {
                    self.silent_dropped += 1;
                    continue;
                }
                // Compress before sending. Raw f32 is ~3 Mbit/s per stream,
                // which saturates the link that input also rides on; Opus
                // carries the same audio at ~100 kbit/s. One encoder per
                // stream — Opus is stateful and interleaving frames from
                // different sources would corrupt both.
                let bitrate = self
                    .stream_bitrate
                    .get(id)
                    .copied()
                    .unwrap_or(BITRATE_START);
                let encoder = match self.encoders.entry(*id) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        // Built at the stream's *current* target bitrate, so a
                        // stream that restarts resumes at the rate the link was
                        // last judged able to carry. Later changes are applied to
                        // this encoder in place (see `adapt_bitrate`).
                        match cursio_audio::codec::OpusEncoder::with_bitrate(bitrate) {
                            Ok(enc) => e.insert(enc),
                            Err(err) => {
                                tracing::warn!(error = %err, id, "no encoder for stream");
                                break;
                            }
                        }
                    }
                };
                let first_timestamp = chunk
                    .timestamp
                    .saturating_sub(encoder.buffered_frames() as u64);
                let packets = match encoder.encode(&chunk.data) {
                    Ok(p) => p,
                    Err(err) => {
                        tracing::warn!(error = %err, id, "opus encode failed");
                        continue;
                    }
                };
                // A chunk smaller than one Opus frame yields nothing yet: the
                // remainder is buffered in the encoder until it completes.
                // Larger chunks may yield several packets; enqueue every one
                // before the next loop iteration spends the remaining budget.
                let pending = self.pending_packets.entry(*id).or_default();
                pending.extend(packets.into_iter().enumerate().map(|(index, data)| {
                    cursio_protocol::AudioChunk {
                        id: *id,
                        seq: chunk.seq,
                        timestamp: first_timestamp
                            .saturating_add((index * cursio_audio::codec::FRAME_SAMPLES) as u64),
                        data,
                    }
                }));
                // (health logging happens once per pump, above — not per stream)
            }
            // Charge what this stream actually used against the whole-pump pot,
            // and hand back what it did not: a quiet stream leaves its share for
            // the others rather than reserving capacity it has no audio for.
            budget -= allotted - budget_for_stream;
            if budget_for_stream == 0
                && (self
                    .pending_packets
                    .get(id)
                    .is_some_and(|packets| !packets.is_empty())
                    || !chunks.is_empty())
            {
                self.over_budget += 1;
            }
        }

        // Feed the sender's rate control: sparse acks for what we've received,
        // and a bitrate re-evaluation for what we send.
        self.send_acks(tx).await?;
        self.adapt_bitrate();

        // Device-state replication: drain monitor changes into broadcasts/requests
        // and flush anything queued by `handle_message`.
        self.pump_device_state(tx).await?;

        // Routing replication: re-broadcast the whole RoutingState whenever it
        // changed, so a reconnecting peer gets the full map in one idempotent
        // object instead of waiting for link events to re-fire.
        if self.routing_dirty {
            self.routing_dirty = false;
            let routes: Vec<cursio_protocol::RouteEntry> =
                self.routed_app_route.values().cloned().collect();
            tracing::info!(count = routes.len(), "broadcasting routing state");
            send_control(
                tx,
                AudioMsg::RoutingState(cursio_protocol::RoutingState { routes }),
            )
            .await?;
        }

        // Route group ops: fan out changed group gain/mute to members and
        // replicate each affected RouteState.
        self.pump_routes(tx).await?;
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        // Chunks first: they are the overwhelming majority of inbound audio
        // frames, and they are the one variant that stayed first-class. Checking
        // them before attempting an envelope decode keeps the hot path cheapest.
        if let Message::AudioChunk(chunk) = msg {
            self.on_chunk(chunk);
            return true;
        }
        let control = match msg.decode_module::<AudioMsg>(ModuleId::AUDIO) {
            Ok(Some(control)) => control,
            Ok(None) => return false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable audio message from peer");
                return true;
            }
        };
        match control {
            AudioMsg::StreamAdvertise(meta) => {
                self.on_advertise(&meta);
                true
            }
            AudioMsg::Gain { id, gain } => {
                self.on_gain(id, gain);
                true
            }
            AudioMsg::StreamDemand { id, listening } => {
                if listening {
                    self.unwanted_streams.remove(&id);
                } else {
                    self.unwanted_streams.insert(id);
                }
                tracing::info!(id, listening, "peer reported demand for one of our streams");
                self.follow_mic_demand(id, listening);
                true
            }
            AudioMsg::FrameAck(ack) => {
                self.on_ack(&ack);
                true
            }
            AudioMsg::DeviceStateSet { device, field } => {
                self.on_device_state_set(&device, field);
                true
            }
            AudioMsg::DeviceState { device, state } => {
                self.on_device_state_replica(&device, state);
                true
            }
            AudioMsg::StreamRoute(entry) => {
                self.on_stream_route(&entry);
                true
            }
            AudioMsg::RoutingState(state) => {
                self.on_routing_state(&state);
                true
            }
            AudioMsg::RouteOp { target_device, op } => {
                self.on_route_op(target_device, op);
                true
            }
            AudioMsg::RouteState(state) => {
                // Receiver side: a Route's group state, for a future UI. We hold
                // no UI yet, so just log it — reflecting it never mutates routing
                // (invariant #4: only the host writes it).
                tracing::info!(
                    device = %state.target_device.1,
                    members = state.members.len(),
                    gain = state.gain,
                    mute = state.mute,
                    "peer replicated a route's group state"
                );
                true
            }
            AudioMsg::StreamClose { id, reason } => {
                // The peer tore down a stream (e.g. an app left an Intent Sink).
                // Drop our playback and any per-stream state so its mixer row and
                // decoder go away.
                tracing::info!(id, ?reason, "peer closed a stream; dropping playback");
                self.playbacks.remove(&id);
                self.pending_labels.remove(&id);
                self.mixer_names.remove(&id);
                self.inbound_app_device.remove(&id);
                self.decoders.remove(&id);
                self.failed_playbacks.remove(&id);
                self.peer_streams.remove(&id);
                self.opening_playbacks.remove(&id);
                // Receiver-side rate-control/gain state for this stream — clear it
                // so a reused id never inherits a stale gain or ack cadence.
                self.gains.remove(&id);
                self.mixer.remove(id);
                self.recv_seq.remove(&id);
                self.recv_timing.remove(&id);
                self.demand_sent.remove(&id);
                self.unwanted_streams.remove(&id);
                self.last_ack_sent.remove(&id);
                self.inbound_calls.close(id);
                if self.peer_mic == Some(id) {
                    self.peer_mic = None;
                }
                true
            }
        }
    }
}
