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
//! So we request [`PersistMode::ExplicitlyRevoked`], which makes the portal
//! return a **restore token**. We persist that token ([`restore_token_path`])
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

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use ashpd::desktop::{
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    PersistMode,
};
use cursio_protocol::{
    WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput, WindowMeta,
};
use tokio::sync::mpsc;

use crate::frame_pipeline::CapturePipeline;
use crate::{CaptureSession, WindowCapture, WindowError};

/// Where the ScreenCast restore token is persisted, next to `identity.json` in
/// the same config dir the rest of Cursio uses. Absent file = first run (expect
/// one picker dialog); present = the portal should restore silently.
fn restore_token_path() -> Result<PathBuf, WindowError> {
    let dirs = directories::ProjectDirs::from("dev", "cursio", "cursio")
        .ok_or_else(|| WindowError::Backend("no config dir for restore token".into()))?;
    Ok(dirs.config_dir().join("window_restore.json"))
}

/// The persisted restore token, in its own tiny file rather than mixed into
/// identity/pairing state: it is portal-scoped, disposable, and safe to delete
/// (worst case the user sees the picker once more).
#[derive(serde::Serialize, serde::Deserialize)]
struct RestoreState {
    /// Opaque token from the portal; meaningful only to it.
    token: String,
}

fn load_restore_token() -> Option<String> {
    let path = restore_token_path().ok()?;
    let bytes = std::fs::read(&path).ok()?;
    let state: RestoreState = serde_json::from_slice(&bytes).ok()?;
    tracing::debug!(path = %path.display(), "loaded ScreenCast restore token");
    Some(state.token)
}

/// Persist a fresh token, best-effort: failing to save only costs an extra
/// dialog next run, so it must never break an otherwise working capture.
fn save_restore_token(token: &str) {
    let Ok(path) = restore_token_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let state = RestoreState {
        token: token.to_owned(),
    };
    match serde_json::to_vec_pretty(&state).map(|b| std::fs::write(&path, b)) {
        Ok(Ok(())) => {
            tracing::info!(path = %path.display(), "saved ScreenCast restore token (portal won't prompt again)")
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "could not write restore token; portal will prompt next time")
        }
        Err(e) => tracing::warn!(error = %e, "could not encode restore token"),
    }
}

/// What the portal handed us: the node to attach to and the fd to attach
/// *through*.
///
/// The portal also reports a source size, but we deliberately don't carry it:
/// the authoritative geometry is whatever PipeWire *negotiates*, which arrives
/// in `param_changed` and is what the buffers actually contain. Trusting the
/// portal's number instead would risk a meta that disagrees with the pixels.
///
/// `_session` is held, never used: the cast lives exactly as long as the portal
/// session object does, so dropping it would kill the stream mid-flight. It
/// stays on the capture thread with everything else that isn't `Send`.
struct PortalCast {
    node_id: u32,
    fd: OwnedFd,
    /// The RemoteDesktop proxy + session of a transfer, kept for two reasons:
    /// the cast lives exactly as long as the session object (dropping it kills
    /// the stream mid-flight), and replaying the viewer's input goes back
    /// through this same pair. Both stay on the capture thread — neither is
    /// `Send`. `None` for a remote-screen view, which takes no input.
    input: Option<(RemoteDesktop, ashpd::desktop::Session<RemoteDesktop>)>,
    /// The plain ScreenCast session of a remote-screen view, held for the same
    /// lifetime reason.
    _view_session: Option<ashpd::desktop::Session<Screencast>>,
    /// Whether the portal actually granted input devices. When it didn't, the
    /// surrogate is view-only and we say so once rather than failing every
    /// event.
    input_granted: bool,
}

/// Which portal session a capture needs.
#[derive(Debug, Clone, Copy)]
enum PortalMode {
    /// A dragged window: RemoteDesktop + ScreenCast, input replayed, restored
    /// silently from the saved token.
    Transfer,
    /// A remote-screen view of a window or a monitor: ScreenCast alone.
    ///
    /// No RemoteDesktop, because a view takes no input — and asking for it
    /// would make the dialog request keyboard and mouse control the user has
    /// no reason to grant. No restore token either: the viewer chose "let me
    /// pick", so the picker is the point; restoring the last source silently
    /// would make every entry show the same thing. The cursor is embedded,
    /// unlike a transfer's: here the audience is watching the host, and the
    /// host's pointer is part of what they should see.
    View(SourceType),
}

