//! Wayland input capture via the `InputCapture` portal + libei (`ashpd`/`reis`).
//!
//! The Wayland counterpart to the evdev [`crate::capture`] backend. On Wayland a
//! background daemon can't read `/dev/input` meaningfully or grab the pointer,
//! so we go through `xdg-desktop-portal`'s **InputCapture** interface: we place
//! **pointer barriers** on the screen's outer edges, and when the cursor hits
//! one the portal *captures* input (grabs it away from the desktop) and streams
//! it to us over libei as **relative** motion + evdev keycodes — exactly our
//! neutral model. This is how edge-crossing works on Wayland.
//!
//! ## Grab semantics differ from evdev
//!
//! With evdev, capture always runs and [`GrabControl`] only decides whether the
//! events *also* act locally. Here the portal couples the two: while capture is
//! *enabled* it already holds input away from the desktop. So on Wayland
//! `set_grabbed(true)` = **enable** the portal capture (this machine is dormant,
//! send input to the peer), and `set_grabbed(false)` = **disable/release** it
//! (this machine is active, keep input local). Nothing is captured while
//! released, which is also what keeps a user from being locked out.
//!
//! ## Lockout safety
//!
//! While capture is enabled the portal routes the real mouse/keyboard to us, not
//! the desktop. The ownership model already gates this (capture is enabled only
//! while dormant; the Ctrl+Alt+G hotkey / going active disables it). As a
//! belt-and-braces guard we also auto-release after [`AUTO_RELEASE`] of wall
//! time so a stuck session can't trap the user indefinitely.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use std::collections::HashMap;

use ashpd::desktop::input_capture::{
    Activated, ActivatedBarrier, Barrier, BarrierID, BarrierPosition, Capabilities,
    CreateSessionOptions, InputCapture,
};
use cursio_protocol::{Edge, InputEvent, MouseButton};
use futures_util::StreamExt;
use reis::{
    ei::{self, button::ButtonState, keyboard::KeyState},
    event::{DeviceCapability, EiEvent},
};
use tokio::sync::mpsc;

use crate::{CaptureHandle, InputError};

/// Safety valve: force-release capture after this long even if nothing else
/// disabled it, so a wedged session can never trap the user's input forever.
/// Overridable via `CURSIO_AUTO_RELEASE_SECS` (mainly for safe live testing —
/// set it low so a run can't trap you).
fn auto_release() -> Duration {
    let secs = std::env::var("CURSIO_AUTO_RELEASE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(600);
    Duration::from_secs(secs)
}

/// Sleep until `deadline`, or (when `None`) forever. The `select!` arm using
/// this is also gated by `capture_deadline.is_some()`, so the far-future branch
/// is inert — this just gives that arm a concrete future to poll either way.
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        // No deadline: never resolve. The arm's `if` guard keeps it unselected.
        None => std::future::pending::<()>().await,
    }
}

/// evdev `BTN_*` codes we forward as pointer buttons (libei reports button codes
/// straight from `input-event-codes.h`).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
/// Thumb buttons — what browsers treat as back/forward.
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// Start Wayland capture.
///
/// The reis event stream is **not `Send`** (it holds callback closures), so it
/// can't live on the shared tokio runtime. We run the whole portal + libei
/// machine on a **dedicated thread** with its own current-thread runtime and a
/// `LocalSet`, and hand only the (Send) channels back out. Blocks until the
/// portal session + libei handshake are up (so a grant failure or a declined
/// dialog surfaces here) via a oneshot readiness signal.
pub fn spawn_wayland_capture(
    peer_edge: Option<Edge>,
) -> Result<CaptureHandle, InputError> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (edges_tx, edges_rx) = mpsc::unbounded_channel();
    let (grab, grab_rx) = crate::GrabControl::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("cursio-wayland".into())
        .spawn(move || {
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
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                match setup_portal(peer_edge).await {
                    Ok((ic, session, event_stream, context, barrier_info)) => {
                        let _ = ready_tx.send(Ok(()));
                        capture_loop(
                            ic,
                            session,
                            event_stream,
                            tx,
                            edges_tx,
                            grab_rx,
                            context,
                            barrier_info,
                            peer_edge,
                        )
                        .await;
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            });
        })
        .map_err(InputError::Io)?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(CaptureHandle {
            events: rx,
            edges: edges_rx,
            devices: vec!["Wayland InputCapture portal".into()],
            grab,
        }),
        Ok(Err(e)) => Err(InputError::Unavailable(e)),
        Err(_) => Err(InputError::Unavailable(
            "wayland capture thread ended before signalling readiness".into(),
        )),
    }
}

