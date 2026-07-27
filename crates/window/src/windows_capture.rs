//! Windows window capture: `Windows.Graphics.Capture` → [`WindowFrame`].
//!
//! The host side on Windows. `CreateForWindow` captures one `HWND` into a pool
//! of D3D11 textures; each is copied to a CPU-readable staging texture, repacked
//! to tight BGRA, and fed to the same [`frame_pipeline`](crate::frame_pipeline)
//! the Linux backend uses — so diffing and codec routing behave identically on
//! both platforms.
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
    TileCodec, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput, WindowMeta,
};
use tokio::sync::mpsc;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

use crate::frame_pipeline::{pick_codec, should_use_video, FrameDiffer};
use crate::{CaptureSession, WindowCapture, WindowError};

mod d3d;
mod hide;
mod window_pick;

pub use hide::HiddenWindow;

/// What the capture thread is told from outside.
enum CaptureCommand {
    /// Replay one viewer input event onto the real window.
    Input(WindowInput),
    /// Stop capturing and end the thread.
    Stop,
}

/// A live capture of one window via Windows.Graphics.Capture.
pub struct WgcCaptureSession {
    meta: WindowMeta,
    frames: mpsc::UnboundedReceiver<WindowFrame>,
    commands: mpsc::UnboundedSender<CaptureCommand>,
    /// Set when the capture thread stops on its own (window closed), so
    /// `close()` doesn't wait on a thread that is already gone.
    ended: Arc<AtomicBool>,
}

impl CaptureSession for WgcCaptureSession {
    fn meta(&self) -> &WindowMeta {
        &self.meta
    }

    fn frames(&mut self) -> &mut mpsc::UnboundedReceiver<WindowFrame> {
        &mut self.frames
    }

    fn apply_input(&mut self, input: WindowInput) -> Result<(), WindowError> {
        // Replay happens on the capture thread, which owns the HWND state and
        // must not be raced from the session loop.
        self.commands
            .send(CaptureCommand::Input(input))
            .map_err(|_| WindowError::Backend("capture thread is gone".into()))
    }

