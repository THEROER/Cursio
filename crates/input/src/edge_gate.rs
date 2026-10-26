//! When a pointer resting against a watched edge means "send it over there".
//!
//! Windows has no barrier object to fire, so its capture decides from the
//! pointer positions its low-level hook sees: a pointer arriving in a 1px jump
//! zone on a watched edge is a crossing. Two things make that harder than it
//! sounds, and the decision lives here, apart from the hook, so both are
//! covered by tests that need no desktop.
//!
//! **A flick is not a crossing.** A game aiming with a cursor it does not
//! clip can carry the pointer hundreds of pixels to an edge in one event, and
//! handing the machine away for that yanks control mid-play. So arriving with
//! a step past [`MAX_CROSS_STEP`] does not cross.
//!
//! **But arriving fast is how people reach an edge.** A swipe across a
//! laptop's touchpad, or a mouse with acceleration, lands on the edge with
//! exactly such a step. The first version of this gate treated that arrival
//! as "already crossed" and fired nothing until the pointer had left the zone
//! and come back slowly — so reaching the peer meant hammering the edge. A
//! pointer held against an edge keeps producing move events with no step at
//! all (it cannot go further), and *that* is the intent: a fast arrival only
//! arms the zone, and the next move that pushes while still in it crosses.
//!
//! Positions and bounds are in one coordinate space (physical pixels of the
//! virtual desktop); the caller makes sure of that.

use cursio_protocol::{Edge, EdgeSet};

use crate::desk::Rect;

/// Largest single-step movement (px, along the edge's normal) that crosses on
/// arrival. Pushing into an edge moves the pointer a few pixels per event; a
/// flick jumps far more.
pub(crate) const MAX_CROSS_STEP: i32 = 60;

/// Width of the jump zone at the true outer edge, matching Deskflow's
/// `getJumpZoneSize`.
const ZONE: i32 = 1;

/// The virtual desktop, as `(x, y, width, height)`.
pub(crate) type Bounds = (i32, i32, i32, i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zone {
    /// The pointer is away from every watched edge.
    Outside,
    /// It arrived too fast to cross; the next push from here does.
    Arrived,
    /// It crossed, or must be treated as having: nothing fires until it
    /// leaves. Set after every ownership change, since the pointer is then
    /// sitting on the edge it just crossed.
    Occupied,
}

/// What a move event decided.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Crossing {
    pub edge: Edge,
    /// Where along the whole desktop's edge, `0.0..=1.0`.
    pub pos_norm: f32,
    /// The pointer's position as it crossed.
    pub at: (i32, i32),
}

/// The jump-zone state across move events.
#[derive(Debug)]
pub(crate) struct EdgeGate {
    zone: Zone,
    last: Option<(i32, i32)>,
    /// The part of each edge (by `edge_index`) that crosses; outside it the
    /// edge is an ordinary wall.
    pub(crate) spans: [(f32, f32); 4],
    /// The pixels along each edge a peer's screen meets (`None`: all).
    pub(crate) windows: [Option<Vec<(i32, i32)>>; 4],
}

impl Default for EdgeGate {
    fn default() -> Self {
        Self {
            zone: Zone::Outside,
            last: None,
            spans: [(0.0, 1.0); 4],
            windows: [None, None, None, None],
        }
    }
}

impl EdgeGate {
    /// Control changed hands: the pointer sits on the edge it crossed, and the
    /// last position seen is stale by however long the hook stopped sampling.
    pub(crate) fn ownership_changed(&mut self) {
        self.zone = Zone::Occupied;
        self.last = None;
    }

    /// One pointer move to `pt` on a desktop of `monitors`. Returns the
    /// crossing it makes, if any.
    ///
    /// The zone is at the edge of the monitor the pointer is on, where that
    /// edge faces off the desktop ([`crate::desk::outer_at`]) — not at the
    /// box around them all, whose bottom a shorter monitor never reaches.
    pub(crate) fn on_move(
        &mut self,
        pt: (i32, i32),
        monitors: &[Rect],
        watched: EdgeSet,
    ) -> Option<Crossing> {
        // Sampled even with nothing armed, so the first move after an edge is
        // armed measures its step from where the pointer really was.
        let prev = self.last.replace(pt);
        let (Some(desk), Some(monitor)) = (
            crate::desk::bounds(monitors),
            crate::desk::monitor_at(monitors, pt),
        ) else {
            return None;
        };
        if watched.is_empty() {
            return None;
        }
        let (vx, vy, vw, vh) = desk;
        let (mx, my, mw, mh) = monitor;

        // In a corner two watched edges can both be hit; the first in
        // `EdgeSet` order wins, which is arbitrary but stable, and moving
        // along the edge a pixel settles it.
        let hit = watched.iter().find_map(|edge| {
            let (in_zone, along, pos_norm) = match edge {
                Edge::Left => (pt.0 <= mx + ZONE, pt.1, frac(pt.1 - vy, vh)),
                Edge::Right => (pt.0 >= mx + mw - 1 - ZONE, pt.1, frac(pt.1 - vy, vh)),
                Edge::Top => (pt.1 <= my + ZONE, pt.0, frac(pt.0 - vx, vw)),
                Edge::Bottom => (pt.1 >= my + mh - 1 - ZONE, pt.0, frac(pt.0 - vx, vw)),
            };
            let (from, to) = self.spans[crate::edge_index(edge)];
            (in_zone
                && crate::desk::outer_at(monitors, monitor, edge, along)
                && (from..=to).contains(&pos_norm)
                && crate::within(&self.windows[crate::edge_index(edge)], along))
            .then_some(Crossing {
                edge,
                pos_norm,
                at: pt,
            })
        });

        let Some(crossing) = hit else {
            self.zone = Zone::Outside;
            return None;
        };
        let step = match prev {
            Some(p) if crossing.edge.is_horizontal() => (pt.1 - p.1).abs(),
            Some(p) => (pt.0 - p.0).abs(),
            None => 0,
        };
        match self.zone {
            Zone::Occupied => None,
            _ if step > MAX_CROSS_STEP => {
                tracing::debug!(step, "edge reached by a fast move; the next push crosses");
                self.zone = Zone::Arrived;
                None
            }
            Zone::Outside | Zone::Arrived => {
                self.zone = Zone::Occupied;
                Some(crossing)
            }
        }
    }
}

