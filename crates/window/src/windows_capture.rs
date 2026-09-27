//! Windows window capture: `Windows.Graphics.Capture` → [`WindowFrame`].
//!
//! The host side on Windows. `CreateForWindow` captures one `HWND` into a pool
//! of D3D11 textures; each is copied to a CPU-readable staging texture, repacked
//! to tight BGRA, and fed to the same [`frame_pipeline`](crate::frame_pipeline)
//! the Linux backend uses — so diffing and codec routing behave identically on
//! both platforms.
//!
//! A remote-screen view reuses the same path for any window or a whole monitor
//! (`CreateForMonitor`), chosen from the list in `sources.rs`. A window shown
//! that way is never hidden: the person at this machine is still using it.
//!
//! ## Hiding the source window
//!
//! The feature's goal is that a transferred window *disappears* from the host,
//! so the pair reads as one desktop. Measured on Windows 11 build 26200 against
//! a running Minecraft (see `docs/window-transfer-design.md`):
//!
//! | Window state | FPS from capture |
//! |---|---|
//! | normal, visible | 30 |
//! | layered `alpha=0` | 30 |
//! | fully offscreen | 30 |
//! | **minimised** | **0** |
//!
//! So hiding is done with `WS_EX_LAYERED` + `alpha=0`, and **minimising is
//! forbidden** — it is the one state that stops frames entirely (and also stops
//! `PostMessage` delivery, which the input path needs). The original style is
//! restored when the transfer ends.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cursio_protocol::{
    WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput, WindowMeta,
};
use tokio::sync::mpsc;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

use crate::frame_pipeline::CapturePipeline;
use crate::video_encoder_win::MfEncoders;
use crate::{CaptureSession, WindowCapture, WindowError};

mod d3d;
mod hide;
mod sources;
mod window_pick;

pub use hide::HiddenWindow;

/// What the capture thread is told from outside.
enum CaptureCommand {
    /// Replay one viewer input event onto the real window.
    Input(WindowInput),
    /// Resize the real window because the viewer resized its surrogate.
    Resize { width: u32, height: u32 },
    /// Send the viewer the whole current picture; see
    /// [`CaptureSession::request_keyframe`].
    Keyframe,
    /// Stop capturing and end the thread.
    Stop,
}

/// A live capture of one window via Windows.Graphics.Capture.
pub struct WgcCaptureSession {
    meta: WindowMeta,
    /// Shared with the capture thread: quality is written here as acks arrive
    /// and read there per frame; `last_seq` the other way round.
    quality: Arc<crate::frame_pipeline::AdaptiveQuality>,
    last_seq: Arc<std::sync::atomic::AtomicU32>,
    frames: mpsc::Receiver<WindowFrame>,
    commands: mpsc::UnboundedSender<CaptureCommand>,
    /// Set when the capture thread stops on its own (window closed), so
    /// `close()` doesn't wait on a thread that is already gone.
    ended: Arc<AtomicBool>,
}

impl CaptureSession for WgcCaptureSession {
    fn meta(&self) -> &WindowMeta {
        &self.meta
    }

    fn frames(&mut self) -> &mut mpsc::Receiver<WindowFrame> {
        &mut self.frames
    }

    fn apply_input(&mut self, input: WindowInput) -> Result<(), WindowError> {
        // Replay happens on the capture thread, which owns the HWND state and
        // must not be raced from the session loop.
        self.commands
            .send(CaptureCommand::Input(input))
            .map_err(|_| WindowError::Backend("capture thread is gone".into()))
    }

    fn request_resize(&mut self, width: u32, height: u32) -> Result<(), WindowError> {
        // Same reasoning as `apply_input`: the HWND belongs to the capture
        // thread, and resizing from here would race the frame it is producing.
        self.commands
            .send(CaptureCommand::Resize { width, height })
            .map_err(|_| WindowError::Backend("capture thread is gone".into()))
    }