/// Open the InputCapture portal session, connect libei, and place outer-edge
/// barriers. Returns the pieces the capture loop drives. Runs on the Wayland
/// thread's LocalSet.
async fn setup_portal(
    peer_edge: Option<Edge>,
) -> Result<
    (
        InputCapture,
        ashpd::desktop::Session<InputCapture>,
        reis::tokio::EiConvertEventStream,
        ei::Context,
        HashMap<u32, BarrierInfo>,
    ),
    String,
> {
    let ic = InputCapture::new()
        .await
        .map_err(|e| format!("open InputCapture portal: {e}"))?;
    let (session, _caps) = ic
        .create_session(
            None,
            CreateSessionOptions::default()
                .set_capabilities(Capabilities::Keyboard | Capabilities::Pointer),
        )
        .await
        .map_err(|e| format!("create portal session: {e}"))?;

    let fd = ic
        .connect_to_eis(&session, Default::default())
        .await
        .map_err(|e| format!("connect_to_eis: {e}"))?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    let context = ei::Context::new(stream).map_err(|e| format!("ei context: {e}"))?;
    context.flush().map_err(|e| format!("ei flush: {e}"))?;
    let (_conn, event_stream) = context
        .handshake_tokio("cursio", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| format!("ei handshake: {e}"))?;

    // Barriers on the OUTER edges only — the portal rejects internal seams.
    let zones = ic
        .zones(&session, Default::default())
        .await
        .and_then(|r| r.response())
        .map_err(|e| format!("zones: {e}"))?;
    let (barriers, barrier_info) = outer_edge_barriers(&zones, peer_edge);
    tracing::info!(count = barriers.len(), ?peer_edge, "placing edge barriers");
    if let Ok(resp) = ic
        .set_pointer_barriers(&session, &barriers, zones.zone_set(), Default::default())
        .await
        .and_then(|r| r.response())
    {
        let failed = resp.failed_barriers();
        if !failed.is_empty() {
            tracing::warn!(?failed, "some barriers were rejected (internal seams)");
        }
    }

    Ok((ic, session, event_stream, context, barrier_info))
}

/// What a placed barrier represents: which desktop edge it sits on, and the
/// pixel span it covers along that edge, so an activation position can be
/// normalized to `0.0`..=`1.0`. For a vertical edge (left/right) the span is in
/// y (`origin` = top, `length` = height); for a horizontal one (top/bottom) it's
/// in x.
#[derive(Clone, Copy)]
struct BarrierInfo {
    edge: Edge,
    /// Pixel coordinate of the span's start along the edge's varying axis.
    origin: i32,
    /// Pixel length of the span (>0).
    length: i32,
}