/// Where a pointer coming in across `edge` belongs: `pos_norm` of the way along
/// it — the fraction the peer measured where the pointer left *its* desktop,
/// in the same terms [`frac`] measures one leaving this one — and `inset`
/// pixels inside, so the jump zone does not send it straight back.
///
/// The fraction is of the whole virtual desktop's edge. With monitors of
/// different heights side by side that is not the monitor the pointer left,
/// but it is the one mapping that needs to know nothing about the peer's
/// layout, and going over and back lands where it started.
pub(crate) fn entry_point(
    edge: Edge,
    pos_norm: f32,
    bounds: Bounds,
    inset: i32,
) -> Option<(i32, i32)> {
    let (vx, vy, vw, vh) = bounds;
    if vw <= 0 || vh <= 0 {
        return None;
    }
    let along = |origin: i32, span: i32| {
        let offset = (pos_norm.clamp(0.0, 1.0) * span as f32) as i32;
        origin + offset.min(span - 1)
    };
    let inset_x = inset.min(vw - 1);
    let inset_y = inset.min(vh - 1);
    Some(match edge {
        Edge::Left => (vx + inset_x, along(vy, vh)),
        Edge::Right => (vx + vw - 1 - inset_x, along(vy, vh)),
        Edge::Top => (along(vx, vw), vy + inset_y),
        Edge::Bottom => (along(vx, vw), vy + vh - 1 - inset_y),
    })
}

