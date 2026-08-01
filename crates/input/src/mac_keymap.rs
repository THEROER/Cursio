//! evdev → macOS keycode translation, and what to do about Command.
//!
//! Cursio's wire namespace for keys is the Linux **evdev** keycode set (see
//! [`cursio_protocol::InputEvent::Key`]). macOS wants a `CGKeyCode`, which is
//! neither evdev nor a PS/2 scancode but Apple's own virtual keycode table
//! (`kVK_*` in `HIToolbox/Events.h`) — a layout-independent, physical-position
//! numbering that happens to be ordered by nothing in particular. There is no
//! arithmetic relation to evdev, so the table below is the whole mapping.
//!
//! ## The Command key a PC keyboard does not have
//!
//! A Mac's shortcuts live on Command; a PC keyboard has Control, Alt and a
//! Windows key. Mapping purely by name (Meta → Command) is faithful to the
//! hardware but means every shortcut moves under the little finger's *other*
//! neighbour: Ctrl+C stops copying and Win+C starts. That is correct and
//! useless, because the muscle memory belongs to Ctrl.
//!
//! So [`ModifierStyle::Pc`] — the default — swaps the two: Control becomes
//! Command and Meta becomes Control, which is what every long-lived
//! keyboard-sharing tool ends up doing. [`ModifierStyle::Mac`] keeps the names,
//! for an Apple keyboard plugged into the sending machine, where the physical
//! keys already sit where macOS expects them.
//!
//! Option/Alt is never touched: it is the same key under both names.

/// How a sending keyboard's modifiers land on this Mac.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModifierStyle {
    /// The sender types on a PC keyboard: its Control acts as Command here and
    /// its Meta (Windows) key acts as Control, so Ctrl+C copies.
    #[default]
    Pc,
    /// The sender's modifiers keep their names: Control is Control, Meta is
    /// Command. Right for an Apple keyboard.
    Mac,
}

impl ModifierStyle {
    /// Apply the style *before* the keycode lookup, so the table itself only
    /// ever states the by-name mapping.
    ///
    /// Its own inverse: swapping Control and Meta twice returns the original,
    /// which is what lets [`mac_to_evdev`] reuse it to undo the swap.
    fn remap(self, evdev: u16) -> u16 {
        match self {
            Self::Mac => evdev,
            Self::Pc => match evdev {
                KEY_LEFTCTRL => KEY_LEFTMETA,
                KEY_RIGHTCTRL => KEY_RIGHTMETA,
                KEY_LEFTMETA => KEY_LEFTCTRL,
                KEY_RIGHTMETA => KEY_RIGHTCTRL,
                other => other,
            },
        }
    }
}

/// Translate one evdev keycode to a macOS virtual keycode.
///
/// `None` for keys macOS has no equivalent of (Scroll Lock, Menu, Print
/// Screen); the caller drops those rather than guessing at a substitute.
pub fn evdev_to_mac(evdev: u16, style: ModifierStyle) -> Option<u16> {
    let evdev = style.remap(evdev);
    TABLE
        .iter()
        .find(|(code, _)| *code == evdev)
        .map(|(_, mac)| *mac)
}

/// Translate a macOS virtual keycode back to evdev, for the capture side.
///
/// The style is undone here, not applied: a Mac sending Command+C to a PC peer
/// must put Control on the wire, or the swap that [`evdev_to_mac`] performs
/// would have to be configured identically on both machines to cancel out.
pub fn mac_to_evdev(mac_key: u16, style: ModifierStyle) -> Option<u16> {
    let evdev = TABLE
        .iter()
        .find(|(_, mac)| *mac == mac_key)
        .map(|(code, _)| *code)?;
    // `remap` swaps Control and Meta, and swapping twice is the identity — so
    // the same function inverts itself.
    Some(style.remap(evdev))
}

