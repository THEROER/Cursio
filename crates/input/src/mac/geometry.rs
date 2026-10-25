//! Where the pointer is and what it may reach: the union of every display, in
//! the global point space CoreGraphics posts events in.

use crate::mac::CFRelease;
use crate::mac::CGDisplayBounds;
use crate::mac::CGEventCreate;
use crate::mac::CGEventGetLocation;
use crate::mac::CGGetActiveDisplayList;

/// Where the cursor is right now, according to the window server.
///
/// A null event source is valid here and means "whatever the system state is",
/// which is exactly the question being asked.
pub(crate) fn current_pointer_position() -> Point {
    // SAFETY: CGEventCreate accepts a null source; the returned event is
    // released before the point (a plain copy) is returned.
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return Point { x: 0.0, y: 0.0 };
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        point
    }
}

/// The area the pointer may occupy: every display's bounds, unioned.
///
/// A union rather than per-display containment, deliberately. Checking which
/// display a point belongs to would trap the pointer on whichever screen it
/// started on; the union lets it cross between them, and the small dead
/// rectangles a stepped arrangement leaves over are the same ones macOS itself
/// refuses to put the cursor in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    pub(crate) min_x: f64,
    pub(crate) min_y: f64,
    pub(crate) max_x: f64,
    pub(crate) max_y: f64,
}

impl Bounds {
    pub(crate) fn of_all_displays() -> Self {
        let mut ids = [0_u32; 16];
        let mut count = 0_u32;
        // SAFETY: the buffer holds `ids.len()` entries and the count is written
        // to a valid local.
        let status =
            unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
        if status != 0 || count == 0 {
            // Better to leave the shadow unbounded than to trap the pointer in
            // a rectangle guessed from nothing.
            return Self {
                min_x: f64::MIN,
                min_y: f64::MIN,
                max_x: f64::MAX,
                max_y: f64::MAX,
            };
        }
        let mut union: Option<Self> = None;
        for id in &ids[..count as usize] {
            // SAFETY: `id` came from CGGetActiveDisplayList.
            let rect = unsafe { CGDisplayBounds(*id) };
            let next = Self {
                min_x: rect.origin.x,
                min_y: rect.origin.y,
                max_x: rect.origin.x + rect.size.width,
                max_y: rect.origin.y + rect.size.height,
            };
            union = Some(match union {
                None => next,
                Some(all) => Self {
                    min_x: all.min_x.min(next.min_x),
                    min_y: all.min_y.min(next.min_y),
                    max_x: all.max_x.max(next.max_x),
                    max_y: all.max_y.max(next.max_y),
                },
            });
        }
        union.expect("count was non-zero")
    }

    /// Hold a point inside the displays. The one-pixel inset keeps the pointer
    /// addressable: a point exactly on `max_x` is outside the last column.
    pub(crate) fn clamp(self, point: Point) -> Point {
        Point {
            x: point
                .x
                .clamp(self.min_x, (self.max_x - 1.0).max(self.min_x)),
            y: point
                .y
                .clamp(self.min_y, (self.max_y - 1.0).max(self.min_y)),
        }
    }
}

/// `CGSize` and `CGRect`, as CoreGraphics lays them out.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct Size {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct Rect {
    origin: Point,
    size: Size,
}

/// `CGPoint`: two doubles, in global display coordinates with the origin at the
/// top-left of the main display.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Point {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Bounds {
        Bounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1512.0,
            max_y: 982.0,
        }
    }

    /// The bug this exists for: without clamping the shadow walked to x=6595 on
    /// a 1512-wide screen, and every later move went nowhere until it had
    /// walked all the way back.
    #[test]
    fn the_shadow_cursor_cannot_walk_off_the_screen() {
        let far = screen().clamp(Point {
            x: 6595.0,
            y: 400.0,
        });
        assert_eq!(far.x, 1511.0);
        let negative = screen().clamp(Point {
            x: -1571.0,
            y: 400.0,
        });
        assert_eq!(negative.x, 0.0);
    }

    /// The last column and row must stay reachable, or the pointer can never
    /// touch a menu bar corner.
    #[test]
    fn the_far_edge_is_still_addressable() {
        let corner = screen().clamp(Point {
            x: 99_999.0,
            y: 99_999.0,
        });
        assert_eq!((corner.x, corner.y), (1511.0, 981.0));
    }

    #[test]
    fn a_point_already_on_screen_is_left_alone() {
        let inside = Point { x: 700.0, y: 300.0 };
        let clamped = screen().clamp(inside);
        assert_eq!((clamped.x, clamped.y), (inside.x, inside.y));
    }
}
