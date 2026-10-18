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

use std::cell::RefCell;
use std::rc::Rc;

use cursio_protocol::{
    Rect, TileCodec, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput,
    WindowMeta,
};
use gtk::prelude::*;
use gtk::{gdk, glib};
use tokio::sync::mpsc;

use crate::{
    codec::decode_tile,
    framebuffer::{geometry_of, Framebuffer, Geometry},
    presentation_ack::PresentationAcks,
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

/// What the session hands the GTK thread.
enum SurrogateCommand {
    /// A decoded tile to blit and present.
    Frame { seq: u32, dirty: Rect, raw: Vec<u8> },
    /// A raw H.264 packet, to be decoded **on the GTK thread**.
    ///
    /// Every other codec is decoded on the session's thread, to keep expensive
    /// work off the UI. Video cannot be: `ffmpeg`'s scaling context is not
    /// `Send` (the crate deliberately declines to claim it is, though it does
    /// claim it for codec contexts), and [`SessionModule`] requires `Send`. So
    /// the packet crosses the channel instead of the pixels, and the decoder
    /// lives beside the framebuffer it feeds. That is also where the decoded
    /// frame is wanted, so it costs one less copy.
    VideoPacket { seq: u32, packet: Vec<u8> },
    /// Window geometry changed; reallocate before the next blit.
    Resize { width: u32, height: u32 },
    /// The window grew, learnt from a tile rather than a full frame. Keeps the
    /// pixels already drawn — see [`Framebuffer::grow_to`].
    Grow { width: u32, height: u32 },
    /// Tear the window down.
    Close(WindowCloseReason),
}

/// A surrogate window driven from the session, rendered by GTK on its own thread.
pub struct GtkSurrogate {
    id: WindowId,
    /// Geometry as the viewer currently believes it — used to spot a resize and
    /// to size-check tiles before they reach the GTK thread.
    width: u32,
    height: u32,
    commands: mpsc::UnboundedSender<SurrogateCommand>,
    input: mpsc::UnboundedReceiver<WindowInput>,
    presented: mpsc::UnboundedReceiver<u32>,
    /// Sizes the user dragged this window to, newest wins. See
    /// [`Surrogate::take_resize_request`].
    resized: mpsc::UnboundedReceiver<(u32, u32)>,
    /// The last size actually asked of the host, so a drag that returns to a
    /// size already requested does not ask again.
    requested_size: Option<(u32, u32)>,
    acks: PresentationAcks,
    /// Whether a `LossyVideo` packet has been seen, so the log says once that
    /// the stream switched to video. The decoder itself lives on the GTK
    /// thread — see [`SurrogateCommand::VideoPacket`].
    video_started: bool,
    /// Raised by the GTK thread while its decoder waits for a keyframe.
    wants_keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Surrogate for GtkSurrogate {
    fn apply_frame(&mut self, frame: &WindowFrame) -> Result<(), WindowError> {
        // A frame the picture cannot hold means the host resized the window;
        // adopt the new geometry before blitting into it.
        match geometry_of(frame.dirty, frame.codec, self.width, self.height) {
            Geometry::Unchanged => {}
            Geometry::Resize { width, height } => {
                (self.width, self.height) = (width, height);
                let _ = self
                    .commands
                    .send(SurrogateCommand::Resize { width, height });
            }
            Geometry::Grow { width, height } => {
                (self.width, self.height) = (width, height);
                let _ = self.commands.send(SurrogateCommand::Grow { width, height });
            }
        }

        // `LossyVideo` is the one codec whose packets are *not* self-contained:
        // each references earlier ones, so it needs a decoder that persists
        // across frames, and one packet may yield zero pictures (the decoder is
        // still accumulating) or several. Every other codec is a pure
        // bytes-in/pixels-out transform.
        if frame.codec == TileCodec::LossyVideo {
            if !self.video_started {
                self.video_started = true;
                tracing::info!(id = self.id, "host switched this window to a video stream");
            }
            // The packet, not the pixels, crosses to the GTK thread — the
            // decoder cannot live here (see `SurrogateCommand::VideoPacket`).
            self.commands
                .send(SurrogateCommand::VideoPacket {
                    seq: frame.seq,
                    packet: frame.data.clone(),
                })
                .map_err(|_| WindowError::Backend("surrogate window is gone".into()))?;
            return Ok(());
        }

        let expected = (frame.dirty.width as usize) * (frame.dirty.height as usize) * 4;
        // Decoding happens here, off the GTK thread: zstd on a large tile is
        // real work and the UI thread must stay free to actually paint.
        let raw = decode_tile(frame.codec, &frame.data, expected)?;
        self.commands
            .send(SurrogateCommand::Frame {
                seq: frame.seq,
                dirty: frame.dirty,
                raw,
            })
            .map_err(|_| WindowError::Backend("surrogate window is gone".into()))?;

        Ok(())
    }

    fn input(&mut self) -> &mut mpsc::UnboundedReceiver<WindowInput> {
        &mut self.input
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        while let Ok(seq) = self.presented.try_recv() {
            self.acks.note_presented(seq);
        }
        self.acks.take()
    }

    fn take_keyframe_request(&mut self) -> bool {
        self.wants_keyframe
            .swap(false, std::sync::atomic::Ordering::Relaxed)
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
        if Some(size) == self.requested_size || size == (self.width, self.height) {
            return None;
        }
        self.requested_size = Some(size);
        Some(size)
    }

    fn close(&mut self, reason: WindowCloseReason) {
        tracing::info!(?reason, id = self.id, "closing surrogate window");
        let _ = self.commands.send(SurrogateCommand::Close(reason));
    }
}

impl Drop for GtkSurrogate {
    fn drop(&mut self) {
        let _ = self
            .commands
            .send(SurrogateCommand::Close(WindowCloseReason::ViewerClosed));
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
    std::thread::Builder::new()
        .name(format!("cursio-surrogate-{}", meta.id))
        .spawn(move || {
            gtk_thread(
                thread_meta,
                cmd_rx,
                input_tx,
                presented_tx,
                resized_tx,
                ready_tx,
                thread_wants_keyframe,
            )
        })
        .map_err(|e| WindowError::Backend(format!("spawn surrogate thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(GtkSurrogate {
            id: meta.id,
            width: meta.width,
            height: meta.height,
            commands: cmd_tx,
            input: input_rx,
            presented: presented_rx,
            resized: resized_rx,
            requested_size: None,
            acks: PresentationAcks::new(meta.id),
            video_started: false,
            wants_keyframe,
        }),
        Ok(Err(e)) => Err(WindowError::Backend(e)),
        Err(_) => Err(WindowError::Backend(
            "surrogate thread ended before signalling readiness".into(),
        )),
    }
}

/// The GTK thread: owns the window, the framebuffer, and the main loop.
fn gtk_thread(
    meta: WindowMeta,
    mut commands: mpsc::UnboundedReceiver<SurrogateCommand>,
    input_tx: mpsc::UnboundedSender<WindowInput>,
    presented_tx: mpsc::UnboundedSender<u32>,
    resized_tx: mpsc::UnboundedSender<(u32, u32)>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
    wants_keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    if let Err(e) = gtk::init() {
        let _ = ready_tx.send(Err(format!("gtk init: {e}")));
        return;
    }

    let id = meta.id;
    let fb = Rc::new(RefCell::new(Framebuffer::new(meta.width, meta.height)));

    let picture = gtk::Picture::builder()
        // `Fill` maps the texture onto the whole allocation, which keeps the
        // widget↔host coordinate mapping a single ratio (see `to_host`) — the
        // letterboxing of `Contain` would put the host's origin at an offset
        // this code would then have to track. Sharpness comes from the window
        // being resized to the source's resolution whenever that changes, so
        // the ratio stays 1:1 and nothing is resampled.
        .content_fit(gtk::ContentFit::Fill)
        .build();
    picture.set_paintable(Some(&to_texture(&fb.borrow())));

    let window = gtk::Window::builder()
        .title(&meta.title)
        .default_width(meta.width as i32)
        .default_height(meta.height as i32)
        .child(&picture)
        .build();

    attach_input_controllers(&window, &picture, id, input_tx, fb.clone());
    // Report the size the user drags this window to, so the host can resize
    // the real one. `default-width`/`default-height` rather than a
    // size-allocate on the picture: the latter also fires for the resizes we
    // perform ourselves when adopting the host's geometry, which would bounce
    // a request straight back at the host for the size it just gave us.
    for property in ["default-width", "default-height"] {
        let (tx, win) = (resized_tx.clone(), window.clone());
        window.connect_notify_local(Some(property), move |_, _| {
            let (w, h) = (win.default_width(), win.default_height());
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
    // Lives on this thread because ffmpeg's scaler isn't `Send`; opened on the
    // first video packet, so a purely lossless stream never allocates one.
    let mut decoder: Option<VideoDecoder> = None;
    // Created before the timeout so the tick can end the loop itself; `run()` is
    // called further down once the window is up.
    let quit = glib::MainLoop::new(None, false);
    let quit_for_cmds = quit.clone();
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
                        // Opened on first use: a window that never sends video
                        // never pays for a decoder.
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
                        match dec.decode(&packet) {
                            Ok(pictures) => {
                                for pic in pictures {
                                    let mut fb = fb_for_frames.borrow_mut();
                                    // A video frame is the whole window, and the
                                    // stream carries its own geometry — adopt it if
                                    // it disagrees, rather than refusing the blit.
                                    if fb.width != pic.width || fb.height != pic.height {
                                        fb.resize(pic.width, pic.height);
                                        // The window follows too. `ContentFit::Fill`
                                        // stretches the texture across whatever
                                        // allocation it is given, so a stream that
                                        // changes resolution — a quality change, or
                                        // an encoder rounding up to whole macroblocks
                                        // — would otherwise be scaled into the old
                                        // window's size for the rest of the session.
                                        main_loop_window
                                            .set_default_size(pic.width as i32, pic.height as i32);
                                    }
                                    let full = Rect {
                                        x: 0,
                                        y: 0,
                                        width: pic.width,
                                        height: pic.height,
                                    };
                                    if let Err(e) = fb.blit(full, &pic.pixels) {
                                        tracing::warn!(error = %e, "dropping decoded video frame");
                                        continue;
                                    }
                                    painted = true;
                                    presented_seq = Some(
                                        presented_seq.map_or(seq, |previous| previous.max(seq)),
                                    );
                                }
                            }
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
                    Ok(SurrogateCommand::Resize { width, height }) => {
                        fb_for_frames.borrow_mut().resize(width, height);
                        main_loop_window.set_default_size(width as i32, height as i32);
                        painted = true;
                    }
                    Ok(SurrogateCommand::Grow { width, height }) => {
                        fb_for_frames.borrow_mut().grow_to(width, height);
                        main_loop_window.set_default_size(width as i32, height as i32);
                        painted = true;
                    }
                    Ok(SurrogateCommand::Close(reason)) => {
                        tracing::info!(?reason, id, "surrogate window closing");
                        main_loop_window.close();
                        // Quit the loop explicitly.
                        //
                        // `ControlFlow::Break` only stops THIS timeout, and
                        // `window.close()` merely *requests* a close — it does not
                        // raise `close-request`, which is what `quit()` was wired to.
                        // So the GTK main loop kept running with nothing left to
                        // service it: the window stayed on screen and stopped
                        // answering the compositor, which the desktop reports as
                        // "GTK Application is not responding" with wait/force-quit.
                        quit_for_cmds.quit();
                        return glib::ControlFlow::Break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        // Session gone (peer disconnected, process shutting down):
                        // same teardown as an explicit Close.
                        tracing::info!(id, "surrogate command channel closed; tearing down");
                        main_loop_window.close();
                        quit_for_cmds.quit();
                        return glib::ControlFlow::Break;
                    }
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

    window.present();
    let _ = ready_tx.send(Ok(()));

    // Run until the window goes away (closed by us or by the user).
    let quit_on_close = quit.clone();
    window.connect_close_request(move |_| {
        quit_on_close.quit();
        glib::Propagation::Proceed
    });
    quit.run();
    tracing::info!(id, "surrogate GTK thread finished");
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
    /// Map a widget-local point onto host pixels.
    fn to_host(
        picture: &gtk::Picture,
        fb: &Rc<RefCell<Framebuffer>>,
        x: f64,
        y: f64,
    ) -> (u32, u32) {
        let (w, h) = (picture.width() as f64, picture.height() as f64);
        let fb = fb.borrow();
        if w <= 0.0 || h <= 0.0 {
            return (0, 0);
        }
        let hx = (x / w * fb.width as f64).clamp(0.0, (fb.width.saturating_sub(1)) as f64);
        let hy = (y / h * fb.height as f64).clamp(0.0, (fb.height.saturating_sub(1)) as f64);
        (hx as u32, hy as u32)
    }

    // Pointer motion.
    let motion = gtk::EventControllerMotion::new();
    {
        let (tx, pic, fb) = (input_tx.clone(), picture.clone(), fb.clone());
        motion.connect_motion(move |_, x, y| {
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
        let (tx, pic, fb) = (input_tx.clone(), picture.clone(), fb.clone());
        click.connect_pressed(move |g, _, x, y| {
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
        let (tx, pic, fb) = (input_tx.clone(), picture.clone(), fb.clone());
        click.connect_released(move |g, _, x, y| {
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
