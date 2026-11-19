//! The peer's devices made real here: an intent sink per device it
//! advertises, and the taps that carry what is played into one back.

use crate::audio_session::module::send_control;
use crate::audio_session::relay::is_startup_leftover;
use crate::audio_session::relay::ROUTED_APP_GRACE;
use crate::audio_session::AudioEngine;
use cursio_protocol::AudioMsg;
use cursio_transport::{TransportError, TransportTx};

/// A tap whose open is in flight, remembered so the node's departure can be
/// handled even while its tap is still being built.
pub(super) struct PendingTap {
    stream_id: u32,
    app_name: String,
    device: (String, String),
    /// Whether this reuses a tunnel the peer already holds. A resumed stream
    /// must not re-advertise: the peer would tear down and rebuild its playback
    /// node, which is the churn this whole path exists to avoid.
    resumed: bool,
    /// Set when the node leaves before its tap is ready, so the finished tap is
    /// dropped on arrival instead of being wired to a node that is gone.
    abandoned: bool,
}

/// The result of opening a tap on a worker thread.
pub(super) struct OpenedTap {
    app_node_id: u32,
    result: Result<cursio_audio::Capture, String>,
}

/// The result of creating an Intent Sink on a worker thread.
///
/// Carries the identity the sink was created for, because the advertisement it
/// answers is long gone by the time it lands.
pub(super) struct OpenedIntentSink {
    /// The peer's device stream id this sink represents.
    pub(super) id: u32,
    /// PipeWire node name, and the peer device `(handle, description)` behind it.
    pub(super) node: String,
    pub(super) device: (String, String),
    pub(super) result: Result<cursio_audio::IntentSink, cursio_audio::AudioError>,
}

/// Builds an Intent Sink from its stream id, node name and description.
pub(super) type IntentSinkOpener =
    fn(u32, String, String) -> Result<cursio_audio::IntentSink, cursio_audio::AudioError>;

/// Whether this process has said it cannot publish a peer's devices.
pub(super) static VIRTUAL_DEVICES_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// What to install so a peer's outputs and microphones appear here.
#[cfg(target_os = "windows")]
pub(super) const VIRTUAL_DEVICES_HINT: &str =
    "install the Cursio virtual audio driver (native/cursio-vad) to get them";
#[cfg(not(target_os = "windows"))]
pub(super) const VIRTUAL_DEVICES_HINT: &str = "this platform has no backend for them yet";

/// Say once per process that a peer's devices cannot be published here.
///
/// A peer's outputs (as Intent Sinks) and its microphone (as a recording
/// device) need a virtual audio endpoint, which Windows cannot create from
/// user mode. That is a capability this machine lacks, not a failure: every
/// device the peer advertises, on every reconnect, hits it again, and logging
/// each as an error buried the one line that says what to install. The
/// streams themselves are refused through demand either way, so nothing is
/// sent for them. Returns whether this call was the one that reported.
pub(super) fn report_no_virtual_devices(
    reported: &std::sync::atomic::AtomicBool,
    what: &str,
) -> bool {
    if reported.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::debug!(%what, "peer audio device not published here (no virtual devices)");
        return false;
    }
    // A phone never publishes a peer's devices: as far as an app can address
    // them it has one output and one input, and that is the design, not a gap.
    // Raised there it was a warning nothing could clear, on every phone.
    if cfg!(target_os = "android") {
        tracing::debug!(%what, "peer audio devices are not published on a phone");
        return true;
    }
    tracing::warn!(
        %what,
        "this machine cannot publish a peer's audio outputs or microphones; {VIRTUAL_DEVICES_HINT} \
         (application audio from the peer still plays)"
    );
    crate::health::audio_devices_unavailable(VIRTUAL_DEVICES_HINT, cfg!(target_os = "windows"));
    true
}

