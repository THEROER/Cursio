//! libei's events turned into the wire's: relative motion with its sub-pixel
//! remainder kept, scroll frames, and buttons as evdev codes.

use cursio_protocol::{InputEvent, MouseButton};
use reis::{
    ei::{self, button::ButtonState, keyboard::KeyState},
    event::{DeviceCapability, EiEvent},
};

/// evdev `BTN_*` codes we forward as pointer buttons (libei reports button codes
/// straight from `input-event-codes.h`).
pub(super) const BTN_LEFT: u32 = 0x110;
pub(super) const BTN_RIGHT: u32 = 0x111;
pub(super) const BTN_MIDDLE: u32 = 0x112;
/// Thumb buttons — what browsers treat as back/forward.
pub(super) const BTN_SIDE: u32 = 0x113;
pub(super) const BTN_EXTRA: u32 = 0x114;

/// Carries the sub-pixel remainder of pointer motion between libei events.
///
/// libei reports motion as `f32`. Rounding each event's `dx`/`dy` on its own
/// throws the fraction away, and a stream of small moves (`|d| < 0.5`) rounds to
/// zero every time — the motion vanishes entirely. That shows up as broken aim in
/// games that read raw relative motion: slow vertical drags disappear while
/// horizontal jitter survives, so the view turns the wrong way.
///
/// Instead we keep the leftover fraction and roll it into the next event, so no
/// movement is ever lost and the summed motion matches the physical mouse.
#[derive(Default)]
pub(super) struct MotionRemainder {
    dx: f32,
    dy: f32,
}

impl MotionRemainder {
    /// Fold one libei delta in and take out whole pixels, keeping the fraction.
    fn take(&mut self, dx: f32, dy: f32) -> (i32, i32) {
        self.dx += dx;
        self.dy += dy;
        // `trunc` (not `round`) so the remainder keeps the same sign as the
        // motion and can never push the next step the other way.
        let whole_x = self.dx.trunc();
        let whole_y = self.dy.trunc();
        self.dx -= whole_x;
        self.dy -= whole_y;
        (whole_x as i32, whole_y as i32)
    }
}

/// Neutral scroll units per logical pixel of libei's smooth scroll: ten pixels
/// to a notch, the step a desktop toolkit scrolls one wheel click by.
pub(super) const SCROLL_PER_PIXEL: f32 = cursio_protocol::SCROLL_NOTCH as f32 / 10.0;

/// Smooth scroll from libei, held until its frame ends.
///
/// A touchpad has no notches, so its two-finger scroll arrives only as smooth
/// deltas — which used to be dropped, leaving it unable to scroll a peer. A
/// compositor may describe one wheel click both ways in the same frame, so a
/// frame that carried a discrete step drops its delta rather than scroll twice.
#[derive(Default)]
pub(super) struct ScrollFrame {
    pending: MotionRemainder,
    dx: i32,
    dy: i32,
    discrete: bool,
}

impl ScrollFrame {
    fn delta(&mut self, dx: f32, dy: f32) {
        let (x, y) = self
            .pending
            .take(dx * SCROLL_PER_PIXEL, dy * SCROLL_PER_PIXEL);
        self.dx += x;
        self.dy += y;
    }

    /// The frame is over: what it scrolled, if a delta is all it carried.
    fn finish(&mut self) -> Option<InputEvent> {
        let (dx, dy, discrete) = (self.dx, self.dy, self.discrete);
        self.dx = 0;
        self.dy = 0;
        self.discrete = false;
        // libei's vertical axis points down; the neutral one points up.
        (!discrete && (dx != 0 || dy != 0)).then_some(InputEvent::PointerScroll { dx, dy: -dy })
    }
}

