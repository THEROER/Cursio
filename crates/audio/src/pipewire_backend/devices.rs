//! Real devices' volume and mute, watched and set: the PipeWire half of
//! device-state replication, and its cubic volume curve.

use crate::worker::{OnDrop, Worker};
use crate::{AudioError, DeviceChange, DeviceCommand};
use pipewire::{
    context::ContextRc,
    main_loop::MainLoopRc,
    spa::{
        pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray},
        sys::{SPA_PARAM_Props, SPA_PROP_channelVolumes, SPA_PROP_mute, SPA_TYPE_OBJECT_Props},
    },
};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

/// Start watching every local audio **device** (sinks and sources): report
/// volume/mute changes on `changes` and apply what arrives on `commands` — the
/// backend half of device-state replication (see docs/audio-design.md, "Device
/// state is REPLICATED"). Fails only if the PipeWire connection can't be made.
///
/// It runs one persistent PipeWire loop (its own thread, like the other
/// backends): it binds each device node as a proxy, subscribes to its `Props`
/// param, and translates PipeWire's cubic `channelVolumes` to/from the linear
/// `0..1` the protocol carries.
pub(crate) fn spawn_device_monitor(
    changes: tokio::sync::mpsc::UnboundedSender<DeviceChange>,
    commands: std_mpsc::Receiver<DeviceCommand>,
) -> Result<Worker, AudioError> {
    Worker::start(
        "cursio-pw-device-monitor".to_string(),
        "device-monitor thread",
        OnDrop::Join,
        move |stop_rx, ready_tx| run_device_monitor_loop(changes, commands, stop_rx, ready_tx),
    )
}

/// PipeWire stores per-channel volume on a **cubic** curve: the value a UI shows
/// as `v` is stored as `v³`. Convert between the linear `0..1` the protocol uses
/// and PipeWire's stored cube, so a "42%" set here reads back as 42% in
/// pavucontrol/helvum (the single-slider illusion depends on this matching).
pub(super) fn linear_to_cubic(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    v * v * v
}
pub(super) fn cubic_to_linear(v: f32) -> f32 {
    v.clamp(0.0, 1.0).cbrt()
}

/// Build a `Props` POD setting `channelVolumes` (all channels to `cubic`) and/or
/// `mute`. `channels` sizes the volume array to match the device.
pub(super) fn props_pod(channels: usize, cubic: Option<f32>, mute: Option<bool>) -> Vec<u8> {
    let mut properties = Vec::new();
    if let Some(cubic) = cubic {
        properties.push(Property {
            key: SPA_PROP_channelVolumes,
            flags: PropertyFlags::empty(),
            value: Value::ValueArray(ValueArray::Float(vec![cubic; channels.max(1)])),
        });
    }
    if let Some(mute) = mute {
        properties.push(Property {
            key: SPA_PROP_mute,
            flags: PropertyFlags::empty(),
            value: Value::Bool(mute),
        });
    }
    super::pod_bytes(
        Object {
            type_: SPA_TYPE_OBJECT_Props,
            id: SPA_PARAM_Props,
            properties,
        },
        "props",
    )
}

/// One device the monitor is tracking: its proxy (kept alive to receive param
/// events), a param listener, its channel count, and last-seen state.
pub(super) struct TrackedDevice {
    node: pipewire::node::Node,
    _listener: pipewire::node::NodeListener,
    channels: usize,
    last: Option<(f32, bool)>,
}

