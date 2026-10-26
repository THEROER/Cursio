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
//!
//! ## Barriers are re-placed, not placed once
//!
//! Which edge borders the peer is known only after that peer greets us, and
//! capture comes up long before any of them does — so a barrier list fixed at
//! construction is a barrier list that is always empty. The watched edge
//! arrives later through [`set_watched_edge`], and the zone set can change
//! underneath us as well (a monitor plugged in invalidates every barrier id we
//! hold). Both are the same operation: drop capture, rebuild the list, put it
//! back. Doing it under a live capture is the case compositors are least
//! likely to agree on, so the enable/disable pair is not optional.

use std::time::Duration;

use cursio_protocol::EdgeSet;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::wl_capture::barriers::crossing_from;
use crate::wl_capture::barriers::entry_position;
use crate::wl_capture::barriers::EDGE_NUDGE_PX;
use crate::wl_capture::barriers::inside_edge;
use crate::wl_capture::barriers::nudge_off_edge;
use crate::wl_capture::events::handle_ei_event;
use crate::wl_capture::events::MotionRemainder;
use crate::wl_capture::events::ScrollFrame;
use crate::wl_capture::portal::read_zones;
use crate::wl_capture::portal::replace_barriers;
use crate::wl_capture::portal::setup_portal;
use crate::wl_capture::portal::Portal;
use crate::wl_capture::portal::PORTAL_ANSWER_WAIT;
use crate::{CaptureHandle, InputError};

mod barriers;
mod events;
mod portal;

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
/// this is also gated by [`CaptureState::can_time_out`], so the far-future
/// branch is inert — this just gives that arm a concrete future to poll either
/// way.
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        // No deadline: never resolve. The arm's `if` guard keeps it unselected.
        None => std::future::pending::<()>().await,
    }
}

/// The edges the barriers are currently placed on, shared with the capture
/// thread.
///
/// A `watch` rather than an atomic: unlike the Windows hook, which reads its
/// edges from inside a callback that may not block, the change here has to
/// *do* something — three D-Bus round trips — so the capture loop needs to be
/// woken, not polled. Lives in a static because [`crate::set_watched_edges`]
/// is called from the node, which holds no handle to this backend.
fn watched_edges() -> &'static tokio::sync::watch::Sender<EdgeSet> {
    static EDGES: std::sync::OnceLock<tokio::sync::watch::Sender<EdgeSet>> =
        std::sync::OnceLock::new();
    EDGES.get_or_init(|| tokio::sync::watch::channel(EdgeSet::EMPTY).0)
}

/// Move the barriers to a different set of edges while capture is running.
///
/// Set from [`crate::set_watched_edges`] as peers greet us, which is seconds
/// after the portal session exists. An empty set takes every barrier down,
/// leaving edge crossing off.
pub(crate) fn set_watched_edges(edges: EdgeSet) {
    // `send_replace` rather than `send`: the capture thread may not have
    // subscribed yet (or may have ended), and neither is an error here — the
    // value is what matters, and a later subscriber reads it as its initial.
    watched_edges().send_replace(edges);
}