/// Build barriers on ONLY the `peer_edge` side of the desktop — the edge that
/// borders the peer. A barrier on any other edge is actively harmful: crossing
/// it would still make the portal *grab* input (locking the user), even though
/// the session then filters that crossing out as "no peer there". So we arm
/// exactly the edge we're willing to hand off across, and every other edge stays
/// a normal wall the cursor just stops at.
///
/// With `peer_edge = None`, no barriers are placed at all (edge-crossing off).
///
/// One barrier is emitted per region on that edge; for a multi-monitor desktop
/// the portal rejects the ones that fall on an internal seam (harmless, logged),
/// leaving the barrier on the true outer edge. Returns the id -> [`BarrierInfo`]
/// map used to recover the edge + normalized position on activation.
fn outer_edge_barriers(
    zones: &ashpd::desktop::input_capture::Zones,
    peer_edge: Option<Edge>,
) -> (Vec<Barrier>, HashMap<u32, BarrierInfo>) {
    let mut barriers = Vec::new();
    let mut info = HashMap::new();
    let Some(peer_edge) = peer_edge else {
        return (barriers, info); // edge-crossing disabled: no barriers
    };
    let regions: Vec<_> = zones.regions().to_vec();
    let mut next_id = 1u32;

    for r in &regions {
        let (x, y) = (r.x_offset(), r.y_offset());
        let (w, h) = (r.width() as i32, r.height() as i32);

        // The barrier line + (origin, length) span for the chosen edge only.
        let (pos, origin, length) = match peer_edge {
            Edge::Left => (BarrierPosition::new(x, y, x, y + h - 1), y, h),
            Edge::Right => (BarrierPosition::new(x + w, y, x + w, y + h - 1), y, h),
            Edge::Top => (BarrierPosition::new(x, y, x + w - 1, y), x, w),
            Edge::Bottom => (BarrierPosition::new(x, y + h, x + w - 1, y + h), x, w),
        };

        if let Some(id) = BarrierID::new(next_id) {
            barriers.push(Barrier::new(id, pos));
            info.insert(
                next_id,
                BarrierInfo {
                    edge: peer_edge,
                    origin,
                    length,
                },
            );
            next_id += 1;
        }
    }
    (barriers, info)
}

