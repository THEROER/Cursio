//! The shared desk: every machine's screens laid out in millimetres, and
//! where two machines' screens meet.
//!
//! Each machine says what its screens are in its own pixels
//! ([`cursio_protocol::Layout`]); this turns them into rectangles on one desk
//! and finds the seams — the stretches where one machine's screen edge lies
//! against another's. A seam is what the cursor crosses: capture arms only
//! those pixels, the input hub sends a crossing to the peer whose screen is
//! on the far side of the very pixel it left by, and the pointer comes in at
//! the spot on that screen beside it.
//!
//! **Millimetres, from pixels, per screen.** A machine's own screens are
//! placed by its operating system in pixels, and a 4K panel beside a 1080p
//! one is twice as many pixels for the same width of desk. So each screen is
//! sized by its panel, and the screens of one machine are joined where their
//! pixels join: one placed, then each that touches it put flush against it,
//! the offset along the shared edge converted at the placed screen's own
//! density. A panel that cannot report its size is taken at 96 pixels to the
//! inch, which is the layout's only guess and shows in the arrangement for
//! the person to see.
//!
//! **Touching, within reason.** Machines are arranged by hand, snapped in
//! the app, and stored in whole millimetres; [`TOUCH_MM`] absorbs the
//! rounding, and a seam shorter than [`MIN_SEAM_MM`] is a corner brushing a
//! corner, which no hand aims at.

use cursio_protocol::{Edge, Layout, PeerId, Screen};

/// How far apart two edges may lie and still meet.
pub const TOUCH_MM: f64 = 3.0;

/// The shortest stretch of two edges overlapping that counts as a seam.
pub const MIN_SEAM_MM: f64 = 5.0;

/// The density assumed for a panel that does not report its size.
const ASSUMED_MM_PER_PX: f64 = 25.4 / 96.0;

/// A rectangle on the desk, in millimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MmRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A screen's size on the desk: its panel's, else a guess from its pixels.
pub fn size_mm(screen: &Screen) -> (f64, f64) {
    match screen.size_mm {
        Some((w, h)) if w > 0 && h > 0 => (f64::from(w), f64::from(h)),
        _ => (
            f64::from(screen.width) * ASSUMED_MM_PER_PX,
            f64::from(screen.height) * ASSUMED_MM_PER_PX,
        ),
    }
}

/// One machine's screens on the desk relative to its own corner: the same
/// order as `screens`, the leftmost and topmost at zero.
pub fn arrange(screens: &[Screen]) -> Vec<MmRect> {
    let mut placed: Vec<Option<MmRect>> = vec![None; screens.len()];
    let Some(first) = (0..screens.len()).min_by_key(|&i| (screens[i].y, screens[i].x)) else {
        return Vec::new();
    };
    let (w, h) = size_mm(&screens[first]);
    placed[first] = Some(MmRect {
        x: 0.0,
        y: 0.0,
        w,
        h,
    });

    // Flush against a screen already placed, wherever the pixels say they
    // touch; a screen touching none is placed from the first by its offset.
    while placed.iter().any(Option::is_none) {
        let next = (0..screens.len())
            .filter(|&b| placed[b].is_none())
            .find_map(|b| {
                (0..screens.len())
                    .filter_map(|a| placed[a].map(|rect| (a, rect)))
                    .find_map(|(a, rect)| beside(&screens[a], rect, &screens[b]).map(|r| (b, r)))
            });
        let (b, rect) = next.unwrap_or_else(|| {
            let b = placed
                .iter()
                .position(Option::is_none)
                .expect("one is unplaced");
            (
                b,
                offset_from(&screens[first], placed[first].expect("placed"), &screens[b]),
            )
        });
        placed[b] = Some(rect);
    }

    let rects: Vec<MmRect> = placed.into_iter().map(|r| r.expect("all placed")).collect();
    let min_x = rects.iter().map(|r| r.x).fold(f64::INFINITY, f64::min);
    let min_y = rects.iter().map(|r| r.y).fold(f64::INFINITY, f64::min);
    rects
        .into_iter()
        .map(|r| MmRect {
            x: r.x - min_x,
            y: r.y - min_y,
            ..r
        })
        .collect()
}

/// Millimetres per pixel across and down `screen`.
fn density(screen: &Screen, rect: MmRect) -> (f64, f64) {
    (
        rect.w / f64::from(screen.width.max(1)),
        rect.h / f64::from(screen.height.max(1)),
    )
}