/// Translate one libei event to a neutral [`InputEvent`] and forward it (binding
/// seat capabilities the first time a seat appears).
pub(super) fn handle_ei_event(
    event: EiEvent,
    tx: &crate::EventSender,
    context: &ei::Context,
    remainder: &mut MotionRemainder,
    scroll: &mut ScrollFrame,
) {
    let neutral = match event {
        EiEvent::SeatAdded(seat) => {
            seat.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Scroll
                    | DeviceCapability::Button,
            );
            let _ = context.flush();
            return;
        }
        EiEvent::PointerMotion(m) => {
            let (dx, dy) = remainder.take(m.dx, m.dy);
            // A move that's entirely sub-pixel so far: nothing to send yet, but
            // it's banked in the remainder for the next one.
            if dx == 0 && dy == 0 {
                return;
            }
            InputEvent::PointerMotion { dx, dy }
        }
        EiEvent::KeyboardKey(k) => {
            let pressed = matches!(k.state, KeyState::Press);
            // The capture end of the key trace, as for buttons below: a peer
            // left holding Alt and Shift after both were let go here shows as
            // a release logged here and never injected there.
            tracing::debug!(key = k.key, pressed, "captured key");
            InputEvent::Key {
                // libei key codes are evdev keycodes — our neutral namespace.
                key: k.key as u16,
                pressed,
            }
        }
        EiEvent::Button(b) => {
            let pressed = matches!(b.state, ButtonState::Press);
            // Buttons are logged at both ends of the wire (capture here, inject
            // on the peer) so a "click made the cursor stick" report can be
            // traced instead of guessed at: if a release is captured here but
            // never injected there, the loss is in between.
            tracing::debug!(button = b.button, pressed, "captured pointer button");
            match b.button {
                BTN_LEFT => InputEvent::PointerButton {
                    button: MouseButton::Left,
                    pressed,
                },
                BTN_RIGHT => InputEvent::PointerButton {
                    button: MouseButton::Right,
                    pressed,
                },
                BTN_MIDDLE => InputEvent::PointerButton {
                    button: MouseButton::Middle,
                    pressed,
                },
                BTN_SIDE => InputEvent::PointerButton {
                    button: MouseButton::Back,
                    pressed,
                },
                BTN_EXTRA => InputEvent::PointerButton {
                    button: MouseButton::Forward,
                    pressed,
                },
                other => {
                    tracing::trace!(button = other, "dropping unmapped button");
                    return;
                }
            }
        }
        EiEvent::ScrollDiscrete(s) => {
            // libei discrete scroll is in 120-unit steps, the neutral unit.
            // libei's vertical axis points down (positive = scroll down) while
            // the neutral event follows evdev REL_WHEEL (positive = up), so dy
            // flips. Horizontal agrees (positive = right) and is passed as-is.
            scroll.discrete = true;
            let dx = s.discrete_dx;
            let dy = -s.discrete_dy;
            if dx == 0 && dy == 0 {
                return;
            }
            InputEvent::PointerScroll { dx, dy }
        }
        EiEvent::ScrollDelta(s) => {
            scroll.delta(s.dx, s.dy);
            return;
        }
        EiEvent::Frame(_) => match scroll.finish() {
            Some(event) => event,
            None => return,
        },
        _ => return,
    };
    let _ = tx.send(neutral);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_pixel_motion_accumulates_instead_of_vanishing() {
        // A slow drag straight up: each step is well under a pixel, which plain
        // rounding would turn into zero forever (the bug that broke aiming in
        // games). The remainder must bank them until a whole pixel comes out.
        let mut r = MotionRemainder::default();
        assert_eq!(r.take(0.0, -0.4), (0, 0));
        assert_eq!(r.take(0.0, -0.4), (0, 0));
        // 3 * -0.4 = -1.2 => one whole pixel up, 0.2 kept back.
        assert_eq!(r.take(0.0, -0.4), (0, -1));
    }

    #[test]
    fn motion_total_is_preserved_over_a_stream() {
        // Ten 0.3px steps are 3px of real movement; none may be lost.
        let mut r = MotionRemainder::default();
        let moved: i32 = (0..10).map(|_| r.take(0.3, 0.0).0).sum();
        assert_eq!(moved, 3);
    }

    #[test]
    fn a_touchpads_smooth_scroll_reaches_the_peer_flipped_upward() {
        let mut frame = ScrollFrame::default();
        frame.delta(0.0, 5.0);
        assert_eq!(
            frame.finish(),
            Some(InputEvent::PointerScroll { dx: 0, dy: -60 }),
            "five pixels down is half a notch down"
        );
        assert_eq!(frame.finish(), None, "a frame is reported once");
    }

    #[test]
    fn a_wheel_click_described_both_ways_scrolls_once() {
        let mut frame = ScrollFrame::default();
        frame.delta(0.0, 10.0);
        frame.discrete = true;
        assert_eq!(frame.finish(), None);
    }

    #[test]
    fn remainder_never_flips_direction() {
        // Truncation keeps the remainder's sign matching the motion, so a left
        // drag can never emit a rightward step.
        let mut r = MotionRemainder::default();
        for _ in 0..20 {
            let (dx, _) = r.take(-0.7, 0.0);
            assert!(dx <= 0, "leftward motion produced dx={dx}");
        }
    }
}