/// The `CGEventFlags` bits a macOS modifier keycode contributes, or `None` if
/// it is not a modifier.
///
/// Two bits, not one: the documented mask says *a* Command is down, while the
/// low device-dependent bit says *which* Command. Applications that
/// distinguish left from right — and the ones that re-derive modifier state
/// from the raw flags rather than trusting the mask — read the second.
pub fn modifier_flags(mac_key: u16) -> Option<u64> {
    let (mask, side) = match mac_key {
        VK_CONTROL => (FLAG_CONTROL, DEVICE_LEFT_CONTROL),
        VK_RIGHT_CONTROL => (FLAG_CONTROL, DEVICE_RIGHT_CONTROL),
        VK_SHIFT => (FLAG_SHIFT, DEVICE_LEFT_SHIFT),
        VK_RIGHT_SHIFT => (FLAG_SHIFT, DEVICE_RIGHT_SHIFT),
        VK_OPTION => (FLAG_OPTION, DEVICE_LEFT_OPTION),
        VK_RIGHT_OPTION => (FLAG_OPTION, DEVICE_RIGHT_OPTION),
        VK_COMMAND => (FLAG_COMMAND, DEVICE_LEFT_COMMAND),
        VK_RIGHT_COMMAND => (FLAG_COMMAND, DEVICE_RIGHT_COMMAND),
        _ => return None,
    };
    Some(mask | side)
}

/// Every modifier keycode, so the capture side can work out which one moved.
///
/// A `flagsChanged` event names a key but carries the whole mask, and the mask
/// is the only thing that says up or down — so capture compares the new mask
/// against the previous one, over this list.
pub const MODIFIER_KEYS: &[u16] = &[
    VK_CONTROL,
    VK_RIGHT_CONTROL,
    VK_SHIFT,
    VK_RIGHT_SHIFT,
    VK_OPTION,
    VK_RIGHT_OPTION,
    VK_COMMAND,
    VK_RIGHT_COMMAND,
];

/// Whether this macOS keycode is Caps Lock.
///
/// Caps Lock is not a held modifier on macOS but a latch: its flag survives the
/// key coming back up, so it toggles on press instead of tracking the key.
pub fn is_caps_lock(mac_key: u16) -> bool {
    mac_key == VK_CAPS_LOCK
}

/// Flags a non-modifier key contributes purely by *being* that key, which real
/// hardware sets and some applications insist on.
///
/// The keypad flag is how a Mac tells `5` from keypad-`5` once a layout has
/// turned both into the same character, and the function flag is what the
/// arrows, the navigation cluster and the function row carry on a real Apple
/// keyboard — Terminal and full-keyboard-access both read it.
pub fn intrinsic_flags(mac_key: u16) -> u64 {
    if KEYPAD_KEYS.contains(&mac_key) {
        return FLAG_NUMERIC_PAD;
    }
    if FUNCTION_KEYS.contains(&mac_key) {
        return FLAG_SECONDARY_FN;
    }
    0
}

// `CGEventFlags`, from CoreGraphics/CGEventTypes.h.
const FLAG_ALPHA_SHIFT: u64 = 0x0001_0000;
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const FLAG_NUMERIC_PAD: u64 = 0x0020_0000;
const FLAG_SECONDARY_FN: u64 = 0x0080_0000;

/// The Caps Lock latch, exported because it is toggled rather than tracked.
pub const CAPS_LOCK_FLAG: u64 = FLAG_ALPHA_SHIFT;

// Device-dependent modifier bits (`NX_DEVICE*KEYMASK` in IOKit's ev_keymap.h),
// which say which side of the keyboard a modifier came from.
const DEVICE_LEFT_CONTROL: u64 = 0x0000_0001;
const DEVICE_LEFT_SHIFT: u64 = 0x0000_0002;
const DEVICE_RIGHT_SHIFT: u64 = 0x0000_0004;
const DEVICE_LEFT_COMMAND: u64 = 0x0000_0008;
const DEVICE_RIGHT_COMMAND: u64 = 0x0000_0010;
const DEVICE_LEFT_OPTION: u64 = 0x0000_0020;
const DEVICE_RIGHT_OPTION: u64 = 0x0000_0040;
const DEVICE_RIGHT_CONTROL: u64 = 0x0000_2000;

