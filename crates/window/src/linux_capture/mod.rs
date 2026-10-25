//! Linux window capture: `ScreenCast` portal → PipeWire → [`WindowFrame`].
//!
//! The **host** side of window transfer on Wayland. A Wayland app cannot read
//! another window's pixels directly — the compositor forbids it, the same wall
//! the input path hits — so the sanctioned route is `xdg-desktop-portal`'s
//! **ScreenCast** interface: the portal lets the user pick the source, hands
//! back a PipeWire node id plus a remote fd, and we attach a PipeWire stream to
//! that node to receive frames.
//!
//! ## Only prompting once, ever
//!
//! The portal's picker is the one piece of friction in this feature: by default
//! it asks "which window do you want to share?" on *every* `Start`. That is
//! badly matched to our trigger — the user already expressed intent by dragging
//! a specific window past the edge, and being re-asked each time would make the
//! feature unusable.
//!
//! So we request [`PersistMode::ExplicitlyRevoked`](ashpd::desktop::PersistMode::ExplicitlyRevoked), which makes the portal
//! return a **restore token**. We persist that token ([`restore_token_path`](portal::restore_token_path))
//! and pass it back on the next `Start`, and the portal restores the same source
//! **without showing the dialog**. Net effect: one prompt the first time on a
//! machine, silence forever after (until the user revokes it in system
//! settings). GNOME 43+/KDE support this; where it isn't supported the token is
//! simply ignored and the picker returns — degraded, not broken.
//!
//! **Where persistence is requested matters.** Because our session is also a
//! remote-desktop one (below), the portal *rejects* `persist_mode` and
//! `restore_token` on `SelectSources` outright — "Remote desktop sessions
//! cannot persist", an unconditional check in the portal's `screen-cast.c`.
//! The reason is sound: a silently-restored session that can drive the keyboard
//! would be a standing input grant nobody re-confirmed. `SelectDevices` *does*
//! accept persistence, so that is the single place we ask for it, and it covers
//! the whole session. Asking on both is what fails.
//!
//! ## Threading
//!
//! Two event loops that both refuse to be moved between threads: `ashpd`'s
//! portal session (`!Send`) and PipeWire's `MainLoop` (blocking `run()`). They
//! live together on **one dedicated thread**, exactly like
//! `cursio-input`'s `wl_capture`: the thread first `block_on`s the async portal
//! setup on a current-thread runtime, then hands control to the blocking
//! PipeWire loop while keeping the portal `Session` alive in scope (dropping it
//! would tear the cast down). Only `Send` channels cross back out, so a slow
//! link can never stall capture.

use ashpd::desktop::screencast::SourceType;
use cursio_protocol::{
    WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput, WindowMeta,
};
use tokio::sync::mpsc;

use crate::linux_capture::portal::setup_portal;
use crate::linux_capture::portal::setup_view_portal;
use crate::linux_capture::portal::PortalMode;
use crate::linux_capture::stream::run_pipewire_loop;
use crate::{CaptureSession, WindowCapture, WindowError};

mod portal;
mod replay;
mod stream;

/// What the capture thread is told to do from the outside.
enum CaptureCommand {
    /// Replay one viewer input event onto the real window.
    ///
    /// Input can't be applied where [`CaptureSession::apply_input`] is called:
    /// the portal's `notify_*` calls are async D-Bus, and the only runtime that
    /// may drive them is the capture thread's (the portal session is `!Send`).
    /// So the event is queued here and replayed there.
    Input(WindowInput),
    /// Send the viewer the whole current picture; see
    /// [`CaptureSession::request_keyframe`].
    Keyframe,
    /// Stop the PipeWire loop and end the thread.
    Stop,
}

/// A live capture of one window via the ScreenCast portal.
///
/// Owns the channels to the capture thread; the thread itself owns everything
/// that isn't `Send` (portal session, PipeWire loop).
pub struct PortalCaptureSession {
    /// Shared with the capture thread; moved as the viewer's acks arrive.
    quality: std::sync::Arc<crate::frame_pipeline::AdaptiveQuality>,
    /// Highest `seq` the capture thread has emitted, so an ack says how far
    /// behind the viewer is. Written there, read here.
    last_seq: std::sync::Arc<std::sync::atomic::AtomicU32>,
    meta: WindowMeta,
    frames: mpsc::Receiver<WindowFrame>,
    commands: mpsc::UnboundedSender<CaptureCommand>,
}

impl CaptureSession for PortalCaptureSession {
    fn meta(&self) -> &WindowMeta {
        &self.meta
    }

    fn frames(&mut self) -> &mut mpsc::Receiver<WindowFrame> {
        &mut self.frames
    }

    fn apply_input(&mut self, input: WindowInput) -> Result<(), WindowError> {
        // Hand off to the capture thread, which owns the portal session and the
        // runtime its async `notify_*` calls need. Queuing rather than blocking
        // keeps the session loop free — replay is fire-and-forget by nature.
        self.commands
            .send(CaptureCommand::Input(input))
            .map_err(|_| WindowError::Backend("capture thread is gone".into()))
    }

