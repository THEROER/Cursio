//! The peer's streams arriving here: what each is called in the mixer,
//! opening its playback off the loop, and decoding its chunks into it.

use crate::audio_session::ids::sanitize_node;
use crate::audio_session::intent_sink::report_no_virtual_devices;
use crate::audio_session::intent_sink::OpenedIntentSink;
use crate::audio_session::intent_sink::VIRTUAL_DEVICES_REPORTED;
use crate::audio_session::AudioEngine;
use std::collections::HashMap;

/// The result of opening a playback on a worker thread.
pub(super) struct OpenedPlayback {
    /// Stream id the playback renders.
    id: u32,
    result: Result<cursio_audio::Playback, cursio_audio::AudioError>,
}

/// The mixer row of an application stream: a relayed call or phone sound
/// carries its phone in a marked label, which is what the row should say.
pub(super) fn mixer_name_of_app(app: &str) -> (String, crate::mixer::StreamKind) {
    use crate::mixer::StreamKind;
    if crate::audio_relay::call_of(app).is_some() {
        (app.to_string(), StreamKind::Call)
    } else if let Some(phone) = crate::audio_relay::phone_sound_of(app) {
        (format!("{phone} sound"), StreamKind::PhoneSound)
    } else {
        (app.to_string(), StreamKind::App)
    }
}

/// What a peer's microphone is called: the device it advertised, or plainly
/// "Microphone" for one without a description, as a phone's sole input may be.
pub(super) fn microphone_name(meta: &cursio_protocol::AudioStreamMeta) -> String {
    meta.device
        .as_ref()
        .map(|(_, d)| d.trim())
        .filter(|d| !d.is_empty())
        .unwrap_or("Microphone")
        .to_string()
}

/// The peer's relayed call voices playing here, withdrawn from the calls hub
/// when the stream or the whole link goes.
#[derive(Default)]
pub(super) struct InboundCalls {
    pub(super) peer_name: String,
    pub(super) streams: HashMap<u32, String>,
}

impl InboundCalls {
    fn open(&mut self, id: u32, phone: &str) {
        crate::calls_hub::hub().relayed_voice(&self.peer_name, phone, true);
        self.streams.insert(id, phone.to_string());
    }

    pub(super) fn close(&mut self, id: u32) {
        if let Some(phone) = self.streams.remove(&id) {
            crate::calls_hub::hub().relayed_voice(&self.peer_name, &phone, false);
        }
    }
}

impl Drop for InboundCalls {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            self.close(id);
        }
    }
}