/// `b` placed flush against `a` (at `rect`), if their pixels touch.
fn beside(a: &Screen, rect: MmRect, b: &Screen) -> Option<MmRect> {
    let (bw, bh) = size_mm(b);
    let (sx, sy) = density(a, rect);
    let (ax0, ay0) = (a.x, a.y);
    let (ax1, ay1) = (a.x + a.width as i32, a.y + a.height as i32);
    let (bx0, by0) = (b.x, b.y);
    let (bx1, by1) = (b.x + b.width as i32, b.y + b.height as i32);
    let rows_overlap = by0 < ay1 && ay0 < by1;
    let columns_overlap = bx0 < ax1 && ax0 < bx1;
    let down = rect.y + f64::from(by0 - ay0) * sy;
    let across = rect.x + f64::from(bx0 - ax0) * sx;
    if bx0 == ax1 && rows_overlap {
        Some(MmRect {
            x: rect.x + rect.w,
            y: down,
            w: bw,
            h: bh,
        })
    } else if bx1 == ax0 && rows_overlap {
        Some(MmRect {
            x: rect.x - bw,
            y: down,
            w: bw,
            h: bh,
        })
    } else if by0 == ay1 && columns_overlap {
        Some(MmRect {
            x: across,
            y: rect.y + rect.h,
            w: bw,
            h: bh,
        })
    } else if by1 == ay0 && columns_overlap {
        Some(MmRect {
            x: across,
            y: rect.y - bh,
            w: bw,
            h: bh,
        })
    } else {
        None
    }
}

/// `b` placed by its pixel offset from `a`, at `a`'s density.
fn offset_from(a: &Screen, rect: MmRect, b: &Screen) -> MmRect {
    let (bw, bh) = size_mm(b);
    let (sx, sy) = density(a, rect);
    MmRect {
        x: rect.x + f64::from(b.x - a.x) * sx,
        y: rect.y + f64::from(b.y - a.y) * sy,
        w: bw,
        h: bh,
    }
}

/// A machine's screens where they stand on the desk, or `None` while it has
/// no screens or has not been arranged.
pub fn on_desk(layout: &Layout) -> Option<Vec<MmRect>> {
    let place = layout.place?;
    if layout.screens.is_empty() {
        return None;
    }
    Some(
        arrange(&layout.screens)
            .into_iter()
            .map(|r| MmRect {
                x: r.x + f64::from(place.x_mm),
                y: r.y + f64::from(place.y_mm),
                ..r
            })
            .collect(),
    )
}

/// A stretch of one of this machine's edges that a peer's screen meets.
#[derive(Debug, Clone, PartialEq)]
pub struct Seam {
    /// This machine's edge.
    pub edge: Edge,
    /// The pixels along it, `from..to`: x on a top or bottom edge, y on a side.
    pub from: i32,
    pub to: i32,
    /// The same stretch along the peer's screen edge, in its pixels, so a
    /// point maps across in proportion.
    pub peer_from: f64,
    pub peer_to: f64,
    /// The peer's edge itself: the first pixel row or column inside its
    /// screen on the side facing this one.
    pub peer_line: i32,
    pub screen: String,
    pub peer_screen: String,
}

impl Seam {
    pub fn contains(&self, along: i32) -> bool {
        (self.from..self.to).contains(&along)
    }

    /// Where on the peer's screen a pointer leaving here at `along` comes in.
    pub fn entry(&self, along: i32) -> (i32, i32) {
        let span = f64::from((self.to - self.from).max(1));
        let t = (f64::from(along - self.from) / span).clamp(0.0, 1.0);
        let peer_along = (self.peer_from + t * (self.peer_to - self.peer_from)).round() as i32;
        if self.edge.is_horizontal() {
            (peer_along, self.peer_line)
        } else {
            (self.peer_line, peer_along)
        }
    }
}

/// The seams between this machine and a peer, or `None` when either has not
/// been arranged on the desk (then the grid decides, as it always did). An
/// empty list means arranged and not touching: the cursor does not reach
/// that peer by any edge.
pub fn seams(me: PeerId, mine: &Layout, peer: PeerId, theirs: &Layout) -> Option<Vec<Seam>> {
    let my_rects = on_desk(mine)?;
    let their_rects = on_desk(theirs)?;
    let mut out = Vec::new();
    for (m, mr) in mine.screens.iter().zip(&my_rects) {
        for (n, nr) in theirs.screens.iter().zip(&their_rects) {
            if closed(me, mine, m, peer, theirs, n) {
                continue;
            }
            for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
                if let Some(seam) = seam(edge, m, *mr, n, *nr) {
                    out.push(seam);
                }
            }
        }
    }
    Some(out)
}

/// Whether either side closed the seam between `m` (mine) and `n` (theirs).
fn closed(
    me: PeerId,
    mine: &Layout,
    m: &Screen,
    peer: PeerId,
    theirs: &Layout,
    n: &Screen,
) -> bool {
    mine.barriers
        .iter()
        .any(|b| b.peer == peer && b.screen == m.id && b.peer_screen == n.id)
        || theirs
            .barriers
            .iter()
            .any(|b| b.peer == me && b.screen == n.id && b.peer_screen == m.id)
}