    fn set_network_budget(&mut self, bits_per_second: u32, congested: bool) {
        self.quality.set_network_budget(bits_per_second, congested);
    }

    fn on_ack(&mut self, ack: WindowFrameAck) {
        let sent = self.last_seq.load(std::sync::atomic::Ordering::Relaxed);
        if let Some((gap, threshold)) = self.quality.on_ack(ack.acked_seq, sent) {
            tracing::info!(
                id = self.meta.id,
                gap,
                threshold,
                "link changed; adjusting how much of the window stays lossless"
            );
        }
    }

    fn set_peer_video(&mut self, caps: cursio_protocol::VideoCaps) {
        self.quality.set_peer_video(caps);
    }

    fn request_keyframe(&mut self) {
        let _ = self.commands.send(CaptureCommand::Keyframe);
    }

    fn close(&mut self, reason: WindowCloseReason) {
        tracing::info!(?reason, id = self.meta.id, "closing window capture");
        let _ = self.commands.send(CaptureCommand::Stop);
    }
}

impl Drop for PortalCaptureSession {
    fn drop(&mut self) {
        let _ = self.commands.send(CaptureCommand::Stop);
    }
}

/// The Linux [`WindowCapture`] backend: ScreenCast portal + PipeWire.
pub struct PortalWindowCapture;

impl WindowCapture for PortalWindowCapture {
    type Session = PortalCaptureSession;

    fn begin(&mut self, id: WindowId) -> Result<Self::Session, WindowError> {
        // The portal deliberately doesn't expose the source window's title (it
        // would leak what the user picked); a neutral label is honest until we
        // can learn it another way.
        spawn_capture(id, PortalMode::Transfer, "Shared window".into())
    }
}

/// The Linux [`ScreenSources`](crate::ScreenSources): two portal pickers.
///
/// GNOME will not tell a client which windows or monitors exist — the only
/// sanctioned way to name one is to let the ScreenCast portal ask the user.
/// So the list is fixed: "choose a window" and "choose a screen", both
/// `picked_on_host`, and choosing either opens the picker on this machine.
/// `machine` is this node's name, so the viewer's menu says where the dialog
/// will appear.
pub struct PortalScreenSources {
    pub machine: String,
}

const PICK_WINDOW: cursio_protocol::SourceId = 1;
const PICK_MONITOR: cursio_protocol::SourceId = 2;

impl crate::ScreenSources for PortalScreenSources {
    fn list(&self) -> Result<Vec<cursio_protocol::ScreenSource>, WindowError> {
        use cursio_protocol::{ScreenSource, SourceKind};
        Ok(vec![
            ScreenSource {
                id: PICK_WINDOW,
                kind: SourceKind::Window,
                title: format!("Choose a window (dialog on {})…", self.machine),
                width: 0,
                height: 0,
                picked_on_host: true,
            },
            ScreenSource {
                id: PICK_MONITOR,
                kind: SourceKind::Monitor,
                title: format!("Choose a screen (dialog on {})…", self.machine),
                width: 0,
                height: 0,
                picked_on_host: true,
            },
        ])
    }

    fn open(
        &self,
        id: WindowId,
        source: &cursio_protocol::ScreenSource,
    ) -> Result<Box<dyn CaptureSession + Send>, WindowError> {
        let kind = match source.id {
            PICK_WINDOW => SourceType::Window,
            PICK_MONITOR => SourceType::Monitor,
            other => {
                return Err(WindowError::WindowGone(format!(
                    "source {other} is not one this machine offers"
                )))
            }
        };
        let title = match kind {
            SourceType::Monitor => format!("Screen of {}", self.machine),
            _ => format!("Window on {}", self.machine),
        };
        Ok(Box::new(spawn_capture(id, PortalMode::View(kind), title)?))
    }
}

/// Start the capture thread and block until the portal + PipeWire stream are up.
///
/// Blocking here is deliberate: a declined portal dialog or a missing PipeWire
/// must surface as an error from `begin()`, not as a stream that silently never
/// produces frames. Mirrors `spawn_wayland_capture` in `cursio-input`.
fn spawn_capture(
    id: WindowId,
    mode: PortalMode,
    title: String,
) -> Result<PortalCaptureSession, WindowError> {
    let (frames_tx, frames_rx) = mpsc::channel(crate::CAPTURE_FRAME_QUEUE_CAPACITY);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    // Readiness carries the negotiated window geometry back, so the meta we
    // offer the peer matches what the stream will actually deliver.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<WindowMeta, String>>();
    // Shared with the capture thread: quality is written here as acks arrive
    // and read there per frame; `last_seq` the other way round.
    let quality = std::sync::Arc::new(crate::frame_pipeline::AdaptiveQuality::default());
    let last_seq = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let thread_quality = quality.clone();
    let thread_last_seq = last_seq.clone();

    std::thread::Builder::new()
        .name("cursio-window-capture".into())
        .spawn(move || {
            capture_thread(
                id,
                mode,
                title,
                frames_tx,
                cmd_rx,
                ready_tx,
                thread_quality,
                thread_last_seq,
            )
        })
        .map_err(|e| WindowError::Backend(format!("spawn capture thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(meta)) => Ok(PortalCaptureSession {
            meta,
            frames: frames_rx,
            commands: cmd_tx,
            quality,
            last_seq,
        }),
        Ok(Err(e)) => Err(WindowError::Backend(e)),
        Err(_) => Err(WindowError::Backend(
            "capture thread ended before signalling readiness".into(),
        )),
    }
}

