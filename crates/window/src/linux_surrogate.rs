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

use std::cell::RefCell;
use std::rc::Rc;

use cursio_protocol::{
    Rect, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput, WindowMeta,
};
use gtk::prelude::*;
use gtk::{gdk, glib};
use tokio::sync::mpsc;

use crate::{
    framebuffer::{contained_to_host, fit_into, refit, Framebuffer},
    surrogate_front::SurrogateFront,
    video_decoder::VideoDecoder,
    Surrogate, WindowError, WindowRenderer,
};
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

/// Start the GTK thread for one surrogate and wait until its window is up.
///
/// Blocks on readiness so a failure to reach the display server surfaces from
/// `open()` rather than as a window that never appears — the same contract as
/// the capture side's `begin()`.
fn spawn_surrogate(meta: &WindowMeta) -> Result<GtkSurrogate, WindowError> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (presented_tx, presented_rx) = mpsc::unbounded_channel();
    let (resized_tx, resized_rx) = mpsc::unbounded_channel();
    let wants_keyframe = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_wants_keyframe = wants_keyframe.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let meta = meta.clone();
    let thread_meta = meta.clone();
    gtk_context()?.invoke(move || {
        build_window(
            thread_meta,
            cmd_rx,
            input_tx,
            presented_tx,
            resized_tx,
            ready_tx,
            thread_wants_keyframe,
        )
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(GtkSurrogate {
            front: SurrogateFront::new(
                meta.id,
                (meta.width, meta.height),
                cmd_tx,
                presented_rx,
                wants_keyframe,
            ),
            input: input_rx,
            resized: resized_rx,
            requested_size: None,
        }),
        Ok(Err(e)) => Err(WindowError::Backend(e)),
        Err(_) => Err(WindowError::Backend(
            "the gtk thread dropped the window before it was shown".into(),
        )),
    }
}

/// The main context of the process's one GTK thread, starting it on first use.
///
/// A failed start (no display yet, at login) is not remembered: GTK counts as
/// initialised only once `gtk::init` succeeds, so the next window may try
/// again on a new thread.
fn gtk_context() -> Result<glib::MainContext, WindowError> {
    static CONTEXT: std::sync::Mutex<Option<glib::MainContext>> = std::sync::Mutex::new(None);
    let mut context = CONTEXT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(context) = context.as_ref() {
        return Ok(context.clone());
    }
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("cursio-gtk".into())
        .spawn(move || {
            if let Err(e) = gtk::init() {
                let _ = started_tx.send(Err(format!("gtk init: {e}")));
                return;
            }
            let _ = started_tx.send(Ok(glib::MainContext::default()));
            glib::MainLoop::new(None, false).run();
        })
        .map_err(|e| WindowError::Backend(format!("spawn the gtk thread: {e}")))?;
    let started = started_rx
        .recv()
        .map_err(|_| WindowError::Backend("the gtk thread ended while starting".into()))?
        .map_err(WindowError::Backend)?;
    *context = Some(started.clone());
    Ok(started)
}

/// The window's default size for a picture of `width`×`height`: the picture
/// fitted to the largest monitor, plus the title bar.
///
/// A GTK 4 window's default size includes its title bar, so a window sized to
/// the picture alone gives the picture that many rows less than it needs, and
/// `Contain` fills the difference with bars down both sides of a phone.
fn fitted(width: u32, height: u32, titlebar: i32) -> (i32, i32) {
    let (w, h) = fit_into(width, height, largest_monitor());
    (w, h + titlebar)
}

/// The largest monitor's size. Wayland tells a client nothing about panels or
/// where its window is, so this is the best `room` there is.
fn largest_monitor() -> Option<(i32, i32)> {
    gtk::gdk::Display::default().and_then(|display| {
        let monitors = display.monitors();
        (0..monitors.n_items())
            .filter_map(|i| monitors.item(i)?.downcast::<gtk::gdk::Monitor>().ok())
            .map(|m| m.geometry())
            .map(|g| (g.width(), g.height()))
            .max_by_key(|(w, h)| w * h)
    })
}

/// One window's H.264 decoder, on a thread of its own.
struct VideoThread {
    packets: std::sync::mpsc::Sender<(u32, Vec<u8>)>,
    /// Converted pictures, at most one waiting: a window that falls behind
    /// holds the decoder back, which then decodes what queued up meanwhile
    /// and converts only the last of it.
    pictures: std::sync::mpsc::Receiver<(u32, crate::video::DecodedFrame)>,
}