// evdev keycodes (`KEY_*` from linux/input-event-codes.h). Spelled out here as
// they are in `win_keymap`, so each platform's table reads as "this key is that
// key" and can be audited without cross-referencing the other.
const KEY_ESC: u16 = 1;
const KEY_1: u16 = 2;
const KEY_2: u16 = 3;
const KEY_3: u16 = 4;
const KEY_4: u16 = 5;
const KEY_5: u16 = 6;
const KEY_6: u16 = 7;
const KEY_7: u16 = 8;
const KEY_8: u16 = 9;
const KEY_9: u16 = 10;
const KEY_0: u16 = 11;
const KEY_MINUS: u16 = 12;
const KEY_EQUAL: u16 = 13;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_Q: u16 = 16;
const KEY_W: u16 = 17;
const KEY_E: u16 = 18;
const KEY_R: u16 = 19;
const KEY_T: u16 = 20;
const KEY_Y: u16 = 21;
const KEY_U: u16 = 22;
const KEY_I: u16 = 23;
const KEY_O: u16 = 24;
const KEY_P: u16 = 25;
const KEY_LEFTBRACE: u16 = 26;
const KEY_RIGHTBRACE: u16 = 27;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_A: u16 = 30;
const KEY_S: u16 = 31;
const KEY_D: u16 = 32;
const KEY_F: u16 = 33;
const KEY_G: u16 = 34;
const KEY_H: u16 = 35;
const KEY_J: u16 = 36;
const KEY_K: u16 = 37;
const KEY_L: u16 = 38;
const KEY_SEMICOLON: u16 = 39;
const KEY_APOSTROPHE: u16 = 40;
const KEY_GRAVE: u16 = 41;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_BACKSLASH: u16 = 43;
const KEY_Z: u16 = 44;
const KEY_X: u16 = 45;
const KEY_C: u16 = 46;
const KEY_V: u16 = 47;
const KEY_B: u16 = 48;
const KEY_N: u16 = 49;
const KEY_M: u16 = 50;
const KEY_COMMA: u16 = 51;
const KEY_DOT: u16 = 52;
const KEY_SLASH: u16 = 53;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_KPASTERISK: u16 = 55;
const KEY_LEFTALT: u16 = 56;
const KEY_SPACE: u16 = 57;
const KEY_CAPSLOCK: u16 = 58;
const KEY_F1: u16 = 59;
const KEY_F2: u16 = 60;
const KEY_F3: u16 = 61;
const KEY_F4: u16 = 62;
const KEY_F5: u16 = 63;
const KEY_F6: u16 = 64;
const KEY_F7: u16 = 65;
const KEY_F8: u16 = 66;
const KEY_F9: u16 = 67;
const KEY_F10: u16 = 68;
const KEY_NUMLOCK: u16 = 69;
const KEY_KP7: u16 = 71;
const KEY_KP8: u16 = 72;
const KEY_KP9: u16 = 73;
const KEY_KPMINUS: u16 = 74;
const KEY_KP4: u16 = 75;
const KEY_KP5: u16 = 76;
const KEY_KP6: u16 = 77;
const KEY_KPPLUS: u16 = 78;
const KEY_KP1: u16 = 79;
const KEY_KP2: u16 = 80;
const KEY_KP3: u16 = 81;
const KEY_KP0: u16 = 82;
const KEY_KPDOT: u16 = 83;
const KEY_102ND: u16 = 86;
const KEY_F11: u16 = 87;
const KEY_F12: u16 = 88;
const KEY_KPENTER: u16 = 96;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_KPSLASH: u16 = 98;
const KEY_RIGHTALT: u16 = 100;
const KEY_HOME: u16 = 102;
const KEY_UP: u16 = 103;
const KEY_PAGEUP: u16 = 104;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_END: u16 = 107;
const KEY_DOWN: u16 = 108;
const KEY_PAGEDOWN: u16 = 109;
const KEY_INSERT: u16 = 110;
const KEY_DELETE: u16 = 111;
const KEY_MUTE: u16 = 113;
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_VOLUMEUP: u16 = 115;
const KEY_KPEQUAL: u16 = 117;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
const KEY_F13: u16 = 183;
const KEY_F14: u16 = 184;
const KEY_F15: u16 = 185;
const KEY_F16: u16 = 186;
const KEY_F17: u16 = 187;
const KEY_F18: u16 = 188;
const KEY_F19: u16 = 189;
const KEY_F20: u16 = 190;

