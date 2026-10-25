//! The viewer's input, replayed onto the hosted window through the portal's
//! RemoteDesktop session.

use ashpd::desktop::remote_desktop::RemoteDesktop;
use cursio_protocol::WindowInput;

/// The protocol's neutral button → its evdev `BTN_*` code.
///
/// Same namespace `cursio-input` injects with, and the same one libei reports —
/// keeping one vocabulary end to end is what lets a click cross machines (and
/// operating systems) without a translation table in the middle.
pub(super) fn evdev_button(button: cursio_protocol::MouseButton) -> i32 {
    use cursio_protocol::MouseButton;
    match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x113,
        MouseButton::Forward => 0x114,
    }
}

/// Replay one [`WindowInput`] onto the real window through the portal.
///
/// Coordinates arrive in the window's own pixel space, which is exactly the
/// space `notify_pointer_motion_absolute` expects when given the cast's stream
/// node — so no translation is needed, and that is precisely why the capture and
/// input halves had to share one session.
///
/// Buttons and keys use evdev codes on both sides of the wire, so they pass
/// through unchanged: the protocol picked that namespace to avoid a keymap
/// round-trip here (see `input-design.md`).
pub(super) async fn replay_input(
    remote: &RemoteDesktop,
    session: &ashpd::desktop::Session<RemoteDesktop>,
    node_id: u32,
    input: WindowInput,
) -> Result<(), ashpd::Error> {
    match input {
        WindowInput::PointerMove { x, y, .. } => {
            remote
                .notify_pointer_motion_absolute(
                    session,
                    node_id,
                    x as f64,
                    y as f64,
                    Default::default(),
                )
                .await
        }
        WindowInput::PointerButton {
            x,
            y,
            button,
            pressed,
            ..
        } => {
            // Move first: the portal has no "click at (x, y)", so the press must
            // land where the viewer's pointer actually is.
            remote
                .notify_pointer_motion_absolute(
                    session,
                    node_id,
                    x as f64,
                    y as f64,
                    Default::default(),
                )
                .await?;
            remote
                .notify_pointer_button(
                    session,
                    evdev_button(button),
                    if pressed {
                        ashpd::desktop::remote_desktop::KeyState::Pressed
                    } else {
                        ashpd::desktop::remote_desktop::KeyState::Released
                    },
                    Default::default(),
                )
                .await
        }
        WindowInput::PointerScroll { dx, dy, .. } => {
            // The protocol counts notches with +y = up; the portal's discrete
            // axis counts steps with +y = down, hence the negation.
            if dx != 0 {
                remote
                    .notify_pointer_axis_discrete(
                        session,
                        ashpd::desktop::remote_desktop::Axis::Horizontal,
                        dx,
                        Default::default(),
                    )
                    .await?;
            }
            if dy != 0 {
                remote
                    .notify_pointer_axis_discrete(
                        session,
                        ashpd::desktop::remote_desktop::Axis::Vertical,
                        -dy,
                        Default::default(),
                    )
                    .await?;
            }
            Ok(())
        }
        WindowInput::Key { key, pressed, .. } => {
            remote
                .notify_keyboard_keycode(
                    session,
                    key as i32,
                    if pressed {
                        ashpd::desktop::remote_desktop::KeyState::Pressed
                    } else {
                        ashpd::desktop::remote_desktop::KeyState::Released
                    },
                    Default::default(),
                )
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_replay_as_evdev_codes() {
        use cursio_protocol::MouseButton;
        // These must match `input-event-codes.h`: the surrogate's click is
        // replayed by number, so a wrong code silently presses another button.
        assert_eq!(evdev_button(MouseButton::Left), 0x110);
        assert_eq!(evdev_button(MouseButton::Right), 0x111);
        assert_eq!(evdev_button(MouseButton::Middle), 0x112);
        assert_eq!(evdev_button(MouseButton::Back), 0x113);
        assert_eq!(evdev_button(MouseButton::Forward), 0x114);
    }
}
