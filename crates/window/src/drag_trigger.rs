//! Deciding when a drag becomes a transfer.
//!
//! The rule the user asked for: drag a window past the screen edge that faces
//! the peer, and it goes over. Drag it back, and it comes home. This is the
//! piece that says *when* — it owns no windows, talks to no OS, and sends
//! nothing, which is what makes it testable without a compositor, a peer, or a
//! second machine.
//!
//! ## Why the edge test lives here rather than reusing the cursor's
//!
//! Input sharing already hands the *cursor* over at the edge, via barriers the
//! compositor arms ([`EdgeCrossing`](cursio_input)). That signal is not the one
//! we want:
//!
//! - It fires on a bare cursor crossing, with no window attached. Dragging is
//!   the whole gesture here.
//! - On Wayland the barrier fires only while input capture is *armed*, which
//!   depends on session ownership state that has nothing to do with windows.
//! - A window is dragged by its titlebar, so the pointer reaches the edge while
//!   the window's body is still on screen — the interesting moment is about the
//!   pointer, but the decision is about the window.
//!
//! So the trigger watches pointer positions reported with the drag itself and
//! compares them to the screen bounds directly. It stays deliberately
//! independent of who owns the cursor.
//!
//! ## Hysteresis
//!
//! A window is handed over when the pointer reaches within [`EDGE_MARGIN`] of
//! the peer-facing edge, and only once per drag: without that latch, a pointer
//! resting on the boundary would toggle the transfer on every poll tick.

use cursio_protocol::Edge;

use crate::drag::{DragEvent, PointerPos};

/// How close to the edge counts as crossing it, in pixels.
///
/// Not zero: the pointer is clamped to `width - 1` by the compositor, and a
/// user shoving a window at the edge should not have to land on the exact last
/// column. Small enough that it can't fire in the middle of the screen.
pub const EDGE_MARGIN: i32 = 2;

/// The screen the drag is happening on, in the same coordinate space the drag
/// events report.
///
/// Carries an origin, not just a size. Both multi-monitor platforms report the
/// pointer in a desktop-wide space whose top-left is wherever the leftmost and
/// topmost displays put it: Windows' virtual screen starts at
/// `SM_XVIRTUALSCREEN`, macOS' display union at the minimum `CGDisplayBounds`
/// corner. A monitor placed left of or above the main one makes that origin
/// negative, and assuming `(0, 0)` then puts the left edge in the middle of the
/// desktop and the right edge past every pixel the pointer can reach — so
/// dragging a window at the edge fires either constantly or never.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenBounds {
    /// Leftmost pointer coordinate on the desktop.
    pub x: i32,
    /// Topmost pointer coordinate on the desktop.
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// What the trigger decided should happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerAction {
    /// Start transferring this window to the peer.
    SendToPeer { title: String },
    /// The user dragged the transferred window back; bring it home.
    BringBack,
}

/// Turns drag events into transfer decisions.
///
/// Fed every [`DragEvent`] from the platform detector; yields an action only at
/// the moments something should actually happen.
pub struct DragTrigger {
    /// Which of this machine's edges faces the peer. Without one, dragging a
    /// window off any edge means nothing and the trigger stays silent.
    peer_edge: Option<Edge>,
    screen: ScreenBounds,
    /// The window being dragged right now, if any.
    dragging: Option<String>,
    /// Whether the current drag has already fired. One transfer per drag.
    fired: bool,
    /// Whether a window is currently transferred to the peer, so a drag back
    /// across the edge is understood as a return rather than a second send.
    transferred: bool,
}

impl DragTrigger {
    pub fn new(peer_edge: Option<Edge>, screen: ScreenBounds) -> Self {
        Self {
            peer_edge,
            screen,
            dragging: None,
            fired: false,
            transferred: false,
        }
    }

    /// The screen geometry changed (resolution, monitor layout).
    pub fn set_screen(&mut self, screen: ScreenBounds) {
        self.screen = screen;
    }

    /// Tell the trigger a transfer ended by some other route — the peer closed
    /// the window, or the session dropped — so the next edge drag sends rather
    /// than trying to bring back a window that is already home.
    pub fn transfer_ended(&mut self) {
        self.transferred = false;
    }

    /// Feed one drag event; get back the action it implies, if any.
    pub fn on_drag(&mut self, event: &DragEvent) -> Option<TriggerAction> {
        match event {
            DragEvent::Started { title, at } => {
                self.dragging = Some(title.clone());
                self.fired = false;
                // A drag can start already at the edge (grabbing a window
                // that's flush against it), so test immediately.
                self.test(*at)
            }
            DragEvent::Moved { at } => self.test(*at),
            DragEvent::Ended { .. } => {
                self.dragging = None;
                self.fired = false;
                None
            }
        }
    }

    /// Has the pointer reached the peer-facing edge during a live drag?
    fn test(&mut self, at: PointerPos) -> Option<TriggerAction> {
        if self.fired {
            return None;
        }
        let title = self.dragging.clone()?;
        let edge = self.peer_edge?;

        if !at_edge(at, edge, self.screen) {
            return None;
        }

        self.fired = true;
        if self.transferred {
            self.transferred = false;
            Some(TriggerAction::BringBack)
        } else {
            self.transferred = true;
            Some(TriggerAction::SendToPeer { title })
        }
    }
}

