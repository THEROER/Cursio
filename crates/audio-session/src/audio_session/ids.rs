//! Stream ids: each node gets its own range, so two machines never hand
//! out the same one, and microphones sit apart from applications in it.

use cursio_protocol::PeerId;

/// Width of one node's stream-id range. A node allocates devices at
/// `base+1..`, per-app streams at `base+500..`, so the range must comfortably
/// exceed both. 1000 matches the original hand-assigned layout.
/// Where microphone stream ids start inside a node's range.
///
/// Devices take `base+1..`, per-app streams `base+500..`; microphones sit at
/// `base+800..` so the three kinds never collide inside one [`ID_RANGE`].
pub(super) const MIC_ID_OFFSET: u32 = 800;

pub(super) const ID_RANGE: u32 = 1_000;
/// How many distinct ranges exist before two nodes must share one.
///
/// Sized so a realistic mesh (a handful of machines) practically never
/// collides, while ids stay small enough to read in a log.
pub(super) const ID_SLOTS: u32 = 1_000;

/// Pick the stream-id range for a node, from its stable [`PeerId`].
///
/// Was `Initiator => 1000 / Responder => 2000`, which only works for exactly
/// two machines: a third node is necessarily *some* link's initiator, so it
/// would reuse 1000 and its streams would collide with the node already there.
///
/// Hashing the id instead gives every node its own range regardless of who
/// dialled whom, and it's stable across reconnects — the same machine keeps the
/// same range, so a peer's stream ids don't churn when a link is re-established.
pub(super) fn id_base_for(node: PeerId) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    node.hash(&mut h);
    // +1 so the range never starts at 0: id 0 is easy to confuse with "unset".
    (h.finish() as u32 % ID_SLOTS + 1) * ID_RANGE
}

/// Make a device id safe to use as a PipeWire node name (letters, digits, `_`).
pub(super) fn sanitize_node(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two nodes must land in different ranges, or their stream ids overlap and
    /// audio gets routed to the wrong device. (Not guaranteed for *every*
    /// possible pair — it's a hash — but it must hold for ordinary distinct ids.)
    #[test]
    fn distinct_nodes_get_distinct_ranges() {
        let a = id_base_for(PeerId::new_v4());
        let b = id_base_for(PeerId::new_v4());
        assert_ne!(a, b, "two random nodes collided in the same id range");
    }

    /// The same machine must keep its range across reconnects, otherwise a
    /// peer's streams would be renumbered every time a link is re-established.
    #[test]
    fn range_is_stable_for_one_node() {
        let id = PeerId::new_v4();
        assert_eq!(id_base_for(id), id_base_for(id));
    }

    /// Ranges must not start at 0 (easily confused with "unset") and must leave
    /// room for the +500 per-app sub-range below the next node's base.
    #[test]
    fn range_layout_leaves_room_for_app_ids() {
        for _ in 0..100 {
            let base = id_base_for(PeerId::new_v4());
            assert!(base >= ID_RANGE, "range started at or below zero: {base}");
            assert_eq!(base % ID_RANGE, 0, "ranges should be aligned: {base}");
        }
    }
}
