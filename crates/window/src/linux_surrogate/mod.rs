//! Linux surrogate window: show a transferred window, send its input back.
//!
//! The **viewer** side. A [`WindowMeta`] from the host opens a local GTK4 window
//! sized to match; inbound [`WindowFrame`] tiles are blitted into a full-window
//! BGRA framebuffer that a `gtk::Picture` draws from; the user's clicks, motion,
//! scrolls, and keys come back out as [`WindowInput`] in that window's own
//! coordinates for the host to replay.
//!
//! A **remote-screen view** is not one of these on Linux: nobody is meant to
//! look at it here, so it is a PipeWire node instead
//! ([`PipeWireScreenSource`](crate::PipeWireScreenSource)); [`LinuxSurrogate`]
//! is the renderer's answer for either.
//!
//! ## Why GTK4
//!
//! It is already in the tree (clipd's popup), which means no new toolkit to
//! carry, and it hands us the two things a hand-rolled Wayland client would cost
//! hundreds of lines to get right: a real xdg-shell window that behaves like a
//! native one on GNOME, and cooked input — including an xkb-resolved keyboard —
//! through event controllers. Pixels stay cheap because
//! [`gdk::MemoryTexture`] takes **B8g8r8a8** directly, which is exactly the wire
//! format, so a frame goes to the screen with no colour conversion.
//!
//! ## Threading
//!
//! GTK owns its thread and its main loop, and none of its objects are `Send` —
//! but [`Surrogate`] is driven from the session task. So GTK runs on a
//! **dedicated thread** and only channels cross the boundary, the same shape as
//! the capture side: frames in, input and acks out.
//!
//! **One** thread for the whole process, started with the first window and
//! never ended. It was one per window, and GTK refuses that twice over: it may
//! be initialised on one thread only, so every window after the first in a
//! process panicked in `gtk::init` ("surrogate thread ended before signalling
//! readiness"); and the first window, closed by ending its thread's loop, never
//! left the screen — destroying it only queues the request to the compositor,
//! and with no loop left to flush it the dead window stayed up until the node
//! was killed. A window now ends by stopping its own tick; the loop goes on.

use cursio_protocol::{WindowCloseReason, WindowFrame, WindowFrameAck, WindowInput, WindowMeta};
use gtk::{gdk, glib};
use tokio::sync::mpsc;

use crate::linux_surrogate::window::spawn_surrogate;
use crate::{
    framebuffer::Framebuffer, surrogate_front::SurrogateFront, Surrogate, WindowError,
    WindowRenderer,
};

mod input;
mod window;

/// Snapshot a framebuffer as a texture GTK can paint.
///
/// `B8g8r8a8` is the wire format verbatim, so this is a copy, not a conversion.
fn to_texture(fb: &Framebuffer) -> gdk::MemoryTexture {
    gdk::MemoryTexture::new(
        fb.width as i32,
        fb.height as i32,
        gdk::MemoryFormat::B8g8r8a8,
        &glib::Bytes::from(&fb.pixels[..]),
        (fb.width as usize) * 4,
    )
}

/// What the session hands the GTK thread; a GTK window has nothing of its own
/// to be told.
type SurrogateCommand = crate::surrogate_front::SurrogateCommand<std::convert::Infallible>;

/// A surrogate window driven from the session, rendered by GTK on its own thread.
pub struct GtkSurrogate {
    front: SurrogateFront<std::convert::Infallible>,
    input: mpsc::UnboundedReceiver<WindowInput>,
    /// Sizes the user dragged this window to, newest wins. See
    /// [`Surrogate::take_resize_request`].
    resized: mpsc::UnboundedReceiver<(u32, u32)>,
    /// The last size actually asked of the host, so a drag that returns to a
    /// size already requested does not ask again.
    requested_size: Option<(u32, u32)>,
}

impl Surrogate for GtkSurrogate {
    fn apply_frame(&mut self, frame: &WindowFrame) -> Result<(), WindowError> {
        self.front.apply_frame(frame)
    }

    fn input(&mut self) -> &mut mpsc::UnboundedReceiver<WindowInput> {
        &mut self.input
    }

    /// The window's tick owns the command receiver and ends with the window.
    fn is_gone(&self) -> bool {
        self.front.is_gone()
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        self.front.take_ack()
    }

    fn take_keyframe_request(&mut self) -> bool {
        self.front.take_keyframe_request()
    }

