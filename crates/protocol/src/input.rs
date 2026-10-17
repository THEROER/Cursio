//! Platform-neutral input events.
//!
//! Cursio streams mouse and keyboard activity between machines as these
//! events. They are intentionally OS-agnostic: the capture side (source)
//! translates its platform's raw events into these, and the injection side
//! (receiver) translates them back into its platform's virtual-device calls.
//!
//! ## Relative motion, not absolute
//!
//! Following the Mouse-Without-Borders model but on *relative* motion: pointer
//! movement is carried as `(dx, dy)` deltas, never absolute screen
//! coordinates. Absolute positioning teleports the cursor, which breaks games
//! that read raw mouse input. Relative deltas behave like a real second mouse
//! plugged into the receiver.

use serde::{Deserialize, Serialize};

/// A single input event to replay on the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Relative pointer motion. `dx`/`dy` are deltas in device units, positive
    /// x = right, positive y = down (matching evdev `REL_X`/`REL_Y`).
    PointerMotion { dx: i32, dy: i32 },

    /// A mouse button changed state.
    PointerButton { button: MouseButton, pressed: bool },

    /// Scroll motion. `dx` = horizontal, `dy` = vertical, positive y = scroll
    /// up; units are [`SCROLL_NOTCH`]ths of a wheel notch, matching evdev
    /// `REL_WHEEL_HI_RES`, Windows' `WHEEL_DELTA` and libei's discrete steps.
    ///
    /// Not whole notches: a touchpad scrolls in fractions of one, and every
    /// fraction rounded to a notch on its own is zero — two-finger scrolling
    /// on a Windows precision touchpad arrived as nothing at all.
    PointerScroll { dx: i32, dy: i32 },

    /// A keyboard key changed state. `key` is a platform-neutral Linux evdev
    /// keycode (see `KEY_*` in `linux/input-event-codes.h`); both sides agree
    /// on this namespace and map to/from their own platform as needed.
    Key { key: u16, pressed: bool },
}

/// One wheel notch in [`InputEvent::PointerScroll`] units.
pub const SCROLL_NOTCH: i32 = 120;

/// Mouse buttons Cursio recognises. Buttons beyond these (thumb clusters past
/// the usual two, tilt-wheel clicks) are dropped rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    /// First side button — "back" in browsers (evdev `BTN_SIDE`, Windows `XBUTTON1`).
    Back,
    /// Second side button — "forward" in browsers (evdev `BTN_EXTRA`, Windows `XBUTTON2`).
    Forward,
}

/// One edge of a machine's desktop.
///
/// Used for edge-crossing handoff: when the shared cursor is shoved past the
/// edge that borders the peer, the active machine hands control over. Each side
/// configures which of *its* edges faces the peer; the peer's matching edge is
/// the [`opposite`](Edge::opposite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Where a machine sits in the user's layout, as a cell on a grid.
///
/// Coordinates rather than a named side, because a side cannot express order.
/// Four machines in a row are all "to the right" of the first, and nothing in
/// that vocabulary says which of them the cursor reaches first — so the moment
/// there are more than two, the layout stops being describable at all.
///
/// Nor is there a hub. A designated centre would make every other machine
/// depend on one being up, which is the arrangement the mesh exists to avoid:
/// with `0,0 — 1,0 — 2,0`, the middle machine going away leaves the other two
/// still knowing exactly where they stand relative to each other.
///
/// The units are cells, not pixels: `--at 1,0` means "one machine to the
/// right", and nothing here needs to know how wide anybody's screen is. A GUI
/// that later lets the user drag machines around writes the same two numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Position {
    /// Increases to the right.
    pub x: i32,
    /// Increases downward, matching screen coordinates rather than graphs.
    pub y: i32,
}

impl Position {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Which of *this* machine's edges faces `peer`.
    ///
    /// The dominant axis wins: a machine one cell right and one cell down is
    /// reached rightward, because a diagonal is not an edge a cursor can cross.
    /// Ties break horizontal, since machines stand side by side far more often
    /// than stacked.
    ///
    /// `None` for a peer in the same cell — two machines nobody has placed yet
    /// (both at the `0,0` default) border nothing, and inventing an edge for
    /// them would send the cursor away on a shove the user did not mean.
    pub fn edge_toward(self, peer: Position) -> Option<Edge> {
        let dx = peer.x - self.x;
        let dy = peer.y - self.y;
        if dx == 0 && dy == 0 {
            return None;
        }
        if dx.abs() >= dy.abs() {
            Some(if dx > 0 { Edge::Right } else { Edge::Left })
        } else {
            Some(if dy > 0 { Edge::Bottom } else { Edge::Top })
        }
    }

    /// How far away `peer` is, for picking between several in one direction.
    ///
    /// Manhattan distance, because movement here is along edges rather than
    /// through space: two cells right is two crossings whatever the diagonal
    /// measures.
    pub fn distance_to(self, peer: Position) -> u32 {
        self.x.abs_diff(peer.x) + self.y.abs_diff(peer.y)
    }
}

/// Which peer the cursor reaches when it leaves across `edge`.
///
/// The nearest one in that direction, which is the whole reason coordinates
/// beat named sides: with `0,0 — 1,0 — 2,0` the first machine's right-hand
/// neighbour is the second, not the third, and only a distance can say so.
///
/// `peers` carries whatever positions this node has learned from greetings, so
/// a machine that is offline is simply absent and the one behind it becomes the
/// neighbour — the layout heals instead of leaving a hole.
pub fn neighbour_across<T: Copy>(own: Position, edge: Edge, peers: &[(T, Position)]) -> Option<T> {
    peers
        .iter()
        .filter(|(_, position)| own.edge_toward(*position) == Some(edge))
        .min_by_key(|(_, position)| own.distance_to(*position))
        .map(|(peer, _)| *peer)
}

