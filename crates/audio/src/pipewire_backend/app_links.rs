//! Linking one application's output ports to a capture stream's inputs,
//! channel by channel, and keeping the links right as the app's node and
//! ports come and go.

use std::time::Duration;

/// How often an app tap re-plans its links even without a registry change.
pub(super) const APP_RELINK_INTERVAL: Duration = Duration::from_millis(400);

/// How long an app tap may stay unlinked before that is reported as an error.
pub(super) const APP_LINK_PATIENCE: Duration = Duration::from_secs(10);

/// The registry's view of the ports and links an app tap cares about, kept
/// current by one listener for the tap's whole life.
///
/// The listener has to outlive every relink pass: a registry bound per pass
/// delivers its initial dump over many loop wakeups, so any pass that stops
/// pumping early sees only part of the graph — and a port absent from a
/// partial dump is indistinguishable from one that was removed.
pub(super) struct PortWatch {
    table: std::rc::Rc<std::cell::RefCell<PortTable>>,
    /// Set by the listener when a port of the app or of our node comes or goes,
    /// or a link on one of them is removed.
    pub(super) dirty: std::rc::Rc<std::cell::Cell<bool>>,
    /// Our capture node's id, `SPA_ID_INVALID` until the stream is bound.
    pub(super) our_node: std::rc::Rc<std::cell::Cell<u32>>,
    _registry: pipewire::registry::RegistryRc,
    _listener: pipewire::registry::Listener,
}

impl PortWatch {
    pub(super) fn new(core: &pipewire::core::CoreRc, app_id: u32) -> Result<Self, String> {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        let registry = core
            .get_registry_rc()
            .map_err(|e| format!("registry: {e}"))?;
        let table = Rc::new(RefCell::new(PortTable::default()));
        let dirty = Rc::new(Cell::new(false));
        let our_node = Rc::new(Cell::new(u32::MAX));

        let relevant = {
            let our_node = our_node.clone();
            move |node: u32| node == app_id || node == our_node.get()
        };
        let (table_g, dirty_g, relevant_g) = (table.clone(), dirty.clone(), relevant.clone());
        let (table_r, dirty_r) = (table.clone(), dirty.clone());
        let listener = registry
            .add_listener_local()
            .global(move |obj| {
                let Some(props) = obj.props else { return };
                let mut table = table_g.borrow_mut();
                if props.get("factory.type.name") == Some("PipeWire:Interface:Link") {
                    if let Some(name) = props.get("factory.name") {
                        table.factory = Some(name.to_string());
                    }
                    return;
                }
                if let (Some(out), Some(input)) = (
                    props
                        .get("link.output.port")
                        .and_then(|v| v.parse::<u32>().ok()),
                    props
                        .get("link.input.port")
                        .and_then(|v| v.parse::<u32>().ok()),
                ) {
                    table.links.insert(obj.id, (out, input));
                    return;
                }
                let Some(node) = props.get("node.id").and_then(|v| v.parse::<u32>().ok()) else {
                    return;
                };
                let Some(dir) = props.get("port.direction") else {
                    return;
                };
                // Absent only before PipeWire 0.3.50; 0 then compares equal
                // every pass, which degrades to matching by id alone.
                let serial = props
                    .get("object.serial")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let port = LinkPort {
                    id: obj.id,
                    serial,
                    channel: props.get("audio.channel").unwrap_or("").to_string(),
                };
                table.add_port(node, dir == "out", port);
                if relevant_g(node) {
                    dirty_g.set(true);
                }
            })
            .global_remove(move |id| {
                let mut table = table_r.borrow_mut();
                let Some((node, link)) = table.remove(id) else {
                    return;
                };
                if relevant(node) {
                    // A session manager may destroy one of our links while
                    // both ports live on; the proxy we hold would otherwise go
                    // on claiming an input nothing feeds any more.
                    table.removed_links.extend(link);
                    dirty_r.set(true);
                }
            })
            .register();

        Ok(Self {
            table,
            dirty,
            our_node,
            _registry: registry,
            _listener: listener,
        })
    }
}

