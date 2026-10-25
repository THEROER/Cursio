//! Where barriers go and what a crossing means: the outer edges of the monitor
//! layout, the span of an edge a crossing may use, and the point a pointer
//! coming home is placed at.

use ashpd::desktop::input_capture::{
    Activated, ActivatedBarrier, Barrier, BarrierID, BarrierPosition,
};
use cursio_protocol::{Edge, EdgeSet, InputEvent};
use std::collections::HashMap;

/// A desktop region as `(x, y, width, height)`. The portal's own `Region` has
/// no public constructor, so the geometry is carried in a shape a test can
/// build.
pub(super) type Rect = (i32, i32, i32, i32);

/// What a placed barrier represents: which desktop edge it sits on, and the
/// pixel span it covers along that edge, so an activation position can be
/// normalized to `0.0`..=`1.0`. For a vertical edge (left/right) the span is in
/// y (`origin` = top, `length` = height); for a horizontal one (top/bottom) it's
/// in x.
#[derive(Clone, Copy)]
pub(super) struct BarrierInfo {
    edge: Edge,
    /// Pixel coordinate of the span's start along the edge's varying axis.
    origin: i32,
    /// Pixel length of the span (>0).
    length: i32,
}

/// Whether `edge` of this region faces off the desktop rather than another
/// monitor.
///
/// A barrier is a line the pointer cannot cross, so it belongs only where the
/// desktop ends. Put one on the seam between two monitors and the portal
/// refuses it — correctly, since stopping the pointer mid-desktop is not
/// something a compositor will do.
///
/// "Another monitor is there" means one that starts exactly where this edge
/// ends *and* overlaps it along the edge's own axis. Monitors of different
/// heights make that overlap partial: a short screen beside a tall one leaves
/// the tall one's edge exposed above and below it. Treating such an edge as
/// outer keeps the barrier, which is the safe way to be wrong — the portal
/// rejects a barrier it dislikes, while a missing one silently stops edge
/// crossing from working at all.
pub(super) fn is_outer_edge(rect: Rect, edge: Edge, rects: &[Rect]) -> bool {
    let (x, y, w, h) = rect;
    rects.iter().all(|&(ox, oy, ow, oh)| {
        if (ox, oy, ow, oh) == rect {
            return true; // a region cannot enclose itself
        }
        // Does the neighbour sit flush against this edge, and cover it whole?
        match edge {
            Edge::Left => !(ox + ow == x && oy <= y && oy + oh >= y + h),
            Edge::Right => !(ox == x + w && oy <= y && oy + oh >= y + h),
            Edge::Top => !(oy + oh == y && ox <= x && ox + ow >= x + w),
            Edge::Bottom => !(oy == y + h && ox <= x && ox + ow >= x + w),
        }
    })
}