// macOS virtual keycodes (`kVK_*` from HIToolbox/Events.h). Named so the table
// is readable; the numbering itself follows no pattern worth deriving.
const VK_A: u16 = 0x00;
const VK_S: u16 = 0x01;
const VK_D: u16 = 0x02;
const VK_F: u16 = 0x03;
const VK_H: u16 = 0x04;
const VK_G: u16 = 0x05;
const VK_Z: u16 = 0x06;
const VK_X: u16 = 0x07;
const VK_C: u16 = 0x08;
const VK_V: u16 = 0x09;
const VK_ISO_SECTION: u16 = 0x0A;
const VK_B: u16 = 0x0B;
const VK_Q: u16 = 0x0C;
const VK_W: u16 = 0x0D;
const VK_E: u16 = 0x0E;
const VK_R: u16 = 0x0F;
const VK_Y: u16 = 0x10;
const VK_T: u16 = 0x11;
const VK_1: u16 = 0x12;
const VK_2: u16 = 0x13;
const VK_3: u16 = 0x14;
const VK_4: u16 = 0x15;
const VK_6: u16 = 0x16;
const VK_5: u16 = 0x17;
const VK_EQUAL: u16 = 0x18;
const VK_9: u16 = 0x19;
const VK_7: u16 = 0x1A;
const VK_MINUS: u16 = 0x1B;
const VK_8: u16 = 0x1C;
const VK_0: u16 = 0x1D;
const VK_RIGHT_BRACKET: u16 = 0x1E;
const VK_O: u16 = 0x1F;
const VK_U: u16 = 0x20;
const VK_LEFT_BRACKET: u16 = 0x21;
const VK_I: u16 = 0x22;
const VK_P: u16 = 0x23;
const VK_RETURN: u16 = 0x24;
const VK_L: u16 = 0x25;
const VK_J: u16 = 0x26;
const VK_QUOTE: u16 = 0x27;
const VK_K: u16 = 0x28;
const VK_SEMICOLON: u16 = 0x29;
const VK_BACKSLASH: u16 = 0x2A;
const VK_COMMA: u16 = 0x2B;
const VK_SLASH: u16 = 0x2C;
const VK_N: u16 = 0x2D;
const VK_M: u16 = 0x2E;
const VK_PERIOD: u16 = 0x2F;
const VK_TAB: u16 = 0x30;
const VK_SPACE: u16 = 0x31;
const VK_GRAVE: u16 = 0x32;
/// Backspace. Apple's "Delete" is the key a PC calls Backspace; forward delete
/// is a separate keycode.
const VK_DELETE: u16 = 0x33;
const VK_ESCAPE: u16 = 0x35;
const VK_RIGHT_COMMAND: u16 = 0x36;
const VK_COMMAND: u16 = 0x37;
const VK_SHIFT: u16 = 0x38;
const VK_CAPS_LOCK: u16 = 0x39;
const VK_OPTION: u16 = 0x3A;
const VK_CONTROL: u16 = 0x3B;
const VK_RIGHT_SHIFT: u16 = 0x3C;
const VK_RIGHT_OPTION: u16 = 0x3D;
const VK_RIGHT_CONTROL: u16 = 0x3E;
const VK_F17: u16 = 0x40;
const VK_KEYPAD_DECIMAL: u16 = 0x41;
const VK_KEYPAD_MULTIPLY: u16 = 0x43;
const VK_KEYPAD_PLUS: u16 = 0x45;
/// The Mac's Clear key sits where a PC keyboard puts Num Lock.
const VK_KEYPAD_CLEAR: u16 = 0x47;
const VK_VOLUME_UP: u16 = 0x48;
const VK_VOLUME_DOWN: u16 = 0x49;
const VK_MUTE: u16 = 0x4A;
const VK_KEYPAD_DIVIDE: u16 = 0x4B;
const VK_KEYPAD_ENTER: u16 = 0x4C;
const VK_KEYPAD_MINUS: u16 = 0x4E;
const VK_F18: u16 = 0x4F;
const VK_F19: u16 = 0x50;
const VK_KEYPAD_EQUALS: u16 = 0x51;
const VK_KEYPAD_0: u16 = 0x52;
const VK_KEYPAD_1: u16 = 0x53;
const VK_KEYPAD_2: u16 = 0x54;
const VK_KEYPAD_3: u16 = 0x55;
const VK_KEYPAD_4: u16 = 0x56;
const VK_KEYPAD_5: u16 = 0x57;
const VK_KEYPAD_6: u16 = 0x58;
const VK_KEYPAD_7: u16 = 0x59;
const VK_F20: u16 = 0x5A;
const VK_KEYPAD_8: u16 = 0x5B;
const VK_KEYPAD_9: u16 = 0x5C;
const VK_F5: u16 = 0x60;
const VK_F6: u16 = 0x61;
const VK_F7: u16 = 0x62;
const VK_F3: u16 = 0x63;
const VK_F8: u16 = 0x64;
const VK_F9: u16 = 0x65;
const VK_F11: u16 = 0x67;
const VK_F13: u16 = 0x69;
const VK_F16: u16 = 0x6A;
const VK_F14: u16 = 0x6B;
const VK_F10: u16 = 0x6D;
const VK_F12: u16 = 0x6F;
const VK_F15: u16 = 0x71;
/// Insert. macOS calls the key in that position Help.
const VK_HELP: u16 = 0x72;
const VK_HOME: u16 = 0x73;
const VK_PAGE_UP: u16 = 0x74;
const VK_FORWARD_DELETE: u16 = 0x75;
const VK_F4: u16 = 0x76;
const VK_END: u16 = 0x77;
const VK_F2: u16 = 0x78;
const VK_PAGE_DOWN: u16 = 0x79;
const VK_F1: u16 = 0x7A;
const VK_LEFT_ARROW: u16 = 0x7B;
const VK_RIGHT_ARROW: u16 = 0x7C;
const VK_DOWN_ARROW: u16 = 0x7D;
const VK_UP_ARROW: u16 = 0x7E;

