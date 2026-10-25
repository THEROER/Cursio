//! What a window drag looks like, independent of how it was detected.
//!
//! The two platforms learn about drags in very different ways — Windows asks
//! the OS (`GetGUIThreadInfo`), Linux is told by a GNOME Shell extension over
//! D-Bus, because a Wayland client is not allowed to observe other windows.
//! Both report *this* vocabulary, so the session logic that turns a drag into a
//! transfer is written once and neither backend leaks into it.
//!
//! See [`windows_drag`](crate::windows_drag),
//! [`linux_drag`](crate::linux_drag) and [`macos_drag`](crate::macos_drag) for
//! the detectors.

use tokio::sync::mpsc;

/// Receives [`DragEvent`]s for as long as it is held; dropping it stops the
/// detector behind it.
///
/// Built by each platform's `watch`, which never fails: drag detection is an
/// optional accelerator for a transfer, so a detector that cannot run costs
/// the drag trigger and nothing else — the watcher simply never yields.
pub struct DragWatcher {
    events: mpsc::UnboundedReceiver<DragEvent>,
    /// Dropped with the watcher, which ends the detector's task.
    _cancel: tokio::sync::oneshot::Sender<()>,
}

impl DragWatcher {
    /// Run `detect` on the runtime until the watcher is dropped, receiving
    /// what it reports.
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    pub(crate) fn run<F, Fut>(detect: F) -> Self
    where
        F: FnOnce(mpsc::UnboundedSender<DragEvent>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, events) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let detector = detect(tx);
        tokio::spawn(async move {
            tokio::select! {
                _ = detector => {}
                _ = cancel_rx => {}
            }
        });
        Self {
            events,
            _cancel: cancel_tx,
        }
    }

    /// The next drag event, or `None` once the detector has stopped.
    pub async fn next(&mut self) -> Option<DragEvent> {
        self.events.recv().await
    }
}

/// Where the pointer is, in whole desktop pixels.
///
/// The coordinate space is the one the platform's edge detection already uses
/// (compositor-global on Linux, virtual-screen on Windows), so a drag position
/// can be compared against a screen edge without conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerPos {
    pub x: i32,
    pub y: i32,
}

/// A window being dragged by the user.
///
/// `Started` is emitted once the drag is known to be a *move* (not a resize)
/// and the window has actually begun moving; every `Started` is eventually
/// followed by exactly one `Ended`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragEvent {
    /// The user started moving a window.
    Started { title: String, at: PointerPos },
    /// The pointer moved while a window is being dragged.
    Moved { at: PointerPos },
    /// The drag finished (button released, or the window went away).
    Ended { at: PointerPos },
}
