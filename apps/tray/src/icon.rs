//! The tray icon, drawn rather than shipped.
//!
//! A PNG in the repository would be a blob nobody reviews and every platform
//! wants at a different size — the notification area, the menu bar and a
//! StatusNotifierItem all ask for their own. Generating it costs a few hundred
//! microseconds once at startup and gives every size for free, at whatever the
//! display's scale is.
//!
//! The mark is two screens with a pointer crossing between them: the screens
//! because that is what Cursio joins, the pointer because moving it across
//! them is what the thing does. A bare arrow was the first attempt and read
//! badly — on a menu bar it is indistinguishable from the real cursor sitting
//! on top of it, and it says nothing about there being a second machine.
//!
//! Everything is drawn by asking, for each pixel, how far inside the shape it
//! is — a signed distance — and using that as coverage. That is what makes the
//! edges smooth at any size without a rasterizer, and it is why the shapes are
//! described as distances below rather than as spans to fill.

/// One drawn icon: RGBA8, straight (not premultiplied) alpha, top row first.
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Draw the icon at `size` pixels square.
pub fn draw(size: u32) -> Icon {
    let mut rgba = vec![0_u8; (size * size * 4) as usize];
    let scale = size as f32;

    for y in 0..size {
        for x in 0..size {
            // Sample at the pixel's centre, in a 0..1 space, so the same
            // geometry works at every size.
            let px = (x as f32 + 0.5) / scale;
            let py = (y as f32 + 0.5) / scale;

            let badge = coverage(rounded_square(px, py), scale);
            let arrow = coverage(mark(px, py), scale);
            if badge <= 0.0 {
                continue;
            }

            // The pointer is punched out of the badge rather than drawn over
            // it: a light arrow on a dark badge stays legible against both a
            // light and a dark menu bar, where a coloured one would vanish
            // into one of them.
            let lit = badge * (1.0 - arrow);
            let (r, g, b) = BADGE_COLOUR;
            let (ar, ag, ab) = ARROW_COLOUR;
            let mix = |dark: u8, light: u8| {
                (f32::from(dark) * (1.0 - arrow) + f32::from(light) * arrow).round() as u8
            };
            let index = ((y * size + x) * 4) as usize;
            rgba[index] = mix(r, ar);
            rgba[index + 1] = mix(g, ag);
            rgba[index + 2] = mix(b, ab);
            rgba[index + 3] = (badge.max(lit) * 255.0).round() as u8;
        }
    }

    Icon {
        width: size,
        height: size,
        rgba,
    }
}

/// Cursio's badge: near-black, so the mark reads as a shape rather than as a
/// colour somebody chose.
const BADGE_COLOUR: (u8, u8, u8) = (0x1c, 0x1c, 0x1e);
/// The pointer punched out of it.
const ARROW_COLOUR: (u8, u8, u8) = (0xf5, 0xf5, 0xf7);

/// Turn a signed distance into pixel coverage.
///
/// Distances are in the same 0..1 space as the geometry, so the width of the
/// transition has to be one pixel *at this size* — hence dividing by the
/// scale. A fixed width would be a hard edge on a large icon and a smear on a
/// small one.
fn coverage(distance: f32, scale: f32) -> f32 {
    let edge = 1.0 / scale;
    (0.5 - distance / edge).clamp(0.0, 1.0)
}

/// Signed distance to a rounded square filling most of the tile.
///
/// Negative inside. Inset from the edges because every platform crops or pads
/// a tray icon slightly differently, and a shape touching the boundary is the
/// one that gets clipped.
fn rounded_square(x: f32, y: f32) -> f32 {
    const INSET: f32 = 0.08;
    const RADIUS: f32 = 0.22;

    let half = 0.5 - INSET - RADIUS;
    let dx = (x - 0.5).abs() - half;
    let dy = (y - 0.5).abs() - half;
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    outside + dx.max(dy).min(0.0) - RADIUS
}

/// Signed distance to the whole mark: two screens and the pointer between
/// them.
///
/// Unioned rather than drawn in layers, because everything is punched out of
/// the badge in one pass — a shape that overlapped another would otherwise
/// show a seam where the two coverages met.
fn mark(x: f32, y: f32) -> f32 {
    /// How much dark to leave between the pointer and anything it crosses.
    const GAP: f32 = 0.028;

    let screens = screen_outline(x, y, 0.135, 0.285, 0.27, 0.22)
        .min(screen_outline(x, y, 0.595, 0.285, 0.27, 0.22));
    // Between the two, overlapping both: a pointer parked on one screen says
    // "there is a cursor", a pointer spanning the gap says it moves across.
    let arrow = pointer(x, y, 0.395, 0.375, 0.265);

    // Cut a gap out of the screens around the pointer. Both are punched out of
    // the badge in the same light colour, so where they touched they merged
    // into one blob and the arrow stopped being an arrow.
    let separated = screens.max(-(arrow - GAP));
    separated.min(arrow)
}

/// A screen: a rounded rectangle outline with a stand under it.
///
/// An outline rather than a filled block, so the pointer resting on one is
/// still visible against it at menu-bar sizes.
fn screen_outline(x: f32, y: f32, left: f32, top: f32, width: f32, height: f32) -> f32 {
    const STROKE: f32 = 0.030;

    let outer = rounded_rect(x, y, left, top, width, height, 0.035);
    // Hollowed by subtracting a smaller copy: the inside of a stroked shape is
    // the outside of the shape one stroke in.
    let inner = rounded_rect(
        x,
        y,
        left + STROKE,
        top + STROKE,
        width - STROKE * 2.0,
        height - STROKE * 2.0,
        0.020,
    );
    let frame = outer.max(-inner);

    // The stand, centred under the screen. Short and wide, which is what makes
    // the rectangle above it read as a display rather than as a window.
    let stand = rounded_rect(
        x,
        y,
        left + width * 0.5 - 0.045,
        top + height,
        0.090,
        0.045,
        0.012,
    );
    frame.min(stand)
}