/// Where `m`'s `edge` meets `n`, if it does.
fn seam(edge: Edge, m: &Screen, mr: MmRect, n: &Screen, nr: MmRect) -> Option<Seam> {
    // The line each edge lies on, and the stretch it covers, on the desk.
    let (mine_line, theirs_line) = match edge {
        Edge::Left => (mr.x, nr.x + nr.w),
        Edge::Right => (mr.x + mr.w, nr.x),
        Edge::Top => (mr.y, nr.y + nr.h),
        Edge::Bottom => (mr.y + mr.h, nr.y),
    };
    if (mine_line - theirs_line).abs() > TOUCH_MM {
        return None;
    }
    let (m0, m1, n0, n1) = if edge.is_horizontal() {
        (mr.x, mr.x + mr.w, nr.x, nr.x + nr.w)
    } else {
        (mr.y, mr.y + mr.h, nr.y, nr.y + nr.h)
    };
    let (lo, hi) = (m0.max(n0), m1.min(n1));
    if hi - lo < MIN_SEAM_MM {
        return None;
    }
    // Millimetres along each screen's edge, to its pixels.
    let (m_px0, m_len) = if edge.is_horizontal() {
        (m.x, m.width)
    } else {
        (m.y, m.height)
    };
    let (n_px0, n_len) = if edge.is_horizontal() {
        (n.x, n.width)
    } else {
        (n.y, n.height)
    };
    let to_px = |mm: f64, start_mm: f64, len_mm: f64, px0: i32, len: u32| {
        f64::from(px0) + (mm - start_mm) / len_mm * f64::from(len)
    };
    let from = to_px(lo, m0, m1 - m0, m_px0, m_len).floor() as i32;
    let to = to_px(hi, m0, m1 - m0, m_px0, m_len).ceil() as i32;
    let peer_line = match edge {
        Edge::Left => n.x + n.width as i32 - 1,
        Edge::Right => n.x,
        Edge::Top => n.y + n.height as i32 - 1,
        Edge::Bottom => n.y,
    };
    Some(Seam {
        edge,
        from: from.max(m_px0),
        to: to.min(m_px0 + m_len as i32),
        peer_from: to_px(lo, n0, n1 - n0, n_px0, n_len),
        peer_to: to_px(hi, n0, n1 - n0, n_px0, n_len) - 1.0,
        peer_line,
        screen: m.id.clone(),
        peer_screen: n.id.clone(),
    })
}

/// The edge a peer is reached by: the one carrying the longest seam to it.
/// What the window module's drag trigger watches, and what the grid would
/// have said for a pair not yet on the desk.
pub fn main_edge(seams: &[Seam]) -> Option<Edge> {
    seams.iter().max_by_key(|s| s.to - s.from).map(|s| s.edge)
}

/// One side of a link, as far as where it stands goes.
pub struct Standing<'a> {
    pub id: PeerId,
    pub position: cursio_protocol::Position,
    pub layout: &'a Layout,
}