impl AudioEngine {
    /// Turn Intent-Sink membership into per-app Remote Streams.
    ///
    /// For each device Intent Sink, drain its Joined/Left events:
    /// - **Joined**: an app was funnelled into the sink → open a per-app tap and
    ///   advertise it tagged with the sink's peer device, so the peer renders it
    ///   into that exact device (not its default). This is the audio path; the
    ///   Intent Sink itself carries nothing.
    /// - **Left**: schedule teardown after a short grace, so a transient re-route
    ///   (unlink+relink) doesn't kill and rebuild the stream.
    ///
    /// Then reap any taps whose grace has expired.
    pub(super) async fn pump_intent_sinks(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        // Collect events first to avoid holding a borrow on `intent_sinks` while we
        // mutate `routed_apps`/allocate ids below.
        let mut joined: Vec<(u32, String, (String, String))> = Vec::new();
        let mut left: Vec<u32> = Vec::new();
        for (sink_id, sink) in self.intent_sinks.iter_mut() {
            let Some(device) = self.intent_sink_device.get(sink_id).cloned() else {
                continue;
            };
            while let Ok(ev) = sink.events().try_recv() {
                match ev {
                    cursio_audio::IntentSinkEvent::Joined {
                        app_node_id,
                        app_name,
                    } => joined.push((app_node_id, app_name, device.clone())),
                    cursio_audio::IntentSinkEvent::Left { app_node_id } => left.push(app_node_id),
                }
            }
        }

        for (app_node_id, app_name, device) in joined {
            // Cancel any pending teardown — the app is (still) routed here.
            self.routed_app_expiry.remove(&app_node_id);
            if self.routed_apps.contains_key(&app_node_id) {
                continue; // already streaming this app
            }
            if let Some(pending) = self.opening_taps.get_mut(&app_node_id) {
                // Its tap is still being built on a worker. The node coming back
                // before it lands just cancels the abandonment.
                pending.abandoned = false;
                continue;
            }
            // Reuse the stream this app already owns, if it has one. The tap is
            // bound to a node id so it must be rebuilt, but the tunnel it feeds —
            // stream id, encoder, the peer's advertisement — is keyed to the app
            // and stays up. That is what makes a pause or a seek inaudible
            // instead of a teardown.
            //
            // Only when the app has NO live tap, though. One application can hold
            // several nodes at once (Firefox opens one per playing tab), and they
            // arrive as separate Joined events. Reusing the id for a second live
            // node pointed the tunnel at the newcomer and orphaned the tap that
            // was actually producing sound — heard as the browser going silent
            // until it happened to recreate its nodes in a luckier order.
            // A tap still opening counts as live too: two nodes of one app can
            // arrive in the same pump, and without this the second would reuse
            // the id the first is already claiming.
            let already_live = self
                .routed_app_names
                .iter()
                .any(|(node, name)| *name == app_name && *node != app_node_id)
                || self
                    .opening_taps
                    .iter()
                    .any(|(node, p)| p.app_name == app_name && *node != app_node_id);
            let resumed = if already_live {
                None
            } else {
                self.app_routes.get(&app_name).map(|(id, _)| *id)
            };
            let id = match resumed {
                Some(id) => id,
                None => {
                    let id = self.id_base + 500 + self.next_app_slot;
                    self.next_app_slot += 1;
                    id
                }
            };
            // Open the tap on a worker. This connects to the PipeWire daemon and
            // waits for the stream to come up, which the session loop cannot
            // afford to sit through — see [`AudioEngine::opening_taps`].
            let ready = self.tap_ready_tx.clone();
            let node_arg = app_node_id.to_string();
            let app_arg = app_name.clone();
            std::thread::Builder::new()
                .name(format!("cursio-tap-open-{id}"))
                .spawn(move || {
                    let result = cursio_audio::Capture::app_tap(id, node_arg, app_arg)
                        .map_err(|e| e.to_string());
                    // The receiver only closes when the session is ending, in
                    // which case the tap is moot.
                    let _ = ready.send(OpenedTap {
                        app_node_id,
                        result,
                    });
                })
                .map_err(|e| {
                    TransportError::Io(std::io::Error::other(format!("spawn tap opener: {e}")))
                })?;
            self.opening_taps.insert(
                app_node_id,
                PendingTap {
                    stream_id: id,
                    app_name: app_name.clone(),
                    device: device.clone(),
                    resumed: resumed.is_some(),
                    abandoned: false,
                },
            );
            // Claim the app's state now rather than when the tap lands. The node
            // is routed the moment it joins; leaving these until the open
            // finished would let the same node be picked up again by the next
            // pump, and a `Left` arriving meanwhile would find nothing to cancel.
            self.routed_app_names.insert(app_node_id, app_name.clone());
            self.routed_app_since
                .insert(app_node_id, std::time::Instant::now());
            // The name -> stream mapping is what a returning node looks itself up
            // by, so it must keep pointing at the app's FIRST stream. A second
            // concurrent node gets its own id and must not overwrite that, or the
            // original tunnel becomes unreachable and its audio stops being sent.
            self.app_routes
                .entry(app_name.clone())
                .or_insert_with(|| (id, device.clone()));
            // Back in the graph: it is not an idle tunnel any more, so the reaper
            // must not collect it.
            self.tunnel_idle_since.remove(&app_name);
            // Record the route for the replicated RoutingState snapshot, and mark
            // it dirty so `pump` re-broadcasts the whole map. This is what lets
            // routing survive a reconnect without waiting on PipeWire to re-emit
            // link events. Keyed by stream id, which outlives the node the app
            // happens to be using.
            self.routed_app_route.insert(
                id,
                cursio_protocol::RouteEntry {
                    id,
                    app_name: Some(app_name.clone()),
                    device: Some(device.clone()),
                },
            );
            self.routing_dirty = true;
            // If this device's Route already has a non-default group gain/mute,
            // mark it dirty so the new member inherits it (the fan-out in
            // `pump_routes` re-applies to all members).
            if self.route_groups.contains_key(&device) {
                self.routes_dirty.insert(device.clone());
            }
        }

        self.collect_opened_taps(tx).await?;

        // Schedule teardown for apps that left (grace period below).
        for app_node_id in left {
            if self.routed_apps.contains_key(&app_node_id) {
                self.routed_app_expiry
                    .insert(app_node_id, std::time::Instant::now() + ROUTED_APP_GRACE);
            }
        }

        // Release the taps of nodes that went away, but KEEP their tunnels.
        //
        // The node is gone, so its tap can only produce silence and is dropped.
        // The Remote Stream is not: the app is merely paused, mid-seek, or
        // reopening its audio client, and the route it belongs to is still what
        // the user asked for. Holding the stream id, the encoder and the peer's
        // playback end open means the audio simply resumes when the app comes
        // back — no close, no re-advertise, no rebuilt playback node.
        //
        // The tunnel is torn down later, by `reap_idle_tunnels`, once the app has
        // stayed away long enough to count as closed rather than paused.
        let now = std::time::Instant::now();
        let expired: Vec<u32> = self
            .routed_app_expiry
            .iter()
            .filter(|(_, &t)| now >= t)
            .map(|(&n, _)| n)
            .collect();
        // Tunnels of nodes an app dropped while keeping others: closed below,
        // once the borrow of `routed_app_names` above is done.
        let mut orphaned: Vec<(u32, Option<String>)> = Vec::new();
        for app_node_id in expired {
            self.routed_app_expiry.remove(&app_node_id);
            // A node can leave while its tap is still being opened. The worker
            // cannot be called back, so mark it: `collect_opened_taps` drops the
            // tap when it lands. The tunnel is left alone — the app may well be
            // pausing, exactly as below.
            if let Some(pending) = self.opening_taps.get_mut(&app_node_id) {
                pending.abandoned = true;
            }
            if let Some((stream_id, cap)) = self.routed_apps.remove(&app_node_id) {
                cap.stop(); // stops the capture thread; the tunnel stays up
                let app = self.routed_app_names.remove(&app_node_id);
                let age = self
                    .routed_app_since
                    .remove(&app_node_id)
                    .map(|since| now.duration_since(since))
                    .unwrap_or_default();
                let still_live = app
                    .as_ref()
                    .is_some_and(|app| self.routed_app_names.values().any(|n| n == app));
                let leftover = is_startup_leftover(still_live, age);

                if leftover {
                    orphaned.push((stream_id, app.clone()));
                } else if let Some(app) = &app {
                    // The tunnel outlives this node. Start the clock that tells
                    // a pause from a quit — but only while the app has no other
                    // node, or a tab still playing would have the reaper
                    // counting down against a tunnel that is in active use.
                    if !still_live {
                        self.tunnel_idle_since
                            .insert(app.clone(), std::time::Instant::now());
                    }
                }

                tracing::info!(
                    id = stream_id,
                    app_node_id,
                    app = app.as_deref().unwrap_or("?"),
                    age_ms = age.as_millis() as u64,
                    orphaned = leftover,
                    "app's node went away; tap released"
                );
            }
        }

        for (stream_id, app) in orphaned {
            // `app_routes` maps a name to ONE stream; only drop the entry if it
            // is this one, or the surviving node loses its route.
            if let Some(app) = &app {
                if self
                    .app_routes
                    .get(app)
                    .is_some_and(|(id, _)| *id == stream_id)
                {
                    self.app_routes.remove(app);
                }
            }
            self.close_app_stream(tx, stream_id, "a node this app dropped")
                .await?;
        }

        self.reap_idle_tunnels(tx).await?;
        Ok(())
    }

