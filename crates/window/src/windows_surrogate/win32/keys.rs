//! The viewer's keys, from Windows virtual-key codes to the wire's evdev ones.

/// Windows virtual-key code → the evdev code the protocol speaks.
///
/// The inverse of the host-side table in
/// [`windows_input`](crate::windows_input): the wire format is evdev
/// everywhere, so Windows is the side that translates — going out here, coming
/// in there.
pub(super) fn evdev_from_vk(vk: u16) -> Option<u16> {
    Some(match vk {
        0x1B => 1,                       // ESC
        0x31..=0x39 => (vk - 0x31) + 2,  // '1'..'9'
        0x30 => 11,                      // '0'
        0xBD => 12,                      // minus
        0xBB => 13,                      // equal
        0x08 => 14,                      // backspace
        0x09 => 15,                      // tab
        0x51 => 16,                      // q
        0x57 => 17,                      // w
        0x45 => 18,                      // e
        0x52 => 19,                      // r
        0x54 => 20,                      // t
        0x59 => 21,                      // y
        0x55 => 22,                      // u
        0x49 => 23,                      // i
        0x4F => 24,                      // o
        0x50 => 25,                      // p
        0xDB => 26,                      // [
        0xDD => 27,                      // ]
        0x0D => 28,                      // enter
        0x11 => 29,                      // ctrl
        0x41 => 30,                      // a
        0x53 => 31,                      // s
        0x44 => 32,                      // d
        0x46 => 33,                      // f
        0x47 => 34,                      // g
        0x48 => 35,                      // h
        0x4A => 36,                      // j
        0x4B => 37,                      // k
        0x4C => 38,                      // l
        0xBA => 39,                      // ;
        0xDE => 40,                      // '
        0xC0 => 41,                      // `
        0x10 => 42,                      // shift
        0xDC => 43,                      // backslash
        0x5A => 44,                      // z
        0x58 => 45,                      // x
        0x43 => 46,                      // c
        0x56 => 47,                      // v
        0x42 => 48,                      // b
        0x4E => 49,                      // n
        0x4D => 50,                      // m
        0xBC => 51,                      // ,
        0xBE => 52,                      // .
        0xBF => 53,                      // /
        0x12 => 56,                      // alt
        0x20 => 57,                      // space
        0x14 => 58,                      // caps lock
        0x70..=0x79 => (vk - 0x70) + 59, // F1..F10
        0x7A => 87,                      // F11
        0x7B => 88,                      // F12
        0x24 => 102,                     // home
        0x26 => 103,                     // up
        0x21 => 104,                     // page up
        0x25 => 105,                     // left
        0x27 => 106,                     // right
        0x23 => 107,                     // end
        0x28 => 108,                     // down
        0x22 => 109,                     // page down
        0x2D => 110,                     // insert
        0x2E => 111,                     // delete
        0x5B => 125,                     // left Windows key
        0x5C => 126,                     // right Windows key
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_to_evdev_round_trips_with_the_host_table() {
        // These must be the inverse of `windows_input::vk_from_evdev`, or a key
        // typed on the surrogate would arrive as a different key on the host.
        assert_eq!(evdev_from_vk(0x41), Some(30)); // 'A' -> KEY_A
        assert_eq!(evdev_from_vk(0x5A), Some(44)); // 'Z' -> KEY_Z
        assert_eq!(evdev_from_vk(0x31), Some(2)); // '1' -> KEY_1
        assert_eq!(evdev_from_vk(0x30), Some(11)); // '0' -> KEY_0
        assert_eq!(evdev_from_vk(0x70), Some(59)); // F1
        assert_eq!(evdev_from_vk(0x7B), Some(88)); // F12
    }

    #[test]
    fn unmapped_keys_are_dropped() {
        assert_eq!(evdev_from_vk(0xFF), None);
    }
}