    fn on_ack(&mut self, ack: WindowFrameAck) {
        // How far ahead of the viewer we are, in frames. `seq` is monotonic per
        // window, so the difference is the backlog the link has not carried.
        let sent = self.last_seq.load(Ordering::Relaxed);
        let gap = sent.saturating_sub(ack.acked_seq);
        if let Some(threshold) = self.quality.observe(gap) {
            tracing::info!(
                id = self.meta.id,
                gap,
                threshold,
                "link changed; adjusting how much of the window stays lossless"
            );
        }
    }

    fn request_keyframe(&mut self) {
        let _ = self.commands.send(CaptureCommand::Keyframe);
    }

    fn close(&mut self, reason: WindowCloseReason) {
        tracing::info!(?reason, id = self.meta.id, "closing window capture");
        let _ = self.commands.send(CaptureCommand::Stop);
    }
}

impl Drop for WgcCaptureSession {
    fn drop(&mut self) {
        if !self.ended.load(Ordering::Relaxed) {
            let _ = self.commands.send(CaptureCommand::Stop);
        }
    }
}

/// The Windows [`WindowCapture`] backend.
///
/// `title_hint` selects which window to transfer. Until the drag trigger exists
/// there is no gesture to infer it from, so the caller names one; an empty hint
/// takes the foreground window, which is the closest thing to "the one the user
/// meant".
pub struct WgcWindowCapture {
    pub title_hint: String,
    /// Whether to hide the source window while it is being transferred.
    pub hide_source: bool,
}

impl Default for WgcWindowCapture {
    fn default() -> Self {
        Self {
            title_hint: String::new(),
            hide_source: true,
        }
    }
}

impl WindowCapture for WgcWindowCapture {
    type Session = WgcCaptureSession;

    fn aim_at(&mut self, title: &str) {
        self.title_hint = title.to_string();
    }

    fn begin(&mut self, id: WindowId) -> Result<Self::Session, WindowError> {
        let (hwnd, title) = window_pick::find(&self.title_hint)?;
        tracing::info!(title = %title, "capturing window");
        spawn_capture(
            id,
            Target::Window {
                hwnd: hwnd.0 as isize,
                hide: self.hide_source,
            },
            title,
        )
    }
}

/// What the capture thread captures.
///
/// Handles cross to the thread as integers: `HWND` and `HMONITOR` are raw
/// pointers and so not `Send`, and they are only ever used on that thread.
#[derive(Debug, Clone, Copy)]
enum Target {
    /// One window — a transfer, or a window picked for a remote-screen view.
    Window { hwnd: isize, hide: bool },
    /// A whole monitor, for a remote-screen view. Never hidden, never driven:
    /// there is no window to hide or to replay input into.
    Monitor {
        monitor: isize,
        width: u32,
        height: u32,
    },
}

/// The Windows [`ScreenSources`](crate::ScreenSources): every top-level window
/// and every monitor, enumerated directly — Windows answers the question GNOME
/// refuses to.
pub struct WgcScreenSources;

impl crate::ScreenSources for WgcScreenSources {
    fn list(&self) -> Result<Vec<cursio_protocol::ScreenSource>, WindowError> {
        Ok(sources::list())
    }

    fn open(
        &self,
        id: WindowId,
        source: &cursio_protocol::ScreenSource,
    ) -> Result<Box<dyn CaptureSession + Send>, WindowError> {
        let target = sources::resolve(source)?;
        tracing::info!(title = %source.title, "capturing a source for the peer's view");
        let session = spawn_capture(id, target, source.title.clone())?;
        Ok(Box::new(session))
    }
}