/// Which of this machine's edges faces `peer`: the desk's answer when both
/// are on it, else the grid's.
pub fn edge_toward(me: &Standing<'_>, peer: &Standing<'_>) -> Option<Edge> {
    match seams(me.id, me.layout, peer.id, peer.layout) {
        Some(seams) => main_edge(&seams),
        None => me.position.edge_toward(peer.position),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::{Barrier, Place};

    fn screen(id: &str, x: i32, y: i32, w: u32, h: u32, mm: Option<(u32, u32)>) -> Screen {
        Screen {
            id: id.into(),
            name: id.into(),
            x,
            y,
            width: w,
            height: h,
            size_mm: mm,
        }
    }

    /// Nocturne as it stands: the LG 22" left, the Philips 24" right.
    fn nocturne() -> Layout {
        Layout {
            screens: vec![
                screen("LG", 0, 0, 1920, 1080, Some((480, 270))),
                screen("PHL", 1920, 0, 1920, 1080, Some((527, 296))),
            ],
            place: Some(Place { x_mm: 0, y_mm: 0 }),
            barriers: Vec::new(),
        }
    }

    /// A laptop panel of 344 by 194 mm.
    fn laptop(x_mm: i32, y_mm: i32) -> Layout {
        Layout {
            screens: vec![screen("BOE", 0, 0, 1920, 1080, Some((344, 194)))],
            place: Some(Place { x_mm, y_mm }),
            barriers: Vec::new(),
        }
    }

    fn me() -> PeerId {
        PeerId::from_u128(1)
    }

    fn them() -> PeerId {
        PeerId::from_u128(2)
    }

    #[test]
    fn a_machines_screens_are_sized_by_their_panels_and_joined_where_their_pixels_join() {
        let rects = arrange(&nocturne().screens);
        assert_eq!(
            rects[0],
            MmRect {
                x: 0.0,
                y: 0.0,
                w: 480.0,
                h: 270.0
            }
        );
        assert_eq!(
            rects[1],
            MmRect {
                x: 480.0,
                y: 0.0,
                w: 527.0,
                h: 296.0
            }
        );
    }

    #[test]
    fn a_monitor_left_of_the_primary_still_starts_the_desk_at_zero() {
        let screens = [
            screen("main", 0, 0, 1920, 1080, Some((480, 270))),
            screen("left", -1920, 0, 1920, 1080, Some((527, 296))),
        ];
        let rects = arrange(&screens);
        assert_eq!(rects[1].x, 0.0);
        assert_eq!(rects[0].x, 527.0);
    }

    #[test]
    fn a_panel_with_no_size_is_taken_at_96_dpi() {
        let rects = arrange(&[screen("tv", 0, 0, 1920, 1080, None)]);
        assert!((rects[0].w - 508.0).abs() < 0.01);
    }

    /// The photo: the laptop under the seam between the two monitors, more
    /// under the left one than the right.
    #[test]
    fn a_laptop_under_both_monitors_meets_each_where_it_stands_under_it() {
        let seams = seams(me(), &nocturne(), them(), &laptop(300, 296)).unwrap();
        // The LG is 270 tall, the Philips 296: only the Philips reaches the
        // laptop's top edge at 296 mm.
        assert_eq!(seams.len(), 1);
        let s = &seams[0];
        assert_eq!((s.edge, s.screen.as_str()), (Edge::Bottom, "PHL"));
        // 480..644 mm of the desk is 0..164 mm along the Philips.
        assert_eq!(
            (s.from, s.to),
            (1920, 1920 + (164.0 * 1920.0 / 527.0_f64).ceil() as i32)
        );
        assert_eq!(s.peer_line, 0);
    }

    #[test]
    fn under_the_right_monitor_only_its_bottom_leads_to_the_laptop() {
        let seams = seams(me(), &nocturne(), them(), &laptop(560, 296)).unwrap();
        assert!(seams
            .iter()
            .all(|s| s.screen == "PHL" && s.edge == Edge::Bottom));
        assert!(!seams.is_empty());
    }

    #[test]
    fn the_laptop_sees_the_same_seam_from_its_side() {
        let back = seams(them(), &laptop(560, 296), me(), &nocturne()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            (back[0].edge, back[0].peer_screen.as_str()),
            (Edge::Top, "PHL")
        );
        assert_eq!((back[0].from, back[0].to), (0, 1920));
    }

    #[test]
    fn the_pointer_comes_in_beside_where_it_left() {
        let seams = seams(me(), &nocturne(), them(), &laptop(560, 296)).unwrap();
        let s = &seams[0];
        // Leaving the Philips 80 mm from its left edge (x = 1920 + 80 mm at
        // 527 mm / 1920 px) is 0 mm into the laptop, which starts at 560.
        let along = 1920 + (80.0 * 1920.0 / 527.0_f64).round() as i32;
        let (x, y) = s.entry(along);
        assert_eq!(y, 0);
        assert!(x <= 1, "came in at x = {x}");
        // And the far end of the seam is the far end of the laptop.
        let (x, _) = s.entry(s.to - 1);
        assert!((1900..1920).contains(&x), "came in at x = {x}");
    }

    #[test]
    fn a_gap_between_machines_is_no_seam() {
        assert_eq!(
            seams(me(), &nocturne(), them(), &laptop(560, 320)),
            Some(Vec::new())
        );
    }

    #[test]
    fn an_unarranged_machine_leaves_the_pair_to_the_grid() {
        let mut unplaced = laptop(0, 0);
        unplaced.place = None;
        assert_eq!(seams(me(), &nocturne(), them(), &unplaced), None);
        let no_screens = Layout {
            screens: Vec::new(),
            ..laptop(0, 296)
        };
        assert_eq!(seams(me(), &nocturne(), them(), &no_screens), None);
    }

    #[test]
    fn a_barrier_on_either_side_closes_the_seam_both_ways() {
        let mut mine = nocturne();
        mine.barriers.push(Barrier {
            screen: "PHL".into(),
            peer: them(),
            peer_screen: "BOE".into(),
        });
        assert_eq!(
            seams(me(), &mine, them(), &laptop(560, 296)),
            Some(Vec::new())
        );
        assert_eq!(
            seams(them(), &laptop(560, 296), me(), &mine),
            Some(Vec::new())
        );
    }

    #[test]
    fn the_peer_is_reached_by_the_edge_with_the_longest_seam() {
        let seams = seams(me(), &nocturne(), them(), &laptop(560, 296)).unwrap();
        assert_eq!(main_edge(&seams), Some(Edge::Bottom));
        assert_eq!(main_edge(&[]), None);
    }
}