/// Start Wayland capture.
///
/// The reis event stream is **not `Send`** (it holds callback closures), so it
/// can't live on the shared tokio runtime. We run the whole portal + libei
/// machine on a **dedicated thread** with its own current-thread runtime and a
/// `LocalSet`, and hand only the (Send) channels back out. Waits briefly for
/// the portal session + libei handshake (so a grant failure surfaces here),
/// but not for a permission dialog nobody has answered yet — see
/// [`PORTAL_ANSWER_WAIT`]. With no edge watched yet the portal is not asked at
/// all until one is.
pub fn spawn_wayland_capture() -> Result<CaptureHandle, InputError> {
    let (tx, rx) = crate::event_channel();
    let (edges_tx, edges_rx) = mpsc::unbounded_channel();
    let (grab, grab_rx) = crate::GrabControl::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    // Subscribed before the thread starts, so an edge set while the portal
    // handshake is still running wakes the loop rather than being missed. On a
    // first run the shared value is empty and the first barriers go up when a
    // peer greets us; on a capture restarted mid-mesh it already holds the
    // right edges.
    let mut edge_rx = watched_edges().subscribe();
    // With no peer bordering any edge there is nothing to capture for, and
    // GNOME's InputCapture portal (version 1) cannot remember a grant: every
    // session is a permission dialog. Asking at start-up put one up on every
    // restart, including on a machine that only had phones to talk to, and an
    // unanswered one is invisible from here — the cursor just never left. So
    // the portal is asked only once a peer that takes the cursor borders us.
    let deferred = edge_rx.borrow().is_empty();

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
                if deferred {
                    if edge_rx.wait_for(|e| !e.is_empty()).await.is_err() {
                        return;
                    }
                    tracing::info!("a peer borders this desktop; asking it to allow input capture");
                }
                let started = {
                    let setup = setup_portal(&mut edge_rx);
                    tokio::pin!(setup);
                    match tokio::time::timeout(PORTAL_ANSWER_WAIT, &mut setup).await {
                        Ok(started) => started,
                        Err(_) => {
                            // Nobody is waiting on `ready_rx` any more when
                            // capture was deferred, so the wait is reported
                            // from here instead.
                            if deferred {
                                tracing::warn!(
                                    "waiting for the desktop to allow input capture; the \
                                     shared cursor starts once the dialog is answered"
                                );
                            }
                            setup.await
                        }
                    }
                };
                match started {
                    Ok(portal) => {
                        let _ = ready_tx.send(Ok(()));
                        capture_loop(portal, tx, edges_tx, grab_rx, edge_rx).await;
                    }
                    Err(e) => {
                        // Read only if the caller is still waiting; past the
                        // wait the dialog was answered late, and this is the
                        // only place that can say how.
                        tracing::warn!(error = %e, "input capture did not start");
                        let _ = ready_tx.send(Err(e));
                    }
                }
            });
        })
        .map_err(InputError::Io)?;

    let handle = CaptureHandle {
        events: rx,
        edges: edges_rx,
        devices: vec!["Wayland InputCapture portal".into()],
        grab,
    };
    if deferred {
        tracing::info!(
            "input capture waits for a peer to border this desktop; the desktop is asked then"
        );
        return Ok(handle);
    }
    match ready_rx.recv_timeout(PORTAL_ANSWER_WAIT) {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => Err(InputError::Unavailable(e)),
        // The portal is showing its permission dialog and nobody has answered
        // yet. Waiting for that held up the whole node — no links, no audio,
        // not even to the phones — for as long as the dialog sat unnoticed.
        // The handle is good already: its events start once the user allows.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                "waiting for the desktop to allow input capture; the mesh runs meanwhile, \
                 and the shared cursor starts once the dialog is answered"
            );
            Ok(handle)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(InputError::Unavailable(
            "wayland capture thread ended before signalling readiness".into(),
        )),
    }
}

/// What the portal is doing with input right now.
///
/// Three facts that only ever move together. Barriers can be *armed* without
/// taking input from the desktop; input is taken only once a barrier fires; and
/// the dead-man countdown belongs to that second state alone, because merely
/// arming a barrier while this machine holds the cursor cannot lock anyone out.
/// They were three loose locals, and every path that updated two of them and
/// forgot the third either leaked a grab or force-released one that was in use.
#[derive(Default)]
struct CaptureState {
    /// Barriers are up (Active or Dormant). Distinct from `captured`.
    enabled: bool,
    /// A barrier fired and the portal is streaming input to us.
    captured: bool,
    /// When to force-release, or `None` when nothing is held.
    ///
    /// Pushed forward by every real input event, so it fires only after
    /// genuine *inactivity* rather than as a wall-clock cap from when capture
    /// began. Without that, leaving the cursor parked on the peer for ten
    /// minutes silently dropped the grab and desynced the session on the next
    /// move.
    deadline: Option<tokio::time::Instant>,
    /// The activation holding input now, which is what `Release` must name.
    activation_id: Option<u32>,
    /// Where the pointer was pinned when that activation began.
    activated_at: Option<(f32, f32)>,
}

impl CaptureState {
    /// A barrier fired, or input arrived while it is held: (re)start the
    /// countdown.
    fn holding(&mut self) {
        self.captured = true;
        self.deadline = Some(tokio::time::Instant::now() + auto_release());
    }

    /// A barrier fired: remember which activation now holds input.
    fn activated(&mut self, id: Option<u32>, at: Option<(f32, f32)>) {
        self.activation_id = id;
        self.activated_at = at;
        self.holding();
    }

    /// Input is back with the desktop. Leaves `enabled` alone: releasing the
    /// grab and taking the barriers down are different things.
    fn released(&mut self) {
        self.captured = false;
        self.deadline = None;
        self.activation_id = None;
        self.activated_at = None;
    }

    /// Whether the dead-man timer should be armed at all.
    fn can_time_out(&self) -> bool {
        self.captured && self.deadline.is_some()
    }
}

