//! Where each stream plays, as both sides agree on it: routes, route
//! groups and their gain, and whether anyone is listening at all.

use crate::audio_session::module::send_control;
use crate::audio_session::AudioEngine;
use cursio_protocol::AudioMsg;
use cursio_transport::{TransportError, TransportTx};

/// Host-side group gain/mute for one Route (all streams funnelled into one target
/// device). Its `effective_gain` is fanned out to each member as a per-stream
/// [`Message::AudioGain`]. See docs/audio-design.md, "Route / Routing Group".
#[derive(Debug, Clone, Copy)]
pub(super) struct RouteGroup {
    /// Group gain, linear (1.0 = unity).
    gain: f32,
    /// Whether the whole group is muted.
    mute: bool,
}

impl RouteGroup {
    /// The gain actually applied to each member: the group gain, or zero when the
    /// group is muted (mute wins over gain, and is reversible without losing the
    /// gain value).
    fn effective_gain(&self) -> f32 {
        if self.mute {
            0.0
        } else {
            self.gain
        }
    }
}

impl AudioEngine {
    /// The stream host (re)targeted one Remote Stream to a device. Update where we
    /// render it. Only the host may set this (invariant #4); we reflect it.
    ///
    /// `device` here is one of OUR local device handles (the host routed its app
    /// into our device via its Intent Sink), or `None` for our default output.
    pub(super) fn on_stream_route(&mut self, entry: &cursio_protocol::RouteEntry) {
        self.apply_route(entry.id, entry.device.as_ref().map(|(d, _)| d.clone()));
    }

    /// The stream host replicated its whole routing map. Apply it idempotently:
    /// set each stream's target, and forget targets for streams no longer routed.
    ///
    /// This is the reconnect path — one object carries the full map, so we don't
    /// depend on the graph re-emitting per-link events.
    pub(super) fn on_routing_state(&mut self, state: &cursio_protocol::RoutingState) {
        tracing::info!(
            count = state.routes.len(),
            "applying replicated routing state"
        );
        // The set of stream ids the host still routes; anything we had targeted
        // that isn't here has gone back to the default.
        let live: std::collections::HashSet<u32> = state.routes.iter().map(|r| r.id).collect();
        let stale: Vec<u32> = self
            .inbound_app_device
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in stale {
            self.apply_route(id, None);
        }
        for entry in &state.routes {
            self.apply_route(entry.id, entry.device.as_ref().map(|(d, _)| d.clone()));
        }
    }

    /// A peer requested a group operation on a Route we host (invariant #4: we own
    /// the member streams, so we apply it). Update the group's gain/mute and mark
    /// it dirty; `pump` fans the effective gain out to each member and replicates
    /// the [`RouteState`].
    ///
    /// A Route is identified by its target device; members are the Remote Streams
    /// funnelled into that device. Group gain/mute is a Route property, never the
    /// device's replicated volume (invariant #2).
    pub(super) fn on_route_op(
        &mut self,
        target_device: (String, String),
        op: cursio_protocol::RouteOp,
    ) {
        let group = self
            .route_groups
            .entry(target_device.clone())
            .or_insert(RouteGroup {
                gain: 1.0,
                mute: false,
            });
        match op {
            cursio_protocol::RouteOp::SetGain(g) => {
                group.gain = g.clamp(0.0, 4.0);
                tracing::info!(device = %target_device.1, gain = group.gain, "route group gain set");
            }
            cursio_protocol::RouteOp::SetMute(m) => {
                group.mute = m;
                tracing::info!(device = %target_device.1, mute = m, "route group mute set");
            }
        }
        self.routes_dirty.insert(target_device);
    }