    fn take_resize_request(&mut self) -> Option<(u32, u32)> {
        // Drain to the newest: a drag emits a size per frame, and only where the
        // user let go matters.
        let mut latest = None;
        while let Ok(size) = self.resized.try_recv() {
            latest = Some(size);
        }
        let size = latest?;
        // The host answers a resize with a full frame, which sets `self.width`
        // — so a size we already asked for, or one that matches what is being
        // shown, would be an echo of our own request.
        if Some(size) == self.requested_size || size == self.front.size() {
            return None;
        }
        self.requested_size = Some(size);
        Some(size)
    }

    fn close(&mut self, reason: WindowCloseReason) {
        self.front.close(reason);
    }
}

/// The Linux [`WindowRenderer`] backend: one GTK window per transferred window,
/// and a PipeWire node — no window — per remote-screen view.
pub struct GtkWindowRenderer;

impl WindowRenderer for GtkWindowRenderer {
    type Surrogate = LinuxSurrogate;

    fn open(&mut self, meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        spawn_surrogate(meta).map(LinuxSurrogate::Window)
    }

    /// A node the ScreenCast portal hands to the capture app; see
    /// [`PipeWireScreenSource`](crate::PipeWireScreenSource).
    fn open_screen_view(&mut self, meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        crate::PipeWireScreenSource::open(meta).map(LinuxSurrogate::Node)
    }
}

/// What the Linux renderer opens: a transfer's window, or a view's node.
pub enum LinuxSurrogate {
    Window(GtkSurrogate),
    Node(crate::PipeWireScreenSource),
}

impl LinuxSurrogate {
    fn inner(&mut self) -> &mut dyn Surrogate {
        match self {
            Self::Window(w) => w,
            Self::Node(n) => n,
        }
    }
}

impl Surrogate for LinuxSurrogate {
    fn apply_frame(&mut self, frame: &WindowFrame) -> Result<(), WindowError> {
        self.inner().apply_frame(frame)
    }

    fn input(&mut self) -> &mut mpsc::UnboundedReceiver<WindowInput> {
        match self {
            Self::Window(w) => w.input(),
            Self::Node(n) => n.input(),
        }
    }

    fn is_gone(&self) -> bool {
        match self {
            Self::Window(w) => w.is_gone(),
            Self::Node(n) => n.is_gone(),
        }
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        self.inner().take_ack()
    }

    fn take_resize_request(&mut self) -> Option<(u32, u32)> {
        self.inner().take_resize_request()
    }

    fn take_keyframe_request(&mut self) -> bool {
        self.inner().take_keyframe_request()
    }

    fn restart_stream(&mut self, title: &str) {
        self.inner().restart_stream(title)
    }

    fn node_id(&self) -> Option<u32> {
        match self {
            Self::Window(_) => None,
            Self::Node(n) => Some(n.node_id()),
        }
    }

    fn close(&mut self, reason: WindowCloseReason) {
        self.inner().close(reason)
    }
}

#[cfg(test)]
mod framebuffer_tests {
    use super::*;
    use cursio_protocol::Rect;

    #[test]
    fn blit_places_a_tile_at_its_offset() {
        let mut fb = Framebuffer::new(4, 2);
        // A 1x1 white tile at (2,1).
        let tile = [0xFF, 0xFF, 0xFF, 0xFF];
        fb.blit(
            Rect {
                x: 2,
                y: 1,
                width: 1,
                height: 1,
            },
            &tile,
        )
        .unwrap();
        let (row, col, width) = (1usize, 2usize, 4usize);
        let i = (row * width + col) * 4;
        assert_eq!(&fb.pixels[i..i + 4], &tile);
        // Everything else stays untouched.
        assert!(fb.pixels[..i].iter().all(|&b| b == 0));
    }

    #[test]
    fn out_of_bounds_tile_is_rejected_not_clipped() {
        let mut fb = Framebuffer::new(4, 2);
        let err = fb.blit(
            Rect {
                x: 3,
                y: 0,
                width: 2,
                height: 1,
            },
            &[0; 8],
        );
        assert!(matches!(err, Err(WindowError::Backend(_))));
    }

    #[test]
    fn resize_reallocates_to_the_new_geometry() {
        let mut fb = Framebuffer::new(4, 2);
        fb.resize(8, 4);
        assert_eq!(fb.width, 8);
        assert_eq!(fb.pixels.len(), 8 * 4 * 4);
    }
}
