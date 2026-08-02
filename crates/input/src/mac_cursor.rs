//! macOS cursor hiding, via CoreGraphics — and parking it when hiding fails.
//!
//! When this Mac hands control to a peer its own pointer should disappear,
//! rather than sitting frozen on screen next to a cursor the user is actually
//! moving somewhere else.
//!
//! ## Hiding alone does not survive
//!
//! `CGDisplayHideCursor` hides the pointer for the calling application's
//! session, and macOS restores it whenever another application comes forward —
//! there is no supported way for a background process to hide the system
//! cursor outright. Run from a terminal it works while that terminal is
//! frontmost and quietly stops the moment the user clicks elsewhere, which for
//! a daemon is nearly always. The documented alternatives are private
//! window-server calls, and a background process that could hide the pointer
//! unconditionally is also one that could hide it permanently after a crash.
//!
//! ## So it is parked as well
//!
//! Hiding is asked for and not relied on: the pointer is *also* warped into a
//! screen corner, where a stale arrow is out of the way whether or not the
//! window server undid the hide a second later. This is what Deskflow does on
//! this platform, for the same reason.
//!
//! Two details make parking safe rather than merely tidy:
//!
//! - **The corner is the one away from the peer.** Parking on the edge that
//!   borders the peer would leave the pointer sitting on the very line a
//!   crossing is detected at.
//! - **Coming back lands short of that edge.** The pointer is restored to where
//!   the user left it, pulled [`EDGE_INSET`] inward from the peer-facing edge —
//!   otherwise taking the cursor back would put it straight onto the edge it
//!   just crossed, and the next small movement would read as a fresh crossing.
//!   The Wayland backend solves the same problem the same way (`nudge_off_edge`
//!   in [`crate::wl_capture`], after Deskflow's `avoidJumpZone`).
//!
//! Linux cannot do either half from a background process; see [`LinuxCursor`].
//!
//! [`LinuxCursor`]: crate::LinuxCursor

use crate::mac::{current_pointer_position, Bounds, Point};
use crate::{CursorControl, InputError};

/// How far inside the peer-facing edge the pointer is put back.
///
/// The same handful of pixels the Wayland backend nudges by: enough to be off
/// the edge, close enough that the cursor is visually where it was left.
const EDGE_INSET: f64 = 12.0;

/// Hides, parks, and restores this Mac's pointer.
pub struct MacCursor {
    /// Whether we currently hold a hide. The calls are reference-counted, so a
    /// second hide would need a second show — tracking it keeps both operations
    /// idempotent, as the trait requires.
    hidden: bool,
    /// Where the pointer was when it was parked, so it can be put back roughly
    /// where the user left it rather than in the corner it was stored in.
    resume_at: Option<Point>,
}

impl MacCursor {
    pub fn new() -> Result<Self, InputError> {
        Ok(Self {
            hidden: false,
            resume_at: None,
        })
    }
}

impl CursorControl for MacCursor {
    fn hide(&mut self) -> Result<(), InputError> {
        if self.hidden {
            return Ok(());
        }
        self.resume_at = Some(current_pointer_position());
        // Hide first, then move: the other order flies a visible pointer across
        // the screen on every handoff.
        //
        // SAFETY: the display id is a documented constant meaning "every
        // display"; the call takes no pointers.
        let status = unsafe { CGDisplayHideCursor(NULL_DISPLAY) };
        warp_to(parking_spot(Bounds::of_all_displays(), crate::peer_edge()));
        if status != 0 {
            return Err(InputError::Unavailable(format!(
                "macOS refused to hide the cursor (CGError {status}); it is parked in a corner \
                 instead"
            )));
        }
        self.hidden = true;
        Ok(())
    }

    fn show(&mut self) -> Result<(), InputError> {
        if !self.hidden {
            return Ok(());
        }
        // Cleared first: if the call fails there is nothing to retry — the
        // pointer is either back or the window server already restored it —
        // and staying "hidden" would make every later show a no-op.
        self.hidden = false;
        // Put it back before it becomes visible, so the user does not watch it
        // travel from the corner.
        if let Some(point) = self.resume_at.take() {
            let bounds = Bounds::of_all_displays();
            warp_to(bounds.clamp(off_edge(point, bounds, crate::peer_edge(), EDGE_INSET)));
        }
        // SAFETY: as above.
        let status = unsafe { CGDisplayShowCursor(NULL_DISPLAY) };
        if status != 0 {
            return Err(InputError::Unavailable(format!(
                "macOS refused to restore the cursor (CGError {status})"
            )));
        }
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "coregraphics"
    }
}

