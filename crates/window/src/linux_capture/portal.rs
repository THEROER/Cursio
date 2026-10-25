//! The portal half: one RemoteDesktop+ScreenCast session per capture, opened
//! with the restore token that keeps the picker from asking again.

use crate::WindowError;
use ashpd::desktop::{
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    PersistMode,
};
use std::os::fd::OwnedFd;
use std::path::PathBuf;

/// Where the ScreenCast restore token is persisted, next to `identity.json` in
/// the same config dir the rest of Cursio uses. Absent file = first run (expect
/// one picker dialog); present = the portal should restore silently.
pub(super) fn restore_token_path() -> Result<PathBuf, WindowError> {
    let dirs = directories::ProjectDirs::from("dev", "cursio", "cursio")
        .ok_or_else(|| WindowError::Backend("no config dir for restore token".into()))?;
    Ok(dirs.config_dir().join("window_restore.json"))
}

/// The persisted restore token, in its own tiny file rather than mixed into
/// identity/pairing state: it is portal-scoped, disposable, and safe to delete
/// (worst case the user sees the picker once more).
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct RestoreState {
    /// Opaque token from the portal; meaningful only to it.
    token: String,
}

pub(super) fn load_restore_token() -> Option<String> {
    let path = restore_token_path().ok()?;
    let bytes = std::fs::read(&path).ok()?;
    let state: RestoreState = serde_json::from_slice(&bytes).ok()?;
    tracing::debug!(path = %path.display(), "loaded ScreenCast restore token");
    Some(state.token)
}

/// Persist a fresh token, best-effort: failing to save only costs an extra
/// dialog next run, so it must never break an otherwise working capture.
pub(super) fn save_restore_token(token: &str) {
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
pub(super) struct PortalCast {
    pub(super) node_id: u32,
    pub(super) fd: OwnedFd,
    /// The RemoteDesktop proxy + session of a transfer, kept for two reasons:
    /// the cast lives exactly as long as the session object (dropping it kills
    /// the stream mid-flight), and replaying the viewer's input goes back
    /// through this same pair. Both stay on the capture thread — neither is
    /// `Send`. `None` for a remote-screen view, which takes no input.
    pub(super) input: Option<(RemoteDesktop, ashpd::desktop::Session<RemoteDesktop>)>,
    /// The plain ScreenCast session of a remote-screen view, held for the same
    /// lifetime reason.
    pub(super) _view_session: Option<ashpd::desktop::Session<Screencast>>,
    /// Whether the portal actually granted input devices. When it didn't, the
    /// surrogate is view-only and we say so once rather than failing every
    /// event.
    pub(super) input_granted: bool,
}

/// Which portal session a capture needs.
#[derive(Debug, Clone, Copy)]
pub(super) enum PortalMode {
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
pub(super) async fn setup_view_portal(kind: SourceType) -> Result<PortalCast, String> {
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
pub(super) async fn setup_portal() -> Result<PortalCast, String> {
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