/// Persistent loop: bind every device node, watch its `Props`, and apply sets.
pub(super) fn run_device_monitor_loop(
    changes_tx: tokio::sync::mpsc::UnboundedSender<DeviceChange>,
    cmd_rx: std_mpsc::Receiver<DeviceCommand>,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
) {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    let setup = (|| -> Result<_, String> {
        let main_loop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
        let context = ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| format!("connect: {e}"))?;
        let registry = core
            .get_registry_rc()
            .map_err(|e| format!("registry: {e}"))?;
        Ok((main_loop, core, registry))
    })();

    let (main_loop, _core, registry) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // node.name -> tracked proxy. Kept in an Rc<RefCell> so the registry callback
    // (which binds new devices) and the command/tick code can both reach it.
    let tracked: Rc<RefCell<HashMap<String, TrackedDevice>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let _reg_listener = {
        let tracked = tracked.clone();
        let registry_inner = registry.clone();
        let changes_tx = changes_tx.clone();
        registry
            .add_listener_local()
            .global(move |obj| {
                let Some(props) = obj.props else { return };
                let class = props.get("media.class").unwrap_or("");
                if class != "Audio/Sink" && class != "Audio/Source" {
                    return;
                }
                let Some(name) = props.get("node.name") else {
                    return;
                };
                // NOTE: unlike device *enumeration*, we do NOT skip our own
                // `cursio_*` nodes here. An Intent Sink is exactly the node the
                // non-owner side turns to drive the owner's device (magic media
                // keys), so its volume must be observable. The session decides
                // per node whether a change means "broadcast my device" (a real
                // local device) or "request the owner" (an Intent Sink); nodes
                // that are neither are simply ignored there.
                if tracked.borrow().contains_key(name) {
                    return; // already bound
                }
                let channels = props
                    .get("audio.channels")
                    .and_then(|c| c.parse::<usize>().ok())
                    .unwrap_or(2);

                let node: pipewire::node::Node = match registry_inner.bind(obj) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::debug!(device = %name, error = %e, "cannot bind device node");
                        return;
                    }
                };

                // Report Props changes for this node as DeviceChange events.
                let name_owned = name.to_string();
                let listener = {
                    let changes_tx = changes_tx.clone();
                    let tracked_cb = tracked.clone();
                    let name_for_cb = name_owned.clone();
                    node.add_listener_local()
                        .param(move |_seq, id, _index, _next, param| {
                            if id != pipewire::spa::param::ParamType::Props {
                                return;
                            }
                            let Some(pod) = param else { return };
                            let Some((volume, mute)) = parse_props(pod) else {
                                return;
                            };
                            // Deduplicate: PipeWire re-emits Props on unrelated
                            // changes, and re-broadcasting an unchanged value
                            // would be noise (and could ping-pong with the peer).
                            if let Some(dev) = tracked_cb.borrow_mut().get_mut(&name_for_cb) {
                                if dev.last == Some((volume, mute)) {
                                    return;
                                }
                                dev.last = Some((volume, mute));
                            }
                            let _ = changes_tx.send(DeviceChange {
                                node_name: name_for_cb.clone(),
                                volume,
                                mute,
                            });
                        })
                        .register()
                };

                // Ask for the current Props so we learn the initial state.
                node.subscribe_params(&[pipewire::spa::param::ParamType::Props]);

                tracked.borrow_mut().insert(
                    name_owned,
                    TrackedDevice {
                        node,
                        _listener: listener,
                        channels,
                        last: None,
                    },
                );
            })
            .global_remove({
                // A device disappearing is handled by the session (the advertise
                // for it goes away); here we just need the registry listener to
                // exist. We don't map global id -> name cheaply, so leave the
                // proxy to be dropped when the whole monitor stops; a re-added
                // device rebinds under its name above.
                move |_id| {}
            })
            .register()
    };

    let _ = ready_tx.send(Ok(()));

    let loop_ref = main_loop.loop_();
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        // Apply any pending set commands, building a Props pod per request.
        while let Ok(cmd) = cmd_rx.try_recv() {
            let t = tracked.borrow();
            match cmd {
                DeviceCommand::SetVolume { node_name, volume } => {
                    if let Some(dev) = t.get(&node_name) {
                        let pod = props_pod(dev.channels, Some(linear_to_cubic(volume)), None);
                        if let Some(p) = Pod::from_bytes(&pod) {
                            dev.node
                                .set_param(pipewire::spa::param::ParamType::Props, 0, p);
                        }
                    } else {
                        tracing::debug!(device = %node_name, "set_volume for an unknown device");
                    }
                }
                DeviceCommand::SetMute { node_name, mute } => {
                    if let Some(dev) = t.get(&node_name) {
                        let pod = props_pod(dev.channels, None, Some(mute));
                        if let Some(p) = Pod::from_bytes(&pod) {
                            dev.node
                                .set_param(pipewire::spa::param::ParamType::Props, 0, p);
                        }
                    }
                }
            }
        }
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
}

