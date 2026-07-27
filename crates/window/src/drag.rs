//! What a window drag looks like, independent of how it was detected.
//!
//! The two platforms learn about drags in very different ways — Windows asks
//! the OS (`GetGUIThreadInfo`), Linux is told by a GNOME Shell extension over
//! D-Bus, because a Wayland client is not allowed to observe other windows.
//! Both report *this* vocabulary, so the session logic that turns a drag into a
//! transfer is written once and neither backend leaks into it.
//!
//! See [`windows_drag`](crate::windows_drag) and
//! [`linux_drag`](crate::linux_drag) for the detectors.

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