/// Signed distance to a rounded rectangle given by its top-left corner.
fn rounded_rect(x: f32, y: f32, left: f32, top: f32, width: f32, height: f32, radius: f32) -> f32 {
    let centre_x = left + width * 0.5;
    let centre_y = top + height * 0.5;
    let dx = (x - centre_x).abs() - (width * 0.5 - radius);
    let dy = (y - centre_y).abs() - (height * 0.5 - radius);
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    outside + dx.max(dy).min(0.0) - radius
}

/// Signed distance to a pointer whose tip is at `(tip_x, tip_y)`, `size` tall.
///
/// Wound clockwise in screen coordinates — y grows downward — so all three edge
/// distances are negative inside and `max` gives a usable distance. Wound the
/// other way every edge flips sign and the head silently vanishes.
fn pointer(x: f32, y: f32, tip_x: f32, tip_y: f32, size: f32) -> f32 {
    let tip = (tip_x, tip_y);
    let left = (tip_x, tip_y + size);
    let right = (tip_x + size * 0.66, tip_y + size * 0.60);
    let head = triangle(x, y, tip, right, left);

    let tail = bar(
        x,
        y,
        (tip_x + size * 0.34, tip_y + size * 0.62),
        (tip_x + size * 0.52, tip_y + size * 0.98),
        size * 0.115,
    );
    head.min(tail)
}

/// Signed distance to a triangle, negative inside.
fn triangle(x: f32, y: f32, a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
    // Inside means on the same side of all three edges. Taking the largest of
    // the three signed edge distances gives exactly that, and outside it is a
    // close-enough approximation for a one-pixel transition.
    let e0 = edge_distance(x, y, a, b);
    let e1 = edge_distance(x, y, b, c);
    let e2 = edge_distance(x, y, c, a);
    e0.max(e1).max(e2)
}

/// Signed distance from a point to the line through `a` and `b`, positive on
/// the right-hand side of `a -> b`.
fn edge_distance(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (ex, ey) = (b.0 - a.0, b.1 - a.1);
    let length = (ex * ex + ey * ey).sqrt();
    if length == 0.0 {
        return f32::MAX;
    }
    ((x - a.0) * ey - (y - a.1) * ex) / length
}

/// Signed distance to a thick line segment: the tail of the pointer.
fn bar(x: f32, y: f32, a: (f32, f32), b: (f32, f32), half_width: f32) -> f32 {
    let (ex, ey) = (b.0 - a.0, b.1 - a.1);
    let (px, py) = (x - a.0, y - a.1);
    let length_squared = ex * ex + ey * ey;
    // How far along the segment the nearest point is, clamped so the ends are
    // round caps rather than infinite lines.
    let t = if length_squared == 0.0 {
        0.0
    } else {
        ((px * ex + py * ey) / length_squared).clamp(0.0, 1.0)
    };
    let (dx, dy) = (px - ex * t, py - ey * t);
    (dx * dx + dy * dy).sqrt() - half_width
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(icon: &Icon, x: u32, y: u32) -> u8 {
        icon.rgba[((y * icon.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_buffer_is_rgba_at_the_requested_size() {
        let icon = draw(32);
        assert_eq!(icon.width, 32);
        assert_eq!(icon.height, 32);
        assert_eq!(icon.rgba.len(), 32 * 32 * 4);
    }

    /// Every platform crops or pads a tray icon slightly differently, so a
    /// shape touching the boundary is the one that gets clipped.
    #[test]
    fn the_corners_are_transparent_and_the_middle_is_not() {
        let icon = draw(64);
        assert_eq!(alpha_at(&icon, 0, 0), 0);
        assert_eq!(alpha_at(&icon, 63, 0), 0);
        assert_eq!(alpha_at(&icon, 0, 63), 0);
        assert_eq!(alpha_at(&icon, 63, 63), 0);
        assert_eq!(alpha_at(&icon, 32, 32), 255, "the badge fills the middle");
    }

    /// The pointer is punched out light; without the contrast the mark is a
    /// featureless square, which is what a wrongly wound triangle produces.
    /// Measured across the whole icon rather than at chosen pixels, so nudging
    /// the arrow's proportions cannot quietly invalidate the check.
    #[test]
    fn the_mark_contrasts_with_the_badge_it_sits_in() {
        let icon = draw(64);
        let opaque: Vec<u8> = icon
            .rgba
            .chunks(4)
            .filter(|pixel| pixel[3] > 200)
            .map(|pixel| pixel[0])
            .collect();
        let darkest = *opaque.iter().min().expect("the badge is drawn");
        let lightest = *opaque.iter().max().expect("the badge is drawn");
        assert!(
            lightest - darkest > 100,
            "the pointer must stand out of the badge (darkest {darkest}, lightest {lightest})"
        );
    }

    /// Drawn at every size a platform might ask for, so a scale factor cannot
    /// produce an empty or malformed buffer.
    #[test]
    fn every_size_produces_a_visible_mark() {
        for size in [16, 22, 32, 44, 64, 128] {
            let icon = draw(size);
            assert_eq!(icon.rgba.len(), (size * size * 4) as usize);
            let opaque = icon.rgba.chunks(4).filter(|pixel| pixel[3] > 128).count();
            assert!(opaque > 0, "size {size} drew nothing");
            let total = (size * size) as usize;
            assert!(opaque < total, "size {size} filled the whole tile");
        }
    }
}
