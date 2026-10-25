//! A peer's device as a silent sink here: what the user routes into it
//! is reported, never played.

use crate::worker::{OnDrop, Worker};
use crate::AudioError;
use cursio_protocol::AudioStreamId;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

/// A membership change on an [`PwIntentSink`]: an application's playback stream
/// was linked into the sink, or unlinked from it.
#[derive(Debug, Clone)]
pub enum IntentSinkEvent {
    /// A playback stream is now routed into this Intent Sink.
    Joined { app_node_id: u32, app_name: String },
    /// A playback stream is no longer routed into this Intent Sink.
    Left { app_node_id: u32 },
}

/// A device Intent Sink: a real local sink that carries no audio, only observes
/// which playback streams are funnelled into it (reported on [`Self::events`]).
pub struct PwIntentSink {
    // First, so the thread is signalled before its channel closes.
    _worker: Worker,
    id: AudioStreamId,
    events_rx: tokio::sync::mpsc::UnboundedReceiver<IntentSinkEvent>,
}

impl PwIntentSink {
    /// Create an Intent Sink named `name`/`description` for a peer device. It
    /// appears in the local output list (so the user can route into it) but plays
    /// nothing; membership changes arrive on [`Self::events`].
    pub fn create(
        id: AudioStreamId,
        name: String,
        description: String,
    ) -> Result<Self, AudioError> {
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = Worker::start(
            format!("cursio-pw-intent-{id}"),
            "intent-sink thread",
            OnDrop::Detach,
            move |stop_rx, ready_tx| {
                run_intent_sink_loop(name, description, events_tx, stop_rx, ready_tx)
            },
        )?;
        Ok(PwIntentSink {
            id,
            events_rx,
            _worker: worker,
        })
    }

