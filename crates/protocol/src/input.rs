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