/// Build barriers on ONLY the `edges` sides of the desktop — the edges that
/// border a peer. A barrier on any other edge is actively harmful: crossing it
/// would still make the portal *grab* input (locking the user), even though
/// nothing then takes the crossing because no peer is there. So we arm exactly
/// the edges we're willing to hand off across, and every other edge stays a
/// normal wall the cursor just stops at.
///
/// With no edges, no barriers are placed at all (edge-crossing off).
///
/// Only outer edges get one (see [`is_outer_edge`]); every barrier remembers
/// its own edge, so an activation says which way the cursor left. Returns the
/// id -> [`BarrierInfo`] map used to recover the edge + normalized position on
/// activation.
///
/// `spans` (by `Edge as usize`) narrows each edge to the part of it that
/// crosses, as fractions of all its outer segments together; a barrier covers
/// only that part, and the rest of the edge is the desktop's own. Each
/// barrier's [`BarrierInfo`] still records its monitor's whole segment, so a
/// crossing is measured along the whole edge, the way the peer places it.
pub(super) fn outer_edge_barriers(
    rects: &[Rect],
    edges: EdgeSet,
    spans: &[(f32, f32); 4],
) -> (Vec<Barrier>, HashMap<u32, BarrierInfo>) {
    let mut barriers = Vec::new();
    let mut info = HashMap::new();
    let mut next_id = 1u32;

    let along =
        |edge: Edge, &(x, y, w, h): &Rect| if edge.is_horizontal() { (x, w) } else { (y, h) };
    let window = |edge: Edge| -> Option<(i32, i32)> {
        let outer = rects.iter().filter(|&&r| is_outer_edge(r, edge, rects));
        let (start, end) = edge_span(outer.map(|r| along(edge, r)))?;
        let (from, to) = spans[crate::edge_index(edge)];
        let at = |f: f32| start + (f * (end - start) as f32).round() as i32;
        Some((at(from), at(to)))
    };

    for (peer_edge, &(x, y, w, h)) in edges
        .iter()
        .flat_map(|edge| rects.iter().map(move |rect| (edge, rect)))
    {
        // Outer edges only, as the name says. A barrier on the seam between two
        // of this desktop's own monitors is one the portal rejects — it logged
        // `some barriers were rejected` on every start of a two-monitor
        // desktop, which is noise that hides a barrier genuinely refused.
        if !is_outer_edge((x, y, w, h), peer_edge, rects) {
            continue;
        }

        // This monitor's segment of the edge, cut to the part that crosses.
        let (origin, length) = along(peer_edge, &(x, y, w, h));
        let Some((lo, hi)) = window(peer_edge) else {
            continue;
        };
        let (a, b) = (origin.max(lo), (origin + length).min(hi));
        if b <= a {
            continue;
        }
        let pos = match peer_edge {
            Edge::Left => BarrierPosition::new(x, a, x, b - 1),
            Edge::Right => BarrierPosition::new(x + w, a, x + w, b - 1),
            Edge::Top => BarrierPosition::new(a, y, b - 1, y),
            Edge::Bottom => BarrierPosition::new(a, y + h, b - 1, y + h),
        };

        if let Some(id) = BarrierID::new(next_id) {
            barriers.push(Barrier::new(id, pos));
            info.insert(
                next_id,
                BarrierInfo {
                    edge: peer_edge,
                    origin,
                    length,
                },
            );
            next_id += 1;
        }
    }
    (barriers, info)
}

/// How far to pull the cursor back from the edge after taking control, so the
/// barrier it just crossed doesn't immediately catch it again. Mirrors
/// Deskflow's jump-zone inset; a handful of pixels is enough to be outside the
/// barrier while staying visually where the user left off.
pub(super) const EDGE_NUDGE_PX: i32 = 12;

/// Push the cursor inward, away from `peer_edge`, by [`EDGE_NUDGE_PX`].
///
/// Sent as ordinary relative motion through our own capture channel, which the
/// session injects locally — the same path peer input takes. Relative is all we
/// have on Wayland (no absolute warp from a uinput device), but it's enough:
/// the cursor is pinned against the edge, so moving inward always lands it off
/// the barrier.
pub(super) fn nudge_off_edge(tx: &crate::EventSender, peer_edge: Option<Edge>) {
    let (dx, dy) = match peer_edge {
        Some(Edge::Right) => (-EDGE_NUDGE_PX, 0),
        Some(Edge::Left) => (EDGE_NUDGE_PX, 0),
        Some(Edge::Bottom) => (0, -EDGE_NUDGE_PX),
        Some(Edge::Top) => (0, EDGE_NUDGE_PX),
        None => return,
    };
    let _ = tx.send(InputEvent::PointerMotion { dx, dy });
}

/// `at`, moved [`EDGE_NUDGE_PX`] inward from `peer_edge`, in the portal's
/// absolute coordinates — where to leave the pointer when handing input back.
pub(super) fn inside_edge(at: (f32, f32), peer_edge: Option<Edge>) -> (f64, f64) {
    let (x, y) = (f64::from(at.0), f64::from(at.1));
    let inset = f64::from(EDGE_NUDGE_PX);
    match peer_edge {
        Some(Edge::Right) => (x - inset, y),
        Some(Edge::Left) => (x + inset, y),
        Some(Edge::Bottom) => (x, y - inset),
        Some(Edge::Top) => (x, y + inset),
        None => (x, y),
    }
}