/// Start the capture thread and block until it reports the window's geometry.
///
/// Blocking mirrors the Linux backend: a window that can't be captured must
/// fail from `begin()`, not silently produce nothing. The wait is short by
/// construction — the thread answers from `GetClientRect` before it builds the
/// D3D pipeline, never from the first frame, which may be a long way off for a
/// window that is not redrawing (see `capture_thread`).
fn spawn_capture(
    id: WindowId,
    target: Target,
    title: String,
) -> Result<WgcCaptureSession, WindowError> {
    let (frames_tx, frames_rx) = mpsc::channel(crate::CAPTURE_FRAME_QUEUE_CAPACITY);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<WindowMeta, String>>();
    let ended = Arc::new(AtomicBool::new(false));
    let quality = Arc::new(crate::frame_pipeline::AdaptiveQuality::default());
    let last_seq = Arc::new(std::sync::atomic::AtomicU32::new(0));

    let thread_ended = ended.clone();
    let thread_quality = quality.clone();
    let thread_last_seq = last_seq.clone();
    std::thread::Builder::new()
        .name("cursio-wgc-capture".into())
        .spawn(move || {
            let result = capture_thread(
                id,
                target,
                title,
                frames_tx,
                cmd_rx,
                &ready_tx,
                thread_quality,
                thread_last_seq,
            );
            if let Err(e) = result {
                tracing::error!(error = %e, "window capture ended with an error");
                let _ = ready_tx.send(Err(e));
            }
            thread_ended.store(true, Ordering::Relaxed);
        })
        .map_err(|e| WindowError::Backend(format!("spawn capture thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(meta)) => Ok(WgcCaptureSession {
            meta,
            frames: frames_rx,
            commands: cmd_tx,
            ended,
            quality,
            last_seq,
        }),
        Ok(Err(e)) => Err(WindowError::Backend(e)),
        Err(_) => Err(WindowError::Backend(
            "capture thread ended before signalling readiness".into(),
        )),
    }
}

/// Copy a GPU capture texture into tightly packed BGRA on the CPU.
///
/// Capture hands back a texture the CPU cannot read, so it is copied into a
/// staging texture first. The mapped rows are `RowPitch` bytes apart, which is
/// usually wider than `width * 4` — the same padding trap as PipeWire's
/// `chunk.stride`, and repacked here for the same reason: everything downstream
/// assumes no padding.
pub(crate) fn texture_to_bgra(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    staging: &mut Option<ID3D11Texture2D>,
) -> Result<(Vec<u8>, u32, u32), String> {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut desc);
        let (w, h) = (desc.Width, desc.Height);

        // Rebuild the staging texture when the window resizes.
        let needs_new = match staging.as_ref() {
            Some(tex) => {
                let mut sd = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut sd);
                sd.Width != w || sd.Height != h
            }
            None => true,
        };
        if needs_new {
            let sd = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            device
                .CreateTexture2D(&sd, None, Some(&mut tex))
                .map_err(|e| format!("staging texture: {e}"))?;
            *staging = tex;
        }
        let dst = staging.as_ref().ok_or("no staging texture")?;

        context.CopyResource(dst, src);

        let mut mapped = Default::default();
        context
            .Map(dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("map staging texture: {e}"))?;

        let row_bytes = (w as usize) * 4;
        let pitch = mapped.RowPitch as usize;
        let mut out = Vec::with_capacity(row_bytes * h as usize);
        let base = mapped.pData as *const u8;
        for y in 0..h as usize {
            let row = std::slice::from_raw_parts(base.add(y * pitch), row_bytes);
            out.extend_from_slice(row);
        }
        context.Unmap(dst, 0);

        Ok((out, w, h))
    }
}