/// Ports and links as the registry has announced them and not yet removed.
#[derive(Debug, Default)]
pub(super) struct PortTable {
    /// Port id → (owning node, is an output, the port).
    ports: std::collections::HashMap<u32, (u32, bool, LinkPort)>,
    /// Link id → (output port, input port).
    links: std::collections::HashMap<u32, (u32, u32)>,
    /// Name of the factory that creates links, once announced.
    factory: Option<String>,
    /// Links removed since the last pass whose output belonged to a node we
    /// watch.
    removed_links: Vec<RemovedLink>,
}

/// `((output port, input port), that output port's serial)`.
pub(super) type RemovedLink = ((u32, u32), u64);

impl PortTable {
    fn add_port(&mut self, node: u32, output: bool, port: LinkPort) {
        self.ports.insert(port.id, (node, output, port));
    }

    /// Forget a removed object. Returns the node it concerned, if it was a
    /// port, or a link whose output port we know — together with that link.
    ///
    /// The serial travels with a removed link because the port id alone may
    /// already belong to a recreated port that we have just linked afresh; a
    /// late removal of the old link must not take the new one with it.
    fn remove(&mut self, id: u32) -> Option<(u32, Option<RemovedLink>)> {
        if let Some((node, _, _)) = self.ports.remove(&id) {
            return Some((node, None));
        }
        let (out, input) = self.links.remove(&id)?;
        let (node, _, port) = self.ports.get(&out)?;
        Some((*node, Some(((out, input), port.serial))))
    }

    fn side(&self, node: u32, output: bool) -> Vec<LinkPort> {
        self.ports
            .values()
            .filter(|(n, o, _)| *n == node && *o == output)
            .map(|(_, _, p)| p.clone())
            .collect()
    }
}

/// Wire an application's output ports to our capture node's input ports.
///
/// Reads both nodes' ports from the [`PortWatch`] table, then drops any link
/// whose app output port is gone and creates a link for any app output port
/// not yet linked, pairing by channel (FL→FL, FR→FR). Both happen in the same
/// pass without yielding to the loop, so an app that recreates its ports loses
/// only what it lost itself.
///
/// `links` is the [`AppLinks`] map; its proxies must be **kept alive** —
/// dropping one tears its link down, which is exactly how a stale link is
/// removed. Idempotent: with no changes it does nothing. The decisions
/// themselves are [`plan_app_links`].
pub(super) fn relink_app(
    core: &pipewire::core::CoreRc,
    app_id: u32,
    our_node_id: u32,
    watch: &PortWatch,
    links: &mut AppLinks,
) {
    // Borrowed only between loop wakeups: `create_object` below queues a
    // request and dispatches nothing, so the listener cannot re-enter here.
    let mut table = watch.table.borrow_mut();
    for (pair, serial) in std::mem::take(&mut table.removed_links) {
        if links.get(&pair).is_some_and(|(s, _)| *s == serial) {
            tracing::info!(
                app_id,
                out = pair.0,
                input = pair.1,
                "app link was removed; relinking"
            );
            links.remove(&pair);
        }
    }
    let outs = table.side(app_id, true);
    let ins = table.side(our_node_id, false);
    let factory = table.factory.clone();
    drop(table);

    let linked: std::collections::HashMap<(u32, u32), u64> = links
        .iter()
        .map(|(&pair, (serial, _))| (pair, *serial))
        .collect();
    let plan = plan_app_links(&outs, &ins, &linked);

    // Removing the proxy from the map drops it, which tears the (already dead)
    // link down.
    for pair in &plan.stale {
        links.remove(pair);
    }
    if !plan.stale.is_empty() {
        tracing::info!(
            app_id,
            dropped = plan.stale.len(),
            "dropped stale app links"
        );
    }

    if plan.create.is_empty() {
        return;
    }
    let Some(factory) = factory else {
        return; // no link factory this pass; try again next tick
    };

    let mut created = 0;
    for (out, input) in plan.create {
        match core.create_object::<pipewire::link::Link>(
            &factory,
            &pipewire::properties::properties! {
                "link.output.node" => app_id.to_string().as_str(),
                "link.output.port" => out.id.to_string().as_str(),
                "link.input.node" => our_node_id.to_string().as_str(),
                "link.input.port" => input.to_string().as_str(),
            },
        ) {
            Ok(link) => {
                links.insert((out.id, input), (out.serial, link));
                created += 1;
            }
            Err(e) => tracing::debug!(app_id, error = %e, "create link failed"),
        }
    }
    if created > 0 {
        tracing::info!(app_id, created, "re-linked app output ports to capture");
    }
}

