//! The GTK window itself: built on the one GTK thread, sized to the picture
//! within the largest monitor, and fed by a video thread of its own.

use crate::linux_surrogate::input::attach_input_controllers;
use crate::linux_surrogate::to_texture;
use crate::linux_surrogate::SurrogateCommand;
use crate::GtkSurrogate;
use crate::{
    framebuffer::{fit_into, refit, Framebuffer},
    surrogate_front::SurrogateFront,
    video_decoder::VideoDecoder,
    WindowError,
};
use cursio_protocol::{Rect, WindowId, WindowInput, WindowMeta};
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc;

/// Start the GTK thread for one surrogate and wait until its window is up.
///
/// Blocks on readiness so a failure to reach the display server surfaces from
/// `open()` rather than as a window that never appears — the same contract as
/// the capture side's `begin()`.
pub(super) fn spawn_surrogate(meta: &WindowMeta) -> Result<GtkSurrogate, WindowError> {
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
pub(super) fn gtk_context() -> Result<glib::MainContext, WindowError> {
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
pub(super) fn fitted(width: u32, height: u32, titlebar: i32) -> (i32, i32) {
    let (w, h) = fit_into(width, height, largest_monitor());
    (w, h + titlebar)
}

/// The largest monitor's size. Wayland tells a client nothing about panels or
/// where its window is, so this is the best `room` there is.
pub(super) fn largest_monitor() -> Option<(i32, i32)> {
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
pub(super) struct VideoThread {
    packets: std::sync::mpsc::Sender<(u32, Vec<u8>)>,
    /// Converted pictures, at most one waiting: a window that falls behind
    /// holds the decoder back, which then decodes what queued up meanwhile
    /// and converts only the last of it.
    pictures: std::sync::mpsc::Receiver<(u32, crate::video::DecodedFrame)>,
}

/// Start the decoder thread for window `id`. It ends when the window drops
/// its [`VideoThread`].
pub(super) fn spawn_video(
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
pub(super) fn build_window(
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