/// Start the decoder thread for window `id`. It ends when the window drops
/// its [`VideoThread`].
fn spawn_video(
    id: WindowId,
    wants_keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> VideoThread {
    let (packets, packets_rx) = std::sync::mpsc::channel::<(u32, Vec<u8>)>();
    let (pictures_tx, pictures) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name(format!("surrogate-video-{id}"))
        .spawn(move || {
            let mut decoder: Option<VideoDecoder> = None;
            while let Ok(first) = packets_rx.recv() {
                let mut newest = None;
                for (seq, packet) in std::iter::once(first).chain(packets_rx.try_iter()) {
                    if decoder.is_none() {
                        match VideoDecoder::open() {
                            Ok(d) => decoder = Some(d),
                            Err(e) => {
                                tracing::error!(error = %e, "no H.264 decoder; video frames will be dropped");
                                continue;
                            }
                        }
                    }
                    let Some(dec) = decoder.as_mut() else {
                        continue;
                    };
                    match dec.feed(&packet) {
                        Ok(()) => newest = Some(seq),
                        Err(e) => {
                            // Its reference pictures are now suspect; start
                            // again from the host's next keyframe.
                            tracing::warn!(error = %e, "video decode failed; restarting the decoder");
                            decoder = None;
                        }
                    }
                    if !decoder.as_ref().is_some_and(VideoDecoder::synced) {
                        wants_keyframe.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                let (Some(dec), Some(seq)) = (decoder.as_mut(), newest) else {
                    continue;
                };
                match dec.take_newest() {
                    Ok(Some(pic)) => {
                        if pictures_tx.send((seq, pic)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "video conversion failed; restarting the decoder");
                        decoder = None;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "cannot start the video decoder thread; video frames will be dropped");
    }
    VideoThread { packets, pictures }
}

/// Build one window on the GTK thread, with the tick that feeds it.
///
/// The window lives as long as its tick: the tick owns the command receiver,
/// the decoder and the senders back to the session, so ending it is what tells
/// the session the window is gone.
fn build_window(
    meta: WindowMeta,
    mut commands: mpsc::UnboundedReceiver<SurrogateCommand>,
    input_tx: mpsc::UnboundedSender<WindowInput>,
    presented_tx: mpsc::UnboundedSender<u32>,
    resized_tx: mpsc::UnboundedSender<(u32, u32)>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
    wants_keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let id = meta.id;
    let fb = Rc::new(RefCell::new(Framebuffer::new(meta.width, meta.height)));

    let picture = gtk::Picture::builder()
        // `Contain`, not `Fill`: a window taller than the monitor — a phone's
        // screen is 1920 high — is cut to the monitor by the compositor, and
        // `Fill` then stretched the picture across whatever was left. The
        // letterbox this leaves is accounted for in `to_host`.
        .content_fit(gtk::ContentFit::Contain)
        .build();
    picture.set_paintable(Some(&to_texture(&fb.borrow())));

    let window = gtk::Window::builder()
        .title(&meta.title)
        .child(&picture)
        .build();
    // An explicit header bar, the same one GTK would draw, so its height can
    // be measured and added to the picture's.
    let header = gtk::HeaderBar::new();
    window.set_titlebar(Some(&header));
    let titlebar = header.measure(gtk::Orientation::Vertical, -1).1;
    let (width, height) = fitted(meta.width, meta.height, titlebar);
    window.set_default_size(width, height);

    attach_input_controllers(&window, &picture, id, input_tx, fb.clone());
    // Report the size the user drags this window to, so the host can resize
    // the real one. `default-width`/`default-height` rather than a
    // size-allocate on the picture: the latter also fires for the resizes we
    // perform ourselves when adopting the host's geometry, which would bounce
    // a request straight back at the host for the size it just gave us.
    for property in ["default-width", "default-height"] {
        let tx = resized_tx.clone();
        window.connect_notify_local(Some(property), move |win, _| {
            let (w, h) = (win.default_width(), win.default_height() - titlebar);
            if w > 0 && h > 0 {
                let _ = tx.send((w as u32, h as u32));
            }
        });
    }

    // Drain the command channel on the GTK thread. `try_recv` in a timeout
    // source keeps this cooperative: GTK stays free to paint and dispatch input
    // between batches instead of being blocked on the channel.
    let main_loop_window = window.clone();
    let picture_for_frames = picture.clone();
    let fb_for_frames = fb.clone();
    // Started on the first video packet, so a purely lossless stream never
    // has one.
    let mut video: Option<VideoThread> = None;
    // Raised when the user closes the window, so the tick ends with it.
    let closed = Rc::new(std::cell::Cell::new(false));
    let closed_for_tick = closed.clone();
    // Below GTK's input and redraw priorities. A phone's H.264 decoded here
    // takes a tick past 100 ms, and at the default priority — the same as
    // input — a tick re-armed every 4 ms kept the loop busy for good: the
    // close button and the host's close waited behind the frames, and the
    // window could only be got rid of by stopping the node.
    glib::timeout_add_local_full(
        std::time::Duration::from_millis(4),
        glib::Priority::DEFAULT_IDLE,
        move || {
            // Time each tick. GTK dispatches this on the UI thread, so anything slow
            // here stops the window answering the compositor — which is exactly what
            // the desktop's "wait / force quit" dialog reports. A threshold log names
            // the culprit instead of leaving "it froze" to guesswork.
            let tick_started = std::time::Instant::now();
            if closed_for_tick.get() {
                tracing::info!(id, "surrogate window closed here");
                return glib::ControlFlow::Break;
            }

            let mut painted = false;
            let mut presented_seq: Option<u32> = None;
            // Bounded per tick: a burst of frames must not starve the paint that
            // makes them visible.
            for _ in 0..16 {
                match commands.try_recv() {
                    Ok(SurrogateCommand::Frame { seq, dirty, raw }) => {
                        if let Err(e) = fb_for_frames.borrow_mut().blit(dirty, &raw) {
                            tracing::warn!(error = %e, "dropping malformed tile");
                            continue;
                        }
                        painted = true;
                        presented_seq =
                            Some(presented_seq.map_or(seq, |previous| previous.max(seq)));
                    }
                    Ok(SurrogateCommand::VideoPacket { seq, packet }) => {
                        let thread =
                            video.get_or_insert_with(|| spawn_video(id, wants_keyframe.clone()));
                        let _ = thread.packets.send((seq, packet));
                    }
                    Ok(SurrogateCommand::Resize { width, height }) => {
                        fb_for_frames.borrow_mut().resize(width, height);
                        let (w, h) = fitted(width, height, titlebar);
                        main_loop_window.set_default_size(w, h);
                        painted = true;
                    }
                    Ok(SurrogateCommand::Grow { width, height }) => {
                        fb_for_frames.borrow_mut().grow_to(width, height);
                        let (w, h) = fitted(width, height, titlebar);
                        main_loop_window.set_default_size(w, h);
                        painted = true;
                    }
                    Ok(SurrogateCommand::Platform(never)) => match never {},
                    Ok(SurrogateCommand::Close(reason)) => {
                        tracing::info!(?reason, id, "surrogate window closing");
                        // `destroy`, not `close`: a close is a request the window
                        // may answer, and this one has to go.
                        main_loop_window.destroy();
                        return glib::ControlFlow::Break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        // Session gone (peer disconnected, process shutting down):
                        // same teardown as an explicit Close.
                        tracing::info!(id, "surrogate command channel closed; tearing down");
                        main_loop_window.destroy();
                        return glib::ControlFlow::Break;
                    }
                }
            }
            // Only the newest picture is worth drawing; the decoder thread already
            // skipped converting the ones before it.
            if let Some((seq, pic)) = video
                .as_ref()
                .and_then(|thread| thread.pictures.try_iter().last())
            {
                let mut fb = fb_for_frames.borrow_mut();
                // A video frame is the whole window, and the stream carries its own
                // geometry — adopt it if it disagrees, rather than refusing the blit.
                // The window follows too, so a stream that changes resolution — a
                // quality change, or an encoder rounding up to whole macroblocks — is
                // not shown scaled into the old window's size for the rest of the
                // session.
                if fb.width != pic.width || fb.height != pic.height {
                    let (w, h) = refit(
                        (picture_for_frames.width(), picture_for_frames.height()),
                        (fb.width, fb.height),
                        (pic.width, pic.height),
                        largest_monitor(),
                    );
                    fb.resize(pic.width, pic.height);
                    main_loop_window.set_default_size(w, h + titlebar);
                }
                let full = Rect {
                    x: 0,
                    y: 0,
                    width: pic.width,
                    height: pic.height,
                };
                match fb.blit(full, &pic.pixels) {
                    Ok(()) => {
                        painted = true;
                        presented_seq =
                            Some(presented_seq.map_or(seq, |previous| previous.max(seq)));
                    }
                    Err(e) => tracing::warn!(error = %e, "dropping decoded video frame"),
                }
            }

            // One texture swap per tick, not per tile: several tiles of the same
            // visual update coalesce into a single present.
            if painted {
                picture_for_frames.set_paintable(Some(&to_texture(&fb_for_frames.borrow())));
                if let Some(seq) = presented_seq {
                    let _ = presented_tx.send(seq);
                }
            }
            let took = tick_started.elapsed();
            if took >= std::time::Duration::from_millis(100) {
                tracing::warn!(
                    took_ms = took.as_millis() as u64,
                    "surrogate tick blocked the GTK loop"
                );
            }
            glib::ControlFlow::Continue
        },
    );

    window.connect_close_request(move |_| {
        closed.set(true);
        glib::Propagation::Proceed
    });
    window.present();
    let _ = ready_tx.send(Ok(()));
}

/// Wire pointer, scroll, and keyboard on the surrogate into [`WindowInput`].
///
/// Coordinates are reported **in the host window's pixel space**: GTK gives us
/// widget-local coordinates, which match 1:1 only while the surrogate is shown
/// unscaled, so we rescale by the ratio between the widget's current size and
/// the framebuffer's. Without that, dragging a resized surrogate would land
/// clicks in the wrong place on the host.
fn attach_input_controllers(
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
fn map_button(gdk_button: u32) -> Option<cursio_protocol::MouseButton> {
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
fn evdev_from_gdk_keycode(gdk_keycode: u32) -> Option<u16> {
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

#[cfg(test)]
mod framebuffer_tests {
    use super::*;

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