/// The long-running capture task: enable/disable the portal per grab state, and
/// while enabled translate libei events onto the capture channel.
async fn capture_loop(
    portal: Portal,
    tx: crate::EventSender,
    edges_tx: mpsc::UnboundedSender<crate::EdgeCrossing>,
    mut grab_rx: tokio::sync::watch::Receiver<crate::InputMode>,
    mut edge_rx: tokio::sync::watch::Receiver<EdgeSet>,
) {
    use crate::InputMode;

    let Portal {
        ic,
        session,
        mut event_stream,
        context,
        mut barrier_info,
        mut rects,
        mut zone_set,
        mut edges,
    } = portal;

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
    // Zone changes arrive on their own signal. Subscribed once, before the
    // loop, for the same reason `activated` is: `receive_*` awaits a D-Bus
    // match rule being installed, and doing that inside a `select!` arm would
    // re-install it on every iteration.
    let mut zones_changed = match ic.receive_zones_changed().await {
        Ok(s) => Some(s),
        // Not fatal: the barriers placed now stay correct until the monitor
        // layout changes, which is rarer than this signal being unavailable.
        Err(e) => {
            tracing::warn!(error = %e, "no zone-change signal; barriers will not follow a monitor change");
            None
        }
    };
    // NOTE: do NOT add a `receive_deactivated()` here. Awaiting it before the
    // loop never returns until the portal first deactivates, so the capture loop
    // never starts — no seat binding, no libei events, and input silently stops
    // reaching the peer even though barriers still fire. Cost an afternoon.
    let mut state = CaptureState::default();
    let mut spans = crate::crossing_spans();
    let mut windows = crate::crossing_windows();
    // Sub-pixel motion carried between libei events, so small moves aren't lost
    // to rounding (see [`MotionRemainder`]).
    let mut remainder = MotionRemainder::default();
    let mut scroll = ScrollFrame::default();
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
                if !want_enabled && state.enabled {
                    // Idle: barriers fully down, input local.
                    let _ = ic.disable(&session, Default::default()).await;
                    state.enabled = false;
                    state.released();
                    tracing::info!("portal barriers down (idle)");
                } else if matches!(mode, InputMode::Active) {
                    tracing::info!(captured = state.captured, enabled = state.enabled, "takeback: became Active");
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
                    //
                    // When the cursor came back across an edge, it belongs at the
                    // height it left the peer at, not where it left here. `Release`
                    // takes that position and the compositor puts the pointer there
                    // itself: no device motion, so nothing a game reads through
                    // relative-pointer or raw input, and the barriers stay armed.
                    let entry = crate::take_entry();
                    if state.captured {
                        if let Some(at) = entry.and_then(|e| match e.at {
                            // The peer placed it through the shared layout.
                            Some(at) => crate::desk::entry_at(&rects, e.edge, at, EDGE_NUDGE_PX)
                                .map(|(x, y)| (f64::from(x), f64::from(y))),
                            None => entry_position(&rects, e.edge, e.pos_norm),
                        }) {
                            let options = ashpd::desktop::input_capture::ReleaseOptions::default()
                                .set_activation_id(state.activation_id)
                                .set_cursor_position(Some(at));
                            match ic.release(&session, options).await {
                                Ok(()) => {
                                    state.released();
                                    tracing::info!(?at, "grab released with the pointer where it left the peer");
                                    continue;
                                }
                                Err(e) => tracing::warn!(error = %e, "release at the entry point failed; nudging off the edge instead"),
                            }
                        }
                    }
                    if state.captured || state.enabled {
                        let _ = ic.disable(&session, Default::default()).await;
                        state.released();
                        nudge_off_edge(&tx, crate::last_crossed_edge());
                        if let Err(e) = ic.enable(&session, Default::default()).await {
                            tracing::warn!(error = %e, "re-arm after takeback failed");
                            state.enabled = false;
                        } else {
                            state.enabled = true;
                            tracing::info!("grab released, cursor nudged off the edge, barrier re-armed");
                        }
                    } else if !state.enabled {
                        if let Err(e) = ic.enable(&session, Default::default()).await {
                            tracing::warn!(error = %e, "portal enable failed");
                        } else {
                            state.enabled = true;
                            tracing::info!("portal barriers armed (active)");
                        }
                    }
                } else if want_enabled && !state.enabled {
                    // Dormant with barriers not yet up (e.g. first entry): arm.
                    if let Err(e) = ic.enable(&session, Default::default()).await {
                        tracing::warn!(error = %e, "portal enable failed");
                    } else {
                        state.enabled = true;
                        tracing::info!(?mode, "portal barriers armed");
                    }
                }
            }

            // The edges bordering a peer have changed — normally because one
            // greeted us for the first time, seconds after capture came up.
            // Everything about the barriers is stale, including the id map an
            // activation is attributed through, so all of it is rebuilt.
            changed = edge_rx.changed() => {
                // The sender is a static and never dropped, so an error here is
                // not reachable; treat it as "nothing to do" rather than
                // tearing capture down over it.
                if changed.is_ok() {
                    let wanted = *edge_rx.borrow_and_update();
                    // A narrowed span is poked through this same channel with
                    // the edges unchanged; the barriers move all the same.
                    if wanted != edges
                        || crate::crossing_spans() != spans
                        || crate::crossing_windows() != windows
                    {
                        edges = wanted;
                        spans = crate::crossing_spans();
                        windows = crate::crossing_windows();
                        barrier_info = replace_barriers(
                            &ic, &session, &rects, zone_set, edges, &mut state,
                        )
                        .await;
                    }
                }
            }

            // The monitor layout changed. Every barrier id we hold belongs to
            // the old zone set and means nothing now, so the zones are read
            // again and the barriers placed against the new set. Without this a
            // plugged-in monitor silently ends edge crossing until restart.
            _ = async { zones_changed.as_mut().unwrap().next().await },
                if zones_changed.is_some() =>
            {
                match read_zones(&ic, &session).await {
                    Ok((new_rects, new_set)) => {
                        tracing::info!(zone_set = new_set, regions = new_rects.len(), "desktop zones changed; re-placing barriers");
                        rects = new_rects;
                        zone_set = new_set;
                        barrier_info = replace_barriers(
                            &ic, &session, &rects, zone_set, edges, &mut state,
                        )
                        .await;
                    }
                    Err(e) => tracing::warn!(error = %e, "could not re-read zones; barriers left as they are"),
                }
            }

            // Cursor hit a barrier: the portal now streams input to us. Work out
            // which edge and where along it, and report the crossing so the
            // session can hand ownership to the peer on that side.
            act = activated.next() => {
                match act {
                    Some(a) => {
                        tracing::info!(pos = ?a.cursor_position(), barrier = ?a.barrier_id(), "barrier activated");
                        // Start the inactivity countdown now that the portal holds
                        // input; each subsequent event pushes it forward.
                        state.activated(a.activation_id(), a.cursor_position());

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
                            crate::note_crossed(crossing.edge);
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
                        handle_ei_event(event, &tx, &context, &mut remainder, &mut scroll);
                        // Real activity: push the dead-man deadline forward so an
                        // in-use session is never force-released mid-work.
                        if state.captured {
                            state.holding();
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
            //
            // `Release`, not `Disable`: the grab is what could trap the user, the
            // barriers are not. Disabling took the barriers down while the mode
            // stayed Dormant, and only a mode change puts them back — so after
            // ten idle minutes with the cursor on the peer, shoving it at the
            // edge again did nothing until the peer sent the cursor back or
            // cursio was restarted. The pointer is placed just inside the edge
            // so the still-armed barrier does not catch it again at once.
            _ = sleep_until_opt(state.deadline), if state.can_time_out() => {
                tracing::warn!("auto-releasing portal capture after inactivity timeout");
                let options = ashpd::desktop::input_capture::ReleaseOptions::default()
                    .set_activation_id(state.activation_id)
                    .set_cursor_position(
                        state
                            .activated_at
                            .map(|at| inside_edge(at, crate::last_crossed_edge())),
                    );
                if let Err(e) = ic.release(&session, options).await {
                    // Without a release the grab is still held, and the dead-man
                    // exists precisely so it cannot be held forever. Cycling the
                    // barriers drops it and leaves them armed.
                    tracing::warn!(error = %e, "release failed; cycling the barriers instead");
                    let _ = ic.disable(&session, Default::default()).await;
                    state.enabled = false;
                    if let Err(e) = ic.enable(&session, Default::default()).await {
                        tracing::warn!(error = %e, "re-arm after auto-release failed; edge crossing is off until the next ownership change");
                    } else {
                        state.enabled = true;
                    }
                }
                state.released();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releasing_the_grab_keeps_the_barriers_armed() {
        let mut state = CaptureState {
            enabled: true,
            ..Default::default()
        };
        state.activated(Some(7), Some((3840.0, 500.0)));
        assert!(state.can_time_out());
        state.released();
        assert!(state.enabled);
        assert!(!state.can_time_out());
        assert_eq!(state.activation_id, None);
    }
}
