//! Detecting a window drag on Windows.
//!
//! The Windows half of the drag trigger, and the counterpart to
//! [`linux_drag`](crate::linux_drag). The two report the same
//! [`DragEvent`](crate::drag::DragEvent)s so the session logic above them is
//! written once.
//!
//! Unlike Wayland — where a client may not know other windows exist, which is
//! why Linux needs a Shell extension — Windows answers this directly.
//! `GetGUIThreadInfo` with a thread id of 0 describes the *foreground* thread,
//! and its `GUI_INMOVESIZE` flag means "this thread is in a move/size loop".
//! `hwndMoveSize` then names the window being moved, which is better than
//! inferring it from the cursor position: during a drag the pointer may leave
//! the window entirely (that is precisely the case that starts a transfer).
//!
//! ## Polling, and why it is acceptable here
//!
//! There is no notification for entering a move loop that reaches another
//! process. A WinEvent hook (`EVENT_SYSTEM_MOVESIZESTART`) exists, but it must
//! be pumped on a thread with a message loop and delivers out-of-context
//! callbacks — machinery the Linux side doesn't need and that would not make
//! the shared logic simpler. `GetGUIThreadInfo` is a cheap kernel query, so a
//! poll at the same 30ms as the GNOME extension keeps both platforms behaving
//! identically for the same negligible cost.
//!
//! ## Move versus resize
//!
//! `GUI_INMOVESIZE` covers both, and only a *move* should send a window to the
//! peer. Windows exposes no flag distinguishing them, so the window's size is
//! sampled when the loop begins: if it changes, this was a resize and the drag
//! is abandoned. That is why [`DragEvent::Started`] is not emitted until the
//! pointer has actually moved with the size holding steady.

use std::time::Duration;

use tokio::sync::mpsc;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetGUIThreadInfo, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GUITHREADINFO, GUI_INMOVESIZE,
};

use crate::drag::{DragEvent, PointerPos};

/// How often the move-size state is sampled. Matches the GNOME extension's
/// poll so a drag feels the same on both machines.
const POLL: Duration = Duration::from_millis(30);

/// Receives [`DragEvent`]s for as long as it is held.
///
/// Dropping it stops the background poller.
pub struct DragWatcher {
    events: mpsc::UnboundedReceiver<DragEvent>,
    _cancel: tokio::sync::oneshot::Sender<()>,
}

impl DragWatcher {
    /// Start watching for window drags.
    ///
    /// Never fails, matching the Linux watcher: drag detection is an optional
    /// accelerator for a feature that also works from the command line, so a
    /// problem here must cost the drag trigger and nothing else.
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

/// The desktop's total size, for deciding what counts as "at the edge".
///
/// The *virtual* screen — the bounding box of every monitor — because that is
/// the space `GetCursorPos` reports in, and comparing a pointer against one
/// monitor's size would put the edge in the middle of a multi-monitor desktop.
pub fn screen_size() -> Option<crate::drag_trigger::ScreenBounds> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    };
    // SAFETY: a pure query with no arguments to get wrong.
    let (width, height) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(crate::drag_trigger::ScreenBounds { width, height })
}

/// What the poller knows about the drag currently in progress.
struct Active {
    /// The window being moved, as a raw handle value (`HWND` is not `Send`).
    hwnd: isize,
    /// Its size when the move loop began, to tell a move from a resize.
    size: (i32, i32),
    /// Where the pointer was when the loop began.
    origin: PointerPos,
    /// Whether `Started` has been emitted. Deferred until the pointer has moved
    /// and the size has held, so a resize never opens a transfer.
    announced: bool,
}

async fn poll_loop(tx: mpsc::UnboundedSender<DragEvent>) {
    let mut active: Option<Active> = None;
    let mut ticker = tokio::time::interval(POLL);

    loop {
        ticker.tick().await;

        let moving = match move_size_window() {
            Some(w) => w,
            None => {
                // The move loop ended. Report it if we ever announced a start,
                // so every `Started` is paired with an `Ended`.
                if let Some(a) = active.take() {
                    if a.announced {
                        let at = cursor().unwrap_or(a.origin);
                        if tx.send(DragEvent::Ended { at }).is_err() {
                            return;
                        }
                    }
                }
                continue;
            }
        };

        let Some(at) = cursor() else { continue };

        match &mut active {
            // A different window took over: end the old drag before starting.
            Some(a) if a.hwnd != moving.hwnd => {
                if a.announced && tx.send(DragEvent::Ended { at }).is_err() {
                    return;
                }
                active = Some(Active {
                    hwnd: moving.hwnd,
                    size: moving.size,
                    origin: at,
                    announced: false,
                });
            }
            Some(a) => {
                // A size change means this is a resize, not a move. Drop it and
                // wait for the loop to end — a resize must never open a
                // transfer, and re-checking every tick would fight the user.
                if moving.size != a.size {
                    if a.announced && tx.send(DragEvent::Ended { at }).is_err() {
                        return;
                    }
                    a.announced = false;
                    a.size = moving.size;
                    continue;
                }

                if !a.announced {
                    // Wait for actual movement: entering the loop alone doesn't
                    // mean the window is going anywhere.
                    if at == a.origin {
                        continue;
                    }
                    a.announced = true;
                    if tx
                        .send(DragEvent::Started {
                            title: moving.title.clone(),
                            at,
                        })
                        .is_err()
                    {
                        return;
                    }
                } else if tx.send(DragEvent::Moved { at }).is_err() {
                    return;
                }
            }
            None => {
                active = Some(Active {
                    hwnd: moving.hwnd,
                    size: moving.size,
                    origin: at,
                    announced: false,
                });
            }
        }
    }
}

/// The window the foreground thread is currently moving or sizing.
struct MoveSize {
    hwnd: isize,
    title: String,
    size: (i32, i32),
}

fn move_size_window() -> Option<MoveSize> {
    unsafe {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // Thread id 0 = whichever thread owns the foreground window, which is
        // the one the user is dragging with.
        GetGUIThreadInfo(0, &mut info).ok()?;

        if !info.flags.contains(GUI_INMOVESIZE) || info.hwndMoveSize.0.is_null() {
            return None;
        }

        let hwnd = info.hwndMoveSize;
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;

        Some(MoveSize {
            hwnd: hwnd.0 as isize,
            title: window_title(hwnd),
            size: (rect.right - rect.left, rect.bottom - rect.top),
        })
    }
}

unsafe fn window_title(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd).max(0) as usize;
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len + 1];
    let got = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..got])
}

fn cursor() -> Option<PointerPos> {
    let mut pt = POINT::default();
    // SAFETY: writes into our own POINT; a failure leaves it unused.
    unsafe { GetCursorPos(&mut pt).ok()? };
    Some(PointerPos { x: pt.x, y: pt.y })
}
