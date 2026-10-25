//! Windows ↔ evdev keycode translation.
//!
//! Cursio's wire namespace for keys is the Linux **evdev** keycode set (see
//! `cursio_protocol::InputEvent::Key`). On Windows we neither capture nor inject
//! evdev codes natively, so both the capture side (RawInput → evdev) and the
//! injection side (evdev → Windows) go through the tables here.
//!
//! We translate by **physical position**, using the PS/2 **scancode set 1** as
//! the pivot, because that is what maps cleanly onto evdev keycodes: evdev's
//! main keycodes were themselves derived from set-1 scancodes, so most keys are
//! a straight offset. Position-based (not VK-based) mapping means a key streamed
//! from Windows lands on the physically-corresponding key on Linux regardless of
//! the layout either side has active — matching the "acts like a second
//! keyboard plugged in" model.
//!
//! Extended keys (arrows, nav cluster, right-hand modifiers, keypad Enter, …)
//! arrive from RawInput with the `RI_KEY_E0` flag and a set-1 make code that
//! collides with a non-extended key; those are handled explicitly.

use crate::gesture_shortcut::Direction;
use cursio_protocol::keys::*;

/// Translate a RawInput keyboard event to an evdev keycode.
///
/// `make_code` is the set-1 scancode from `RAWKEYBOARD::MakeCode`; `e0` is true
/// when the `RI_KEY_E0` prefix flag was set (the key is an "extended" one).
/// Returns `None` for codes we don't map (fake shift injected with some E0
/// sequences, unknown keys).
pub fn scancode_to_evdev(make_code: u16, e0: bool) -> Option<u16> {
    if e0 {
        // Extended keys: the make code alone is ambiguous (e.g. 0x1C is both
        // Enter and keypad-Enter), so the E0 prefix disambiguates.
        return e0_scancode_to_evdev(make_code);
    }
    base_scancode_to_evdev(make_code)
}

/// The modifier Windows zooms with when held over the wheel.
pub(crate) const ZOOM_MODIFIER: u16 = KEY_LEFTCTRL;

/// The shortcut Windows binds a touchpad swipe to by default, as evdev keys to
/// press in order and release in reverse — what a peer's swipe is replayed as,
/// since the gesture itself cannot be injected. Content follows the fingers,
/// as on the touchpad: four fingers sliding left bring in the desktop on the
/// right. `None` for a swipe Windows gives no meaning to.
pub(crate) fn swipe_chord(fingers: u8, direction: Direction) -> Option<&'static [u16]> {
    Some(match (fingers, direction) {
        (3 | 4, Direction::Up) => &[KEY_LEFTMETA, KEY_TAB],
        (3 | 4, Direction::Down) => &[KEY_LEFTMETA, KEY_D],
        (3, Direction::Left) => &[KEY_LEFTALT, KEY_LEFTSHIFT, KEY_TAB],
        (3, Direction::Right) => &[KEY_LEFTALT, KEY_TAB],
        (4, Direction::Left) => &[KEY_LEFTMETA, KEY_LEFTCTRL, KEY_RIGHT],
        (4, Direction::Right) => &[KEY_LEFTMETA, KEY_LEFTCTRL, KEY_LEFT],
        _ => return None,
    })
}

/// Translate an evdev keycode to what Windows injection needs: a set-1 scancode
/// and whether it is an extended (E0) key. Returns `None` for codes we can't
/// represent as a Windows scancode.
pub fn evdev_to_scancode(evdev: u16) -> Option<(u16, bool)> {
    // Walk the same tables in reverse. The key sets are small, so a linear
    // scan is fine and keeps a single source of truth.
    for &(sc, ev) in BASE_TABLE {
        if ev == evdev {
            return Some((sc, false));
        }
    }
    for &(sc, ev) in E0_TABLE {
        if ev == evdev {
            return Some((sc, true));
        }
    }
    None
}

fn base_scancode_to_evdev(make_code: u16) -> Option<u16> {
    BASE_TABLE
        .iter()
        .find(|(sc, _)| *sc == make_code)
        .map(|(_, ev)| *ev)
}

fn e0_scancode_to_evdev(make_code: u16) -> Option<u16> {
    E0_TABLE
        .iter()
        .find(|(sc, _)| *sc == make_code)
        .map(|(_, ev)| *ev)
}

