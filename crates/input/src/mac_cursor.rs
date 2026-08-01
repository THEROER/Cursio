//! macOS cursor hiding, via CoreGraphics.
//!
//! When this Mac hands control to a peer its own pointer should disappear,
//! rather than sitting frozen on screen next to a cursor the user is actually
//! moving somewhere else.
//!
//! ## It only works while Cursio is the active application
//!
//! `CGDisplayHideCursor` hides the pointer for the calling application's
//! session, and macOS restores it whenever another application comes forward —
//! there is no supported way for a background process to hide the system
//! cursor outright. So this is real hiding that the OS may undo, not a no-op:
//! run from a terminal it works while that terminal is frontmost and quietly
//! stops when the user clicks elsewhere.
//!
//! Deliberately not worked around. The documented alternatives are private
//! window-server calls, and a background daemon that could hide the pointer
//! unconditionally is also one that could hide it permanently after a crash.
//! Linux makes the same trade for the same reason (see [`LinuxCursor`]).
//!
//! [`LinuxCursor`]: crate::LinuxCursor

use crate::{CursorControl, InputError};

/// Hides and restores this Mac's pointer.
pub struct MacCursor {
    /// Whether we currently hold a hide. The calls are reference-counted, so a
    /// second hide would need a second show — tracking it keeps both operations
    /// idempotent, as the trait requires.
    hidden: bool,
}

impl MacCursor {
    pub fn new() -> Result<Self, InputError> {
        Ok(Self { hidden: false })
    }
}

impl CursorControl for MacCursor {
    fn hide(&mut self) -> Result<(), InputError> {
        if self.hidden {
            return Ok(());
        }
        // SAFETY: the display id is a documented constant meaning "every
        // display"; the call takes no pointers.
        let status = unsafe { CGDisplayHideCursor(NULL_DISPLAY) };
        if status != 0 {
            return Err(InputError::Unavailable(format!(
                "macOS refused to hide the cursor (CGError {status})"
            )));
        }
        self.hidden = true;
        Ok(())
    }

    fn show(&mut self) -> Result<(), InputError> {
        if !self.hidden {
            return Ok(());
        }
        // Cleared first: if the call fails there is nothing to retry — the
        // pointer is either back or the window server already restored it —
        // and staying "hidden" would make every later show a no-op.
        self.hidden = false;
        // SAFETY: as above.
        let status = unsafe { CGDisplayShowCursor(NULL_DISPLAY) };
        if status != 0 {
            return Err(InputError::Unavailable(format!(
                "macOS refused to restore the cursor (CGError {status})"
            )));
        }
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "coregraphics"
    }
}

/// `kCGNullDirectDisplay`: apply to the whole session rather than one screen.
const NULL_DISPLAY: u32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The calls are reference-counted, so an unbalanced pair would leave the
    /// pointer invisible for the rest of the login session — which no amount of
    /// restarting Cursio would fix.
    #[test]
    fn hiding_twice_still_takes_one_show_to_undo() {
        let mut cursor = MacCursor::new().unwrap();
        assert!(!cursor.hidden);
        // The real calls are skipped here — a test must not hide the pointer of
        // whoever is running it — so the bookkeeping is what is checked.
        cursor.hidden = true;
        assert!(cursor.hide().is_ok());
        assert!(cursor.hidden, "a second hide must not stack");
        cursor.hidden = false;
        assert!(cursor.show().is_ok());
        assert!(!cursor.hidden, "showing when visible must stay a no-op");
    }
}