    fn on_ack(&mut self, _ack: WindowFrameAck) {
        // Fixed rate for now; the adaptive controller is milestone 3.
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

    fn begin(&mut self, id: WindowId) -> Result<Self::Session, WindowError> {
        let (hwnd, title) = window_pick::find(&self.title_hint)?;
        tracing::info!(title = %title, "capturing window");
        spawn_capture(id, hwnd, title, self.hide_source)
    }
}

/// Start the capture thread and block until the first frame defines geometry.
///
/// Blocking mirrors the Linux backend: a window that can't be captured must
/// fail from `begin()`, not silently produce nothing.
fn spawn_capture(
    id: WindowId,
    hwnd: HWND,
    title: String,
    hide_source: bool,
) -> Result<WgcCaptureSession, WindowError> {
    let (frames_tx, frames_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<WindowMeta, String>>();
    let ended = Arc::new(AtomicBool::new(false));

    let thread_ended = ended.clone();
    // `HWND` is a raw pointer and so not `Send`; it is only ever used on the
    // capture thread, so move it as a plain integer.
    let hwnd_bits = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("cursio-wgc-capture".into())
        .spawn(move || {
            let hwnd = HWND(hwnd_bits as *mut core::ffi::c_void);
            let result = capture_thread(id, hwnd, title, hide_source, frames_tx, cmd_rx, &ready_tx);
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

/// Turn one captured frame into `WindowFrame`s and send them.
///
/// Shares the diff and codec choice with the Linux path via
/// [`frame_pipeline`](crate::frame_pipeline).
///
/// Large updates ride the Media Foundation H.264 encoder
/// ([`video_encoder_win`](crate::video_encoder_win)); small ones stay on
/// lossless zstd tiles. Same routing as the Linux path, and the same reason: a
/// moving window costs 742 Mbit/s as lossless tiles, which fits nowhere.
fn emit_frame(
    id: WindowId,
    frame: &[u8],
    width: u32,
    height: u32,
    differ: &mut FrameDiffer,
    seq: &mut u32,
    video: &mut Option<crate::video_encoder_win::VideoEncoder>,
    // Geometry whose encoder already failed to open, so we don't retry it once
    // per frame.
    video_failed_for: &mut Option<(u32, u32)>,
    frames_tx: &mpsc::UnboundedSender<WindowFrame>,
) {
    let Some(dirty) = differ.diff(frame, width, height) else {
        return; // unchanged — a static window costs nothing
    };

    // Which path? A small change rides a lossless tile; a large one goes
    // through the video encoder, which codes the difference from the previous
    // frame rather than the pixels themselves.
    if should_use_video(dirty, width, height) {
        // Geometry changes invalidate the encoder: H.264 is fixed-size.
        if video.as_ref().is_some_and(|e| e.size() != (width, height)) {
            tracing::info!(width, height, "window resized; reopening video encoder");
            *video = None;
        }
        // Try to open the encoder once per geometry, not once per frame.
        //
        // A failure is permanent for that size (an unsupported dimension, a
        // missing MFT), but this used to retry on every single frame — 594
        // "opening the video encoder" lines plus 777 warnings in one session,
        // and a **1.4 GB** log file. Remembering the failed size keeps the
        // fallback silent and cheap.
        // H.264 4:2:0 has no chroma plane for an odd row/column, so the encoder
        // only accepts even dimensions. Round DOWN and crop the frame rather
        // than refusing: losing one edge pixel is invisible, while refusing sent
        // every frame down the far more expensive lossless path.
        let (vw, vh) = (width & !1, height & !1);
        let failed_here = *video_failed_for == Some((vw, vh));
        if vw == 0 || vh == 0 {
            // Too small to encode at all; the tile path below handles it.
        } else if video.is_none() && !failed_here {
            tracing::info!(width = vw, height = vh, "opening the video encoder");
            match crate::video_encoder_win::VideoEncoder::open(vw, vh) {
                Ok(enc) => {
                    tracing::info!("video encoder opened");
                    *video = Some(enc);
                    *video_failed_for = None;
                }
                Err(e) => {
                    // Not fatal: fall through to the lossless path, which
                    // always works — just more expensively. Said once.
                    tracing::warn!(error = %e, width = vw, height = vh,
                        "no video encoder for this size; using lossless tiles");
                    *video_failed_for = Some((vw, vh));
                }
            }
        }

        if let Some(enc) = video.as_mut() {
            let first = *seq < 3;
            if first {
                tracing::info!(seq = *seq, "encoding frame");
            }
            // Crop to the encoder's even geometry when the window is odd-sized.
            // Row-by-row, since dropping a column changes the stride.
            let cropped;
            let to_encode = if (vw, vh) == (width, height) {
                frame
            } else {
                let src_stride = width as usize * 4;
                let dst_stride = vw as usize * 4;
                let mut buf = Vec::with_capacity(dst_stride * vh as usize);
                for row in 0..vh as usize {
                    let start = row * src_stride;
                    buf.extend_from_slice(&frame[start..start + dst_stride]);
                }
                cropped = buf;
                &cropped
            };
            match enc.encode(to_encode) {
                Ok(packets) => {
                    if first {
                        tracing::info!(packets = packets.len(), "frame encoded");
                    }
                    // One visual update can yield several packets, and
                    // sometimes none — the encoder decides.
                    let full = enc.full_rect();
                    for data in packets {
                        *seq = seq.wrapping_add(1);
                        let _ = frames_tx.send(WindowFrame {
                            id,
                            seq: *seq,
                            dirty: full,
                            codec: TileCodec::LossyVideo,
                            data,
                        });
                    }
                    return;
                }
                Err(e) => {
                    // Drop the encoder so the next frame retries cleanly
                    // rather than repeating a broken session.
                    tracing::warn!(error = %e, "video encode failed; falling back to tiles");
                    *video = None;
                }
            }
        }
    }

    let tile = FrameDiffer::extract(frame, width, dirty);
    let codec = pick_codec(tile.len());
    match crate::codec::encode_tile(codec, &tile) {
        Ok(data) => {
            *seq = seq.wrapping_add(1);
            let _ = frames_tx.send(WindowFrame {
                id,
                seq: *seq,
                dirty,
                codec,
                data,
            });
        }
        Err(e) => tracing::warn!(error = %e, "tile encode failed; dropping frame"),
    }
}

/// The capture thread: owns the WGC session, the D3D device, and the HWND.
///
/// All of it stays here because none of it is `Send`; only channels cross out.
fn capture_thread(
    id: WindowId,
    hwnd: HWND,
    title: String,
    hide_source: bool,
    frames_tx: mpsc::UnboundedSender<WindowFrame>,
    mut cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
    ready_tx: &std::sync::mpsc::Sender<Result<WindowMeta, String>>,
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

    let (device, context) = d3d::create_device()?;
    let mut session = d3d::CaptureStream::start(&device, &context, hwnd)?;

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

    let mut differ = FrameDiffer::new();
    let mut seq = 0u32;
    // How many times the loop asked for a frame, and how many actually came —
    // used only to keep tracing bounded (see below).
    let mut polls: u64 = 0;
    let mut frames_seen: u32 = 0;
    let mut announced = false;
    // H.264 encoder for large updates, opened lazily and dropped on resize
    // (H.264 cannot change frame size mid-stream).
    let mut video: Option<crate::video_encoder_win::VideoEncoder> = None;
    let mut video_failed_for: Option<(u32, u32)> = None;

    loop {
        match cmd_rx.try_recv() {
            Ok(CaptureCommand::Stop) => break,
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
                    tracing::info!(seq, w, h, frames_seen, "frame received");
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

                if trace {
                    tracing::info!(seq, "encoding and sending the frame");
                }
                emit_frame(
                    id,
                    &frame,
                    w,
                    h,
                    &mut differ,
                    &mut seq,
                    &mut video,
                    &mut video_failed_for,
                    &frames_tx,
                );
                if trace {
                    tracing::info!(seq, "frame sent");
                }
            }
            // No frame ready yet: normal for a window that isn't redrawing.
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(4)),
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