/// Keys that carry the numeric-pad flag on real hardware.
const KEYPAD_KEYS: &[u16] = &[
    VK_KEYPAD_0,
    VK_KEYPAD_1,
    VK_KEYPAD_2,
    VK_KEYPAD_3,
    VK_KEYPAD_4,
    VK_KEYPAD_5,
    VK_KEYPAD_6,
    VK_KEYPAD_7,
    VK_KEYPAD_8,
    VK_KEYPAD_9,
    VK_KEYPAD_DECIMAL,
    VK_KEYPAD_PLUS,
    VK_KEYPAD_MINUS,
    VK_KEYPAD_MULTIPLY,
    VK_KEYPAD_DIVIDE,
    VK_KEYPAD_EQUALS,
    VK_KEYPAD_CLEAR,
    VK_KEYPAD_ENTER,
];

/// Keys that carry the secondary-function flag on real hardware: the arrows,
/// the navigation cluster and the function row.
const FUNCTION_KEYS: &[u16] = &[
    VK_LEFT_ARROW,
    VK_RIGHT_ARROW,
    VK_UP_ARROW,
    VK_DOWN_ARROW,
    VK_HOME,
    VK_END,
    VK_PAGE_UP,
    VK_PAGE_DOWN,
    VK_FORWARD_DELETE,
    VK_HELP,
    VK_F1,
    VK_F2,
    VK_F3,
    VK_F4,
    VK_F5,
    VK_F6,
    VK_F7,
    VK_F8,
    VK_F9,
    VK_F10,
    VK_F11,
    VK_F12,
    VK_F13,
    VK_F14,
    VK_F15,
    VK_F16,
    VK_F17,
    VK_F18,
    VK_F19,
    VK_F20,
];