/// Open a ScreenCast-only session for a remote-screen view.
async fn setup_view_portal(kind: SourceType) -> Result<PortalCast, String> {
    let screencast = Screencast::new()
        .await
        .map_err(|e| format!("open ScreenCast portal: {e}"))?;
    let session = screencast
        .create_session(Default::default())
        .await
        .map_err(|e| format!("create portal session: {e}"))?;
    tracing::info!(
        ?kind,
        "a peer asked to see this machine — pick what to show in the screen-share dialog \
         (it may open BEHIND other windows; check Alt+Tab)"
    );
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from(kind))
                .set_multiple(false),
        )
        .await
        .map_err(|e| format!("select sources: {e}"))?;
    let response = screencast
        .start(&session, None, Default::default())
        .await
        .map_err(|e| format!("start session: {e}"))?
        .response()
        .map_err(|e| {
            let detail = format!("the screen-share dialog was cancelled or failed: {e}");
            crate::notice::report(crate::notice::Notice::ScreenCastRefused {
                detail: detail.clone(),
            });
            detail
        })?;
    crate::notice::report(crate::notice::Notice::ScreenCastGranted);
    let node_id = response
        .streams()
        .first()
        .ok_or_else(|| "portal returned no stream (nothing selected?)".to_string())?
        .pipe_wire_node_id();
    let fd = screencast
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| format!("open_pipe_wire_remote: {e}"))?;
    Ok(PortalCast {
        node_id,
        fd,
        input: None,
        _view_session: Some(session),
        input_granted: false,
    })
}

/// Open the portal and start streaming one window — *and* claim the right to
/// drive it.
///
/// The session is created on **RemoteDesktop**, not ScreenCast, then
/// `select_sources` is called on that same session (ashpd's `RemoteDesktop`
/// implements `IsScreencastSession`, and the portal spec explicitly supports
/// pairing the two). That pairing is the whole point: one session means the
/// user is asked **once**, and the grant covers both directions — pixels out
/// through PipeWire, input back through `notify_*`. Two separate sessions would
/// mean two dialogs and a cast the input side can't address.
///
/// Asks for `SourceType::Window` so the picker offers windows rather than whole
/// monitors, and for `CursorMode::Hidden` so the pointer stays out of the pixels.
///
/// `Embedded` looked cheaper — the viewer would see a cursor without us
/// compositing one — but the pointer it bakes in is the **host's**, and the host
/// is a machine the user has usually just left. Having taken the cursor to the
/// other side, they still saw the laptop's idle pointer sitting in the streamed
/// window, and it slid across the picture whenever anything moved it over there.
/// The viewer's own cursor is drawn by its compositor over the surrogate, which
/// is the one that actually tracks the user's hand.
async fn setup_portal() -> Result<PortalCast, String> {
    let remote = RemoteDesktop::new()
        .await
        .map_err(|e| format!("open RemoteDesktop portal: {e}"))?;
    let screencast = Screencast::new()
        .await
        .map_err(|e| format!("open ScreenCast portal: {e}"))?;

    let session = remote
        .create_session(Default::default())
        .await
        .map_err(|e| format!("create portal session: {e}"))?;

    // Warn *before* `Start` blocks on the picker. A daemon has no toplevel to
    // pass as `WindowIdentifier`, so the portal logs "Failed to associate
    // portal window with parent window" and shows the dialog **unfocused** —
    // it can end up behind a fullscreen window and look like nothing happened,
    // while we sit here waiting indefinitely. There is no API fix (every
    // `WindowIdentifier` constructor needs a real window we don't have), so
    // being loud about it is the fix.
    tracing::info!(
        "waiting for the screen-share dialog — it may open BEHIND other windows; \
         pick the window to share (check Alt+Tab if you don't see it)"
    );

    let token = load_restore_token();

    // Input devices first, then the pixel source, both on this one session.
    remote
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                .set_restore_token(token.as_deref())
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await
        .map_err(|e| format!("select devices: {e}"))?;

    // NOTE: no `persist_mode`/`restore_token` here, deliberately. The portal
    // rejects both outright on a session that is also a remote-desktop one
    // ("Remote desktop sessions cannot persist" — see `screen-cast.c`, the
    // check is unconditional on `IS_REMOTE_DESKTOP_SESSION`). Persistence for
    // the combined session is carried by `select_devices` above instead, which
    // does accept it; passing it twice is what fails.
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Hidden)
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Window))
                .set_multiple(false),
        )
        .await
        .map_err(|e| format!("select sources: {e}"))?;

    // One `Start` for both capabilities — and one dialog for the user.
    let response = remote
        .start(&session, None, Default::default())
        .await
        .map_err(|e| format!("start session: {e}"))?
        .response()
        .map_err(|e| {
            let detail = format!("start session response: {e}");
            crate::notice::report(crate::notice::Notice::ScreenCastRefused {
                detail: detail.clone(),
            });
            detail
        })?;
    crate::notice::report(crate::notice::Notice::ScreenCastGranted);

    // A new token is issued whenever the portal decides to; store it so the
    // next run restores silently.
    if let Some(t) = response.restore_token() {
        save_restore_token(t);
    }

    let devices = response.devices();
    let input_granted = devices.contains(DeviceType::Pointer);
    if !input_granted {
        // Not fatal: watching a window is still useful, and this is exactly
        // what a compositor without RemoteDesktop support will do.
        tracing::warn!(
            ?devices,
            "portal granted no pointer device — the surrogate will be view-only"
        );
    }

    let stream = response
        .streams()
        .first()
        .ok_or_else(|| "portal returned no stream (nothing selected?)".to_string())?;
    let node_id = stream.pipe_wire_node_id();
    let size = stream.size(); // logged only; see `PortalCast` on why we don't keep it

    let fd = screencast
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| format!("open_pipe_wire_remote: {e}"))?;

    tracing::info!(
        node_id,
        ?size,
        input_granted,
        "portal session started (cast + input)"
    );

    Ok(PortalCast {
        node_id,
        fd,
        input: Some((remote, session)),
        _view_session: None,
        input_granted,
    })
}

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
/// inside [`PortalCast`] must outlive the stream, so it stays in scope for the
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

