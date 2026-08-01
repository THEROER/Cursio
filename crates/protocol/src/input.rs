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

    /// Scroll wheel motion. `dx` = horizontal, `dy` = vertical; units are
    /// notches (positive y = scroll up), matching evdev `REL_WHEEL`.
    PointerScroll { dx: i32, dy: i32 },

    /// A keyboard key changed state. `key` is a platform-neutral Linux evdev
    /// keycode (see `KEY_*` in `linux/input-event-codes.h`); both sides agree
    /// on this namespace and map to/from their own platform as needed.
    Key { key: u16, pressed: bool },
}

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

/// Where a machine sits in the user's physical layout.
///
/// Declared per machine instead of per link, which is what makes more than two
/// of them workable. `--peer-edge right` answers "which way is the peer", and
/// that question has one answer only while there is one peer: a third machine
/// needs every node to know where every *other* node is, and asking the user to
/// spell out each pair is O(n²) configuration for a layout they can describe in
/// one word each.
///
/// So the layout is a hub and its spokes: one machine is [`Position::Center`]
/// — the one whose keyboard the user actually sits at — and every other names
/// the side of it that it stands on. Adding a machine is then one flag on that
/// machine and nothing at all on the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Position {
    /// The hub. The machine the user sits at; every other borders it.
    #[default]
    Center,
    /// To the left of the centre.
    Left,
    /// To the right of the centre.
    Right,
    /// Above the centre.
    Above,
    /// Below the centre.
    Below,
}

impl Position {
    /// Which of *this* machine's edges borders `peer`, if the two are
    /// neighbours at all.
    ///
    /// `None` when neither is the centre: two spokes do not touch, so a cursor
    /// travels between them through the hub rather than teleporting. Saying so
    /// is better than inventing an adjacency, which would let the cursor leave
    /// a machine across an edge the user has nothing standing at.
    pub fn edge_toward(self, peer: Position) -> Option<Edge> {
        match (self, peer) {
            // The same side twice is one machine described twice, or two
            // stacked in a spot the layout cannot tell apart.
            (a, b) if a == b => None,
            (Position::Center, other) => other.edge_from_center(),
            (own, Position::Center) => own.edge_from_center().map(Edge::opposite),
            _ => None,
        }
    }

    /// The one edge this machine can hand the cursor across, when that is not
    /// ambiguous.
    ///
    /// A spoke has exactly one neighbour — the hub — so it needs no
    /// configuration at all beyond naming its own side. The hub borders every
    /// spoke, so which of its edges to watch is a question only the set of
    /// connected peers can answer, and `None` says so rather than guessing.
    pub fn own_edge(self) -> Option<Edge> {
        self.edge_toward(Position::Center)
    }

    /// The centre's edge that faces this position.
    fn edge_from_center(self) -> Option<Edge> {
        match self {
            Position::Center => None,
            Position::Left => Some(Edge::Left),
            Position::Right => Some(Edge::Right),
            Position::Above => Some(Edge::Top),
            Position::Below => Some(Edge::Bottom),
        }
    }
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

#[cfg(test)]
mod position_tests {
    use super::*;

    /// The layout the flag exists for: the user sits at the centre, the Mac
    /// stands to its right, and the cursor leaves rightward and comes back
    /// leftward.
    #[test]
    fn the_centre_and_a_spoke_face_each_other() {
        assert_eq!(
            Position::Center.edge_toward(Position::Right),
            Some(Edge::Right)
        );
        assert_eq!(
            Position::Right.edge_toward(Position::Center),
            Some(Edge::Left)
        );
    }

    #[test]
    fn every_side_of_the_centre_has_an_edge() {
        for (position, edge) in [
            (Position::Left, Edge::Left),
            (Position::Right, Edge::Right),
            (Position::Above, Edge::Top),
            (Position::Below, Edge::Bottom),
        ] {
            assert_eq!(Position::Center.edge_toward(position), Some(edge));
            assert_eq!(
                position.edge_toward(Position::Center),
                Some(edge.opposite())
            );
        }
    }

    /// Two spokes do not touch: the cursor travels between them through the
    /// hub. Inventing an adjacency here would let it leave across an edge the
    /// user has nothing standing at.
    #[test]
    fn two_machines_on_opposite_sides_do_not_border_each_other() {
        assert_eq!(Position::Left.edge_toward(Position::Right), None);
        assert_eq!(Position::Above.edge_toward(Position::Left), None);
    }

    /// A spoke needs no configuration beyond its own side; the hub cannot know
    /// which edge to watch until it knows who is connected.
    #[test]
    fn a_spoke_knows_its_own_edge_and_the_hub_does_not() {
        assert_eq!(Position::Right.own_edge(), Some(Edge::Left));
        assert_eq!(Position::Below.own_edge(), Some(Edge::Top));
        assert_eq!(Position::Center.own_edge(), None);
    }

    /// Two machines claiming one spot cannot be told apart, so neither gets an
    /// edge rather than both getting the same one.
    #[test]
    fn a_position_does_not_border_itself() {
        for position in [
            Position::Center,
            Position::Left,
            Position::Right,
            Position::Above,
            Position::Below,
        ] {
            assert_eq!(position.edge_toward(position), None);
        }
    }
}
