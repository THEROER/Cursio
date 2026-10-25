//! A device's own state across the link: its volume and mute kept by the
//! machine that owns it, replicated to the one that shows it.

use crate::audio_session::module::send_control;
use crate::audio_session::AudioEngine;
use cursio_protocol::AudioMsg;
use cursio_transport::{TransportError, TransportTx};

impl AudioEngine {
    /// Turn device-monitor changes into wire traffic (see docs/audio-design.md,
    /// "All changes go THROUGH the owner").
    ///
    /// Two cases per observed change, decided by which node changed:
    /// - a **real device we own** changed (the user turned its knob locally, or we
    ///   just applied a peer's request) → broadcast its authoritative
    ///   [`DeviceState`] so every node mirrors it.
    /// - **one of our Intent Sinks** changed (the user turned the mirror's slider,
    ///   e.g. a media key while it is the default sink) → this is a *request* to
    ///   the owner, not a local write (invariant #4) → send `AudioDeviceStateSet`.
    ///   Unless it matches the replica we just applied — then it is our own echo,
    ///   swallowed so state doesn't ping-pong.
    pub(super) async fn pump_device_state(
        &mut self,
        tx: &mut dyn TransportTx,
    ) -> Result<(), TransportError> {
        // Drain changes synchronously first, so we don't hold a borrow on the
        // monitor across the awaits below.
        let mut changes = Vec::new();
        if let Some(monitor) = &mut self.device_monitor {
            while let Ok(c) = monitor.changes().try_recv() {
                changes.push(c);
            }
        }

        for c in changes {
            let state = cursio_protocol::DeviceState {
                volume: c.volume,
                mute: c.mute,
            };
            if self.owned_device_nodes.contains(&c.node_name) {
                // Owner side: broadcast, but only on a real change.
                if self.owned_device_state.get(&c.node_name) == Some(&state) {
                    continue;
                }
                self.owned_device_state.insert(c.node_name.clone(), state);
                tracing::info!(device = %c.node_name, volume = c.volume, mute = c.mute, "broadcasting owned device state");
                send_control(
                    tx,
                    AudioMsg::DeviceState {
                        device: c.node_name,
                        state,
                    },
                )
                .await?;
            } else if let Some(peer_dev) = self.peer_device_for_sink_node.get(&c.node_name).cloned()
            {
                // Non-owner side: a turn of the Intent Sink is a request — unless
                // it is the echo of a replica we just applied.
                if self.replica_echo.get(&c.node_name) == Some(&state) {
                    self.replica_echo.remove(&c.node_name);
                    continue;
                }
                tracing::info!(device = %peer_dev, volume = c.volume, mute = c.mute, "Intent Sink turned locally; requesting owner");
                send_control(
                    tx,
                    AudioMsg::DeviceStateSet {
                        device: peer_dev.clone(),
                        field: cursio_protocol::DeviceStateField::Volume(c.volume),
                    },
                )
                .await?;
                send_control(
                    tx,
                    AudioMsg::DeviceStateSet {
                        device: peer_dev,
                        field: cursio_protocol::DeviceStateField::Mute(c.mute),
                    },
                )
                .await?;
            }
            // else: some other node (a Cursio playback/mic node); not a device.
        }
        Ok(())
    }

    /// The peer set a logical gain for one of the streams we play.
    ///
    /// This is the per-stream gain of the "one system" model — a property of the
    /// Stream/Route, applied *above* the OS mixer and separate from a device's
    /// single replicated volume (see docs/audio-design.md, "Separation of
    /// responsibilities"). It attenuates the peer's audio *as we play it*, which
    /// is exactly "make only the remote audio quieter".
    ///
    /// Remembered in `gains` so it also applies to a playback that hasn't opened
    /// yet (gain can arrive before the first chunk).
    pub(super) fn on_gain(&mut self, id: u32, gain: f32) {
        tracing::info!(id, gain, "peer set stream gain");
        self.gains.insert(id, gain);
        self.apply_gain(id);
    }

    /// Set a playback's gain from both knobs that govern it: the owner's
    /// (`AudioGain`, kept in `gains`) and this machine's listener's (the
    /// [`LocalMixer`]). Multiplied, so neither overrides the other.
    pub(super) fn apply_gain(&self, id: u32) {
        let Some(p) = self.playbacks.get(&id) else {
            return;
        };
        let owner = self.gains.get(&id).copied().unwrap_or(1.0);
        p.set_gain(owner * self.mixer.level(id));
    }

    /// A peer **requested** a change to a device we OWN (invariant #3: only the
    /// owner mutates `DeviceState`). Apply it to the real device via the monitor;
    /// the resulting monitor change is broadcast to everyone in `pump`, so the
    /// requester learns the applied value the same way every other node does.
    ///
    /// We deliberately do NOT broadcast here from the request itself — the real
    /// device is the source of truth, and echoing the *request* value could
    /// diverge from what the hardware actually accepted (a device may quantise or
    /// clamp). Applying and letting the observed change propagate keeps one path.
    pub(super) fn on_device_state_set(
        &mut self,
        device: &str,
        field: cursio_protocol::DeviceStateField,
    ) {
        if !self.owned_device_nodes.contains(device) {
            // Not ours to mutate. Could be a device that has since disappeared, or
            // a misdirected request; ignore rather than guess.
            tracing::debug!(
                device,
                "device-state set for a device we don't own; ignoring"
            );
            return;
        }
        let Some(monitor) = &self.device_monitor else {
            return;
        };
        match field {
            cursio_protocol::DeviceStateField::Volume(v) => {
                tracing::info!(
                    device,
                    volume = v,
                    "applying peer's volume request to owned device"
                );
                monitor.set_volume(device.to_string(), v);
            }
            cursio_protocol::DeviceStateField::Mute(m) => {
                tracing::info!(
                    device,
                    mute = m,
                    "applying peer's mute request to owned device"
                );
                monitor.set_mute(device.to_string(), m);
            }
        }
    }

    /// The owner broadcast a device's authoritative state. We hold an Intent Sink
    /// mirroring that device, so apply the state to the sink node — that is what
    /// makes helvum/pavucontrol show the SAME slider value as the owner (the
    /// single-machine illusion). Record it as `replica_echo` so the monitor change
    /// this provokes is not bounced back to the owner as a fresh request.
    pub(super) fn on_device_state_replica(
        &mut self,
        device: &str,
        state: cursio_protocol::DeviceState,
    ) {
        let Some(node) = self.sink_node_for_peer_device.get(device).cloned() else {
            // We don't mirror this device (no Intent Sink for it). Nothing to do.
            tracing::debug!(
                device,
                "device-state replica for a device we don't mirror; ignoring"
            );
            return;
        };
        let Some(monitor) = &self.device_monitor else {
            return;
        };
        tracing::info!(device, %node, volume = state.volume, mute = state.mute, "reflecting owner's device state on Intent Sink");
        self.replica_echo.insert(node.clone(), state);
        monitor.set_volume(node.clone(), state.volume);
        monitor.set_mute(node, state.mute);
    }
}
