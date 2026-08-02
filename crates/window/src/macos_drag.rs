//! Noticing a window drag on macOS, so one can be thrown at the peer.
//!
//! The counterpart to [`crate::windows_drag`] and [`crate::linux_drag`], and the
//! reason a Mac could host a window but never start sending one: the other two
//! platforms are *told* a drag is happening — Windows by `GetGUIThreadInfo`,
//! GNOME by the Cursio Shell extension republishing Mutter's grab ops — and
//! macOS has no equivalent question to ask. So it is inferred, in
//! `native/macos/window_drag.m`: the button is down, and a window has moved
//! since it went down.
//!
//! Polled rather than pushed, like the Windows watcher, because the underlying
//! fact is a state and not an event. [`POLL`] is the compromise the trigger
//! actually needs: it compares the pointer against a screen edge, so the cost
//! of being late is overshooting the edge by a few pixels of travel.
//!
//! Nothing here fails loudly. Drag detection is an accelerator — the window
//! module works without it, receiving what a peer sends — so a Mac that cannot
//! read the window list costs the drag and nothing else.

use std::time::Duration;

use tokio::sync::mpsc;

use crate::drag::{DragEvent, PointerPos};

unsafe extern "C" {
    /// See `native/macos/window_drag.m`. Returns 1 while a window is being
    /// dragged; fills `title` only then, and the pointer position always.
    fn cursio_window_drag_poll(
        title: *mut std::ffi::c_char,
        title_cap: i32,
        px: *mut f64,
        py: *mut f64,
    ) -> i32;
}

/// How often the drag state is sampled.
///
/// The same rate the Windows watcher polls at. Faster buys nothing — a hand
/// moving a window does not change direction in 16 ms — and each poll reads the
/// window server's list, which is cheap but not free.
const POLL: Duration = Duration::from_millis(16);

/// Longest window title carried. Titles are matched as substrings by the
/// capture, so a truncated one still finds its window; an unbounded buffer
/// across an FFI boundary would not be worth the byte it saved.
const MAX_TITLE_BYTES: usize = 512;

/// Watches for windows being dragged on macOS.
pub struct DragWatcher {
    events: mpsc::UnboundedReceiver<DragEvent>,
    _cancel: tokio::sync::oneshot::Sender<()>,
}

impl DragWatcher {
    /// Start watching. Never fails, matching the other two platforms.
    pub fn spawn() -> Self {
        let (tx, events) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            tokio::select! {
                _ = poll_loop(tx) => {}
                _ = cancel_rx => {}
            }
        });

        Self {
            events,
            _cancel: cancel_tx,
        }
    }

    /// The next drag event, or `None` once the watcher has stopped.
    pub async fn next(&mut self) -> Option<DragEvent> {
        self.events.recv().await
    }
}

/// One sample of what the window server says.
struct Sample {
    dragging: bool,
    title: String,
    at: PointerPos,
}

fn poll_once() -> Sample {
    let mut title = [0_i8; MAX_TITLE_BYTES];
    let (mut x, mut y) = (0.0_f64, 0.0_f64);
    // SAFETY: the buffer is `MAX_TITLE_BYTES` long and its capacity is passed
    // alongside it; the two doubles are valid locals. The callee writes a
    // NUL-terminated string and never unwinds.
    let dragging = unsafe {
        cursio_window_drag_poll(
            title.as_mut_ptr().cast(),
            MAX_TITLE_BYTES as i32,
            &mut x,
            &mut y,
        ) == 1
    };
    Sample {
        dragging,
        title: read_title(&title),
        at: PointerPos {
            x: x as i32,
            y: y as i32,
        },
    }
}

/// Read the NUL-terminated title the poll wrote.
///
/// Lossy rather than strict: a window is free to put anything in its title, and
/// a name this crate cannot decode is still a name the capture can match
/// against — refusing it would lose the drag over a stray byte.
fn read_title(buffer: &[i8]) -> String {
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn poll_loop(tx: mpsc::UnboundedSender<DragEvent>) {
    let mut ticker = tokio::time::interval(POLL);
    // Whether a `Started` is outstanding. Every `Started` owes exactly one
    // `Ended`, or the trigger keeps believing a drag is in progress and the
    // next one is read as a continuation of it.
    let mut announced = false;
    let mut last = PointerPos { x: 0, y: 0 };

    loop {
        ticker.tick().await;
        let sample = poll_once();

        if !sample.dragging {
            if announced {
                announced = false;
                if tx.send(DragEvent::Ended { at: sample.at }).is_err() {
                    return;
                }
            }
            continue;
        }

        if !announced {
            announced = true;
            last = sample.at;
            let event = DragEvent::Started {
                title: sample.title,
                at: sample.at,
            };
            if tx.send(event).is_err() {
                return;
            }
            continue;
        }

        // Only real movement. The poll runs at 60 Hz and a hand holding a
        // window still is not moving it, so forwarding every sample would fill
        // the trigger's channel with nothing.
        if sample.at == last {
            continue;
        }
        last = sample.at;
        if tx.send(DragEvent::Moved { at: sample.at }).is_err() {
            return;
        }
    }
}

/// The rectangle every display covers between them.
///
/// The same union the input backend clamps its shadow cursor to, and it has to
/// be: the drag trigger compares a pointer position against these bounds, and
/// the pointer positions come from the window server in exactly these
/// coordinates.
pub fn screen_size() -> Option<crate::drag_trigger::ScreenBounds> {
    let mut ids = [0_u32; 16];
    let mut count = 0_u32;
    // SAFETY: the buffer holds `ids.len()` entries and the count is written to
    // a valid local.
    let status = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status != 0 || count == 0 {
        return None;
    }

    let mut union: Option<(f64, f64, f64, f64)> = None;
    for id in ids.iter().take(count as usize) {
        // SAFETY: `id` came from the list above, so it names a live display.
        let rect = unsafe { CGDisplayBounds(*id) };
        let next = (
            rect.origin.x,
            rect.origin.y,
            rect.origin.x + rect.size.width,
            rect.origin.y + rect.size.height,
        );
        union = Some(match union {
            None => next,
            Some(all) => (
                all.0.min(next.0),
                all.1.min(next.1),
                all.2.max(next.2),
                all.3.max(next.3),
            ),
        });
    }

    let (min_x, min_y, max_x, max_y) = union?;
    Some(crate::drag_trigger::ScreenBounds {
        width: (max_x - min_x) as i32,
        height: (max_y - min_y) as i32,
    })
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CgSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CgRect {
    origin: CgPoint,
    size: CgSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CgRect;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The title crosses a C boundary, so where it stops matters: reading past
    /// the NUL would carry whatever the last, longer title left behind.
    #[test]
    fn a_title_stops_at_its_terminator() {
        let mut buffer = [0_i8; 8];
        for (slot, byte) in buffer.iter_mut().zip(b"Term\0old") {
            *slot = *byte as i8;
        }
        assert_eq!(read_title(&buffer), "Term");
    }

    /// A window may put anything in its title, and losing the drag over a
    /// stray byte would be worse than a replacement character.
    #[test]
    fn an_undecodable_title_is_still_a_title() {
        let buffer = [0x54_i8, 0x65, -1, 0x00, 0x00];
        assert!(read_title(&buffer).starts_with("Te"));
    }

    #[test]
    fn an_empty_buffer_is_an_empty_title() {
        assert_eq!(read_title(&[0_i8; 4]), "");
    }
}
