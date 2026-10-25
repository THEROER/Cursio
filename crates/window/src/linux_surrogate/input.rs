//! The surrogate's pointer and keys, read off GTK's controllers and sent to the
//! host as the window's input.

use crate::framebuffer::{contained_to_host, Framebuffer};
use cursio_protocol::{WindowId, WindowInput};
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc;

/// Wire pointer, scroll, and keyboard on the surrogate into [`WindowInput`].
///
/// Coordinates are reported **in the host window's pixel space**: GTK gives us
/// widget-local coordinates, which match 1:1 only while the surrogate is shown
/// unscaled, so we rescale by the ratio between the widget's current size and
/// the framebuffer's. Without that, dragging a resized surrogate would land
/// clicks in the wrong place on the host.
pub(super) fn attach_input_controllers(
    window: &gtk::Window,
    picture: &gtk::Picture,
    id: WindowId,
    input_tx: mpsc::UnboundedSender<WindowInput>,
    fb: Rc<RefCell<Framebuffer>>,
) {
    // The handlers hold the picture weakly: a controller owned by the picture
    // holding it strongly is a cycle, which kept a closed window — its
    // framebuffer and the senders of its input — alive for the process's life.

    /// Map a widget-local point onto host pixels.
    fn to_host(
        picture: &gtk::Picture,
        fb: &Rc<RefCell<Framebuffer>>,
        x: f64,
        y: f64,
    ) -> (u32, u32) {
        let fb = fb.borrow();
        contained_to_host(
            (picture.width() as f64, picture.height() as f64),
            (fb.width, fb.height),
            x,
            y,
        )
    }

    // Pointer motion.
    let motion = gtk::EventControllerMotion::new();
    {
        let (tx, pic, fb) = (input_tx.clone(), picture.downgrade(), fb.clone());
        motion.connect_motion(move |_, x, y| {
            let Some(pic) = pic.upgrade() else { return };
            let (x, y) = to_host(&pic, &fb, x, y);
            let _ = tx.send(WindowInput::PointerMove { id, x, y });
        });
    }
    picture.add_controller(motion);

    // Buttons. `set_button(0)` makes the gesture report every button, not just
    // the primary one, so middle/right/thumb clicks reach the host too.
    let click = gtk::GestureClick::new();
    click.set_button(0);
    {
        let (tx, pic, fb) = (input_tx.clone(), picture.downgrade(), fb.clone());
        click.connect_pressed(move |g, _, x, y| {
            let Some(pic) = pic.upgrade() else { return };
            let (x, y) = to_host(&pic, &fb, x, y);
            // Traced at both ends (see windows_input.rs) so a button that does
            // nothing on the host can be told apart from one never sent.
            let raw = g.current_button();
            match map_button(raw) {
                Some(button) => {
                    tracing::debug!(?button, raw, "surrogate button pressed");
                    let _ = tx.send(WindowInput::PointerButton {
                        id,
                        x,
                        y,
                        button,
                        pressed: true,
                    });
                }
                None => tracing::debug!(raw, "unmapped surrogate button; dropping"),
            }
        });
    }
    {
        let (tx, pic, fb) = (input_tx.clone(), picture.downgrade(), fb.clone());
        click.connect_released(move |g, _, x, y| {
            let Some(pic) = pic.upgrade() else { return };
            let (x, y) = to_host(&pic, &fb, x, y);
            if let Some(button) = map_button(g.current_button()) {
                let _ = tx.send(WindowInput::PointerButton {
                    id,
                    x,
                    y,
                    button,
                    pressed: false,
                });
            }
        });
    }
    picture.add_controller(click);

    // Scroll, in notches. GTK's y grows downward while the protocol's positive
    // y means "up" (matching `InputEvent::PointerScroll`), hence the negation.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    {
        let tx = input_tx.clone();
        scroll.connect_scroll(move |_, dx, dy| {
            tracing::debug!(dx, dy, "surrogate scroll");
            let _ = tx.send(WindowInput::PointerScroll {
                id,
                dx: dx.round() as i32,
                dy: -(dy.round() as i32),
            });
            glib::Propagation::Stop
        });
    }
    picture.add_controller(scroll);

    // Keyboard on the window (the toplevel holds focus, not the Picture).
    let keys = gtk::EventControllerKey::new();
    {
        let tx = input_tx.clone();
        keys.connect_key_pressed(move |_, _, keycode, _| {
            if let Some(key) = evdev_from_gdk_keycode(keycode) {
                let _ = tx.send(WindowInput::Key {
                    id,
                    key,
                    pressed: true,
                });
            }
            glib::Propagation::Proceed
        });
    }
    {
        let tx = input_tx.clone();
        keys.connect_key_released(move |_, _, keycode, _| {
            if let Some(key) = evdev_from_gdk_keycode(keycode) {
                let _ = tx.send(WindowInput::Key {
                    id,
                    key,
                    pressed: false,
                });
            }
        });
    }
    window.add_controller(keys);
}

/// GDK button number → the protocol's neutral [`MouseButton`].
pub(super) fn map_button(gdk_button: u32) -> Option<cursio_protocol::MouseButton> {
    use cursio_protocol::MouseButton;
    Some(match gdk_button {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        8 => MouseButton::Back,
        9 => MouseButton::Forward,
        _ => return None,
    })
}

/// GDK hardware keycode → evdev keycode, the namespace the protocol uses.
///
/// On Linux GDK's "hardware keycode" is the X11-style code, which is the evdev
/// code plus the historic 8-key offset. Undoing that offset is the whole
/// mapping — no keymap lookup needed, which is exactly why the wire format is
/// evdev-based (see `input-design.md`).
pub(super) fn evdev_from_gdk_keycode(gdk_keycode: u32) -> Option<u16> {
    const X11_EVDEV_OFFSET: u32 = 8;
    gdk_keycode
        .checked_sub(X11_EVDEV_OFFSET)
        .and_then(|k| u16::try_from(k).ok())
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use cursio_protocol::MouseButton;

    #[test]
    fn gdk_keycodes_lose_the_x11_offset() {
        // GDK 38 is X11's code for `a`, whose evdev code is 30 (KEY_A).
        assert_eq!(evdev_from_gdk_keycode(38), Some(30));
        // Below the offset there is no evdev key to map to.
        assert_eq!(evdev_from_gdk_keycode(4), None);
    }

    #[test]
    fn buttons_map_to_neutral_names() {
        assert_eq!(map_button(1), Some(MouseButton::Left));
        assert_eq!(map_button(2), Some(MouseButton::Middle));
        assert_eq!(map_button(3), Some(MouseButton::Right));
        assert_eq!(map_button(8), Some(MouseButton::Back));
        assert_eq!(map_button(9), Some(MouseButton::Forward));
        assert_eq!(map_button(42), None);
    }
}