    /// Take delivery of taps the workers finished opening.
    ///
    /// The advertisement is sent from here rather than at request time because
    /// only a tap that actually came up should make the peer build a playback
    /// node for it.
    pub(super) async fn collect_opened_taps(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        while let Ok(opened) = self.tap_ready_rx.try_recv() {
            let Some(pending) = self.opening_taps.remove(&opened.app_node_id) else {
                continue;
            };
            let mut cap = match opened.result {
                Ok(cap) => cap,
                Err(e) => {
                    tracing::warn!(
                        app = %pending.app_name,
                        error = %e,
                        "cannot tap app routed into Intent Sink"
                    );
                    // Undo the state claimed when the open was requested, or the
                    // node would look routed with nothing behind it.
                    self.routed_app_names.remove(&opened.app_node_id);
                    self.routed_app_since.remove(&opened.app_node_id);
                    continue;
                }
            };
            if pending.abandoned {
                tracing::debug!(
                    id = pending.stream_id,
                    app = %pending.app_name,
                    "app's node left before its tap was ready; dropping it"
                );
                continue;
            }
            // Advertise it as a per-app stream, but tag the peer DEVICE so the
            // peer renders it into that exact device rather than its default
            // output. Only for a NEW stream: re-advertising one the peer already
            // holds would make it tear down and rebuild the playback end, which
            // is the very churn this avoids.
            if pending.resumed {
                // Drain the advertisement the tap emits on open; the peer already
                // has this stream and needs no second copy.
                let _ = cap.advertisements().try_recv();
                tracing::info!(
                    id = pending.stream_id,
                    app = %pending.app_name,
                    device = %pending.device.1,
                    "app returned; re-pointed its tap, tunnel kept open"
                );
            } else if let Ok(mut meta) = cap.advertisements().try_recv() {
                meta.device = Some(pending.device.clone());
                tracing::info!(
                    id = pending.stream_id,
                    app = %pending.app_name,
                    device = %pending.device.1,
                    "app routed into Intent Sink; opening Remote Stream"
                );
                send_control(tx, AudioMsg::StreamAdvertise(meta)).await?;
            }
            self.routed_apps
                .insert(opened.app_node_id, (pending.stream_id, cap));
        }
        Ok(())
    }