/// The long-running capture task: enable/disable the portal per grab state, and
/// while enabled translate libei events onto the capture channel.
#[allow(clippy::too_many_arguments)]
async fn capture_loop(
    ic: InputCapture,
    session: ashpd::desktop::Session<InputCapture>,
    mut event_stream: reis::tokio::EiConvertEventStream,
    tx: mpsc::UnboundedSender<InputEvent>,
    edges_tx: mpsc::UnboundedSender<crate::EdgeCrossing>,
    mut grab_rx: tokio::sync::watch::Receiver<crate::InputMode>,
    context: ei::Context,
    barrier_info: HashMap<u32, BarrierInfo>,
    peer_edge: Option<Edge>,
) {
    use crate::InputMode;

    let mut activated = match ic.receive_activated().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot receive portal activations; capture inert");
            // Bailing out early still has to close the session — see the note at
            // the end of this function. Leaving it open here would wedge the next
            // reconnect exactly the same way.
            let _ = session.close().await;
            return;
        }
    };
    // NOTE: do NOT add a `receive_deactivated()` here. Awaiting it before the
    // loop never returns until the portal first deactivates, so the capture loop
    // never starts — no seat binding, no libei events, and input silently stops
    // reaching the peer even though barriers still fire. Cost an afternoon.
    //
    // Barriers up (Active or Dormant) vs down (Idle). Distinct from `captured`:
    // barriers can be armed (enabled) yet not yet grabbing input.
    let mut enabled = false;
    // The portal has activated and is actually streaming input to us (i.e. a
    // barrier fired). This is the state that can lock the user out, so only this
    // arms the dead-man release — not mere arming while Active.
    let mut captured = false;
    // Sub-pixel motion carried between libei events, so small moves aren't lost
    // to rounding (see [`MotionRemainder`]).
    let mut remainder = MotionRemainder::default();
    // Dead-man release deadline: when set, force-release capture at this instant.
    // It is pushed forward on every real input event, so it fires only after
    // `auto_release()` of genuine *inactivity* — not a fixed wall-clock cap from
    // when capture began. Without this, leaving the cursor parked on the peer for
    // 10 min (no libei events, so the old per-iteration timer never reset) would
    // silently drop the grab and desync the session on the next move.
    let mut capture_deadline: Option<tokio::time::Instant> = None;
    // When set, re-arm the barriers at this instant. Used after a takeback: we
    // drop the barriers so the returning cursor isn't instantly re-captured by
    // the very edge it just crossed, then bring them back once the user has had
    // a moment to move away from it.
    loop {
        tokio::select! {
            // Mode changed: arm/disarm the portal to match.
            //  - Active or Dormant  => barriers ENABLED. Enabling only *arms*
            //    them; it does NOT grab input (the user keeps normal control).
            //    Input is grabbed only once a barrier fires (Activated). So on
            //    Wayland both ownership states keep barriers up: Active to catch
            //    an off-edge shove toward the peer, Dormant because a fired
            //    barrier is exactly how we got here.
            //  - Idle => barriers DOWN (disable), input fully local.
            changed = grab_rx.changed() => {
                if changed.is_err() {
                    break; // control dropped
                }
                let mode = *grab_rx.borrow_and_update();
                if matches!(mode, InputMode::Shutdown) {
                    // Session over for good: leave the loop so the portal session
                    // is released below. Staying alive would leave a second
                    // capture running against the same desktop once the next
                    // session starts, and both would report every barrier hit.
                    tracing::info!("capture shutting down");
                    break;
                }
                let want_enabled = matches!(mode, InputMode::Active | InputMode::Dormant);
                if !want_enabled && enabled {
                    // Idle: barriers fully down, input local.
                    let _ = ic.disable(&session, Default::default()).await;
                    enabled = false;
                    captured = false;
                    capture_deadline = None;
                    tracing::info!("portal barriers down (idle)");
                } else if matches!(mode, InputMode::Active) {
                    tracing::info!(captured, enabled, "takeback: became Active");
                    // We just became the active machine (holding the cursor).
                    //
                    // If the portal was CAPTURING (we'd crossed out and were
                    // streaming to the peer, and now we're taking control back),
                    // the grab is still held — the cursor is stuck "past the
                    // edge" on the peer side. We must disable() to RELEASE that
                    // grab so the cursor returns to this desktop, then enable()
                    // to re-arm the barrier for the next crossing. Without the
                    // disable, Ctrl+Alt+G "switches" ownership but the cursor
                    // never comes back (the bug we hit).
                    //
                    // The cursor comes back sitting on the very edge it crossed,
                    // so re-arming would catch it again at once. Rather than
                    // stalling the barrier on a timer — which just makes the next
                    // deliberate crossing feel stuck — nudge the cursor inward,
                    // off the edge, the way Deskflow's `avoidJumpZone()` does.
                    // Crossing again then requires actually travelling back to the
                    // edge, which is exactly the intent we want to detect.
                    if captured || enabled {
                        let _ = ic.disable(&session, Default::default()).await;
                        captured = false;
                        capture_deadline = None;
                        nudge_off_edge(&tx, peer_edge);
                        if let Err(e) = ic.enable(&session, Default::default()).await {
                            tracing::warn!(error = %e, "re-arm after takeback failed");
                            enabled = false;
                        } else {
                            enabled = true;
                            tracing::info!("grab released, cursor nudged off the edge, barrier re-armed");
                        }
                    } else if !enabled {
                        if let Err(e) = ic.enable(&session, Default::default()).await {
                            tracing::warn!(error = %e, "portal enable failed");
                        } else {
                            enabled = true;
                            tracing::info!("portal barriers armed (active)");
                        }
                    }
                } else if want_enabled && !enabled {
                    // Dormant with barriers not yet up (e.g. first entry): arm.
                    if let Err(e) = ic.enable(&session, Default::default()).await {
                        tracing::warn!(error = %e, "portal enable failed");
                    } else {
                        enabled = true;
                        tracing::info!(?mode, "portal barriers armed");
                    }
                }
            }

            // Cursor hit a barrier: the portal now streams input to us. Work out
            // which edge and where along it, and report the crossing so the
            // session can hand ownership to the peer on that side.
            act = activated.next() => {
                match act {
                    Some(a) => {
                        tracing::info!(pos = ?a.cursor_position(), barrier = ?a.barrier_id(), "barrier activated");
                        captured = true;
                        // Start the inactivity countdown now that the portal holds
                        // input; each subsequent event pushes it forward.
                        capture_deadline = Some(tokio::time::Instant::now() + auto_release());

                        // Only the side that OWNS the cursor may hand it off.
                        //
                        // Barriers stay armed while Dormant (that's how a
                        // crossing brought us here in the first place), and after
                        // handing off, the cursor is left sitting right on the
                        // edge it crossed — so the barrier fires again almost
                        // immediately. Reporting that as a fresh crossing made
                        // BOTH sides go dormant: we hand off a second time while
                        // the peer, already active, later hands back — and nobody
                        // owns input until the user hits Ctrl+Alt+G. That was the
                        // intermittent "frozen mouse".
                        //
                        // The session has an `if am_active` guard too, but it is
                        // not enough on its own: a crossing queued before we went
                        // dormant still arrives afterwards. Dropping it at the
                        // source keeps the stale event out of the channel
                        // entirely.
                        let owns_cursor = matches!(*grab_rx.borrow(), InputMode::Active);
                        if !owns_cursor {
                            tracing::debug!("barrier fired while dormant; not a crossing (peer owns the cursor)");
                        } else if let Some(crossing) = crossing_from(&a, &barrier_info) {
                            let _ = edges_tx.send(crossing);
                        }
                    }
                    None => break,
                }
            }

            // libei input events (valid while captured).
            ev = event_stream.next() => {
                match ev {
                    Some(Ok(event)) => {
                        handle_ei_event(event, &tx, &context, &mut remainder);
                        // Real activity: push the dead-man deadline forward so an
                        // in-use session is never force-released mid-work.
                        if captured {
                            capture_deadline =
                                Some(tokio::time::Instant::now() + auto_release());
                        }
                    }
                    Some(Err(e)) => { tracing::debug!(error = %e, "ei stream error"); }
                    None => break,
                }
            }

            // Dead-man release: never trap the user forever. Fires only after
            // `auto_release()` of genuine *inactivity* — the deadline is pushed
            // forward on every input event (above), so parking the cursor on the
            // peer without moving it no longer counts as being wedged. Merely
            // arming barriers while Active can't lock anyone out, so `captured`
            // (a barrier fired, portal holds input) gates the timer, and the
            // deadline is cleared whenever capture ends.
            _ = sleep_until_opt(capture_deadline), if captured && capture_deadline.is_some() => {
                tracing::warn!("auto-releasing portal capture after inactivity timeout");
                let _ = ic.disable(&session, Default::default()).await;
                enabled = false;
                captured = false;
                capture_deadline = None;
            }
        }
    }

    // Always release on exit so input returns to the desktop, and close the
    // session so it doesn't outlive us on the bus.
    //
    // `release` only hands input back; the session object itself lives until
    // explicitly closed. A leaked session breaks the NEXT capture rather than
    // this one: the portal still considers the orphan to hold capture, so a
    // fresh InputCapture request never gets answered — the capture task hangs
    // right after "using Wayland InputCapture portal for capture", with no
    // permission dialog shown, and edge-crossing stays dead until the process
    // is killed.
    //
    // Symptom to recognise in the portal log:
    //   Failed to release captured input: ... AccessDenied: Capture not active
    // (`release` on a session that isn't capturing), plus a stale path under
    // /org/freedesktop/portal/desktop/session.
    //
    // Note this only runs when capture itself is shutting down — the capture
    // task now outlives individual peer sessions (see `serve`), so a peer
    // reconnecting does NOT come through here.
    let _ = ic.disable(&session, Default::default()).await;
    let _ = ic.release(&session, Default::default()).await;
    if let Err(e) = session.close().await {
        tracing::warn!(error = %e, "failed to close portal capture session; a later capture may hang");
    }
}