/// The capture thread: owns the WGC session, the D3D device, and the HWND.
///
/// All of it stays here because none of it is `Send`; only channels cross out.
#[allow(clippy::too_many_arguments)]
fn capture_thread(
    id: WindowId,
    target: Target,
    title: String,
    frames_tx: mpsc::Sender<WindowFrame>,
    mut cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
    ready_tx: &std::sync::mpsc::Sender<Result<WindowMeta, String>>,
    quality: Arc<crate::frame_pipeline::AdaptiveQuality>,
    last_seq: Arc<std::sync::atomic::AtomicU32>,
) -> Result<(), String> {
    // Join the multi-threaded apartment BEFORE touching anything WinRT.
    //
    // Windows.Graphics.Capture is WinRT, and its objects — the capture item, the
    // frame pool, the session — have apartment affinity. On a bare thread with
    // no apartment, creation and `StartCapture` still *succeed*, but
    // `FrameArrived` is never delivered: no error anywhere, simply no frames.
    // That was the bug — 15500 polls, zero frames, on a thread that had never
    // called CoInitializeEx.
    //
    // MULTITHREADED specifically: `CreateFreeThreaded` delivers events on a
    // pool thread, which an MTA can receive directly. An STA would additionally
    // require a running message pump, which this thread does not have.
    //
    // The audio backend already does exactly this (`ComGuard` in
    // wasapi_backend.rs); window capture simply never did.
    // SAFETY: called once, on this thread, before any COM/WinRT use; the
    // matching CoUninitialize runs when the thread ends below.
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .map_err(|e| format!("CoInitializeEx: {e}"))?;
    }
    // Balance the apartment on every exit path.
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            // SAFETY: matches the CoInitializeEx above, on the same thread.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
    let _com = ComGuard;

    let (hwnd, hide_source) = match target {
        Target::Window { hwnd, hide } => (HWND(hwnd as *mut core::ffi::c_void), hide),
        Target::Monitor {
            monitor,
            width,
            height,
        } => {
            return monitor_capture_loop(
                id,
                windows::Win32::Graphics::Gdi::HMONITOR(monitor as *mut core::ffi::c_void),
                (width, height),
                title,
                frames_tx,
                cmd_rx,
                ready_tx,
                quality,
                last_seq,
            )
        }
    };

    // Un-minimise first: a minimised window is the one state WGC yields nothing
    // from (see the table at the top of this file), and it also reports a stub
    // geometry — that is why a tray-minimised Spotify produced 146x21 frames and
    // looked like a broken capture. Restoring costs nothing when the window is
    // already up.
    // SAFETY: plain window-state calls on a HWND this thread owns for capture.
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{IsIconic, ShowWindow, SW_RESTORE};
        if IsIconic(hwnd).as_bool() {
            tracing::info!("source window was minimised; restoring it so capture can work");
            let _ = ShowWindow(hwnd, SW_RESTORE);
            // Give the app a moment to lay out before asking for its size.
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    // Announce before building the capture pipeline, not after.
    //
    // Creating the D3D device and starting the WGC session take tens of
    // milliseconds, and `begin()` — which the session loop calls, and which the
    // shared cursor therefore waits on — blocks until this handshake lands. The
    // geometry is already known, so the offer need not wait for any of it: the
    // viewer's window opens while this thread is still setting up.
    let mut announced = match client_size(hwnd) {
        Ok((w, h)) => {
            let _ = ready_tx.send(Ok(WindowMeta {
                id,
                title: title.clone(),
                width: w,
                height: h,
            }));
            true
        }
        // Fall back to announcing from the first frame, as this always did.
        Err(e) => {
            tracing::debug!(error = %e, "no client rect yet; announcing from the first frame");
            false
        }
    };

    let (device, context) = d3d::create_device()?;
    let mut session =
        d3d::CaptureStream::start(&device, &context, d3d::CaptureTarget::Window(hwnd))?;

    // Hide only after capture is running: if starting fails, the user's window
    // must not be left invisible.
    let _hidden = if hide_source {
        match HiddenWindow::hide(hwnd) {
            Ok(h) => {
                tracing::info!("source window hidden (layered alpha=0)");
                Some(h)
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not hide source window; it stays visible");
                None
            }
        }
    } else {
        None
    };

    // Step-by-step tracing of the first frame.
    //
    // Three runs in a row died between "source window hidden" and the first
    // frame ever reaching the peer, with the process alive but using no CPU —
    // i.e. blocked, not looping. The two candidates (WGC capture and the Media
    // Foundation encoder) are both code that had never actually run before, so
    // rather than guess, each step below says what it is about to do.
    tracing::info!("capture loop starting");

    // Ask the window to paint. A hidden, idle window (a file manager sitting
    // there) has nothing to redraw for, and WGC would hand out its first frame
    // only when something else happened to dirty it.
    force_repaint(hwnd);

    // How many times the loop asked for a frame, and how many actually came —
    // used only to keep tracing bounded (see below).
    let mut polls: u64 = 0;
    let mut frames_seen: u32 = 0;
    let mut pipeline =
        CapturePipeline::new(id, MfEncoders::default(), quality, last_seq, frames_tx);

    loop {
        match cmd_rx.try_recv() {
            Ok(CaptureCommand::Stop) => break,
            Ok(CaptureCommand::Keyframe) => pipeline.request_resync(),
            Ok(CaptureCommand::Resize { width, height }) => {
                if let Err(e) = resize_source(hwnd, width, height) {
                    tracing::debug!(error = %e, width, height, "could not resize the source window");
                }
            }
            Ok(CaptureCommand::Input(input)) => {
                if let Err(e) = crate::windows_input::replay(hwnd, input) {
                    tracing::debug!(error = %e, "could not replay input");
                }
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Err(mpsc::error::TryRecvError::Empty) => {}
        }

        // Only the first few passes are traced: after that the window is
        // streaming and per-frame logging would be the same flood we just
        // removed from the audio path.
        // Gate on a poll counter, NOT on `seq`: `seq` only advances once a
        // frame is actually sent, so when nothing arrives it stays 0 and the
        // "trace the first few" gate never closes — that flooded the log with
        // one line every 4 ms.
        polls += 1;
        // One line if frames never start, then silence: enough to tell "capture
        // is dead" from "capture is fine", without a line per poll.
        if polls == 500 && frames_seen == 0 {
            tracing::warn!(polls, "no frames from WGC yet; window may be minimised");
        }
        let trace = frames_seen < 3;
        match session.next_frame() {
            // Pixels are already on the CPU: the FrameArrived handler copies
            // them so a pool slot is never held across threads (see d3d.rs).
            Ok(Some(d3d::CapturedFrame {
                pixels: frame,
                width: w,
                height: h,
            })) => {
                frames_seen += 1;
                if trace || frames_seen <= 3 {
                    tracing::info!(w, h, frames_seen, "frame received");
                }

                if !announced {
                    announced = true;
                    let _ = ready_tx.send(Ok(WindowMeta {
                        id,
                        title: title.clone(),
                        width: w,
                        height: h,
                    }));
                }
                // The size the first frame actually carries may differ from the
                // client rect the offer was built from (DPI rounding, a window
                // that lays out late). The viewer follows a frame that does not
                // fit its geometry, so this needs no message of its own.

                if trace {
                    tracing::info!(frames_seen, "encoding and sending the frame");
                }
                pipeline.frame(&frame, w, h, std::time::Instant::now());
                if trace {
                    tracing::info!(frames_seen, "frame sent");
                }
            }
            // No frame ready yet: normal for a window that isn't redrawing.
            Ok(None) => {
                pipeline.idle(std::time::Instant::now());
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
            Err(e) => {
                // The window closed, or the session died. Either way, done.
                tracing::info!(error = %e, "capture session ended");
                break;
            }
        }
    }

    tracing::info!(id, "window capture stopped");
    Ok(())
}

/// The capture loop for a whole monitor, run on the capture thread after COM
/// is up.
///
/// Kept apart from the window loop rather than threaded through it: every
/// window-specific step there — restoring a minimised window, hiding it,
/// nudging it to repaint, replaying input, resizing it — has no monitor
/// equivalent, and a loop full of "unless it is a monitor" would hide which
/// steps a transfer actually depends on. What they share, turning frames into
/// packets, is already one function.
#[allow(clippy::too_many_arguments)]
fn monitor_capture_loop(
    id: WindowId,
    monitor: windows::Win32::Graphics::Gdi::HMONITOR,
    (width, height): (u32, u32),
    title: String,
    frames_tx: mpsc::Sender<WindowFrame>,
    mut cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
    ready_tx: &std::sync::mpsc::Sender<Result<WindowMeta, String>>,
    quality: Arc<crate::frame_pipeline::AdaptiveQuality>,
    last_seq: Arc<std::sync::atomic::AtomicU32>,
) -> Result<(), String> {
    let (device, context) = d3d::create_device()?;
    let mut session =
        d3d::CaptureStream::start(&device, &context, d3d::CaptureTarget::Monitor(monitor))?;
    let _ = ready_tx.send(Ok(WindowMeta {
        id,
        title,
        width,
        height,
    }));

    let mut pipeline =
        CapturePipeline::new(id, MfEncoders::default(), quality, last_seq, frames_tx);
    loop {
        match cmd_rx.try_recv() {
            Ok(CaptureCommand::Stop) | Err(mpsc::error::TryRecvError::Disconnected) => break,
            Ok(CaptureCommand::Keyframe) => pipeline.request_resync(),
            // A view is view-only, and a monitor cannot be resized from here.
            Ok(CaptureCommand::Input(_)) | Ok(CaptureCommand::Resize { .. }) => {}
            Err(mpsc::error::TryRecvError::Empty) => {}
        }
        match session.next_frame() {
            Ok(Some(d3d::CapturedFrame {
                pixels,
                width: w,
                height: h,
            })) => pipeline.frame(&pixels, w, h, std::time::Instant::now()),
            Ok(None) => {
                // WGC delivers nothing while the screen is still, and what
                // the viewer is owed must not wait for it.
                pipeline.idle(std::time::Instant::now());
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
            Err(e) => {
                tracing::info!(error = %e, "monitor capture ended");
                break;
            }
        }
    }
    tracing::info!(id, "monitor capture stopped");
    Ok(())
}

/// Resize the captured window so its **client area** becomes `width`x`height`.
///
/// The viewer measures in client pixels — that is what the frames carry — while
/// `SetWindowPos` takes the outer frame, so the difference (title bar, borders,
/// and whatever the window's own styles add) has to be added back. Asking for
/// the client size directly would shrink the picture by exactly the chrome on
/// every resize, compounding each time.
///
/// Position and Z-order are left alone: the user is resizing a window on the
/// other machine, not moving it or raising it over what they are working on
/// there.
fn resize_source(hwnd: HWND, width: u32, height: u32) -> Result<(), WindowError> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::UpdateWindow;
    use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOZORDER, WINDOW_EX_STYLE, WINDOW_STYLE,
    };

    if width == 0 || height == 0 {
        return Err(WindowError::Backend("refusing a zero-sized resize".into()));
    }

    // SAFETY: `hwnd` is the window this capture thread owns and has already
    // used for capture; every call below only reads or sets its geometry.
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
        let ex_style = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let dpi = GetDpiForWindow(hwnd);
        if dpi == 0 {
            return Err(WindowError::Backend("window reported no DPI".into()));
        }
        AdjustWindowRectExForDpi(&mut rect, style, false, ex_style, dpi)
            .map_err(|e| WindowError::Backend(format!("sizing the frame: {e}")))?;

        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .map_err(|e| WindowError::Backend(format!("resizing the window: {e}")))?;
        // The window is deliberately hidden while captured, so it gets no paint
        // from the compositor on its own; force one, or the capture keeps
        // handing out frames at the old size until the app repaints for some
        // other reason.
        let _ = UpdateWindow(hwnd);
    }
    Ok(())
}