/// State the PipeWire callbacks share.
///
/// `param_changed` learns the negotiated geometry, `process` needs it to
/// interpret the buffer, and both run on the PipeWire loop thread — hence
/// `Rc<RefCell<_>>` rather than any locking. Same shape as the audio backend's
/// per-stream `Cell`s, just with more to carry.
struct StreamState {
    /// Negotiated video format; `None` until the first `param_changed`.
    format: Option<pipewire::spa::param::video::VideoInfoRaw>,
    /// Set once we've reported readiness, so it only happens on the first
    /// format.
    announced: bool,
    /// Lives here, on the PipeWire thread, rather than behind a lock: the
    /// encoder is driven from `process` and, between buffers, from the loop
    /// that iterates PipeWire — both this thread.
    pipeline: CapturePipeline<crate::video_encoder::FfmpegEncoders>,
}

/// Build the format Pod we offer PipeWire.
///
/// We ask for **BGRA only** — deliberately narrow. It is exactly the wire
/// format [`TileCodec::RawBgra8888`] specifies, so accepting anything else
/// would mean a per-frame colour conversion on the hot path. The compositor can
/// produce BGRA, so constraining here costs nothing and saves the conversion.
/// Size and framerate stay ranges: the portal decides the window's real size,
/// and the compositor caps the rate.
fn format_param_pod() -> Vec<u8> {
    use pipewire::spa::{
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
            ParamType,
        },
        pod::{object, property, serialize::PodSerializer, Value},
        utils::{Fraction, Rectangle, SpaTypes},
    };

    let obj = object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRA,
            VideoFormat::BGRA,
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 640,
                height: 480
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 144, denom: 1 }
        ),
    );

    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serializing a static format pod cannot fail")
        .0
        .into_inner()
}