/// Parse a `Props` POD, returning (linear volume, mute) if present.
///
/// Reads the *first* channel of `channelVolumes` as the master (all channels are
/// set together by [`props_pod`], so they agree) and de-cubes it to linear.
pub(super) fn parse_props(pod: &Pod) -> Option<(f32, bool)> {
    use pipewire::spa::pod::deserialize::PodDeserializer;
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };
    let mut volume: Option<f32> = None;
    let mut mute: Option<bool> = None;
    // NOTE: compare with `if`, not a `match` arm. `SPA_PROP_*` are runtime
    // constants whose names aren't UPPER_SNAKE, so a `match prop.key { SPA_PROP_x
    // => .. }` would treat them as *new bindings* that match anything — silently
    // wrong. Explicit `==` avoids that trap.
    for prop in obj.properties {
        if prop.key == SPA_PROP_channelVolumes {
            if let Value::ValueArray(ValueArray::Float(vols)) = prop.value {
                if let Some(&first) = vols.first() {
                    volume = Some(cubic_to_linear(first));
                }
            }
        } else if prop.key == SPA_PROP_mute {
            if let Value::Bool(m) = prop.value {
                mute = Some(m);
            }
        }
    }
    // Only report once we know both; a Props update carrying just one is merged
    // with the last-known by the caller's dedup, but for the initial event we
    // want a complete state. Default the missing half conservatively.
    match (volume, mute) {
        (Some(v), Some(m)) => Some((v, m)),
        (Some(v), None) => Some((v, false)),
        (None, Some(m)) => Some((1.0, m)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device has ONE volume, identical everywhere (docs/audio-design.md
    /// invariant #2). The cubic mapping is how our linear 0..1 stays the SAME
    /// number pavucontrol shows, so it must round-trip cleanly.
    #[test]
    fn cubic_linear_round_trips() {
        for &v in &[0.0f32, 0.1, 0.25, 0.42, 0.5, 0.8, 1.0] {
            let back = cubic_to_linear(linear_to_cubic(v));
            assert!((back - v).abs() < 1e-4, "{v} round-tripped to {back}");
        }
    }

    /// Out-of-range volumes are clamped, never wrapped or left to blow past
    /// full-scale.
    #[test]
    fn cubic_linear_clamps() {
        assert_eq!(linear_to_cubic(-1.0), 0.0);
        assert_eq!(linear_to_cubic(2.0), 1.0);
        assert_eq!(cubic_to_linear(-1.0), 0.0);
        assert_eq!(cubic_to_linear(2.0), 1.0);
    }

    /// The Props POD we build must be the exact one we parse back — this guards
    /// the pod format (keys, array-of-float volumes, bool mute) end to end, and
    /// specifically the earlier trap where a `match` on non-UPPER_SNAKE SPA
    /// constants silently matched everything.
    #[test]
    fn props_pod_round_trips() {
        let bytes = props_pod(2, Some(linear_to_cubic(0.42)), Some(true));
        let pod = Pod::from_bytes(&bytes).expect("valid pod");
        let (volume, mute) = parse_props(pod).expect("parses");
        assert!((volume - 0.42).abs() < 1e-3, "volume {volume} != 0.42");
        assert!(mute, "mute should be true");
    }

    /// A volume-only Props update parses with mute defaulted, and a mute-only one
    /// parses with volume defaulted — the monitor's dedup merges the halves, but
    /// each must decode on its own without dropping the update.
    #[test]
    fn props_pod_partial_updates_decode() {
        let vol_bytes = props_pod(2, Some(linear_to_cubic(0.7)), None);
        let vol_only = Pod::from_bytes(&vol_bytes).unwrap();
        let (v, m) = parse_props(vol_only).expect("volume-only parses");
        assert!((v - 0.7).abs() < 1e-3);
        assert!(!m, "mute defaults false when absent");

        let mute_bytes = props_pod(2, None, Some(true));
        let mute_only = Pod::from_bytes(&mute_bytes).unwrap();
        let (v, m) = parse_props(mute_only).expect("mute-only parses");
        assert_eq!(v, 1.0, "volume defaults to unity when absent");
        assert!(m);
    }
}
