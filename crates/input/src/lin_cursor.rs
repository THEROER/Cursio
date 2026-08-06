//! Linux cursor hiding — which no display server here allows from a daemon.
//!
//! Every other platform gets the pointer out of the way while this machine
//! drives a peer: Windows swaps the system cursors, macOS hides it and parks it
//! in a corner. Linux can do neither, and the reasons are different enough per
//! server to be worth stating rather than filed under "not implemented".
//!
//! **Wayland.** The compositor only lets a focused client with its own surface
//! touch the cursor, and Cursio is a background process with no surface at all.
//! Nor can the pointer be parked instead: there is no absolute warp, and the
//! relative-motion trick the capture backend uses to nudge the cursor off an
//! edge does not apply here — while this machine is dormant the InputCapture
//! portal already holds the pointer pinned at the barrier, so injected motion
//! goes to the peer rather than the local desktop.
//!
//! The supported way through is for Cursio to own a surface: a fullscreen GTK4
//! window, transparent, with an empty input region so it never takes a click,
//! and its cursor set to `none`. That is real work in `cursio-window` (GTK is
//! thread-affine and each window already owns a thread) rather than a call to
//! add here, so this stays an honest no-op until it exists.
//!
//! **X11.** XFixes can hide the pointer globally, which would work — but the
//! evdev backend that serves X11 sessions cannot detect an edge crossing in the
//! first place (see [`crate::capture`]), so a machine there hands the cursor
//! over by hotkey alone and a frozen pointer is the least of it.
//!
//! What actually happens meanwhile: the pointer stops moving, because local
//! input is grabbed, and stays visible where it stopped. That is stated once in
//! the log rather than left for the user to wonder about.
//!
//! The backend is chosen at construction from the running session type, so the
//! session code calls [`CursorControl`] the same way everywhere.

use crate::{CursorControl, InputError};

/// Which display server we're running under, as far as cursor control cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Server {
    /// Wayland: needs a surface Cursio does not have.
    Wayland,
    /// X11: XFixes could hide it, but this session cannot cross an edge anyway.
    X11,
    /// Neither detected: treat as unsupported.
    Unknown,
}

/// Detect the display server from the environment, matching how the desktop
/// session advertises itself.
fn detect_server() -> Server {
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland")
    {
        Server::Wayland
    } else if std::env::var_os("DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "x11")
    {
        Server::X11
    } else {
        Server::Unknown
    }
}

/// Linux cursor controller: a no-op that says so, once, with the reason for
/// this particular session.
pub struct LinuxCursor {
    server: Server,
    warned: bool,
}

impl LinuxCursor {
    /// Create the controller for the current session.
    pub fn new() -> Result<Self, InputError> {
        let server = detect_server();
        tracing::info!(?server, "linux cursor control");
        Ok(Self {
            server,
            warned: false,
        })
    }

    /// Log, once, what will happen to the pointer and why.
    fn warn_unsupported(&mut self) {
        if self.warned {
            return;
        }
        self.warned = true;
        match self.server {
            Server::Wayland => tracing::warn!(
                "this machine's pointer stays visible while it drives the peer: hiding it on \
                 Wayland needs a surface of our own, which Cursio does not have yet. It stops \
                 moving, since local input is grabbed"
            ),
            Server::X11 => tracing::warn!(
                "this machine's pointer stays visible while it drives the peer (X11 XFixes hiding \
                 is not wired up)"
            ),
            Server::Unknown => tracing::warn!(
                "no display server detected, so this machine's pointer cannot be hidden"
            ),
        }
    }
}

impl CursorControl for LinuxCursor {
    fn hide(&mut self) -> Result<(), InputError> {
        self.warn_unsupported();
        Ok(())
    }

    fn show(&mut self) -> Result<(), InputError> {
        // Nothing was hidden, so there is nothing to restore.
        Ok(())
    }

    fn backend(&self) -> &'static str {
        match self.server {
            Server::Wayland => "wayland-noop",
            Server::X11 => "x11-noop",
            Server::Unknown => "linux-noop",
        }
    }
}