/// Attach a PipeWire stream to the portal's node and pump frames until stopped.
#[allow(clippy::too_many_arguments)]
fn run_pipewire_loop(
    id: WindowId,
    title: String,
    cast: PortalCast,
    frames_tx: mpsc::Sender<WindowFrame>,
    mut cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
    ready_tx: &std::sync::mpsc::Sender<Result<WindowMeta, String>>,
    rt: &tokio::runtime::Runtime,
    quality: std::sync::Arc<crate::frame_pipeline::AdaptiveQuality>,
    last_seq: std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> Result<(), String> {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use pipewire::{
        context::ContextRc,
        main_loop::MainLoopRc,
        properties::properties,
        spa::{pod::Pod, utils::Direction},
        stream::{StreamFlags, StreamRc},
    };

    pipewire::init();

    // Split the cast up front: the fd is consumed by `connect_fd_rc`, while the
    // portal proxy/session must stay alive for the whole loop (they are both
    // what keeps the cast open and how input is replayed).
    let PortalCast {
        node_id,
        fd,
        input,
        _view_session,
        input_granted,
    } = cast;

    let main_loop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context = ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
    // Connect through the portal's fd, not the session bus: this is the
    // permission-scoped PipeWire the portal opened for us.
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| format!("connect to portal pipewire: {e}"))?;

    let stream = StreamRc::new(
        core.clone(),
        "cursio-window-capture",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("stream: {e}"))?;

    let state = Rc::new(RefCell::new(StreamState {
        format: None,
        announced: false,
        pipeline: CapturePipeline::new(id, Default::default(), quality, last_seq, frames_tx),
    }));

    let ready = ready_tx.clone();
    let state_fmt = state.clone();
    let state_proc = state.clone();

    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed(move |_, (), param_id, param| {
            let Some(param) = param else { return };
            if param_id != pipewire::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) =
                pipewire::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pipewire::spa::param::format::MediaType::Video
                || media_subtype != pipewire::spa::param::format::MediaSubtype::Raw
            {
                return;
            }

            let mut info = pipewire::spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                tracing::warn!("could not parse negotiated video format");
                return;
            }
            let size = info.size();
            tracing::info!(
                width = size.width,
                height = size.height,
                format = ?info.format(),
                "window capture format negotiated"
            );

            let mut st = state_fmt.borrow_mut();
            st.format = Some(info);
            // The first negotiated format is what makes the offer meaningful —
            // only now do we know the real geometry to tell the peer about.
            if !st.announced {
                st.announced = true;
                let _ = ready.send(Ok(WindowMeta {
                    id,
                    title: title.clone(),
                    width: size.width,
                    height: size.height,
                }));
            }
        })
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };

            let chunk = data.chunk();
            let (offset, size, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                chunk.stride() as usize,
            );
            if size == 0 || stride == 0 {
                return;
            }
            // Mutter queues a buffer marked corrupted when it had no new
            // picture to record into it (a cursor-only update, a failed
            // blit). Its memory is whatever an earlier frame left in that pool
            // slot, and diffing it as if it were current sends the viewer a
            // picture from several frames ago.
            if chunk
                .flags()
                .contains(pipewire::spa::buffer::ChunkFlags::CORRUPTED)
            {
                return;
            }
            let Some(bytes) = data.data() else { return };

            let mut st = state_proc.borrow_mut();
            let Some(info) = st.format else { return };
            let (width, height) = (info.size().width, info.size().height);
            let row_bytes = (width as usize) * 4;

            // PipeWire rows are `stride` bytes apart, which may exceed the
            // visible `width * 4` (alignment padding). Repack tightly so every
            // later stage — differ, tile extraction, the wire format — can
            // assume no padding.
            let mut frame = Vec::with_capacity(row_bytes * height as usize);
            for y in 0..height as usize {
                let start = offset + y * stride;
                let end = start + row_bytes;
                if end > bytes.len() {
                    // Short buffer: the format and the data disagree, so this
                    // frame isn't safe to interpret. Skip rather than send
                    // garbage or panic on the RT thread.
                    return;
                }
                frame.extend_from_slice(&bytes[start..end]);
            }

            st.pipeline
                .frame(&frame, width, height, std::time::Instant::now());
        })
        .register()
        .map_err(|e| format!("register stream listener: {e}"))?;

    let pod_bytes = format_param_pod();
    let mut params = [Pod::from_bytes(&pod_bytes).expect("valid format pod")];
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("stream connect: {e}"))?;

    tracing::info!(node_id, "window capture stream connected");

    // Drive the loop, servicing commands between iterations — PipeWire's loop
    // has no cross-thread "run until signalled" (same approach as the audio
    // capture loop).
    let mut warned_no_input = false;
    'outer: loop {
        // Drain queued input before iterating: replay should track the user's
        // hand, and a batch that sat through a 50 ms PipeWire tick would feel
        // like lag. Bounded so a flood of motion can't stall capture entirely.
        for _ in 0..64 {
            match cmd_rx.try_recv() {
                Ok(CaptureCommand::Input(event)) => {
                    let Some((remote, session)) = input.as_ref() else {
                        continue;
                    };
                    if !input_granted {
                        if !warned_no_input {
                            warned_no_input = true;
                            tracing::warn!(
                                "ignoring viewer input: the portal granted no input devices"
                            );
                        }
                        continue;
                    }
                    // `block_on` is safe here: this thread's runtime is
                    // current-thread and nothing else is driving it while the
                    // PipeWire loop is between iterations.
                    if let Err(e) = rt.block_on(replay_input(remote, session, node_id, event)) {
                        tracing::debug!(error = %e, "could not replay input onto the window");
                    }
                }
                Ok(CaptureCommand::Keyframe) => state.borrow_mut().pipeline.request_resync(),
                Ok(CaptureCommand::Stop) => break 'outer,
                Err(mpsc::error::TryRecvError::Disconnected) => break 'outer,
                Err(mpsc::error::TryRecvError::Empty) => break,
            }
        }
        main_loop
            .loop_()
            .iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(16)));
        // Between buffers, on this same thread: a still screen delivers none,
        // and what the viewer is owed must not wait for one.
        state.borrow_mut().pipeline.idle(std::time::Instant::now());
        if state.borrow().pipeline.closed() {
            break;
        }
    }

    tracing::info!("window capture stream stopped");
    Ok(())
}

/// The protocol's neutral button → its evdev `BTN_*` code.
///
/// Same namespace `cursio-input` injects with, and the same one libei reports —
/// keeping one vocabulary end to end is what lets a click cross machines (and
/// operating systems) without a translation table in the middle.
fn evdev_button(button: cursio_protocol::MouseButton) -> i32 {
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
async fn replay_input(
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