/// Where a pointer coming in across `edge` at `pos_norm` belongs, in the
/// portal's absolute coordinates: that far along the desktop's outer edge on
/// that side, [`EDGE_NUDGE_PX`] inside it so the armed barrier does not catch
/// it at once.
///
/// "Along the edge" is along every outer edge on that side taken together —
/// the span [`crossing_from`] measures a pointer leaving by — so a desktop of
/// two monitors stacked against the peer maps the peer's top half to the upper
/// one. A fraction falling in a gap between them goes to the nearest.
pub(super) fn entry_position(rects: &[Rect], edge: Edge, pos_norm: f32) -> Option<(f64, f64)> {
    let outer: Vec<Rect> = rects
        .iter()
        .copied()
        .filter(|&r| is_outer_edge(r, edge, rects))
        .collect();
    let along = |&(x, y, w, h): &Rect| if edge.is_horizontal() { (x, w) } else { (y, h) };
    let (origin, end) = edge_span(outer.iter().map(along))?;
    let t = origin as f32 + pos_norm.clamp(0.0, 1.0) * (end - origin) as f32;
    let t = t.min((end - 1) as f32);
    let rect = *outer.iter().min_by_key(|r| {
        let (start, len) = along(r);
        let t = t as i32;
        if t < start {
            start - t
        } else {
            (t - (start + len - 1)).max(0)
        }
    })?;
    let (start, len) = along(&rect);
    let t = f64::from(t.clamp(start as f32, (start + len - 1) as f32));
    let (x, y, w, h) = rect;
    let inset = EDGE_NUDGE_PX;
    Some(match edge {
        Edge::Left => (f64::from(x + inset.min(w - 1)), t),
        Edge::Right => (f64::from(x + w - 1 - inset.min(w - 1)), t),
        Edge::Top => (t, f64::from(y + inset.min(h - 1))),
        Edge::Bottom => (t, f64::from(y + h - 1 - inset.min(h - 1))),
    })
}

/// The extent `(start, end)` covered by spans given as `(start, length)`.
pub(super) fn edge_span(spans: impl Iterator<Item = (i32, i32)>) -> Option<(i32, i32)> {
    spans
        .filter(|&(_, len)| len > 0)
        .map(|(start, len)| (start, start + len))
        .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
}

/// Turn a portal [`Activated`] into an [`EdgeCrossing`], or `None` if we can't
/// attribute it to a known barrier (unknown/absent id, or no cursor position).
///
/// Normalizes the crossing position to `0.0`..=`1.0` along the edge: for a
/// vertical edge (left/right) that's the y-fraction, for a horizontal one the
/// x-fraction. The value is clamped, since the reported position can sit a pixel
/// outside the span.
pub(super) fn crossing_from(
    a: &Activated,
    info: &HashMap<u32, BarrierInfo>,
) -> Option<crate::EdgeCrossing> {
    let id = match a.barrier_id()? {
        ActivatedBarrier::Barrier(id) => id.get(),
        ActivatedBarrier::UnknownBarrier => {
            tracing::debug!("activation with unknown barrier id; can't map to an edge");
            return None;
        }
    };
    let bi = info.get(&id)?;
    let (x, y) = a.cursor_position()?;
    // Position varies along x for a horizontal edge, along y for a vertical one.
    let coord = if bi.edge.is_horizontal() { x } else { y };
    Some(crate::EdgeCrossing {
        edge: bi.edge,
        pos_norm: fraction_along(bi.edge, coord, info),
    })
}

