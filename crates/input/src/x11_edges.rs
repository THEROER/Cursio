//! Edge crossing on an X11 desktop, where capture is evdev.
//!
//! evdev reports relative motion and nothing else, so it cannot say that the
//! pointer reached a screen edge (see [`crate::capture`]). The X server can:
//! `QueryPointer` answers where the pointer is, in root-window coordinates.
//! This is the one question asked of it; everything else — the events, the
//! grab — stays with evdev, which already works on these machines.
//!
//! The question is asked only while this machine holds the cursor and only in
//! answer to motion, so an idle desktop costs nothing. A crossing needs the
//! pointer at a watched edge *and* the motion that arrived pushing outward:
//! the pointer rests against edges constantly (every throw at a menu), and
//! only a push through one is a request to leave. That is the rule the macOS
//! tap follows, for the same reason.
//!
//! The root window spans every monitor, so its bounds are the outer edges of
//! the whole desktop, the way the Windows hook uses the virtual screen. A
//! stepped layout's gaps are not edges here; nobody pushes a cursor into one
//! on purpose.
//!
//! After reporting a crossing the pointer is warped a little way back inside,
//! as the other backends nudge theirs: left pinned against the edge, the next
//! small outward movement after the cursor comes home would read as a second
//! crossing.
//!
//! Runs on its own thread: x11rb's calls block, and a round trip to a local X
//! server is far below the rate motion arrives at, so batching whatever queued
//! meanwhile keeps it level with the mouse.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use cursio_protocol::{Edge, EdgeSet};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;

use crate::{EdgeCrossing, InputMode};

/// How far inside the edge to put the pointer after a crossing.
const NUDGE_PX: i32 = 12;

/// How often the desktop size is re-read: a monitor plugged in or rotated
/// changes it, and asking on every motion would double the round trips.
const GEOMETRY_REFRESH: Duration = Duration::from_secs(2);

/// Where motion is reported, once a watcher is running.
///
/// Fed from two places, because the pointer on this desktop moves for two
/// reasons: this machine's own mouse, read by the evdev readers, and a peer's,
/// injected through uinput. The second is the common case — the user sits at
/// the other machine and pushes this one's cursor back up to its edge — and
/// the readers never see it, since capture skips Cursio's own virtual devices
/// so injected input cannot loop back out. Without it the cursor could reach
/// this machine and never leave by the edge, exactly the fault this module
/// exists to fix. The Windows hook and the macOS tap see injected motion
/// themselves; here it has to be handed over.
static MOTION: std::sync::OnceLock<Sender<(i32, i32)>> = std::sync::OnceLock::new();

/// Report pointer motion to the watcher, if one is running.
pub(crate) fn note_motion(dx: i32, dy: i32) {
    if let Some(motion) = MOTION.get() {
        let _ = motion.send((dx, dy));
    }
}

/// Start watching, if there is an X server to ask. Motion reaches it through
/// [`note_motion`]. Returns whether it is running.
pub(crate) fn spawn(
    edges: tokio::sync::mpsc::UnboundedSender<EdgeCrossing>,
    mode: tokio::sync::watch::Receiver<InputMode>,
) -> bool {
    if std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    let (conn, screen) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cannot reach the X server; edge crossing is off, Ctrl+Alt+G still hands the cursor over");
            return false;
        }
    };
    let root = conn.setup().roots[screen].root;
    let (tx, rx) = std::sync::mpsc::channel();
    // Capture starts once per process; a second start would find the first
    // watcher's channel here and leave the new one unfed.
    if MOTION.set(tx).is_err() {
        tracing::debug!("an X11 edge watcher is already running");
        return true;
    }
    let spawned = std::thread::Builder::new()
        .name("cursio-x11-edges".into())
        .spawn(move || watch(conn, root, rx, edges, mode));
    match spawned {
        Ok(_) => {
            tracing::info!("watching the X11 pointer for edge crossings");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start the X11 edge watcher; edge crossing is off");
            false
        }
    }
}

fn watch(
    conn: impl Connection,
    root: u32,
    motion: Receiver<(i32, i32)>,
    edges: tokio::sync::mpsc::UnboundedSender<EdgeCrossing>,
    mode: tokio::sync::watch::Receiver<InputMode>,
) {
    let mut size: Option<(i32, i32)> = None;
    let mut size_read = Instant::now();
    let mut crossed = false;
    while let Ok(first) = motion.recv() {
        let (mut dx, mut dy) = first;
        loop {
            match motion.try_recv() {
                Ok((x, y)) => {
                    dx += x;
                    dy += y;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        // Only the machine holding the cursor may send it away; a dormant
        // one's pointer is frozen under the grab anyway.
        if !matches!(*mode.borrow(), InputMode::Active) {
            continue;
        }
        let watched = crate::watched_edges();
        if watched.is_empty() {
            crossed = false;
            continue;
        }
        if size.is_none() || size_read.elapsed() >= GEOMETRY_REFRESH {
            match conn.get_geometry(root).map(|c| c.reply()) {
                Ok(Ok(g)) => {
                    size = Some((i32::from(g.width), i32::from(g.height)));
                    size_read = Instant::now();
                }
                Ok(Err(e)) => tracing::debug!(error = %e, "reading the desktop size failed"),
                Err(e) => {
                    tracing::warn!(error = %e, "lost the X server; edge crossing is off");
                    return;
                }
            }
        }
        let Some(size) = size else { continue };
        let at = match conn.query_pointer(root).map(|c| c.reply()) {
            Ok(Ok(p)) => (i32::from(p.root_x), i32::from(p.root_y)),
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "reading the pointer failed");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "lost the X server; edge crossing is off");
                return;
            }
        };
        match judge(at, size, (dx, dy), watched) {
            Touch::Away => crossed = false,
            Touch::Resting => {}
            Touch::Pushed { edge, pos_norm } => {
                if crossed {
                    continue;
                }
                crossed = true;
                crate::note_crossed(edge);
                if edges.send(EdgeCrossing { edge, pos_norm }).is_err() {
                    return;
                }
                let (nx, ny) = inward(edge);
                if let Err(e) = conn
                    .warp_pointer(x11rb::NONE, x11rb::NONE, 0, 0, 0, 0, nx as i16, ny as i16)
                    .and_then(|_| conn.flush())
                {
                    tracing::debug!(error = %e, "could not move the pointer off the edge");
                }
            }
        }
    }
}