impl Edge {
    /// The edge on the far side — where the cursor conceptually *enters* the
    /// peer when it leaves us across this edge. Leaving our right edge means
    /// entering the peer's left edge, and so on.
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    /// Whether this edge is a horizontal one (top/bottom). The normalized
    /// crossing position along a horizontal edge is an x-fraction; along a
    /// vertical edge it's a y-fraction.
    pub fn is_horizontal(self) -> bool {
        matches!(self, Edge::Top | Edge::Bottom)
    }
}

/// The edges of this machine that border some peer.
///
/// A machine with a neighbour to the right and another below watches both,
/// and each edge leads somewhere different — which one is the business of
/// whoever routes the crossing, not of capture. Capture only needs to know
/// which edges to arm, and a set is what lets one mouse leave in more than one
/// direction. Never on the wire; a bitmask so a hook that may not lock can
/// read it from an atomic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct EdgeSet(u8);

impl EdgeSet {
    pub const EMPTY: EdgeSet = EdgeSet(0);

    fn bit(edge: Edge) -> u8 {
        match edge {
            Edge::Left => 1,
            Edge::Right => 2,
            Edge::Top => 4,
            Edge::Bottom => 8,
        }
    }

    pub fn with(self, edge: Edge) -> EdgeSet {
        EdgeSet(self.0 | Self::bit(edge))
    }

    pub fn contains(self, edge: Edge) -> bool {
        self.0 & Self::bit(edge) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The edges in a fixed order, so barriers are placed the same way each
    /// time and a log line reads the same for the same set.
    pub fn iter(self) -> impl Iterator<Item = Edge> {
        [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
            .into_iter()
            .filter(move |edge| self.contains(*edge))
    }

    /// The raw mask, for storing in an atomic.
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Back from [`Self::bits`]; bits no edge uses are dropped.
    pub fn from_bits(bits: u8) -> EdgeSet {
        EdgeSet(bits & 0b1111)
    }
}

impl FromIterator<Edge> for EdgeSet {
    fn from_iter<I: IntoIterator<Item = Edge>>(edges: I) -> Self {
        edges.into_iter().fold(EdgeSet::EMPTY, EdgeSet::with)
    }
}

#[cfg(test)]
mod position_tests {
    use super::*;

    #[test]
    fn a_machine_to_the_right_is_reached_rightward_and_answers_leftward() {
        let here = Position::new(0, 0);
        let there = Position::new(1, 0);
        assert_eq!(here.edge_toward(there), Some(Edge::Right));
        assert_eq!(there.edge_toward(here), Some(Edge::Left));
    }

    #[test]
    fn stacking_uses_the_vertical_edges() {
        let here = Position::new(0, 0);
        assert_eq!(here.edge_toward(Position::new(0, 1)), Some(Edge::Bottom));
        assert_eq!(here.edge_toward(Position::new(0, -1)), Some(Edge::Top));
    }

    /// A diagonal is not an edge a cursor can cross, so one axis has to win.
    #[test]
    fn a_diagonal_neighbour_is_reached_along_its_dominant_axis() {
        let here = Position::new(0, 0);
        assert_eq!(here.edge_toward(Position::new(3, 1)), Some(Edge::Right));
        assert_eq!(here.edge_toward(Position::new(1, 3)), Some(Edge::Bottom));
        // Machines stand side by side more often than stacked, so an exact tie
        // goes horizontal.
        assert_eq!(here.edge_toward(Position::new(1, 1)), Some(Edge::Right));
    }

    /// Two machines nobody has placed both sit at the default, and must not
    /// invent an edge between them.
    #[test]
    fn an_unplaced_pair_borders_nothing() {
        let unplaced = Position::default();
        assert_eq!(unplaced, Position::new(0, 0));
        assert_eq!(unplaced.edge_toward(unplaced), None);
    }

    /// The case a named side cannot express: with three in a row, the first
    /// machine's right-hand neighbour is the second, not the third.
    #[test]
    fn the_nearest_machine_in_a_row_is_the_neighbour() {
        let own = Position::new(0, 0);
        let peers = [
            ("middle", Position::new(1, 0)),
            ("far", Position::new(2, 0)),
        ];
        assert_eq!(neighbour_across(own, Edge::Right, &peers), Some("middle"));
    }

    /// And when the middle machine is not there, the layout heals rather than
    /// leaving a hole — which is what a designated centre could not do.
    #[test]
    fn a_missing_machine_hands_its_place_to_the_next_one() {
        let own = Position::new(0, 0);
        let peers = [("far", Position::new(2, 0))];
        assert_eq!(neighbour_across(own, Edge::Right, &peers), Some("far"));
    }

    #[test]
    fn nothing_in_a_direction_means_no_neighbour() {
        let own = Position::new(0, 0);
        let peers = [("right", Position::new(1, 0))];
        assert_eq!(neighbour_across(own, Edge::Left, &peers), None);
        assert_eq!(neighbour_across::<&str>(own, Edge::Right, &[]), None);
    }

    #[test]
    fn an_edge_set_holds_each_edge_once_and_survives_its_bits() {
        let set: EdgeSet = [Edge::Bottom, Edge::Right, Edge::Bottom]
            .into_iter()
            .collect();
        assert!(set.contains(Edge::Right) && set.contains(Edge::Bottom));
        assert!(!set.contains(Edge::Left) && !set.contains(Edge::Top));
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![Edge::Right, Edge::Bottom]
        );
        assert_eq!(EdgeSet::from_bits(set.bits()), set);
        assert!(EdgeSet::EMPTY.is_empty());
        assert_eq!(EdgeSet::from_bits(0xF0), EdgeSet::EMPTY);
    }
}