/// The window's client area, for announcing a transfer before any pixels.
///
/// The client rect, not the frame: it is what WGC hands out and therefore what
/// the viewer's surrogate must be sized to.
fn client_size(hwnd: HWND) -> Result<(u32, u32), WindowError> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

    let mut rect = RECT::default();
    // SAFETY: reads geometry from a HWND this capture owns.
    unsafe {
        GetClientRect(hwnd, &mut rect)
            .map_err(|e| WindowError::Backend(format!("client rect: {e}")))?;
    }
    let (w, h) = (
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    );
    if w == 0 || h == 0 {
        return Err(WindowError::Backend("window has no client area yet".into()));
    }
    Ok((w, h))
}

/// Nudge a window into painting, so capture has something to send at once.
///
/// WGC produces a frame when the window redraws, and a window nobody is
/// touching — a file manager, a paused player — has no reason to. Invalidating
/// it makes DWM composite it again, which is what capture is watching for.
///
/// Deliberately **without** `RDW_UPDATENOW`: that flag sends `WM_PAINT`
/// synchronously and returns only once the app has handled it, and Windows
/// pauses an application's UI thread for the duration of a window drag — which
/// is exactly when this runs. Posting the invalidation instead lets the paint
/// happen when the app is next able, and never blocks the capture thread on an
/// application that cannot answer yet.
fn force_repaint(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE};

    // SAFETY: invalidates a HWND this capture owns; no state is read back.
    unsafe {
        let _ = RedrawWindow(
            Some(hwnd),
            None,
            None,
            RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
        );
    }
}