/// Where the pointer stands relative to the watched edges.
#[derive(Debug, PartialEq)]
enum Touch {
    /// Not against any watched edge.
    Away,
    /// Against one, but the motion was not outward through it.
    Resting,
    /// Pushed through `edge`, `pos_norm` of the way along it.
    Pushed { edge: Edge, pos_norm: f32 },
}

fn judge(at: (i32, i32), size: (i32, i32), delta: (i32, i32), watched: EdgeSet) -> Touch {
    let ((x, y), (w, h), (dx, dy)) = (at, size, delta);
    let mut touching = watched
        .iter()
        .filter(|edge| match edge {
            Edge::Left => x <= 0,
            Edge::Right => x >= w - 1,
            Edge::Top => y <= 0,
            Edge::Bottom => y >= h - 1,
        })
        .peekable();
    if touching.peek().is_none() {
        return Touch::Away;
    }
    let pushed = touching.find(|edge| match edge {
        Edge::Left => dx < 0,
        Edge::Right => dx > 0,
        Edge::Top => dy < 0,
        Edge::Bottom => dy > 0,
    });
    match pushed {
        Some(edge) => {
            let pos_norm = if edge.is_horizontal() {
                fraction(x, w)
            } else {
                fraction(y, h)
            };
            Touch::Pushed { edge, pos_norm }
        }
        None => Touch::Resting,
    }
}

fn fraction(v: i32, span: i32) -> f32 {
    if span <= 1 {
        0.0
    } else {
        (v as f32 / (span - 1) as f32).clamp(0.0, 1.0)
    }
}

fn inward(edge: Edge) -> (i32, i32) {
    match edge {
        Edge::Left => (NUDGE_PX, 0),
        Edge::Right => (-NUDGE_PX, 0),
        Edge::Top => (0, NUDGE_PX),
        Edge::Bottom => (0, -NUDGE_PX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESK: (i32, i32) = (1280, 800);

    fn only(edge: Edge) -> EdgeSet {
        EdgeSet::EMPTY.with(edge)
    }

    /// The MacBook the mesh stands above: its top edge leads home.
    #[test]
    fn pushing_up_through_the_watched_top_edge_is_a_crossing() {
        let touch = judge((320, 0), DESK, (0, -3), only(Edge::Top));
        assert_eq!(
            touch,
            Touch::Pushed {
                edge: Edge::Top,
                pos_norm: 320.0 / 1279.0
            }
        );
    }

    /// Thrown at the top bar to reach a menu, the pointer rests on the edge
    /// while moving sideways; that must not send the cursor away.
    #[test]
    fn sliding_along_an_edge_is_not_a_crossing() {
        assert_eq!(
            judge((320, 0), DESK, (5, 0), only(Edge::Top)),
            Touch::Resting
        );
    }

    #[test]
    fn an_unwatched_edge_is_a_wall() {
        assert_eq!(judge((0, 400), DESK, (-4, 0), only(Edge::Top)), Touch::Away);
    }

    #[test]
    fn a_pointer_inside_the_desktop_touches_nothing() {
        assert_eq!(
            judge((640, 400), DESK, (0, -9), only(Edge::Top)),
            Touch::Away
        );
    }

    /// In a corner two watched edges meet; the one being pushed through wins.
    #[test]
    fn in_a_corner_the_edge_pushed_through_is_the_one_crossed() {
        let both: EdgeSet = [Edge::Top, Edge::Right].into_iter().collect();
        assert!(matches!(
            judge((1279, 0), DESK, (4, 0), both),
            Touch::Pushed {
                edge: Edge::Right,
                ..
            }
        ));
        assert!(matches!(
            judge((1279, 0), DESK, (0, -4), both),
            Touch::Pushed {
                edge: Edge::Top,
                ..
            }
        ));
    }

    #[test]
    fn the_far_edges_are_the_last_pixel_not_one_past_it() {
        assert!(matches!(
            judge((1279, 10), DESK, (2, 0), only(Edge::Right)),
            Touch::Pushed {
                edge: Edge::Right,
                ..
            }
        ));
        assert!(matches!(
            judge((10, 799), DESK, (0, 2), only(Edge::Bottom)),
            Touch::Pushed {
                edge: Edge::Bottom,
                ..
            }
        ));
    }
}