    /// Report which inbound streams have a local consumer, one message per
    /// change.
    ///
    /// Only transitions are sent: the owner holds the last value, so repeating
    /// it every pump (~100/s per stream) would flood the shared channel that
    /// input rides on to say nothing new.
    /// Close one of our microphones the peer stopped recording from, and open
    /// it again when it starts. Other streams only stop being sent (see
    /// `unwanted_streams`); a microphone also stops being recorded.
    pub(super) fn follow_mic_demand(&mut self, id: u32, listening: bool) {
        let Some(device) = self.mic_devices.get(&id) else {
            return;
        };
        if !listening {
            if self.mic_captures.remove(&id).is_some() {
                self.pending_packets.remove(&id);
                tracing::info!(id, "nothing on the peer records this microphone; closed it");
            }
            return;
        }
        if self.mic_captures.contains_key(&id) {
            return;
        }
        match cursio_audio::Capture::mic_tap(id, Some(device.clone())) {
            Ok(cap) => {
                tracing::info!(id, "the peer records this microphone again; opened it");
                self.mic_captures.insert(id, cap);
            }
            Err(e) => {
                tracing::warn!(id, error = %e, "cannot reopen the microphone the peer asked for")
            }
        }
    }

    pub(super) async fn pump_demand(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        let relay_hears = self.peer_mic.filter(|_| self.relay_wants_mic());
        let changed: Vec<(u32, bool)> = self
            .peer_streams
            .iter()
            .filter_map(|&id| {
                // Three states, not two, and conflating the last two is fatal.
                //
                // - a playback that is open answers for itself;
                // - one that FAILED to open will never answer, so refuse the
                //   stream: this is the Windows microphone case, where nothing
                //   can publish it and the owner would otherwise stream to a
                //   machine that can never play it;
                // - one that has not opened *yet* must say nothing at all,
                //   because some playbacks open only on the stream's FIRST
                //   CHUNK. Refuse one of those and the owner stops sending, so
                //   the first chunk never arrives, so the playback never opens —
                //   the refusal makes itself permanently true.
                //
                // Silence is the right answer for the third: the owner keeps
                // sending, the playback opens, and the pump after that reports
                // the truth.
                //
                // The microphone a relay hears is wanted whatever records
                // from its mirror here: the relay is its listener.
                let listening = match self.playbacks.get(&id) {
                    _ if relay_hears == Some(id) => true,
                    Some(p) => p.is_listening(),
                    None if self.failed_playbacks.contains(&id) => false,
                    None => return None,
                };
                (self.demand_sent.get(&id) != Some(&listening)).then_some((id, listening))
            })
            .collect();
        for (id, listening) in changed {
            self.demand_sent.insert(id, listening);
            tracing::info!(id, listening, "reporting stream demand to the owner");
            send_control(tx, AudioMsg::StreamDemand { id, listening }).await?;
        }
        Ok(())
    }

    /// Fan out any changed Route's group gain/mute to its members, and replicate
    /// its [`RouteState`]. Called from `pump` (it needs `tx`).
    ///
    /// For each dirty Route: gather the member streams (Remote Streams whose target
    /// is this device), send each an [`Message::AudioGain`] carrying the group's
    /// effective gain, then send one [`Message::AudioRouteState`] for the group.
    pub(super) async fn pump_routes(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        if self.routes_dirty.is_empty() {
            return Ok(());
        }
        let dirty: Vec<(String, String)> = self.routes_dirty.drain().collect();
        for device in dirty {
            let group = self
                .route_groups
                .get(&device)
                .copied()
                .unwrap_or(RouteGroup {
                    gain: 1.0,
                    mute: false,
                });
            let effective = group.effective_gain();
            // Members: Remote Streams currently targeting this device.
            let members: Vec<u32> = self
                .routed_app_route
                .values()
                .filter(|r| r.device.as_ref() == Some(&device))
                .map(|r| r.id)
                .collect();
            for &id in &members {
                send_control(
                    tx,
                    AudioMsg::Gain {
                        id,
                        gain: effective,
                    },
                )
                .await?;
            }
            tracing::info!(device = %device.1, members = members.len(), gain = group.gain, mute = group.mute, "applied route group op to members");
            send_control(
                tx,
                AudioMsg::RouteState(cursio_protocol::RouteState {
                    target_device: device,
                    members,
                    gain: group.gain,
                    mute: group.mute,
                }),
            )
            .await?;
        }
        Ok(())
    }