/// How far along `edge` the pixel `coord` is, over every barrier on that side
/// together — the span [`entry_position`] places a pointer coming back over.
/// Measured per barrier, a desktop of two stacked monitors reported the middle
/// of the lower one as halfway, and the pointer came home on the upper one.
pub(super) fn fraction_along(edge: Edge, coord: f32, info: &HashMap<u32, BarrierInfo>) -> f32 {
    let spans = info
        .values()
        .filter(|b| b.edge == edge)
        .map(|b| (b.origin, b.length));
    match edge_span(spans) {
        Some((start, end)) => ((coord - start as f32) / (end - start) as f32).clamp(0.0, 1.0),
        None => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(edge: Edge) -> EdgeSet {
        EdgeSet::EMPTY.with(edge)
    }

    fn whole_edge_barriers(
        rects: &[Rect],
        edges: EdgeSet,
    ) -> (Vec<Barrier>, HashMap<u32, BarrierInfo>) {
        outer_edge_barriers(rects, edges, &[(0.0, 1.0); 4])
    }

    fn bottom_only(from: f32, to: f32) -> [(f32, f32); 4] {
        let mut spans = [(0.0, 1.0); 4];
        spans[crate::edge_index(Edge::Bottom)] = (from, to);
        spans
    }

    #[test]
    fn a_narrowed_edge_gets_a_barrier_only_where_it_crosses() {
        let (barriers, info) = outer_edge_barriers(
            &[(0, 0, 1920, 1080)],
            only(Edge::Bottom),
            &bottom_only(0.66, 1.0),
        );
        assert_eq!(barriers.len(), 1);
        assert_eq!(barriers[0].position().as_tuple(), (1267, 1080, 1919, 1080));
        // Still measured along the whole edge, as the peer places a pointer.
        let bi = *info.values().next().unwrap();
        assert_eq!((bi.origin, bi.length), (0, 1920));
    }

    #[test]
    fn a_span_that_misses_a_monitor_leaves_that_monitor_without_a_barrier() {
        let desk = [(0, 0, 1920, 1080), (1920, 0, 1920, 1080)];
        let (barriers, _) = outer_edge_barriers(&desk, only(Edge::Bottom), &bottom_only(0.75, 1.0));
        assert_eq!(barriers.len(), 1);
        assert_eq!(barriers[0].position().as_tuple().0, 2880);
    }

    #[test]
    fn auto_release_leaves_the_pointer_off_the_barrier_it_crossed() {
        let at = (3840.0, 1061.5);
        assert_eq!(inside_edge(at, Some(Edge::Right)), (3828.0, 1061.5));
        assert_eq!(inside_edge(at, Some(Edge::Left)), (3852.0, 1061.5));
        assert_eq!(inside_edge(at, Some(Edge::Bottom)), (3840.0, 1049.5));
        assert_eq!(inside_edge(at, None), (3840.0, 1061.5));
    }

    /// A single screen with the peer on the right: one barrier, on the right
    /// hand side, spanning the full height.
    #[test]
    fn a_barrier_lands_on_the_edge_that_borders_the_peer() {
        let (barriers, info) = whole_edge_barriers(&[(0, 0, 1920, 1080)], only(Edge::Right));
        assert_eq!(barriers.len(), 1);
        assert_eq!(barriers[0].position().as_tuple(), (1920, 0, 1920, 1079));
        let bi = info.values().next().unwrap();
        assert_eq!(bi.edge, Edge::Right);
        assert_eq!((bi.origin, bi.length), (0, 1080));
    }

    /// A peer to the right and another below: both edges are armed at once,
    /// and each barrier remembers which one it is, so a crossing says which
    /// way — and so to whom — the cursor left.
    #[test]
    fn two_neighbours_arm_two_edges_that_each_name_themselves() {
        let edges: EdgeSet = [Edge::Right, Edge::Bottom].into_iter().collect();
        let (barriers, info) = whole_edge_barriers(&[(0, 0, 1920, 1080)], edges);
        assert_eq!(barriers.len(), 2);
        let mut placed: Vec<Edge> = info.values().map(|bi| bi.edge).collect();
        placed.sort_by_key(|edge| format!("{edge:?}"));
        assert_eq!(placed, vec![Edge::Bottom, Edge::Right]);
        let lines: Vec<_> = barriers.iter().map(|b| b.position().as_tuple()).collect();
        assert!(lines.contains(&(1920, 0, 1920, 1079)));
        assert!(lines.contains(&(0, 1080, 1919, 1080)));
    }

    /// Every other edge stays an ordinary wall. A barrier there would still
    /// make the portal grab input when the cursor hit it — locking the user out
    /// for a crossing the session then discards as "no peer that way".
    #[test]
    fn no_barrier_is_placed_without_an_edge_to_watch() {
        let (barriers, info) = whole_edge_barriers(&[(0, 0, 1920, 1080)], EdgeSet::EMPTY);
        assert!(barriers.is_empty());
        assert!(info.is_empty());
    }

    /// Two monitors of equal height, side by side: only the outer edge gets a
    /// barrier.
    ///
    /// The seam between them used to get one too, and the portal rejected it on
    /// every start — `some barriers were rejected` in the log of any
    /// two-monitor desktop, which is exactly the noise that hides a barrier
    /// refused for a reason worth knowing.
    #[test]
    fn a_seam_between_two_monitors_gets_no_barrier() {
        let desktop = [(0, 0, 1920, 1080), (1920, 0, 1920, 1080)];
        let (barriers, info) = whole_edge_barriers(&desktop, only(Edge::Right));
        assert_eq!(barriers.len(), 1, "the seam must not be offered");
        assert_eq!(info.len(), 1);
        let line = barriers[0].position().as_tuple().0;
        assert_eq!(line, 3840, "the barrier belongs on the desktop's own edge");
    }

    /// A neighbour that does not cover the whole edge leaves it partly facing
    /// off the desktop, so the barrier stays.
    ///
    /// A 1080-tall screen beside a 1440-tall one covers only part of the taller
    /// one's edge; the rest is outer. Dropping the barrier there would leave
    /// the pointer able to leave the desktop with nothing to notice it, which
    /// is worse than offering one the portal might reject.
    #[test]
    fn a_partly_covered_edge_keeps_its_barrier() {
        let desktop = [(0, 0, 2560, 1440), (2560, 0, 1920, 1080)];
        let (barriers, _) = whole_edge_barriers(&desktop, only(Edge::Right));
        let lines: Vec<i32> = barriers.iter().map(|b| b.position().as_tuple().0).collect();
        assert!(
            lines.contains(&2560),
            "the taller screen's edge is exposed above the shorter one"
        );
        assert!(lines.contains(&4480), "and the outer edge is still there");
    }

    /// Stacked monitors, with the peer to the right: neither edge is a seam,
    /// because the seam between them is horizontal.
    #[test]
    fn a_seam_on_another_axis_does_not_suppress_a_barrier() {
        let desktop = [(0, 0, 1920, 1080), (0, 1080, 1920, 1080)];
        let (barriers, _) = whole_edge_barriers(&desktop, only(Edge::Right));
        assert_eq!(barriers.len(), 2, "both right edges face off the desktop");
    }

    /// A horizontal edge varies along x, a vertical one along y. Getting this
    /// backwards puts the crossing at the wrong end of the peer's screen.
    #[test]
    fn a_horizontal_edge_spans_the_width_and_a_vertical_one_the_height() {
        let screen = [(0, 0, 1920, 1080)];
        let (_, top) = whole_edge_barriers(&screen, only(Edge::Top));
        assert_eq!(top.values().next().unwrap().length, 1920);
        let (_, left) = whole_edge_barriers(&screen, only(Edge::Left));
        assert_eq!(left.values().next().unwrap().length, 1080);
    }

    /// A monitor whose origin is not (0, 0) — the second screen of any
    /// multi-monitor desktop — must normalize against its own span, or a
    /// crossing halfway down it reports a position off the end of the edge.
    #[test]
    fn an_offset_monitor_normalizes_against_its_own_span() {
        let (_, info) = whole_edge_barriers(&[(1920, 200, 2560, 1440)], only(Edge::Right));
        let bi = *info.values().next().unwrap();
        assert_eq!((bi.origin, bi.length), (200, 1440));
    }

    #[test]
    fn a_pointer_coming_home_lands_at_the_height_it_left_the_peer_at() {
        let desk = [(0, 0, 1920, 1080)];
        assert_eq!(
            entry_position(&desk, Edge::Right, 0.5),
            Some((1907.0, 540.0))
        );
        assert_eq!(entry_position(&desk, Edge::Left, 1.0), Some((12.0, 1079.0)));
        assert_eq!(entry_position(&desk, Edge::Top, 0.25), Some((480.0, 12.0)));
    }

    #[test]
    fn stacked_monitors_share_one_edge_both_leaving_and_coming_back() {
        let desk = [(0, 0, 1920, 1080), (0, 1080, 1920, 1080)];
        let (_, info) = whole_edge_barriers(&desk, only(Edge::Right));
        let out = fraction_along(Edge::Right, 1620.0, &info);
        assert!((out - 0.75).abs() < 1e-6, "left at {out}");
        assert_eq!(
            entry_position(&desk, Edge::Right, out),
            Some((1907.0, 1620.0))
        );
    }

    #[test]
    fn an_edge_with_no_outer_monitor_has_nowhere_to_place_the_pointer() {
        assert_eq!(entry_position(&[], Edge::Left, 0.5), None);
    }
}