    /// Membership changes: apps joining/leaving this Intent Sink.
    pub fn events(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<IntentSinkEvent> {
        &mut self.events_rx
    }

    pub fn id(&self) -> AudioStreamId {
        self.id
    }
}

/// Main-loop body for an Intent Sink. Creates a null-audio-sink node (a real sink
/// that discards what it receives — no monitor, no capture stream), then watches
/// the registry: whenever a link is created into one of our input ports, resolve
/// the far-end output node to an application and emit `Joined`; on link removal,
/// emit `Left`.
pub(super) fn run_intent_sink_loop(
    name: String,
    description: String,
    events_tx: tokio::sync::mpsc::UnboundedSender<IntentSinkEvent>,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
) {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    let setup = (|| -> Result<_, String> {
        let (main_loop, core) = super::connect()?;

        // A null-audio-sink is a real Audio/Sink the system lists as an output, but
        // it discards whatever is played into it — exactly a silent funnel. We do
        // NOT create a capture stream on it, so no PCM is ever shipped. Give it a
        // 2-channel stereo layout so ordinary stereo apps link cleanly.
        let sink = core
            .create_object::<pipewire::node::Node>(
                "adapter",
                &pipewire::properties::properties! {
                    "factory.name" => "support.null-audio-sink",
                    *pipewire::keys::MEDIA_CLASS => "Audio/Sink",
                    *pipewire::keys::NODE_NAME => name.as_str(),
                    *pipewire::keys::NODE_DESCRIPTION => description.as_str(),
                    "audio.position" => "[ FL FR ]",
                    // No monitor ports: nothing should tap this sink's output. It
                    // is a dead end by design.
                    "monitor.channel-volumes" => "false",
                },
            )
            .map_err(|e| format!("create null-sink: {e}"))?;

        Ok((main_loop, core, sink))
    })();

    let (main_loop, core, _sink) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let loop_ref = main_loop.loop_();

    // Let the null-sink register so its ports exist before we resolve our node id.
    for _ in 0..20 {
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(25)));
    }

    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("registry: {e}")));
            return;
        }
    };

    // State for interpreting link events. The registry delivers links, ports and
    // nodes in ANY order — a link can arrive before the ports it references, and a
    // port before its node — so we never discard a link we can't resolve yet.
    // Instead we keep everything raw and RE-RESOLVE membership from scratch each
    // time anything changes (and once per loop tick, to catch late arrivals).
    //
    //  - ports:  port_id -> owning node id
    //  - nodes:  node_id -> display name
    //  - links:  link_id -> (output_port, input_port)   [raw, kept until removed]
    //  - our_node: our sink's node id, once we've seen it
    //  - members: node_id -> the set of link_ids from it into our sink (last emit)
    let ports: Rc<RefCell<HashMap<u32, u32>>> = Rc::new(RefCell::new(HashMap::new()));
    let nodes: Rc<RefCell<HashMap<u32, String>>> = Rc::new(RefCell::new(HashMap::new()));
    let links: Rc<RefCell<HashMap<u32, (u32, u32)>>> = Rc::new(RefCell::new(HashMap::new()));
    let our_node: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
    // The set of app node ids we last reported as Joined, so re-resolving is
    // idempotent: we diff against it and emit only real transitions.
    let joined_set: Rc<RefCell<std::collections::HashSet<u32>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));

    // Recompute which app nodes are linked into our sink and emit Joined/Left for
    // the difference vs last time. Cheap and idempotent — safe to call often.
    let resolve = {
        let ports = ports.clone();
        let nodes = nodes.clone();
        let links = links.clone();
        let our_node = our_node.clone();
        let joined_set = joined_set.clone();
        let events = events_tx.clone();
        move || {
            let Some(sink_node) = *our_node.borrow() else {
                return; // don't know our own node id yet
            };
            let ports = ports.borrow();
            // Current app nodes whose output is linked into our sink's input.
            let mut current: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (out_port, in_port) in links.borrow().values() {
                let (Some(&out_node), Some(&in_node)) = (ports.get(out_port), ports.get(in_port))
                else {
                    continue; // ports not registered yet; try again next tick
                };
                if in_node == sink_node && out_node != sink_node {
                    current.insert(out_node);
                }
            }
            let mut last = joined_set.borrow_mut();
            // Joined: in current but not last.
            for &node in current.iter() {
                if !last.contains(&node) {
                    let app_name = nodes
                        .borrow()
                        .get(&node)
                        .cloned()
                        .unwrap_or_else(|| format!("node {node}"));
                    let _ = events.send(IntentSinkEvent::Joined {
                        app_node_id: node,
                        app_name,
                    });
                }
            }
            // Left: in last but not current.
            for &node in last.iter() {
                if !current.contains(&node) {
                    let _ = events.send(IntentSinkEvent::Left { app_node_id: node });
                }
            }
            *last = current;
        }
    };

    let ports_g = ports.clone();
    let nodes_g = nodes.clone();
    let our_node_g = our_node.clone();
    let links_g = links.clone();
    let sink_name = name.clone();
    let resolve_g = resolve.clone();
    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            let mut changed = false;
            // Node: record a display name; detect our own sink by node.name.
            if let Some(node_name) = props.get("node.name") {
                let display = props
                    .get("application.name")
                    .or_else(|| props.get("node.description"))
                    .unwrap_or(node_name)
                    .to_string();
                nodes_g.borrow_mut().insert(obj.id, display);
                if node_name == sink_name && our_node_g.borrow().is_none() {
                    *our_node_g.borrow_mut() = Some(obj.id);
                }
                changed = true;
            }
            // Port: map port id -> owning node id.
            if let Some(node) = props.get("node.id").and_then(|v| v.parse::<u32>().ok()) {
                if props.get("port.direction").is_some() {
                    ports_g.borrow_mut().insert(obj.id, node);
                    changed = true;
                }
            }
            // Link: keep it raw (by its own object id) with the ports it joins.
            if let (Some(op), Some(ip)) = (
                props
                    .get("link.output.port")
                    .and_then(|v| v.parse::<u32>().ok()),
                props
                    .get("link.input.port")
                    .and_then(|v| v.parse::<u32>().ok()),
            ) {
                links_g.borrow_mut().insert(obj.id, (op, ip));
                changed = true;
            }
            if changed {
                resolve_g();
            }
        })
        .register();

    let links_r = links.clone();
    let resolve_r = resolve.clone();
    let _rm_listener = registry
        .add_listener_local()
        .global_remove(move |id| {
            // If a removed object was a link we tracked, forget it and re-resolve;
            // membership recompute will emit Left for any node that dropped to zero.
            if links_r.borrow_mut().remove(&id).is_some() {
                resolve_r();
            }
        })
        .register();

    let _ = ready_tx.send(Ok(()));

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(50)));
        // Re-resolve every tick too: a link may have arrived before its ports/node
        // were registered, so the event-time resolve saw nothing. This catches it.
        resolve();
    }
}