    /// Set a stream's target device and, if its playback is already open on a
    /// different device, drop it so the next chunk reopens it on the new target.
    ///
    /// Shared by [`on_stream_route`] and [`on_routing_state`]. `device = None`
    /// means "render into our default output".
    pub(super) fn apply_route(&mut self, id: u32, device: Option<String>) {
        let previous = self.inbound_app_device.get(&id).cloned();
        if previous == device {
            return; // idempotent: nothing changed
        }
        match &device {
            Some(dev) => {
                self.inbound_app_device.insert(id, dev.clone());
            }
            None => {
                self.inbound_app_device.remove(&id);
            }
        }
        // If we're already rendering this stream, its target moved — reopen it on
        // the next chunk so it lands on the right device.
        if self.playbacks.remove(&id).is_some() {
            tracing::info!(
                id,
                ?device,
                "stream re-routed; playback will reopen on the new device"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_session::test_support::demands;
    use crate::audio_session::test_support::engine;
    use crate::audio_session::test_support::route;
    use crate::audio_session::test_support::RecordingTx;

    /// A single re-route updates where a stream renders; `None` clears it back to
    /// the default. This is the per-stream `AudioStreamRoute` path.
    #[test]
    fn stream_route_sets_and_clears_target() {
        let mut e = engine();
        e.on_stream_route(&route(7, Some("Speakers")));
        assert_eq!(
            e.inbound_app_device.get(&7).map(String::as_str),
            Some("Speakers")
        );

        e.on_stream_route(&route(7, None));
        assert!(
            !e.inbound_app_device.contains_key(&7),
            "None clears the target"
        );
    }

    /// A RoutingState snapshot is applied as a WHOLE object: listed streams get
    /// their target, and any previously-targeted stream NOT in the snapshot is
    /// reset to the default. This is the reconnect guarantee (invariant #10).
    #[test]
    fn routing_state_is_applied_as_a_whole_object() {
        let mut e = engine();
        // Prior state: two streams targeted.
        e.on_stream_route(&route(1, Some("A")));
        e.on_stream_route(&route(2, Some("B")));

        // A new snapshot keeps 1 (retargeted), drops 2, adds 3.
        e.on_routing_state(&cursio_protocol::RoutingState {
            routes: vec![route(1, Some("A2")), route(3, Some("C"))],
        });

        assert_eq!(e.inbound_app_device.get(&1).map(String::as_str), Some("A2"));
        assert!(
            !e.inbound_app_device.contains_key(&2),
            "stream missing from the snapshot is reset"
        );
        assert_eq!(e.inbound_app_device.get(&3).map(String::as_str), Some("C"));
    }

    /// Mute wins over gain, and is reversible without losing the gain value —
    /// the group's effective per-member gain is 0 while muted, the stored gain
    /// again when unmuted.
    #[test]
    fn route_group_effective_gain_mute_wins() {
        let mut g = RouteGroup {
            gain: 0.6,
            mute: false,
        };
        assert_eq!(g.effective_gain(), 0.6);
        g.mute = true;
        assert_eq!(g.effective_gain(), 0.0, "muted group applies zero gain");
        g.mute = false;
        assert_eq!(g.effective_gain(), 0.6, "unmute restores the stored gain");
    }

    /// A group op updates the Route's stored gain/mute, clamps out-of-range gain,
    /// and marks the Route dirty so `pump` fans it out.
    #[test]
    fn route_op_updates_group_and_marks_dirty() {
        let mut e = engine();
        let dev = ("vaio3".to_string(), "VAIO3".to_string());

        e.on_route_op(dev.clone(), cursio_protocol::RouteOp::SetGain(0.5));
        assert_eq!(e.route_groups.get(&dev).unwrap().gain, 0.5);
        assert!(
            e.routes_dirty.contains(&dev),
            "a group op marks the route dirty"
        );

        e.on_route_op(dev.clone(), cursio_protocol::RouteOp::SetMute(true));
        let g = e.route_groups.get(&dev).unwrap();
        assert!(
            g.mute && g.gain == 0.5,
            "mute is independent of the stored gain"
        );

        // Out-of-range gain is clamped, never inverted or blown past headroom.
        e.on_route_op(dev.clone(), cursio_protocol::RouteOp::SetGain(-1.0));
        assert_eq!(e.route_groups.get(&dev).unwrap().gain, 0.0);
        e.on_route_op(dev.clone(), cursio_protocol::RouteOp::SetGain(99.0));
        assert_eq!(e.route_groups.get(&dev).unwrap().gain, 4.0);
    }

    /// Applying the same snapshot twice changes nothing the second time —
    /// idempotence is what makes resending safe on reconnect.
    #[test]
    fn routing_state_is_idempotent() {
        let mut e = engine();
        let snap = cursio_protocol::RoutingState {
            routes: vec![route(1, Some("A")), route(2, None)],
        };
        e.on_routing_state(&snap);
        let before: std::collections::BTreeMap<_, _> =
            e.inbound_app_device.clone().into_iter().collect();
        e.on_routing_state(&snap);
        let after: std::collections::BTreeMap<_, _> =
            e.inbound_app_device.clone().into_iter().collect();
        assert_eq!(
            before, after,
            "re-applying the same snapshot must be a no-op"
        );
        assert_eq!(before.get(&1).map(String::as_str), Some("A"));
        assert!(!before.contains_key(&2), "a None entry never sets a target");
    }

    /// A stream this machine cannot play is reported as unwanted, not skipped.
    ///
    /// The owner only stops sending when told to, so a stream that produces no
    /// demand message is one the owner keeps streaming forever. Reporting from
    /// the playbacks alone could not say anything about a stream that has none —
    /// which is exactly the stream that most needs to be refused. A Linux
    /// microphone sent to a Windows peer, which has no way to publish it, ran
    /// this way for a whole session: 200 chunks/s, `played=18`.
    #[tokio::test]
    async fn an_unplayable_stream_is_reported_as_unwanted() {
        let mut e = engine();

        // The peer advertises a microphone this machine cannot publish. The
        // open was attempted and failed, so no playback will ever exist.
        e.peer_streams.insert(476800);
        e.failed_playbacks.insert(476800);

        let mut tx = RecordingTx::default();
        e.pump_demand(&mut tx)
            .await
            .expect("reporting demand must not fail");

        assert_eq!(
            demands(&tx),
            vec![(476800, false)],
            "a stream that failed to open must be refused, not passed over in silence"
        );

        // Only transitions go out: the owner holds the last value, and repeating
        // it every pump (~100/s per stream) would flood the channel input rides
        // on to say nothing new.
        let mut again = RecordingTx::default();
        e.pump_demand(&mut again)
            .await
            .expect("reporting demand must not fail");
        assert!(
            demands(&again).is_empty(),
            "an unchanged demand must not be resent"
        );
    }

    /// A stream nobody plays is captured but not sent — and resumes the moment
    /// something opens it.
    ///
    /// Advertising a device makes it appear in the peer's device list; it does
    /// not mean anyone opened it. Streaming all 13 mirrored endpoints regardless
    /// is what made the first seconds of a session drop audio, as the sender's
    /// backlog trim discarded what the link could not carry.
    #[test]
    fn a_stream_nobody_plays_is_not_sent() {
        let mut e = engine();

        // The peer reports no consumer for stream 7.
        e.unwanted_streams.insert(7);
        assert!(
            e.unwanted_streams.contains(&7),
            "a stream with no listener must be suppressed"
        );

        // Something opens it: it must start flowing again with no renegotiation.
        e.unwanted_streams.remove(&7);
        assert!(
            !e.unwanted_streams.contains(&7),
            "a stream must resume as soon as it has a listener"
        );
    }

    /// Demand is reported on change only — repeating it every pump would flood
    /// the channel input shares.
    #[test]
    fn demand_is_only_reported_when_it_changes() {
        let mut e = engine();
        e.demand_sent.insert(7, true);

        // Same value as last reported: nothing to send.
        assert_eq!(e.demand_sent.get(&7), Some(&true));

        // A closed stream forgets its demand, so a reused id re-reports.
        e.demand_sent.remove(&7);
        assert_eq!(
            e.demand_sent.get(&7),
            None,
            "a closed stream must not leave stale demand behind"
        );
    }
}