/// How far to pull the cursor back from the edge after taking control, so the
/// barrier it just crossed doesn't immediately catch it again. Mirrors
/// Deskflow's jump-zone inset; a handful of pixels is enough to be outside the
/// barrier while staying visually where the user left off.
const EDGE_NUDGE_PX: i32 = 12;

/// Push the cursor inward, away from `peer_edge`, by [`EDGE_NUDGE_PX`].
///
/// Sent as ordinary relative motion through our own capture channel, which the
/// session injects locally — the same path peer input takes. Relative is all we
/// have on Wayland (no absolute warp from a uinput device), but it's enough:
/// the cursor is pinned against the edge, so moving inward always lands it off
/// the barrier.
fn nudge_off_edge(tx: &mpsc::UnboundedSender<InputEvent>, peer_edge: Option<Edge>) {
    let (dx, dy) = match peer_edge {
        Some(Edge::Right) => (-EDGE_NUDGE_PX, 0),
        Some(Edge::Left) => (EDGE_NUDGE_PX, 0),
        Some(Edge::Bottom) => (0, -EDGE_NUDGE_PX),
        Some(Edge::Top) => (0, EDGE_NUDGE_PX),
        None => return,
    };
    let _ = tx.send(InputEvent::PointerMotion { dx, dy });
}

