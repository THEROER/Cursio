//! Turning a peer's touch gesture into the shortcut it stands for.
//!
//! Windows publishes no way to inject a touchpad gesture: a precision
//! touchpad's swipes are recognised inside its own driver stack, and nothing
//! a user-mode program sends reaches that. What the gestures *do* is reachable
//! — the same shortcuts the system binds them to — so a receiver that cannot
//! replay the fingers (Linux does, `uinput_touchpad.rs`) recognises the
//! gesture here and presses those.
//!
//! A swipe is decided when it ends, by its dominant direction and only past a
//! threshold: deciding mid-swipe would fire on a gesture the user was still
//! steering, and a few pixels of drift are not a swipe at all. A pinch acts
//! while it happens, one zoom step per [`ZOOM_STEP`] change in scale, because
//! zooming that waits for the fingers to lift is not zooming.

use cursio_protocol::{GesturePhase, InputEvent, GESTURE_SCALE_ONE};

/// Total travel, in pointer units, before a swipe counts.
const SWIPE_THRESHOLD: i32 = 60;
/// Change in scale per zoom step: a fifth closer or further apart.
const ZOOM_STEP: f64 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// What a gesture asks of the receiving desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GestureAction {
    Swipe {
        fingers: u8,
        direction: Direction,
    },
    /// Positive steps zoom in.
    Zoom {
        steps: i32,
    },
}

/// The gesture in progress.
#[derive(Debug, Default)]
pub(crate) struct GestureShortcuts {
    swipe: Option<(u8, i32, i32)>,
    /// The scale at the last zoom step taken, as a ratio.
    pinch: Option<f64>,
}

impl GestureShortcuts {
    /// Fold one event in; returns what to do now, if anything.
    pub(crate) fn feed(&mut self, event: InputEvent) -> Option<GestureAction> {
        match event {
            InputEvent::GestureSwipe {
                phase,
                fingers,
                dx,
                dy,
            } => match phase {
                GesturePhase::Begin => {
                    self.swipe = Some((fingers, dx, dy));
                    None
                }
                GesturePhase::Update => {
                    if let Some((_, x, y)) = self.swipe.as_mut() {
                        *x += dx;
                        *y += dy;
                    }
                    None
                }
                GesturePhase::End => {
                    let (fingers, x, y) = self.swipe.take()?;
                    let (x, y) = (x + dx, y + dy);
                    if x.abs().max(y.abs()) < SWIPE_THRESHOLD {
                        return None;
                    }
                    let direction = if x.abs() >= y.abs() {
                        if x < 0 {
                            Direction::Left
                        } else {
                            Direction::Right
                        }
                    } else if y < 0 {
                        Direction::Up
                    } else {
                        Direction::Down
                    };
                    Some(GestureAction::Swipe { fingers, direction })
                }
                GesturePhase::Cancel => {
                    self.swipe = None;
                    None
                }
            },
            InputEvent::GesturePinch { phase, scale, .. } => {
                let scale = f64::from(scale) / f64::from(GESTURE_SCALE_ONE);
                match phase {
                    GesturePhase::Begin => {
                        self.pinch = Some(1.0);
                        None
                    }
                    GesturePhase::Update => {
                        let last = self.pinch.as_mut()?;
                        // Steps on a log scale, so spreading to 2× and pinching
                        // back to 1× undoes exactly what was done.
                        let steps = ((scale / *last).ln() / (1.0 + ZOOM_STEP).ln()).trunc() as i32;
                        if steps == 0 {
                            return None;
                        }
                        *last *= (1.0 + ZOOM_STEP).powi(steps);
                        Some(GestureAction::Zoom { steps })
                    }
                    GesturePhase::End | GesturePhase::Cancel => {
                        self.pinch = None;
                        None
                    }
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swipe(phase: GesturePhase, dx: i32, dy: i32) -> InputEvent {
        InputEvent::GestureSwipe {
            phase,
            fingers: 3,
            dx,
            dy,
        }
    }

    fn pinch(phase: GesturePhase, scale: f64) -> InputEvent {
        InputEvent::GesturePinch {
            phase,
            fingers: 2,
            scale: (scale * f64::from(GESTURE_SCALE_ONE)) as i32,
            rotation: 0,
            dx: 0,
            dy: 0,
        }
    }

    #[test]
    fn a_swipe_is_decided_by_its_dominant_direction_when_it_ends() {
        let mut g = GestureShortcuts::default();
        assert_eq!(g.feed(swipe(GesturePhase::Begin, 0, 0)), None);
        assert_eq!(g.feed(swipe(GesturePhase::Update, -50, 10)), None);
        assert_eq!(
            g.feed(swipe(GesturePhase::Update, -40, 5)),
            None,
            "not mid-swipe"
        );
        assert_eq!(
            g.feed(swipe(GesturePhase::End, 0, 0)),
            Some(GestureAction::Swipe {
                fingers: 3,
                direction: Direction::Left
            })
        );
    }

    #[test]
    fn drift_short_of_the_threshold_is_not_a_swipe() {
        let mut g = GestureShortcuts::default();
        g.feed(swipe(GesturePhase::Begin, 0, 0));
        g.feed(swipe(GesturePhase::Update, 20, 15));
        assert_eq!(g.feed(swipe(GesturePhase::End, 0, 0)), None);
    }

    #[test]
    fn a_cancelled_swipe_does_nothing() {
        let mut g = GestureShortcuts::default();
        g.feed(swipe(GesturePhase::Begin, 0, 0));
        g.feed(swipe(GesturePhase::Update, 0, 300));
        g.feed(swipe(GesturePhase::Cancel, 0, 0));
        assert_eq!(g.feed(swipe(GesturePhase::End, 0, 0)), None);
    }

    #[test]
    fn spreading_and_pinching_back_zooms_in_and_out_by_the_same_steps() {
        let mut g = GestureShortcuts::default();
        g.feed(pinch(GesturePhase::Begin, 1.0));
        let zoom = |a| match a {
            Some(GestureAction::Zoom { steps }) => steps,
            _ => 0,
        };
        let out = zoom(g.feed(pinch(GesturePhase::Update, 1.5)));
        assert!(out > 0);
        let back = zoom(g.feed(pinch(GesturePhase::Update, 1.0)));
        assert_eq!(out, -back);
    }
}