impl AudioEngine {
    /// The peer offered a stream. Devices get a local mirror (so they show up
    /// in our output list) plus playback; apps get playback only — you don't
    /// route audio *into* someone else's application.
    pub(super) fn on_advertise(&mut self, meta: &cursio_protocol::AudioStreamMeta) {
        // Recorded before any branch below can return early. Every advertised
        // stream owes the owner an answer about demand, including the ones this
        // machine turns out to be unable to play — those especially, since a
        // stream that never opens is the one whose silence the owner would
        // otherwise read as consent to keep sending.
        self.peer_streams.insert(meta.id);

        let peer_name = &self.peer_name;
        if let Some(app) = &meta.app_name {
            if let Some(phone) = crate::audio_relay::call_of(app) {
                self.inbound_calls.open(meta.id, phone);
            }
            let label = format!("Cursio({peer_name}): {app}");
            // A per-app stream may carry a target device: the peer routed this app
            // into one of OUR devices (via its Intent Sink), so we must render it
            // into that exact device, not the default. The device handle is the one
            // WE advertised, i.e. a local device id/name.
            if let Some((dev, _desc)) = &meta.device {
                tracing::info!(id = meta.id, %label, device = %dev, "peer advertised an app stream routed to our device");
                self.inbound_app_device.insert(meta.id, dev.clone());
            } else {
                tracing::info!(id = meta.id, %label, "peer advertised an app stream");
            }
            // Open the playback NOW, on the advertisement, rather than waiting
            // for the stream's first chunk.
            //
            // Waiting made a mixer row appear only once an application actually
            // made a sound, which is too late to be useful: the row is where the
            // user sets that application's volume, and an application they have
            // not started playing yet is exactly when they want to set it. It
            // also made the set of rows look arbitrary — three applications
            // running, two rows, because the third had been quiet since the
            // session began. Measured here: a Java application's row appeared 90
            // seconds after it was advertised, when it finally played something.
            //
            // The cost is a node per advertised stream whether or not it ever
            // carries audio, which is what this used to avoid. That trade is now
            // the right way round: a silent row is a row the user can still
            // reach, while a missing row is a control they cannot find. No
            // filtering by name — a stream that looks like a service today
            // ("speech-dispatcher") is one that chimes tomorrow, and guessing
            // wrong hides a real application.
            self.pending_labels.insert(meta.id, label.clone());
            self.mixer_names.insert(meta.id, mixer_name_of_app(app));
            let target = self
                .inbound_app_device
                .get(&meta.id)
                .cloned()
                .or_else(|| self.render_device.clone());
            let id = meta.id;
            self.open_playback_off_loop(id, move || {
                cursio_audio::Playback::open_on(id, label, target)
            });
            return;
        }

        // A microphone the peer is offering. This is the mirror image of a
        // device: instead of a virtual speaker we publish a virtual *source*, so
        // local apps can select the peer's mic as a recording input. Opened
        // eagerly because it must appear in the input list before anything can
        // choose to record from it.
        if meta.role == cursio_protocol::StreamRole::Capture {
            let desc = microphone_name(meta);
            let label = format!("Cursio({peer_name}): {desc}");
            let id = meta.id;
            tracing::info!(id, %label, "peer advertised a microphone");
            self.mixer_names
                .insert(id, (desc, crate::mixer::StreamKind::Microphone));
            if self.peer_mic.is_none() {
                self.peer_mic = Some(id);
            }
            // Published on a worker for the same reason as an Intent Sink: this
            // waits for a PipeWire node, and a peer offering several microphones
            // would otherwise stall the loop once per microphone.
            self.open_playback_off_loop(meta.id, move || {
                cursio_audio::Playback::virtual_source(id, label)
            });
            return;
        }

        // A device the peer offered. We create an INTENT SINK for it: a real local
        // output the user can route into, but which carries no audio itself. When
        // an app is funnelled into it we open a per-app Remote Stream tagged to
        // render into this peer device (see `pump_intent_sinks`). The old code made
        // a capturing `virtual_sink` here and shipped its whole mix — the source of
        // duplicate audio and lost per-app separation.
        let Some((dev_id, desc)) = &meta.device else {
            // No device handle ⇒ nothing to represent. In the unified model every
            // crossable device carries one; a deviceless playback advert is legacy
            // (the removed global stream) and is simply ignored.
            tracing::debug!(
                id = meta.id,
                "peer advertised a deviceless playback stream; ignoring"
            );
            return;
        };
        let node = format!("cursio_mirror_{}", sanitize_node(dev_id));
        let label = format!("Cursio({peer_name}): {desc}");
        tracing::info!(id = meta.id, %label, "peer advertised a device; creating Intent Sink");

        // Already have it, or already making it: a device is advertised again on
        // every reconnect, and creating a second node for it would leave the
        // user with duplicate outputs for one peer device.
        if self.intent_sinks.contains_key(&meta.id) || !self.opening_intent_sinks.insert(meta.id) {
            return;
        }

        // Create on a worker: this waits for a PipeWire node to come up (~430 ms
        // here), and the session loop cannot sit through that — see
        // [`AudioEngine::opening_intent_sinks`]. The finished sink is collected
        // by a later pump.
        let ready = self.intent_sink_ready_tx.clone();
        let id = meta.id;
        let device = (dev_id.clone(), desc.clone());
        let node_arg = node.clone();
        let label_arg = label.clone();
        let opener = self.intent_sink_opener;
        if let Err(e) = std::thread::Builder::new()
            .name(format!("cursio-pw-sink-open-{id}"))
            .spawn(move || {
                let result = opener(id, node_arg.clone(), label_arg);
                // The receiver only closes when the session is ending, in which
                // case the sink is moot.
                let _ = ready.send(OpenedIntentSink {
                    id,
                    node: node_arg,
                    device,
                    result,
                });
            })
        {
            self.opening_intent_sinks.remove(&id);
            tracing::error!(error = %e, %label, "cannot spawn the Intent Sink opener");
        }
    }

