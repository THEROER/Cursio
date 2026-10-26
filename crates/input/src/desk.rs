//! A desktop of several monitors, as the capture backends that see each one
//! measure it: which stretch of a monitor's edge faces off the desktop, and
//! where a pointer coming in across one belongs.
//!
//! Wayland places barriers by these rules and Windows checks its hook's
//! positions against them, so a crossing means the same edge on both: the
//! side of *a monitor* that no other monitor continues, not the side of the
//! box around them all. With a short monitor beside a tall one the box's
//! bottom is not where the short one ends, and a peer under the short one was
//! out of reach.

use cursio_protocol::Edge;

/// A monitor as `(x, y, width, height)` in the backend's own pixels.
pub(crate) type Rect = (i32, i32, i32, i32);

/// Whether the pixel `along` the `edge` of `rect` faces off the desktop: no
/// other monitor starts flush against that edge and covers that pixel.
/// Wayland asks the compositor instead, through which barriers it accepts.
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn outer_at(rects: &[Rect], rect: Rect, edge: Edge, along: i32) -> bool {
    let (x, y, w, h) = rect;
    !rects.iter().any(|&(ox, oy, ow, oh)| {
        (ox, oy, ow, oh) != rect
            && match edge {
                Edge::Left => ox + ow == x && (oy..oy + oh).contains(&along),
                Edge::Right => ox == x + w && (oy..oy + oh).contains(&along),
                Edge::Top => oy + oh == y && (ox..ox + ow).contains(&along),
                Edge::Bottom => oy == y + h && (ox..ox + ow).contains(&along),
            }
    })
}

/// The monitor holding `point`, else the one nearest it.
pub(crate) fn monitor_at(rects: &[Rect], point: (i32, i32)) -> Option<Rect> {
    let distance = |&(x, y, w, h): &Rect| {
        let dx = (x - point.0).max(point.0 - (x + w - 1)).max(0);
        let dy = (y - point.1).max(point.1 - (y + h - 1)).max(0);
        i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
    };
    rects
        .iter()
        .filter(|&&(_, _, w, h)| w > 0 && h > 0)
        .min_by_key(|r| distance(r))
        .copied()
}

/// The box around every monitor, as a [`Rect`].
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn bounds(rects: &[Rect]) -> Option<Rect> {
    rects
        .iter()
        .filter(|&&(_, _, w, h)| w > 0 && h > 0)
        .map(|&(x, y, w, h)| (x, y, x + w, y + h))
        .reduce(|(a, b, c, d), (e, f, g, h)| (a.min(e), b.min(f), c.max(g), d.max(h)))
        .map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0))
}

/// Where a pointer coming in across `edge` at `at` belongs: on the monitor
/// there, `inset` pixels inside that edge so the barrier or jump zone does
/// not send it straight back. The peer worked `at` out from the layout, so it
/// lies on the edge already; the inset is the only adjustment.
pub(crate) fn entry_at(
    rects: &[Rect],
    edge: Edge,
    at: (i32, i32),
    inset: i32,
) -> Option<(i32, i32)> {
    let (x, y, w, h) = monitor_at(rects, at)?;
    let px = at.0.clamp(x, x + w - 1);
    let py = at.1.clamp(y, y + h - 1);
    let ix = inset.min(w - 1);
    let iy = inset.min(h - 1);
    Some(match edge {
        Edge::Left => (x + ix, py),
        Edge::Right => (x + w - 1 - ix, py),
        Edge::Top => (px, y + iy),
        Edge::Bottom => (px, y + h - 1 - iy),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1440-tall monitor with a 1080-tall one to its right, tops aligned.
    const STEPPED: [Rect; 2] = [(0, 0, 2560, 1440), (2560, 0, 1920, 1080)];

    #[test]
    fn a_short_monitors_bottom_faces_off_the_desktop_though_the_box_goes_lower() {
        let short = STEPPED[1];
        assert!(outer_at(&STEPPED, short, Edge::Bottom, 3000));
        assert_eq!(bounds(&STEPPED), Some((0, 0, 4480, 1440)));
    }

    #[test]
    fn the_seam_between_two_monitors_is_not_an_outer_edge() {
        let tall = STEPPED[0];
        assert!(!outer_at(&STEPPED, tall, Edge::Right, 500));
        // Below the short monitor the tall one's right edge is exposed.
        assert!(outer_at(&STEPPED, tall, Edge::Right, 1200));
    }

    #[test]
    fn a_pointer_coming_in_is_placed_on_its_monitor_just_inside_the_edge() {
        assert_eq!(
            entry_at(&STEPPED, Edge::Bottom, (3000, 1079), 12),
            Some((3000, 1067))
        );
        assert_eq!(
            entry_at(&STEPPED, Edge::Left, (0, 700), 12),
            Some((12, 700))
        );
        // A point a pixel off the desktop still lands on the nearest monitor.
        assert_eq!(
            entry_at(&STEPPED, Edge::Right, (4480, 2000), 12),
            Some((4467, 1079))
        );
    }

    #[test]
    fn no_monitors_place_nothing() {
        assert_eq!(entry_at(&[], Edge::Left, (0, 0), 12), None);
        assert_eq!(bounds(&[]), None);
    }
}