/// Turn a portal [`Activated`] into an [`EdgeCrossing`], or `None` if we can't
/// attribute it to a known barrier (unknown/absent id, or no cursor position).
///
/// Normalizes the crossing position to `0.0`..=`1.0` along the edge: for a
/// vertical edge (left/right) that's the y-fraction, for a horizontal one the
/// x-fraction. The value is clamped, since the reported position can sit a pixel
/// outside the span.
fn crossing_from(a: &Activated, info: &HashMap<u32, BarrierInfo>) -> Option<crate::EdgeCrossing> {
    let id = match a.barrier_id()? {
        ActivatedBarrier::Barrier(id) => id.get(),
        ActivatedBarrier::UnknownBarrier => {
            tracing::debug!("activation with unknown barrier id; can't map to an edge");
            return None;
        }
    };
    let bi = info.get(&id)?;
    let (x, y) = a.cursor_position()?;
    // Position varies along x for a horizontal edge, along y for a vertical one.
    let coord = if bi.edge.is_horizontal() { x } else { y };
    let pos_norm = if bi.length > 0 {
        ((coord - bi.origin as f32) / bi.length as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Some(crate::EdgeCrossing {
        edge: bi.edge,
        pos_norm,
    })
}


/// Carries the sub-pixel remainder of pointer motion between libei events.
///
/// libei reports motion as `f32`. Rounding each event's `dx`/`dy` on its own
/// throws the fraction away, and a stream of small moves (`|d| < 0.5`) rounds to
/// zero every time — the motion vanishes entirely. That shows up as broken aim in
/// games that read raw relative motion: slow vertical drags disappear while
/// horizontal jitter survives, so the view turns the wrong way.
///
/// Instead we keep the leftover fraction and roll it into the next event, so no
/// movement is ever lost and the summed motion matches the physical mouse.
#[derive(Default)]
struct MotionRemainder {
    dx: f32,
    dy: f32,
}

impl MotionRemainder {
    /// Fold one libei delta in and take out whole pixels, keeping the fraction.
    fn take(&mut self, dx: f32, dy: f32) -> (i32, i32) {
        self.dx += dx;
        self.dy += dy;
        // `trunc` (not `round`) so the remainder keeps the same sign as the
        // motion and can never push the next step the other way.
        let whole_x = self.dx.trunc();
        let whole_y = self.dy.trunc();
        self.dx -= whole_x;
        self.dy -= whole_y;
        (whole_x as i32, whole_y as i32)
    }
}

/// Translate one libei event to a neutral [`InputEvent`] and forward it (binding
/// seat capabilities the first time a seat appears).
fn handle_ei_event(
    event: EiEvent,
    tx: &mpsc::UnboundedSender<InputEvent>,
    context: &ei::Context,
    remainder: &mut MotionRemainder,
) {
    let neutral = match event {
        EiEvent::SeatAdded(seat) => {
            seat.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Scroll
                    | DeviceCapability::Button,
            );
            let _ = context.flush();
            return;
        }
        EiEvent::PointerMotion(m) => {
            let (dx, dy) = remainder.take(m.dx, m.dy);
            // A move that's entirely sub-pixel so far: nothing to send yet, but
            // it's banked in the remainder for the next one.
            if dx == 0 && dy == 0 {
                return;
            }
            InputEvent::PointerMotion { dx, dy }
        }
        EiEvent::KeyboardKey(k) => InputEvent::Key {
            // libei key codes are evdev keycodes — our neutral namespace.
            key: k.key as u16,
            pressed: matches!(k.state, KeyState::Press),
        },
        EiEvent::Button(b) => {
            let pressed = matches!(b.state, ButtonState::Press);
            // Buttons are logged at both ends of the wire (capture here, inject
            // on the peer) so a "click made the cursor stick" report can be
            // traced instead of guessed at: if a release is captured here but
            // never injected there, the loss is in between.
            tracing::debug!(button = b.button, pressed, "captured pointer button");
            match b.button {
                BTN_LEFT => InputEvent::PointerButton { button: MouseButton::Left, pressed },
                BTN_RIGHT => InputEvent::PointerButton { button: MouseButton::Right, pressed },
                BTN_MIDDLE => InputEvent::PointerButton { button: MouseButton::Middle, pressed },
                BTN_SIDE => InputEvent::PointerButton { button: MouseButton::Back, pressed },
                BTN_EXTRA => InputEvent::PointerButton { button: MouseButton::Forward, pressed },
                other => {
                    tracing::trace!(button = other, "dropping unmapped button");
                    return;
                }
            }
        }
        EiEvent::ScrollDiscrete(s) => {
            // libei discrete scroll is in 120-unit steps; convert to notches.
            // libei's vertical axis points down (positive = scroll down) while
            // the neutral event follows evdev REL_WHEEL (positive = up), so dy
            // flips. Horizontal agrees (positive = right) and is passed as-is.
            let dx = s.discrete_dx / 120;
            let dy = -s.discrete_dy / 120;
            if dx == 0 && dy == 0 {
                return;
            }
            InputEvent::PointerScroll { dx, dy }
        }
        _ => return,
    };
    let _ = tx.send(neutral);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_pixel_motion_accumulates_instead_of_vanishing() {
        // A slow drag straight up: each step is well under a pixel, which plain
        // rounding would turn into zero forever (the bug that broke aiming in
        // games). The remainder must bank them until a whole pixel comes out.
        let mut r = MotionRemainder::default();
        assert_eq!(r.take(0.0, -0.4), (0, 0));
        assert_eq!(r.take(0.0, -0.4), (0, 0));
        // 3 * -0.4 = -1.2 => one whole pixel up, 0.2 kept back.
        assert_eq!(r.take(0.0, -0.4), (0, -1));
    }

    #[test]
    fn motion_total_is_preserved_over_a_stream() {
        // Ten 0.3px steps are 3px of real movement; none may be lost.
        let mut r = MotionRemainder::default();
        let moved: i32 = (0..10).map(|_| r.take(0.3, 0.0).0).sum();
        assert_eq!(moved, 3);
    }

    #[test]
    fn remainder_never_flips_direction() {
        // Truncation keeps the remainder's sign matching the motion, so a left
        // drag can never emit a rightward step.
        let mut r = MotionRemainder::default();
        for _ in 0..20 {
            let (dx, _) = r.take(-0.7, 0.0);
            assert!(dx <= 0, "leftward motion produced dx={dx}");
        }
    }
}
