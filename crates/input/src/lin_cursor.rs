//! Linux cursor hiding — X11 real hide, Wayland honest no-op.
//!
//! Hiding the system cursor on Linux is display-server-specific:
//! - **X11** can hide it globally (XFixes) — not yet implemented here; the
//!   backend reports it and falls back to no-op until it's wired.
//! - **Wayland** does **not** let a background daemon hide the system cursor at
//!   all: the compositor only allows a focused client with its own surface to
//!   touch the cursor. Real Wayland hiding waits until Cursio also runs as a
//!   Wayland/libei client (see the cursor-hiding notes). Until then this is a
//!   deliberate no-op that logs once.
//!
//! The backend is chosen at construction from the running session type, so the
//! session code calls [`CursorControl`] the same way everywhere.

use crate::{CursorControl, InputError};

/// Which display server we're running under, as far as cursor control cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Server {
    /// Wayland: cannot hide from a daemon yet (no-op).
    Wayland,
    /// X11: hide is possible via XFixes (not yet implemented — no-op for now).
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

/// Linux cursor controller. Currently a no-op on every server (X11 XFixes hide
/// is a TODO), but it detects and reports the server so wiring the real X11
/// path later is a localized change.
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

    /// Log, once, that hiding isn't available on this server yet.
    fn warn_unsupported(&mut self) {
        if self.warned {
            return;
        }
        self.warned = true;
        match self.server {
            Server::Wayland => tracing::warn!(
                "cursor hide unsupported on Wayland from an evdev daemon; \
                 the cursor freezes but stays visible (needs the libei client)"
            ),
            Server::X11 => tracing::warn!("X11 cursor hide (XFixes) not implemented yet; no-op"),
            Server::Unknown => tracing::warn!("no known display server; cursor hide is a no-op"),
        }
    }
}

impl CursorControl for LinuxCursor {
    fn hide(&mut self) -> Result<(), InputError> {
        // TODO(x11): XFixesHideCursor when server == X11.
        self.warn_unsupported();
        Ok(())
    }

    fn show(&mut self) -> Result<(), InputError> {
        // No-op: nothing was hidden. (X11 XFixesShowCursor lands with hide.)
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