/// Non-extended set-1 scancode → evdev. For the main block, evdev keycode ==
/// scancode, but we spell the table out so it's auditable rather than assuming
/// the identity holds everywhere.
const BASE_TABLE: &[(u16, u16)] = &[
    (0x01, KEY_ESC),
    (0x02, KEY_1),
    (0x03, KEY_2),
    (0x04, KEY_3),
    (0x05, KEY_4),
    (0x06, KEY_5),
    (0x07, KEY_6),
    (0x08, KEY_7),
    (0x09, KEY_8),
    (0x0A, KEY_9),
    (0x0B, KEY_0),
    (0x0C, KEY_MINUS),
    (0x0D, KEY_EQUAL),
    (0x0E, KEY_BACKSPACE),
    (0x0F, KEY_TAB),
    (0x10, KEY_Q),
    (0x11, KEY_W),
    (0x12, KEY_E),
    (0x13, KEY_R),
    (0x14, KEY_T),
    (0x15, KEY_Y),
    (0x16, KEY_U),
    (0x17, KEY_I),
    (0x18, KEY_O),
    (0x19, KEY_P),
    (0x1A, KEY_LEFTBRACE),
    (0x1B, KEY_RIGHTBRACE),
    (0x1C, KEY_ENTER),
    (0x1D, KEY_LEFTCTRL),
    (0x1E, KEY_A),
    (0x1F, KEY_S),
    (0x20, KEY_D),
    (0x21, KEY_F),
    (0x22, KEY_G),
    (0x23, KEY_H),
    (0x24, KEY_J),
    (0x25, KEY_K),
    (0x26, KEY_L),
    (0x27, KEY_SEMICOLON),
    (0x28, KEY_APOSTROPHE),
    (0x29, KEY_GRAVE),
    (0x2A, KEY_LEFTSHIFT),
    (0x2B, KEY_BACKSLASH),
    (0x2C, KEY_Z),
    (0x2D, KEY_X),
    (0x2E, KEY_C),
    (0x2F, KEY_V),
    (0x30, KEY_B),
    (0x31, KEY_N),
    (0x32, KEY_M),
    (0x33, KEY_COMMA),
    (0x34, KEY_DOT),
    (0x35, KEY_SLASH),
    (0x36, KEY_RIGHTSHIFT),
    (0x37, KEY_KPASTERISK),
    (0x38, KEY_LEFTALT),
    (0x39, KEY_SPACE),
    (0x3A, KEY_CAPSLOCK),
    (0x3B, KEY_F1),
    (0x3C, KEY_F2),
    (0x3D, KEY_F3),
    (0x3E, KEY_F4),
    (0x3F, KEY_F5),
    (0x40, KEY_F6),
    (0x41, KEY_F7),
    (0x42, KEY_F8),
    (0x43, KEY_F9),
    (0x44, KEY_F10),
    (0x45, KEY_NUMLOCK),
    (0x46, KEY_SCROLLLOCK),
    (0x47, KEY_KP7),
    (0x48, KEY_KP8),
    (0x49, KEY_KP9),
    (0x4A, KEY_KPMINUS),
    (0x4B, KEY_KP4),
    (0x4C, KEY_KP5),
    (0x4D, KEY_KP6),
    (0x4E, KEY_KPPLUS),
    (0x4F, KEY_KP1),
    (0x50, KEY_KP2),
    (0x51, KEY_KP3),
    (0x52, KEY_KP0),
    (0x53, KEY_KPDOT),
    (0x57, KEY_F11),
    (0x58, KEY_F12),
];

/// Extended (E0-prefixed) set-1 scancode → evdev. These share make codes with
/// BASE_TABLE entries, so they're only valid when RI_KEY_E0 was set.
const E0_TABLE: &[(u16, u16)] = &[
    (0x1C, KEY_KPENTER),
    (0x1D, KEY_RIGHTCTRL),
    (0x35, KEY_KPSLASH),
    (0x38, KEY_RIGHTALT),
    (0x47, KEY_HOME),
    (0x48, KEY_UP),
    (0x49, KEY_PAGEUP),
    (0x4B, KEY_LEFT),
    (0x4D, KEY_RIGHT),
    (0x4F, KEY_END),
    (0x50, KEY_DOWN),
    (0x51, KEY_PAGEDOWN),
    (0x52, KEY_INSERT),
    (0x53, KEY_DELETE),
    (0x5B, KEY_LEFTMETA),
    (0x5C, KEY_RIGHTMETA),
    (0x5D, KEY_COMPOSE),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_round_trip() {
        for evdev in [KEY_A, KEY_Z, KEY_0, KEY_9, KEY_SPACE, KEY_ENTER] {
            let (sc, e0) = evdev_to_scancode(evdev).expect("mappable");
            assert!(!e0, "main-block keys are not extended");
            assert_eq!(scancode_to_evdev(sc, false), Some(evdev));
        }
    }

    #[test]
    fn extended_keys_need_the_e0_flag() {
        // 0x1C is Enter without E0, keypad-Enter with it.
        assert_eq!(scancode_to_evdev(0x1C, false), Some(KEY_ENTER));
        assert_eq!(scancode_to_evdev(0x1C, true), Some(KEY_KPENTER));
        // Right ctrl vs left ctrl on 0x1D.
        assert_eq!(scancode_to_evdev(0x1D, false), Some(KEY_LEFTCTRL));
        assert_eq!(scancode_to_evdev(0x1D, true), Some(KEY_RIGHTCTRL));
    }

    #[test]
    fn arrows_round_trip_as_extended() {
        for evdev in [KEY_UP, KEY_DOWN, KEY_LEFT, KEY_RIGHT] {
            let (sc, e0) = evdev_to_scancode(evdev).expect("mappable");
            assert!(e0, "arrows are extended keys");
            assert_eq!(scancode_to_evdev(sc, true), Some(evdev));
        }
    }

    #[test]
    fn unknown_scancode_is_none() {
        assert_eq!(scancode_to_evdev(0xFE, false), None);
        assert_eq!(evdev_to_scancode(0xFFFF), None);
    }
}