/// The capture thread: portal setup (async), then the PipeWire loop (blocking).
///
/// Both halves are pinned here because neither is `Send`. The portal `Session`
/// inside [`PortalCast`](portal::PortalCast) must outlive the stream, so it stays in scope for the
/// whole function.
#[allow(clippy::too_many_arguments)]
fn capture_thread(
    id: WindowId,
    mode: PortalMode,
    title: String,
    frames_tx: mpsc::Sender<WindowFrame>,
    cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
    ready_tx: std::sync::mpsc::Sender<Result<WindowMeta, String>>,
    quality: std::sync::Arc<crate::frame_pipeline::AdaptiveQuality>,
    last_seq: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    // Phase 1: the portal handshake is async (D-Bus), so it needs a runtime —
    // current-thread, because none of it may migrate.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("build runtime: {e}")));
            return;
        }
    };
    let setup = match mode {
        PortalMode::Transfer => rt.block_on(setup_portal()),
        PortalMode::View(kind) => rt.block_on(setup_view_portal(kind)),
    };
    let cast = match setup {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // Held for the capture's life; dropping the connection is what releases
    // the inhibition, so a crash cannot leave it behind.
    let _scanout = match mode {
        PortalMode::View(_) => rt.block_on(inhibit_direct_scanout()),
        PortalMode::Transfer => None,
    };

    // Phase 2: hand the thread to PipeWire. `cast` (and its portal session)
    // stays alive here for the duration.
    if let Err(e) = run_pipewire_loop(
        id, title, cast, frames_tx, cmd_rx, &ready_tx, &rt, quality, last_seq,
    ) {
        // If we never signalled readiness, this is the error the caller sees;
        // if we already did, the send is a no-op and the log is the record.
        tracing::error!(error = %e, "window capture loop ended with an error");
        let _ = ready_tx.send(Err(e));
    }
}

/// Ask the Cursio Shell extension to keep Mutter compositing every frame.
///
/// A fullscreen window Mutter scans out directly is never composited, and a
/// monitor cast records only what is: measured on GNOME 49, a fullscreen video
/// in Chrome streamed ~30 buffers a second windowed and none a few seconds
/// after going fullscreen, so the viewer's picture froze while the video
/// played on. No client API turns scanout off; the compositor can, so the
/// extension does it on request, per D-Bus sender. Returns the connection
/// that holds the request — it lasts exactly as long as that connection does —
/// or `None` when the extension is missing or too old, which costs only that
/// fullscreen case.
async fn inhibit_direct_scanout() -> Option<zbus::Connection> {
    let conn = zbus::Connection::session().await.ok()?;
    let reply = conn
        .call_method(
            Some("dev.cursio.Shell"),
            "/dev/cursio/Shell",
            Some("dev.cursio.Shell"),
            "InhibitDirectScanout",
            &(),
        )
        .await;
    match reply {
        Ok(_) => {
            tracing::info!("direct scanout inhibited while this screen is shared");
            crate::notice::report(crate::notice::Notice::ShellExtensionCurrent);
            Some(conn)
        }
        Err(e) => {
            // Once per process: nothing changes between captures until the
            // Shell reloads its extensions, and that takes a new login.
            static REPORTED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::debug!(error = %e, "direct scanout still not inhibited");
                return None;
            }
            let missing_method = matches!(
                &e,
                zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"
            );
            if missing_method {
                tracing::warn!(
                    error = %e,
                    "the running GNOME Shell has an older Cursio extension without \
                     InhibitDirectScanout; expected until you log out and back in, after \
                     which a fullscreen app on the shared screen no longer freezes the peer's view"
                );
                crate::notice::report(crate::notice::Notice::ShellExtensionOutdated {
                    missing: "no InhibitDirectScanout".into(),
                });
            } else {
                tracing::warn!(
                    error = %e,
                    "could not inhibit direct scanout, so a fullscreen app on the shared screen \
                     may freeze the peer's view; is the Cursio GNOME extension installed and enabled?"
                );
                crate::notice::report(crate::notice::Notice::ShellExtensionMissing {
                    detail: e.to_string(),
                });
            }
            None
        }
    }
}