/// An app tap's live links: (app output port id, input port it feeds) → (that
/// output port's `object.serial`, the link proxy keeping it alive). Keyed by
/// the pair because a mono output feeds two inputs.
pub(super) type AppLinks = std::collections::HashMap<(u32, u32), (u64, pipewire::link::Link)>;

/// A port as the registry reported it on this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LinkPort {
    id: u32,
    /// `object.serial`: unlike `id`, never reused for another object.
    serial: u64,
    /// `audio.channel` (`FL`, `FR`, `MONO`…), empty when the port has none.
    channel: String,
}

/// What one [`relink_app`] pass has to change.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct AppLinkPlan {
    /// `(app output port, input port)` links whose output port no longer
    /// exists.
    stale: Vec<(u32, u32)>,
    /// `(app output port, our input port id)` pairs to link.
    create: Vec<(LinkPort, u32)>,
}

/// Decide which of an app tap's links are stale and which to create, given the
/// app's output ports, our input ports, and what is linked already
/// (`(out port id, input port id) → the out port's serial`).
///
/// **A link is recognised by port id *and* serial.** PipeWire recycles object
/// ids, and a browser recreates its output ports on every pause, seek or new
/// video. When a recreated port happens to receive a dead port's id, matching
/// by id alone takes the dead link for a live one: that port is never linked,
/// and its record goes on claiming the input it used to feed. The other
/// channel's new port then found its own input "taken" and fell back to the
/// only free one — the *other* channel's — so the tap carried a single channel
/// into a single ear, and which ear depended on which id was recycled. That is
/// the Shorts pattern: both ears, then only left, then only right. The serial
/// is never reused (PipeWire ≥ 0.3.50), so a changed serial marks the record
/// stale.
///
/// **A labelled channel only ever feeds the input of the same channel.** If
/// that input is taken this pass, the port waits for the next one
/// ([`APP_RELINK_INTERVAL`]) rather than crossing over: a crossed link is heard
/// as a missing ear, while a wait is a few hundred milliseconds of silence that
/// fixes itself.
///
/// **A mono output feeds every input.** A `MONO` port, or an app's only port
/// when it carries no label, is one signal meant for both ears; linking it to
/// the first free input alone played it in the left ear only. It is fanned out
/// to each input no labelled port has a claim on. Any other unlabelled port
/// takes one free input, again never one a labelled port has a claim on.
///
/// Both sides are sorted by channel first, because the registry lists ports in
/// arrival order, not channel order.
pub(super) fn plan_app_links(
    outs: &[LinkPort],
    ins: &[LinkPort],
    linked: &std::collections::HashMap<(u32, u32), u64>,
) -> AppLinkPlan {
    let mut plan = AppLinkPlan::default();
    let live = |id: u32, serial: u64| outs.iter().any(|o| o.id == id && o.serial == serial);

    let mut stale: Vec<(u32, u32)> = linked
        .iter()
        .filter(|(&(out, _), &serial)| !live(out, serial))
        .map(|(&pair, _)| pair)
        .collect();
    stale.sort_unstable();
    plan.stale = stale;

    // Which output each input is already fed by, from the links that survive.
    let mut taken: std::collections::HashMap<u32, u32> = linked
        .keys()
        .filter(|pair| !plan.stale.contains(pair))
        .map(|&(out, input)| (input, out))
        .collect();

    let mut outs: Vec<&LinkPort> = outs.iter().collect();
    let mut ins: Vec<&LinkPort> = ins.iter().collect();
    outs.sort_by(|a, b| a.channel.cmp(&b.channel).then(a.id.cmp(&b.id)));
    ins.sort_by(|a, b| a.channel.cmp(&b.channel));

    let own_input = |out: &LinkPort| {
        ins.iter()
            .find(|i| !out.channel.is_empty() && i.channel == out.channel)
            .map(|i| i.id)
    };
    let is_mono =
        |out: &LinkPort| out.channel == "MONO" || (out.channel.is_empty() && outs.len() == 1);
    // Inputs some labelled output has a claim on, so a fallback never takes
    // one away from the port it belongs to.
    let claimed: std::collections::HashSet<u32> =
        outs.iter().filter_map(|o| own_input(o)).collect();
    let unclaimed: Vec<u32> = ins
        .iter()
        .map(|i| i.id)
        .filter(|id| !claimed.contains(id))
        .collect();

    // An input already fed — by this port or another — is left as it is: one
    // port per input, and a port waits rather than doubling up.
    fn link(
        taken: &mut std::collections::HashMap<u32, u32>,
        plan: &mut AppLinkPlan,
        out: &LinkPort,
        input: u32,
    ) {
        if let std::collections::hash_map::Entry::Vacant(slot) = taken.entry(input) {
            slot.insert(out.id);
            plan.create.push((out.clone(), input));
        }
    }
    for &out in &outs {
        if let Some(input) = own_input(out) {
            link(&mut taken, &mut plan, out, input);
        } else if is_mono(out) {
            for &input in &unclaimed {
                link(&mut taken, &mut plan, out, input);
            }
        }
    }
    for &out in outs
        .iter()
        .filter(|o| own_input(o).is_none() && !is_mono(o))
    {
        if taken.values().any(|&owner| owner == out.id) {
            continue;
        }
        if let Some(&input) = unclaimed.iter().find(|id| !taken.contains_key(id)) {
            link(&mut taken, &mut plan, out, input);
        }
    }
    plan
}