/// Whether a pointer position is at `edge` of a screen this size.
fn at_edge(at: PointerPos, edge: Edge, screen: ScreenBounds) -> bool {
    match edge {
        Edge::Left => at.x <= screen.x + EDGE_MARGIN,
        Edge::Right => at.x >= screen.x + screen.width - 1 - EDGE_MARGIN,
        Edge::Top => at.y <= screen.y + EDGE_MARGIN,
        Edge::Bottom => at.y >= screen.y + screen.height - 1 - EDGE_MARGIN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: ScreenBounds = ScreenBounds {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    fn trigger(edge: Option<Edge>) -> DragTrigger {
        DragTrigger::new(edge, SCREEN)
    }

    fn start(x: i32, y: i32) -> DragEvent {
        DragEvent::Started {
            title: "Editor".into(),
            at: PointerPos { x, y },
        }
    }

    fn moved(x: i32, y: i32) -> DragEvent {
        DragEvent::Moved {
            at: PointerPos { x, y },
        }
    }

    fn ended(x: i32, y: i32) -> DragEvent {
        DragEvent::Ended {
            at: PointerPos { x, y },
        }
    }

    #[test]
    fn dragging_to_the_peer_edge_sends_the_window() {
        let mut t = trigger(Some(Edge::Right));
        assert_eq!(t.on_drag(&start(960, 540)), None);
        assert_eq!(t.on_drag(&moved(1500, 540)), None);
        assert_eq!(
            t.on_drag(&moved(1919, 540)),
            Some(TriggerAction::SendToPeer {
                title: "Editor".into()
            })
        );
    }

    #[test]
    fn a_desktop_starting_left_of_zero_still_has_both_its_edges() {
        // A second monitor to the left of the primary one: the pointer runs
        // from -1920 to 1919, so neither edge is where a bare width/height
        // would put it.
        const OFFSET: ScreenBounds = ScreenBounds {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };

        let mut right = DragTrigger::new(Some(Edge::Right), OFFSET);
        assert_eq!(right.on_drag(&start(0, 540)), None);
        assert_eq!(
            right.on_drag(&moved(1919, 540)),
            Some(TriggerAction::SendToPeer {
                title: "Editor".into()
            })
        );

        let mut left = DragTrigger::new(Some(Edge::Left), OFFSET);
        assert_eq!(left.on_drag(&start(0, 540)), None);
        assert_eq!(
            left.on_drag(&moved(-1920, 540)),
            Some(TriggerAction::SendToPeer {
                title: "Editor".into()
            })
        );
    }

    #[test]
    fn a_drag_fires_only_once_however_long_it_lingers() {
        // Without the latch, every poll tick at the edge would re-fire —
        // toggling the transfer dozens of times a second.
        let mut t = trigger(Some(Edge::Right));
        t.on_drag(&start(960, 540));
        assert!(t.on_drag(&moved(1919, 540)).is_some());
        assert_eq!(t.on_drag(&moved(1919, 541)), None);
        assert_eq!(t.on_drag(&moved(1918, 542)), None);
    }

    #[test]
    fn the_opposite_edge_does_nothing() {
        // The left edge has no peer beyond it: the window just stops there.
        let mut t = trigger(Some(Edge::Right));
        t.on_drag(&start(960, 540));
        assert_eq!(t.on_drag(&moved(0, 540)), None);
    }

    #[test]
    fn without_a_peer_edge_no_drag_ever_transfers() {
        let mut t = trigger(None);
        t.on_drag(&start(960, 540));
        assert_eq!(t.on_drag(&moved(1919, 540)), None);
        assert_eq!(t.on_drag(&moved(0, 0)), None);
    }

    #[test]
    fn dragging_back_across_the_edge_returns_the_window() {
        let mut t = trigger(Some(Edge::Right));
        t.on_drag(&start(960, 540));
        assert!(matches!(
            t.on_drag(&moved(1919, 540)),
            Some(TriggerAction::SendToPeer { .. })
        ));
        t.on_drag(&ended(1919, 540));

        // A second drag to the same edge means "bring it home", not "send
        // another copy".
        t.on_drag(&start(960, 540));
        assert_eq!(t.on_drag(&moved(1919, 540)), Some(TriggerAction::BringBack));
    }

    #[test]
    fn a_transfer_ending_elsewhere_resets_the_direction() {
        // The peer closed the window; the next edge drag must send again
        // rather than try to bring back something already home.
        let mut t = trigger(Some(Edge::Right));
        t.on_drag(&start(960, 540));
        t.on_drag(&moved(1919, 540));
        t.on_drag(&ended(1919, 540));

        t.transfer_ended();

        t.on_drag(&start(960, 540));
        assert!(matches!(
            t.on_drag(&moved(1919, 540)),
            Some(TriggerAction::SendToPeer { .. })
        ));
    }

    #[test]
    fn pointer_motion_without_a_drag_is_ignored() {
        // Moves arrive only during a drag, but a detector that misbehaves must
        // not be able to transfer a window nobody grabbed.
        let mut t = trigger(Some(Edge::Right));
        assert_eq!(t.on_drag(&moved(1919, 540)), None);
    }

    #[test]
    fn grabbing_a_window_already_at_the_edge_fires_immediately() {
        let mut t = trigger(Some(Edge::Right));
        assert!(t.on_drag(&start(1919, 540)).is_some());
    }

    #[test]
    fn every_edge_is_detected_at_its_own_boundary() {
        for (edge, at) in [
            (Edge::Left, PointerPos { x: 0, y: 540 }),
            (Edge::Right, PointerPos { x: 1919, y: 540 }),
            (Edge::Top, PointerPos { x: 960, y: 0 }),
            (Edge::Bottom, PointerPos { x: 960, y: 1079 }),
        ] {
            assert!(at_edge(at, edge, SCREEN), "{edge:?} at {at:?}");
            // The screen's middle is never an edge.
            assert!(!at_edge(PointerPos { x: 960, y: 540 }, edge, SCREEN));
        }
    }
}