/// Where to leave the pointer while the peer has the cursor.
///
/// The corner furthest from `peer_edge`, so a parked pointer is never sitting
/// on the line a crossing is measured at. With no peer edge known yet, the
/// bottom-right corner — the conventional out-of-the-way spot, and the one
/// least likely to sit under a menu bar or a dock.
fn parking_spot(bounds: Bounds, peer_edge: Option<cursio_protocol::Edge>) -> Point {
    // A pixel inside, because the far edges are exclusive: a point exactly at
    // `max_x` is on the next screen, or on none.
    let (left, right) = (bounds.min_x, bounds.max_x - 1.0);
    let (top, bottom) = (bounds.min_y, bounds.max_y - 1.0);
    match peer_edge {
        Some(cursio_protocol::Edge::Right) => Point { x: left, y: bottom },
        Some(cursio_protocol::Edge::Left) => Point {
            x: right,
            y: bottom,
        },
        Some(cursio_protocol::Edge::Bottom) => Point { x: right, y: top },
        Some(cursio_protocol::Edge::Top) | None => Point {
            x: right,
            y: bottom,
        },
    }
}

/// Move `point` `inset` away from `peer_edge`, if it is within `inset` of it.
///
/// Only the one axis moves, and only when the point is actually close to the
/// edge — restoring a pointer that was nowhere near it must not shift it.
fn off_edge(
    point: Point,
    bounds: Bounds,
    peer_edge: Option<cursio_protocol::Edge>,
    inset: f64,
) -> Point {
    let Some(edge) = peer_edge else {
        return point;
    };
    match edge {
        cursio_protocol::Edge::Left => Point {
            x: point.x.max(bounds.min_x + inset),
            y: point.y,
        },
        cursio_protocol::Edge::Right => Point {
            x: point.x.min(bounds.max_x - inset),
            y: point.y,
        },
        cursio_protocol::Edge::Top => Point {
            x: point.x,
            y: point.y.max(bounds.min_y + inset),
        },
        cursio_protocol::Edge::Bottom => Point {
            x: point.x,
            y: point.y.min(bounds.max_y - inset),
        },
    }
}

/// Move the pointer without generating an event.
///
/// A warp, not a posted mouse event: the injector's shadow cursor and the
/// capture tap both watch real events, and a synthetic move here would look to
/// them like the user had done it.
fn warp_to(point: Point) {
    // SAFETY: takes a plain struct by value and no pointers. A failure means
    // the pointer stayed where it was, which is the pre-existing behaviour.
    let status = unsafe { CGWarpMouseCursorPosition(point) };
    if status != 0 {
        tracing::debug!(status, "could not move the pointer out of the way");
    }
}

/// `kCGNullDirectDisplay`: apply to the whole session rather than one screen.
const NULL_DISPLAY: u32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn CGWarpMouseCursorPosition(point: Point) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::Edge;

    fn screen() -> Bounds {
        Bounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1920.0,
            max_y: 1080.0,
        }
    }

    /// The calls are reference-counted, so an unbalanced pair would leave the
    /// pointer invisible for the rest of the login session — which no amount of
    /// restarting Cursio would fix.
    #[test]
    fn hiding_twice_still_takes_one_show_to_undo() {
        let mut cursor = MacCursor::new().unwrap();
        assert!(!cursor.hidden);
        // The real calls are skipped here — a test must not hide or move the
        // pointer of whoever is running it — so the bookkeeping is what is
        // checked.
        cursor.hidden = true;
        assert!(cursor.hide().is_ok());
        assert!(cursor.hidden, "a second hide must not stack");
        cursor.hidden = false;
        assert!(cursor.show().is_ok());
        assert!(!cursor.hidden, "showing when visible must stay a no-op");
    }

    /// Parking on the peer-facing edge would leave the pointer sitting on the
    /// line a crossing is measured at, which is the one place it must not be.
    #[test]
    fn the_pointer_parks_away_from_the_edge_that_borders_the_peer() {
        assert_eq!(parking_spot(screen(), Some(Edge::Right)).x, 0.0);
        assert_eq!(parking_spot(screen(), Some(Edge::Left)).x, 1919.0);
        assert_eq!(parking_spot(screen(), Some(Edge::Bottom)).y, 0.0);
    }

    /// Taking the cursor back must not put it straight onto the edge it just
    /// crossed: the next small movement would read as another crossing, and the
    /// cursor would bounce back to the peer on its own.
    #[test]
    fn coming_back_lands_short_of_the_edge_it_left_by() {
        let at_edge = Point {
            x: 1919.0,
            y: 500.0,
        };
        let back = off_edge(at_edge, screen(), Some(Edge::Right), EDGE_INSET);
        assert_eq!(back.x, 1920.0 - EDGE_INSET);
        assert_eq!(back.y, 500.0, "the other axis must not move");
    }

    /// A pointer that was nowhere near the edge is put back exactly where the
    /// user left it — the inset is a floor, not a shove.
    #[test]
    fn a_pointer_away_from_the_edge_is_restored_untouched() {
        let middle = Point { x: 900.0, y: 500.0 };
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let back = off_edge(middle, screen(), Some(edge), EDGE_INSET);
            assert_eq!((back.x, back.y), (900.0, 500.0), "moved for {edge:?}");
        }
    }

    /// Before any peer has greeted us there is no edge to avoid, and moving the
    /// pointer on that basis would be a guess.
    #[test]
    fn without_a_peer_edge_nothing_is_inset() {
        let at_edge = Point {
            x: 1919.0,
            y: 500.0,
        };
        let back = off_edge(at_edge, screen(), None, EDGE_INSET);
        assert_eq!(back.x, 1919.0);
    }
}