// ---------------------------------------------------------------------------
// Playback (sink) side
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn port(id: u32, serial: u64, channel: &str) -> LinkPort {
        LinkPort {
            id,
            serial,
            channel: channel.into(),
        }
    }

    /// Our capture's two inputs: 50 is FL, 51 is FR.
    fn our_inputs() -> Vec<LinkPort> {
        vec![port(51, 901, "FR"), port(50, 900, "FL")]
    }

    /// A recreated port that inherits a dead port's id is relinked, and each
    /// channel lands on its own input.
    ///
    /// The Shorts case: Chromium replaced FL (10) and FR (11) with new ports,
    /// and the new FR happened to get id 10. Matched by id alone, 10 still
    /// looked linked, so it was never relinked and kept FL's input "taken" —
    /// the new FL then crossed over into FR, and the tap carried one channel.
    #[test]
    fn a_port_that_reuses_a_dead_ports_id_is_relinked_to_its_own_channel() {
        let linked = std::collections::HashMap::from([((10, 50), 100), ((11, 51), 101)]);
        let outs = vec![port(12, 102, "FL"), port(10, 103, "FR")];

        let plan = plan_app_links(&outs, &our_inputs(), &linked);

        assert_eq!(
            plan.stale,
            vec![(10, 50), (11, 51)],
            "both old links died with their ports"
        );
        assert_eq!(
            plan.create,
            vec![(port(12, 102, "FL"), 50), (port(10, 103, "FR"), 51)],
            "each channel must be linked to the input of the same channel"
        );
    }

    /// An unchanged graph needs no work, which is what makes the periodic pass
    /// safe.
    #[test]
    fn a_live_link_with_the_same_serial_is_left_alone() {
        let linked = std::collections::HashMap::from([((10, 50), 100), ((11, 51), 101)]);
        let outs = vec![port(11, 101, "FR"), port(10, 100, "FL")];

        assert_eq!(
            plan_app_links(&outs, &our_inputs(), &linked),
            AppLinkPlan::default()
        );
    }

    /// A channel whose own input is occupied waits rather than crossing over.
    ///
    /// Crossing is heard as a missing ear for as long as the link lives; a
    /// wait costs one relink interval and fixes itself once the occupant is
    /// gone.
    #[test]
    fn a_channel_never_crosses_into_the_other_channels_input() {
        // FL's input is held by a live FL port; a second FL port appears.
        let linked = std::collections::HashMap::from([((10, 50), 100)]);
        let outs = vec![port(10, 100, "FL"), port(12, 102, "FL")];

        let plan = plan_app_links(&outs, &our_inputs(), &linked);

        assert_eq!(
            plan,
            AppLinkPlan::default(),
            "the second FL must not be linked into FR's input"
        );
    }

    /// A port with no channel of ours falls back to a free input, but never to
    /// one a labelled port is about to need.
    #[test]
    fn an_unlabelled_port_takes_only_an_unclaimed_input() {
        let outs = vec![port(20, 200, ""), port(21, 201, "FR")];

        let plan = plan_app_links(&outs, &our_inputs(), &Default::default());

        assert_eq!(
            plan.create,
            vec![(port(21, 201, "FR"), 51), (port(20, 200, ""), 50)]
        );
    }

    /// A mono output plays in both ears: it is linked to every input.
    #[test]
    fn a_mono_output_is_linked_to_both_inputs() {
        for channel in ["MONO", ""] {
            let outs = vec![port(30, 300, channel)];

            let plan = plan_app_links(&outs, &our_inputs(), &Default::default());

            assert_eq!(
                plan.create,
                vec![(port(30, 300, channel), 50), (port(30, 300, channel), 51)],
                "a single {channel:?} port must feed FL and FR alike"
            );
        }
    }

    /// A mono output linked to one input so far gains the other on a later
    /// pass, and its existing link is kept.
    #[test]
    fn a_half_linked_mono_output_gains_its_missing_input() {
        let linked = std::collections::HashMap::from([((30, 50), 300)]);
        let outs = vec![port(30, 300, "MONO")];

        let plan = plan_app_links(&outs, &our_inputs(), &linked);

        assert!(plan.stale.is_empty(), "the live FL link must be kept");
        assert_eq!(plan.create, vec![(port(30, 300, "MONO"), 51)]);
    }

    /// A live app port stays linked however the rest of the graph churns.
    ///
    /// Firefox kept ports 149/130 for hours while its tap was dropped and
    /// relinked every few minutes: each pass read a registry dump that had not
    /// fully arrived, so its ports were missing and looked removed. The table
    /// is only ever shrunk by an explicit removal.
    #[test]
    fn a_stable_app_port_is_never_judged_stale_by_unrelated_graph_changes() {
        let mut table = PortTable::default();
        table.add_port(290, true, port(149, 101_576, "FL"));
        table.add_port(290, true, port(130, 101_577, "FR"));
        for input in our_inputs() {
            table.add_port(303, false, input);
        }
        let linked = std::collections::HashMap::from([((149, 50), 101_576), ((130, 51), 101_577)]);

        table.add_port(410, false, port(152, 101_427, "FL"));
        table.links.insert(424, (149, 152));
        assert_eq!(table.remove(424), Some((290, Some(((149, 152), 101_576)))));
        assert_eq!(table.remove(9999), None, "an unknown id concerns nothing");

        let plan = plan_app_links(&table.side(290, true), &table.side(303, false), &linked);
        assert_eq!(plan, AppLinkPlan::default());
    }

    /// A recreated port is replaced and linked in the same pass that retires
    /// the old one, so the only gap is the one the app made itself.
    #[test]
    fn a_recreated_app_port_is_relinked_in_the_same_pass_it_is_retired() {
        let mut table = PortTable::default();
        table.add_port(290, true, port(149, 100, "FL"));
        for input in our_inputs() {
            table.add_port(303, false, input);
        }
        let linked = std::collections::HashMap::from([((149, 50), 100)]);

        assert_eq!(table.remove(149), Some((290, None)));
        table.add_port(290, true, port(149, 105, "FL"));

        let plan = plan_app_links(&table.side(290, true), &table.side(303, false), &linked);
        assert_eq!(plan.stale, vec![(149, 50)]);
        assert_eq!(plan.create, vec![(port(149, 105, "FL"), 50)]);
    }
}