/// Normalize `v` into `0.0..=1.0` over `span` pixels (guards span > 0).
fn frac(v: i32, span: i32) -> f32 {
    if span <= 0 {
        0.0
    } else {
        (v as f32 / span as f32).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESK: Bounds = (0, 0, 1920, 1080);
    const MON: [Rect; 1] = [DESK];

    fn left() -> EdgeSet {
        EdgeSet::EMPTY.with(Edge::Left)
    }

    #[test]
    fn a_slow_push_into_a_watched_edge_crosses_at_once() {
        let mut gate = EdgeGate::default();
        assert_eq!(gate.on_move((10, 540), &MON, left()), None);
        let crossed = gate.on_move((0, 540), &MON, left()).unwrap();
        assert_eq!(crossed.edge, Edge::Left);
        assert!((crossed.pos_norm - 0.5).abs() < 0.01);
    }

    #[test]
    fn a_fast_arrival_crosses_on_the_next_push_without_leaving_the_edge() {
        let mut gate = EdgeGate::default();
        gate.on_move((900, 540), &MON, left());
        assert_eq!(gate.on_move((0, 540), &MON, left()), None);
        // Held against the edge, the pointer cannot move further: step 0.
        assert!(gate.on_move((0, 541), &MON, left()).is_some());
    }

    #[test]
    fn a_pointer_resting_on_the_edge_crosses_only_once() {
        let mut gate = EdgeGate::default();
        gate.on_move((5, 540), &MON, left());
        assert!(gate.on_move((0, 540), &MON, left()).is_some());
        assert_eq!(gate.on_move((0, 540), &MON, left()), None);
        assert_eq!(gate.on_move((0, 545), &MON, left()), None);
    }

    #[test]
    fn leaving_the_edge_lets_the_next_approach_cross_again() {
        let mut gate = EdgeGate::default();
        gate.on_move((5, 540), &MON, left());
        assert!(gate.on_move((0, 540), &MON, left()).is_some());
        gate.on_move((30, 540), &MON, left());
        assert!(gate.on_move((0, 540), &MON, left()).is_some());
    }

    #[test]
    fn a_flick_that_carries_on_away_from_the_edge_never_crosses() {
        let mut gate = EdgeGate::default();
        gate.on_move((900, 540), &MON, left());
        assert_eq!(gate.on_move((0, 540), &MON, left()), None);
        assert_eq!(gate.on_move((400, 540), &MON, left()), None);
    }

    #[test]
    fn the_edge_it_arrived_on_does_not_send_control_straight_back() {
        let mut gate = EdgeGate::default();
        gate.ownership_changed();
        assert_eq!(gate.on_move((0, 540), &MON, left()), None);
        assert_eq!(gate.on_move((0, 541), &MON, left()), None);
        gate.on_move((12, 541), &MON, left());
        assert!(gate.on_move((0, 541), &MON, left()).is_some());
    }

    #[test]
    fn an_unwatched_edge_never_crosses() {
        let mut gate = EdgeGate::default();
        gate.on_move((1900, 540), &MON, left());
        assert_eq!(gate.on_move((1919, 540), &MON, left()), None);
        assert_eq!(gate.on_move((0, 540), &MON, EdgeSet::EMPTY), None);
    }

    #[test]
    fn a_narrowed_edge_crosses_only_inside_its_span() {
        let mut gate = EdgeGate::default();
        gate.spans[crate::edge_index(Edge::Left)] = (0.5, 1.0);
        gate.on_move((5, 100), &MON, left());
        assert_eq!(
            gate.on_move((0, 100), &MON, left()),
            None,
            "above the span is a wall"
        );
        gate.on_move((5, 900), &MON, left());
        assert!(gate.on_move((0, 900), &MON, left()).is_some());
    }

    #[test]
    fn a_pointer_entering_lands_at_the_height_it_left_the_peer_at() {
        assert_eq!(entry_point(Edge::Left, 0.5, DESK, 12), Some((12, 540)));
        assert_eq!(entry_point(Edge::Right, 0.0, DESK, 12), Some((1907, 0)));
        assert_eq!(entry_point(Edge::Bottom, 0.25, DESK, 12), Some((480, 1067)));
    }

    #[test]
    fn the_bottom_of_the_peer_is_the_bottom_here_not_one_past_it() {
        assert_eq!(entry_point(Edge::Left, 1.0, DESK, 12), Some((12, 1079)));
        assert_eq!(entry_point(Edge::Left, 7.0, DESK, 12), Some((12, 1079)));
        assert_eq!(entry_point(Edge::Left, -1.0, DESK, 12), Some((12, 0)));
    }

    #[test]
    fn leaving_and_coming_back_lands_where_the_pointer_left() {
        let mut gate = EdgeGate::default();
        gate.on_move((5, 811), &MON, left());
        let out = gate.on_move((0, 811), &MON, left()).unwrap();
        let (_, y) = entry_point(out.edge, out.pos_norm, DESK, 12).unwrap();
        assert!((y - 811).abs() <= 1, "came back at {y}");
    }

    #[test]
    fn an_entry_on_a_desktop_left_of_the_primary_starts_at_its_origin() {
        let desk = (-1920, 0, 3840, 1080);
        assert_eq!(entry_point(Edge::Left, 0.5, desk, 12), Some((-1908, 540)));
        assert_eq!(entry_point(Edge::Left, 0.5, (0, 0, 0, 0), 12), None);
    }

    #[test]
    fn a_desktop_left_of_the_primary_monitor_has_its_edge_at_its_own_origin() {
        let mut gate = EdgeGate::default();
        let desk = [(-1920, 0, 1920, 1080), (0, 0, 1920, 1080)];
        assert_eq!(gate.on_move((0, 540), &desk, left()), None);
        gate.on_move((-1910, 540), &desk, left());
        assert!(gate.on_move((-1920, 540), &desk, left()).is_some());
    }

    fn bottom() -> EdgeSet {
        EdgeSet::EMPTY.with(Edge::Bottom)
    }

    /// The case the box around the monitors got wrong: the short monitor's
    /// bottom is an edge of the desktop although the box goes lower.
    #[test]
    fn the_bottom_of_a_short_monitor_beside_a_tall_one_crosses() {
        let desk = [(0, 0, 2560, 1440), (2560, 0, 1920, 1080)];
        let mut gate = EdgeGate::default();
        gate.on_move((3000, 1070), &desk, bottom());
        let crossed = gate.on_move((3000, 1079), &desk, bottom()).unwrap();
        assert_eq!(crossed.at, (3000, 1079));
    }

    /// A peer under the right monitor only: the left one's bottom is a wall.
    #[test]
    fn only_the_pixels_a_peers_screen_meets_cross() {
        let desk = [(0, 0, 1920, 1080), (1920, 0, 1920, 1080)];
        let mut gate = EdgeGate::default();
        gate.windows[crate::edge_index(Edge::Bottom)] = Some(vec![(1920, 3840)]);
        gate.on_move((500, 1070), &desk, bottom());
        assert_eq!(gate.on_move((500, 1079), &desk, bottom()), None);
        gate.on_move((2500, 1070), &desk, bottom());
        assert!(gate.on_move((2500, 1079), &desk, bottom()).is_some());
    }
}
