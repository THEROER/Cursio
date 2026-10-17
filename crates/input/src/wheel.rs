//! Turning fractional scroll back into whole notches for injectors that need
//! them.
//!
//! The wire carries scroll in [`SCROLL_NOTCH`]ths of a notch, because a
//! touchpad scrolls by fractions and rounding each one to a notch makes it
//! zero. Some of what receives it counts only whole notches: evdev's
//! `REL_WHEEL`, which X11 clients read even beside `REL_WHEEL_HI_RES`, and
//! macOS line scrolling. Those keep the remainder here until it adds up.

use cursio_protocol::SCROLL_NOTCH;

/// The part of a scroll that has not added up to a notch yet.
///
/// Like the kernel's own accumulator for high-resolution wheels it starts over
/// when the direction reverses, so a flick back does not first have to undo
/// the leftover of the way out.
#[derive(Debug, Default)]
pub(crate) struct WheelRemainder(i32);

impl WheelRemainder {
    /// Fold `delta` in and take out the whole notches it completes.
    pub(crate) fn notches(&mut self, delta: i32) -> i32 {
        if self.0.signum() * delta.signum() < 0 {
            self.0 = 0;
        }
        self.0 += delta;
        let whole = self.0 / SCROLL_NOTCH;
        self.0 -= whole * SCROLL_NOTCH;
        whole
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touchpad_fractions_add_up_to_whole_notches() {
        let mut wheel = WheelRemainder::default();
        assert_eq!(wheel.notches(40), 0);
        assert_eq!(wheel.notches(40), 0);
        assert_eq!(wheel.notches(40), 1, "three thirds of a notch are one");
        assert_eq!(wheel.notches(240), 2);
    }

    #[test]
    fn reversing_the_scroll_drops_what_was_left_the_other_way() {
        let mut wheel = WheelRemainder::default();
        assert_eq!(wheel.notches(100), 0);
        assert_eq!(wheel.notches(-120), -1, "not held back by the 100 going up");
    }
}