    /// Open a playback on a worker thread, to be collected by a later pump.
    ///
    /// Every kind of playback goes through here — a peer microphone published
    /// as a virtual source, an application stream opened on its advertisement,
    /// a device stream opened on its first chunk — because the cost being
    /// avoided is the same in all of them: bringing a PipeWire node up takes
    /// hundreds of milliseconds, and the session loop is single-threaded, so
    /// waiting inline stops input, audio and file traffic alike.
    pub(super) fn open_playback_off_loop(
        &mut self,
        id: u32,
        open: impl FnOnce() -> Result<cursio_audio::Playback, cursio_audio::AudioError> + Send + 'static,
    ) {
        // One playback per stream. Chunks keep arriving at ~200/s while the
        // node comes up, and a stream is advertised again on every reconnect;
        // either would otherwise start another node for a stream that has one,
        // and the newcomer would replace — and so tear down — the row the user
        // had already set a volume on.
        if self.playbacks.contains_key(&id) || !self.opening_playbacks.insert(id) {
            return;
        }
        let ready = self.playback_ready_tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("cursio-pw-play-open-{id}"))
            .spawn(move || {
                let result = open();
                // The receiver only closes when the session is ending, in which
                // case the playback is moot.
                let _ = ready.send(OpenedPlayback { id, result });
            })
        {
            self.opening_playbacks.remove(&id);
            tracing::error!(error = %e, id, "cannot spawn the playback opener");
        }
    }

    /// What a stream's mixer row says. A stream the peer never described still
    /// says whose sound it is, rather than its id.
    pub(super) fn mixer_name(&self, id: u32) -> (String, crate::mixer::StreamKind) {
        self.mixer_names.get(&id).cloned().unwrap_or_else(|| {
            (
                format!("Sound from {}", self.peer_name),
                crate::mixer::StreamKind::App,
            )
        })
    }

    /// Take delivery of playbacks whose worker has finished.
    pub(super) fn collect_opened_playbacks(&mut self) {
        while let Ok(opened) = self.playback_ready_rx.try_recv() {
            // No longer wanted: the peer closed the stream while its node was
            // coming up. Keeping it would leave a mixer row for a stream that
            // will never be closed again, since its close has already come.
            if !self.opening_playbacks.remove(&opened.id) {
                continue;
            }
            match opened.result {
                Ok(p) => {
                    let (name, kind) = self.mixer_name(opened.id);
                    self.mixer.add(opened.id, &self.peer_name, &name, kind);
                    self.playbacks.insert(opened.id, p);
                    // Any gain that arrived while the node was coming up, so a
                    // knob turned during the open is not lost.
                    self.apply_gain(opened.id);
                }
                Err(e) => {
                    // Complain ONCE per stream: chunks arrive at ~200/s, and a
                    // stream that cannot open would otherwise log at that rate.
                    if !self.failed_playbacks.insert(opened.id) {
                        continue;
                    }
                    if matches!(e, cursio_audio::AudioError::Unsupported) {
                        let what = format!("stream {}", opened.id);
                        report_no_virtual_devices(&VIRTUAL_DEVICES_REPORTED, &what);
                    } else {
                        tracing::warn!(error = %e, id = opened.id,
                            "cannot open playback; ignoring this stream");
                    }
                }
            }
        }
    }

    /// Audio for one of our streams: render it into the matching real device.
    /// Decompress one chunk of a stream. One decoder per stream, mirroring the
    /// encoders — Opus carries state between frames.
    pub(super) fn decode(&mut self, id: u32, data: &[u8]) -> Option<Vec<u8>> {
        let decoder = match self.decoders.entry(id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                match cursio_audio::codec::OpusDecoder::new() {
                    Ok(d) => e.insert(d),
                    Err(err) => {
                        tracing::warn!(error = %err, id, "no decoder for stream");
                        return None;
                    }
                }
            }
        };
        let started = std::time::Instant::now();
        let result = decoder.decode(data);
        self.max_decode_time = self.max_decode_time.max(started.elapsed());
        match result {
            Ok(p) => Some(p),
            Err(err) => {
                tracing::warn!(error = %err, id, "opus decode failed");
                None
            }
        }
    }

    /// Hand the peer's microphone to every relay listening to it.
    pub(super) fn tee_mic(&self, chunk: &cursio_protocol::AudioChunk, pcm: &[u8]) {
        for (relay, _) in self.relays.values().filter(|(r, _)| r.wants_mic) {
            let _ = relay.mic.send(cursio_protocol::AudioChunk {
                id: chunk.id,
                seq: chunk.seq,
                timestamp: chunk.timestamp,
                data: pcm.to_vec(),
            });
        }
    }

    pub(super) fn on_chunk(&mut self, chunk: &cursio_protocol::AudioChunk) {
        let id = chunk.id;

        // Audio is flowing again: note the arrival, and if we had warned about a
        // stall, say that it recovered — a warning with no matching "resumed" is
        // impossible to interpret later.
        let now = std::time::Instant::now();
        if self.stall_reported {
            let gap = self.last_chunk_at.map(|t| now.duration_since(t));
            tracing::info!(
                stalled_for_ms = gap.map(|g| g.as_millis() as u64),
                "audio from the peer resumed"
            );
            self.stall_reported = false;
        }
        self.last_chunk_at = Some(now);

        if let Some((previous, timestamp)) = self.recv_timing.insert(id, (now, chunk.timestamp)) {
            let gap = now.saturating_duration_since(previous);
            let media_gap = std::time::Duration::from_secs_f64(
                chunk.timestamp.saturating_sub(timestamp) as f64 / 48000.0,
            );
            self.max_arrival_gap = self.max_arrival_gap.max(gap);
            self.max_arrival_lateness =
                self.max_arrival_lateness.max(gap.saturating_sub(media_gap));
        }
        // Capture sequence gaps also include silence gating and encoder framing.
        // They are diagnostic discontinuities, not a count of lost network packets.
        if let Some(&previous) = self.recv_seq.get(&id) {
            self.capture_seq_skipped = self
                .capture_seq_skipped
                .saturating_add(chunk.seq.saturating_sub(previous).saturating_sub(1) as u64);
        }
        self.recv_seq
            .entry(id)
            .and_modify(|s| *s = (*s).max(chunk.seq))
            .or_insert(chunk.seq);

        // Already coming up on a worker: drop this chunk and wait.
        //
        // Checked BEFORE the label is taken below, because taking it is
        // destructive. Bringing a node up takes hundreds of milliseconds and
        // chunks arrive at ~200/s, so without this the second chunk of a stream
        // finds `pending_labels` already emptied by the first, falls through to
        // the "never advertised" arm, and the stream loses the name it was
        // advertised under. Every app then rendered under whichever name did
        // survive — one mixer row on the peer, carrying every application's
        // audio at once.
        // The microphone a relay hears reaches it whether or not its mirror
        // here is up: nothing local may be recording from it at all.
        let tee = self.peer_mic == Some(id) && self.relay_wants_mic();
        if tee && !self.playbacks.contains_key(&id) {
            if let Some(pcm) = self.decode(id, &chunk.data) {
                self.tee_mic(chunk, &pcm);
            }
            return;
        }

        if self.opening_playbacks.contains(&id) {
            return;
        }

        if !self.playbacks.contains_key(&id) {
            // No playback for this stream yet. A stream rendering into one of
            // OUR devices opens here, on its first chunk; an application stream
            // was already opened on its advertisement and arrives here only if
            // that open failed, which makes this its retry.
            let peer_name = &self.peer_name;
            if let Some(dev) = self.inbound_targets.get(&id).cloned() {
                // A device WE advertised: render into that exact local device.
                let label = format!("Cursio({peer_name}): {dev}");
                self.open_playback_off_loop(id, move || {
                    cursio_audio::Playback::open_on(id, label, Some(dev))
                });
            } else if let Some(label) = self.pending_labels.get(&id).cloned() {
                // A stream the PEER advertised (its device or app). If it was
                // tagged with one of OUR devices (the peer routed this app into our
                // device via its Intent Sink), render into that device; otherwise
                // into our default output.
                //
                // Read, not taken: the label identifies the stream for as long as
                // the peer offers it, and a failed open must be able to try again
                // under the right name. It is dropped with the rest of the
                // stream's state on `StreamClose`.
                let target = self
                    .inbound_app_device
                    .get(&id)
                    .cloned()
                    .or_else(|| self.render_device.clone());
                tracing::info!(id, %label, ?target, "first audio; opening playback");
                self.open_playback_off_loop(id, move || {
                    cursio_audio::Playback::open_on(id, label, target)
                });
            } else if self.failed_playbacks.insert(id) {
                tracing::warn!(id, "audio for a stream that was never advertised; ignoring");
            }
            // The node is not up yet, so this chunk has nowhere to go. Dropping
            // it is the right answer and not a loss: playback opens on the first
            // chunk of a stream that has only just started, the receiver fills a
            // gap with silence anyway, and the alternative — holding chunks
            // until the node arrives — is the buffering that made audio latency
            // grow without bound.
            return;
        }
        let Some(pcm) = self.decode(id, &chunk.data) else {
            return;
        };
        if tee {
            self.tee_mic(chunk, &pcm);
        }
        let chunk = &cursio_protocol::AudioChunk {
            id,
            seq: chunk.seq,
            timestamp: chunk.timestamp,
            data: pcm,
        };

        if let Some(p) = self.playbacks.get(&id) {
            if let Err(e) = p.play(chunk) {
                tracing::warn!(error = %e, "playback failed");
            } else {
                self.played += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_session::test_support::capture_meta;
    use crate::audio_session::test_support::engine;
    use crate::audio_session::test_support::RecordingTx;
    use crate::module::SessionModule;
    use cursio_protocol::{AudioMsg, Message, ModuleId};

    #[test]
    fn a_call_voice_relayed_here_is_shown_on_its_phone_until_it_closes() {
        let mut e = engine();
        e.on_advertise(&cursio_protocol::AudioStreamMeta {
            id: 9001,
            role: cursio_protocol::StreamRole::Playback,
            app_name: Some(crate::audio_relay::call_label("Relay Test Phone")),
            device: None,
            format: cursio_protocol::AudioFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_kind: cursio_protocol::SampleKind::F32,
            },
            codec: cursio_protocol::AudioCodec::Opus,
        });
        assert_eq!(
            e.inbound_calls.streams.get(&9001).map(String::as_str),
            Some("Relay Test Phone")
        );
        e.inbound_calls.close(9001);
        assert!(e.inbound_calls.streams.is_empty());
    }

    #[test]
    fn a_peers_microphone_is_named_by_the_device_it_advertised() {
        assert_eq!(
            microphone_name(&capture_meta(Some((
                "alsa_input.usb",
                "Blue Yeti Analog Stereo"
            )))),
            "Blue Yeti Analog Stereo"
        );
        assert_eq!(
            microphone_name(&capture_meta(Some(("mic", "  ")))),
            "Microphone"
        );
        assert_eq!(microphone_name(&capture_meta(None)), "Microphone");
    }

    #[test]
    fn a_relayed_call_and_phone_sound_are_told_from_an_application() {
        use crate::mixer::StreamKind;
        assert_eq!(
            mixer_name_of_app(&crate::audio_relay::call_label("POCO X7 Pro")),
            ("Call · POCO X7 Pro".to_string(), StreamKind::Call)
        );
        assert_eq!(
            mixer_name_of_app(&crate::audio_relay::phone_sound_label("POCO X7 Pro")),
            ("POCO X7 Pro sound".to_string(), StreamKind::PhoneSound)
        );
        assert_eq!(
            mixer_name_of_app("Firefox"),
            ("Firefox".to_string(), StreamKind::App)
        );
    }

    #[test]
    fn a_stream_never_described_is_named_after_its_peer_not_its_id() {
        let e = engine();
        let (name, kind) = e.mixer_name(424242);
        assert!(!name.contains("424242"), "got {name}");
        assert!(name.starts_with("Sound from "), "got {name}");
        assert_eq!(kind, crate::mixer::StreamKind::App);
    }

    #[test]
    fn a_closed_stream_forgets_its_mixer_name() {
        let mut e = engine();
        e.mixer_names.insert(
            901800,
            ("Blue Yeti".into(), crate::mixer::StreamKind::Microphone),
        );
        let close = Message::module(
            ModuleId::AUDIO,
            AudioMsg::PRIORITY,
            &AudioMsg::StreamClose {
                id: 901800,
                reason: cursio_protocol::AudioStreamCloseReason::SourceGone,
            },
        )
        .unwrap();
        assert!(e.handle_message(&close));
        assert!(!e.mixer_names.contains_key(&901800));
    }

    /// A stream keeps the name it was advertised under while its node comes up.
    ///
    /// The label used to be *taken* out of `pending_labels` by the first chunk,
    /// which was safe only while opening blocked the loop: no second chunk
    /// could exist yet. Once the open moved to a worker, chunks kept arriving
    /// at ~200/s during the hundreds of milliseconds it takes, each finding the
    /// label gone and treating a properly advertised stream as one that was
    /// never announced. On the peer every application collapsed into a single
    /// mixer row bearing whichever name survived.
    #[test]
    fn an_advertised_label_survives_the_wait_for_a_node() {
        let mut e = engine();

        e.pending_labels
            .insert(85505, "Cursio(peer): spotify".into());

        // The open is in flight on a worker; chunks keep arriving meanwhile.
        e.opening_playbacks.insert(85505);
        for seq in 0..3 {
            e.on_chunk(&cursio_protocol::AudioChunk {
                id: 85505,
                seq,
                timestamp: 0,
                data: vec![0; 16],
            });
        }

        assert_eq!(
            e.pending_labels.get(&85505).map(String::as_str),
            Some("Cursio(peer): spotify"),
            "the label must outlive the open, so a retry uses the right name"
        );
    }

    /// A stream that has not opened *yet* is not refused.
    ///
    /// A stream into one of our devices opens its playback on its first chunk,
    /// so refusing one that has no playback yet stops the owner from sending,
    /// which stops the first chunk from arriving, which stops the playback from
    /// ever opening: the refusal makes itself true and the audio never comes
    /// back.
    #[tokio::test]
    async fn a_stream_whose_playback_has_not_opened_yet_is_left_alone() {
        let mut e = engine();

        // Advertised, never yet carried a chunk: no playback, and no failure.
        e.peer_streams.insert(476500);

        let mut tx = RecordingTx::default();
        e.pump_demand(&mut tx)
            .await
            .expect("reporting demand must not fail");

        assert!(
            tx.demands().is_empty(),
            "a stream still waiting for its first chunk must not be refused"
        );
    }

    /// Opening a playback never happens twice for one stream.
    ///
    /// The open is handed to a worker and the node takes hundreds of
    /// milliseconds to appear, during which chunks keep arriving at ~200/s. If
    /// each of those started another opener, one stream would spawn a node per
    /// chunk — the session-loop stall this moved off the loop, turned into a
    /// flood of PipeWire nodes instead.
    #[test]
    fn a_stream_opens_only_one_playback_while_its_node_comes_up() {
        let mut e = engine();

        for _ in 0..5 {
            e.open_playback_off_loop(7, || {
                Err(cursio_audio::AudioError::StreamGone("test".into()))
            });
        }

        assert_eq!(
            e.opening_playbacks.len(),
            1,
            "one stream must have exactly one open in flight"
        );
    }
}