/// evdev keycode → macOS virtual keycode, by name. Any Control/Command swap has
/// already happened in [`ModifierStyle::remap`] before this is consulted.
const TABLE: &[(u16, u16)] = &[
    (KEY_ESC, VK_ESCAPE),
    (KEY_1, VK_1),
    (KEY_2, VK_2),
    (KEY_3, VK_3),
    (KEY_4, VK_4),
    (KEY_5, VK_5),
    (KEY_6, VK_6),
    (KEY_7, VK_7),
    (KEY_8, VK_8),
    (KEY_9, VK_9),
    (KEY_0, VK_0),
    (KEY_MINUS, VK_MINUS),
    (KEY_EQUAL, VK_EQUAL),
    (KEY_BACKSPACE, VK_DELETE),
    (KEY_TAB, VK_TAB),
    (KEY_Q, VK_Q),
    (KEY_W, VK_W),
    (KEY_E, VK_E),
    (KEY_R, VK_R),
    (KEY_T, VK_T),
    (KEY_Y, VK_Y),
    (KEY_U, VK_U),
    (KEY_I, VK_I),
    (KEY_O, VK_O),
    (KEY_P, VK_P),
    (KEY_LEFTBRACE, VK_LEFT_BRACKET),
    (KEY_RIGHTBRACE, VK_RIGHT_BRACKET),
    (KEY_ENTER, VK_RETURN),
    (KEY_LEFTCTRL, VK_CONTROL),
    (KEY_A, VK_A),
    (KEY_S, VK_S),
    (KEY_D, VK_D),
    (KEY_F, VK_F),
    (KEY_G, VK_G),
    (KEY_H, VK_H),
    (KEY_J, VK_J),
    (KEY_K, VK_K),
    (KEY_L, VK_L),
    (KEY_SEMICOLON, VK_SEMICOLON),
    (KEY_APOSTROPHE, VK_QUOTE),
    (KEY_GRAVE, VK_GRAVE),
    (KEY_LEFTSHIFT, VK_SHIFT),
    (KEY_BACKSLASH, VK_BACKSLASH),
    (KEY_Z, VK_Z),
    (KEY_X, VK_X),
    (KEY_C, VK_C),
    (KEY_V, VK_V),
    (KEY_B, VK_B),
    (KEY_N, VK_N),
    (KEY_M, VK_M),
    (KEY_COMMA, VK_COMMA),
    (KEY_DOT, VK_PERIOD),
    (KEY_SLASH, VK_SLASH),
    (KEY_RIGHTSHIFT, VK_RIGHT_SHIFT),
    (KEY_KPASTERISK, VK_KEYPAD_MULTIPLY),
    (KEY_LEFTALT, VK_OPTION),
    (KEY_SPACE, VK_SPACE),
    (KEY_CAPSLOCK, VK_CAPS_LOCK),
    (KEY_F1, VK_F1),
    (KEY_F2, VK_F2),
    (KEY_F3, VK_F3),
    (KEY_F4, VK_F4),
    (KEY_F5, VK_F5),
    (KEY_F6, VK_F6),
    (KEY_F7, VK_F7),
    (KEY_F8, VK_F8),
    (KEY_F9, VK_F9),
    (KEY_F10, VK_F10),
    (KEY_NUMLOCK, VK_KEYPAD_CLEAR),
    (KEY_KP7, VK_KEYPAD_7),
    (KEY_KP8, VK_KEYPAD_8),
    (KEY_KP9, VK_KEYPAD_9),
    (KEY_KPMINUS, VK_KEYPAD_MINUS),
    (KEY_KP4, VK_KEYPAD_4),
    (KEY_KP5, VK_KEYPAD_5),
    (KEY_KP6, VK_KEYPAD_6),
    (KEY_KPPLUS, VK_KEYPAD_PLUS),
    (KEY_KP1, VK_KEYPAD_1),
    (KEY_KP2, VK_KEYPAD_2),
    (KEY_KP3, VK_KEYPAD_3),
    (KEY_KP0, VK_KEYPAD_0),
    (KEY_KPDOT, VK_KEYPAD_DECIMAL),
    (KEY_102ND, VK_ISO_SECTION),
    (KEY_F11, VK_F11),
    (KEY_F12, VK_F12),
    (KEY_KPENTER, VK_KEYPAD_ENTER),
    (KEY_RIGHTCTRL, VK_RIGHT_CONTROL),
    (KEY_KPSLASH, VK_KEYPAD_DIVIDE),
    (KEY_RIGHTALT, VK_RIGHT_OPTION),
    (KEY_HOME, VK_HOME),
    (KEY_UP, VK_UP_ARROW),
    (KEY_PAGEUP, VK_PAGE_UP),
    (KEY_LEFT, VK_LEFT_ARROW),
    (KEY_RIGHT, VK_RIGHT_ARROW),
    (KEY_END, VK_END),
    (KEY_DOWN, VK_DOWN_ARROW),
    (KEY_PAGEDOWN, VK_PAGE_DOWN),
    (KEY_INSERT, VK_HELP),
    (KEY_DELETE, VK_FORWARD_DELETE),
    (KEY_MUTE, VK_MUTE),
    (KEY_VOLUMEDOWN, VK_VOLUME_DOWN),
    (KEY_VOLUMEUP, VK_VOLUME_UP),
    (KEY_KPEQUAL, VK_KEYPAD_EQUALS),
    (KEY_LEFTMETA, VK_COMMAND),
    (KEY_RIGHTMETA, VK_RIGHT_COMMAND),
    (KEY_F13, VK_F13),
    (KEY_F14, VK_F14),
    (KEY_F15, VK_F15),
    (KEY_F16, VK_F16),
    (KEY_F17, VK_F17),
    (KEY_F18, VK_F18),
    (KEY_F19, VK_F19),
    (KEY_F20, VK_F20),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_map_to_apples_numbering() {
        // A worthwhile check precisely because the numbering is arbitrary: if
        // it were derivable, the table would not need to exist.
        assert_eq!(evdev_to_mac(KEY_A, ModifierStyle::Mac), Some(VK_A));
        assert_eq!(evdev_to_mac(KEY_Z, ModifierStyle::Mac), Some(VK_Z));
        assert_eq!(evdev_to_mac(KEY_5, ModifierStyle::Mac), Some(VK_5));
        assert_eq!(evdev_to_mac(KEY_6, ModifierStyle::Mac), Some(VK_6));
    }

    #[test]
    fn a_pc_keyboards_control_becomes_command() {
        assert_eq!(
            evdev_to_mac(KEY_LEFTCTRL, ModifierStyle::Pc),
            Some(VK_COMMAND)
        );
        assert_eq!(
            evdev_to_mac(KEY_RIGHTCTRL, ModifierStyle::Pc),
            Some(VK_RIGHT_COMMAND)
        );
        assert_eq!(
            evdev_to_mac(KEY_LEFTMETA, ModifierStyle::Pc),
            Some(VK_CONTROL)
        );
    }

    #[test]
    fn an_apple_keyboards_modifiers_keep_their_names() {
        assert_eq!(
            evdev_to_mac(KEY_LEFTCTRL, ModifierStyle::Mac),
            Some(VK_CONTROL)
        );
        assert_eq!(
            evdev_to_mac(KEY_LEFTMETA, ModifierStyle::Mac),
            Some(VK_COMMAND)
        );
    }

    /// Option is the same key under either name, so neither style may move it.
    #[test]
    fn alt_is_option_whichever_keyboard_is_sending() {
        for style in [ModifierStyle::Pc, ModifierStyle::Mac] {
            assert_eq!(evdev_to_mac(KEY_LEFTALT, style), Some(VK_OPTION));
            assert_eq!(evdev_to_mac(KEY_RIGHTALT, style), Some(VK_RIGHT_OPTION));
        }
    }

    #[test]
    fn backspace_and_delete_are_different_keys() {
        assert_eq!(
            evdev_to_mac(KEY_BACKSPACE, ModifierStyle::Pc),
            Some(VK_DELETE)
        );
        assert_eq!(
            evdev_to_mac(KEY_DELETE, ModifierStyle::Pc),
            Some(VK_FORWARD_DELETE)
        );
    }

    #[test]
    fn each_side_of_a_modifier_reports_its_own_bit() {
        let left = modifier_flags(VK_COMMAND).unwrap();
        let right = modifier_flags(VK_RIGHT_COMMAND).unwrap();
        assert_ne!(left, right);
        assert_eq!(left & FLAG_COMMAND, FLAG_COMMAND);
        assert_eq!(right & FLAG_COMMAND, FLAG_COMMAND);
    }

    #[test]
    fn ordinary_keys_are_not_modifiers() {
        assert_eq!(modifier_flags(VK_A), None);
        assert_eq!(modifier_flags(VK_SPACE), None);
        // Caps Lock latches instead of being held, so it is not one either.
        assert_eq!(modifier_flags(VK_CAPS_LOCK), None);
        assert!(is_caps_lock(VK_CAPS_LOCK));
    }

    #[test]
    fn keypad_and_function_keys_carry_their_hardware_flags() {
        assert_eq!(intrinsic_flags(VK_KEYPAD_5), FLAG_NUMERIC_PAD);
        assert_eq!(intrinsic_flags(VK_LEFT_ARROW), FLAG_SECONDARY_FN);
        assert_eq!(intrinsic_flags(VK_A), 0);
    }

    #[test]
    fn keys_macos_has_no_equivalent_of_are_dropped() {
        const KEY_SCROLLLOCK: u16 = 70;
        const KEY_COMPOSE: u16 = 127;
        assert_eq!(evdev_to_mac(KEY_SCROLLLOCK, ModifierStyle::Pc), None);
        assert_eq!(evdev_to_mac(KEY_COMPOSE, ModifierStyle::Pc), None);
    }

    /// Capture and injection must cancel out, or a Mac driving a PC would send
    /// the swap that the PC never applied.
    #[test]
    fn translating_to_macos_and_back_returns_the_original_key() {
        for style in [ModifierStyle::Pc, ModifierStyle::Mac] {
            for &(evdev, _) in TABLE {
                let mac = evdev_to_mac(evdev, style).expect("in the table");
                assert_eq!(mac_to_evdev(mac, style), Some(evdev), "style {style:?}");
            }
        }
    }

    #[test]
    fn every_modifier_key_reports_flags() {
        for &key in MODIFIER_KEYS {
            assert!(modifier_flags(key).is_some(), "keycode {key:#x}");
        }
    }

    /// Two evdev keys landing on one macOS keycode would make the second
    /// unreachable, which is the failure mode a hand-written table invites.
    #[test]
    fn no_two_keys_share_a_destination() {
        let mut seen: Vec<u16> = TABLE.iter().map(|(_, mac)| *mac).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "a macOS keycode is claimed twice");
    }
}