    /// Take delivery of Intent Sinks whose worker has finished.
    ///
    /// The bookkeeping lands here rather than at request time because a sink
    /// that failed to come up must leave no trace: a node name mapped to a sink
    /// that does not exist would silently swallow every device-state replica
    /// aimed at that peer device.
    pub(super) fn collect_opened_intent_sinks(&mut self) {
        while let Ok(opened) = self.intent_sink_ready_rx.try_recv() {
            self.opening_intent_sinks.remove(&opened.id);
            let (dev_id, desc) = opened.device;
            match opened.result {
                Ok(sink) => {
                    self.intent_sinks.insert(opened.id, sink);
                    self.intent_sink_device
                        .insert(opened.id, (dev_id.clone(), desc));
                    // Remember the mapping both ways, so an inbound DeviceState replica
                    // reaches this Intent Sink and a local turn of the sink is
                    // attributed to the peer device it mirrors.
                    self.sink_node_for_peer_device
                        .insert(dev_id.clone(), opened.node.clone());
                    self.peer_device_for_sink_node.insert(opened.node, dev_id);
                }
                Err(cursio_audio::AudioError::Unsupported) => {
                    report_no_virtual_devices(&VIRTUAL_DEVICES_REPORTED, &desc);
                }
                Err(e) => {
                    tracing::error!(error = %e, device = %desc, "failed to create Intent Sink")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_session::test_support::engine;

    /// A machine that cannot publish virtual devices says so once, however
    /// many devices the peer advertises and however often it reconnects.
    #[test]
    fn a_missing_virtual_device_backend_is_reported_once() {
        let reported = std::sync::atomic::AtomicBool::new(false);
        assert!(report_no_virtual_devices(&reported, "Speakers"));
        assert!(!report_no_virtual_devices(&reported, "Virtual"));
        assert!(!report_no_virtual_devices(&reported, "stream 85800"));
    }

    /// A device this machine cannot publish leaves nothing behind that would
    /// swallow the peer's device-state replicas.
    #[test]
    fn an_unpublishable_device_leaves_no_intent_sink() {
        let mut e = engine();
        e.intent_sink_opener = |_, _, _| Err(cursio_audio::AudioError::Unsupported);
        e.on_advertise(&cursio_protocol::AudioStreamMeta {
            id: 43,
            app_name: None,
            device: Some(("dev-handle".into(), "Speakers".into())),
            role: cursio_protocol::StreamRole::Playback,
            format: cursio_protocol::AudioFormat {
                sample_rate: 48000,
                channels: 2,
                sample_kind: cursio_protocol::SampleKind::F32,
            },
            codec: cursio_protocol::AudioCodec::Opus,
        });
        for _ in 0..400 {
            e.collect_opened_intent_sinks();
            if e.opening_intent_sinks.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(e.opening_intent_sinks.is_empty());
        assert!(e.intent_sinks.is_empty());
        assert!(e.sink_node_for_peer_device.is_empty());
    }

    /// A device advertised twice does not become two local outputs.
    ///
    /// The peer re-advertises its devices on every reconnect, and creating a
    /// second Intent Sink for one device would leave the user picking between
    /// duplicate outputs that mirror the same thing.
    #[test]
    fn a_device_advertised_twice_creates_one_intent_sink() {
        let mut e = engine();
        e.intent_sink_opener = |_, _, _| Err(cursio_audio::AudioError::Unsupported);

        let meta = cursio_protocol::AudioStreamMeta {
            id: 42,
            app_name: None,
            device: Some(("dev-handle".into(), "Speakers".into())),
            role: cursio_protocol::StreamRole::Playback,
            format: cursio_protocol::AudioFormat {
                sample_rate: 48000,
                channels: 2,
                sample_kind: cursio_protocol::SampleKind::F32,
            },
            codec: cursio_protocol::AudioCodec::Opus,
        };
        e.on_advertise(&meta);
        e.on_advertise(&meta);

        assert_eq!(
            e.opening_intent_sinks.len(),
            1,
            "one device must have exactly one Intent Sink in flight"
        );
    }
}
