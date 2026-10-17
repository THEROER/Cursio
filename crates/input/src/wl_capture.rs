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

use std::os::unix::net::UnixStream;
use std::time::Duration;

use std::collections::HashMap;

use ashpd::desktop::input_capture::{
    Activated, ActivatedBarrier, Barrier, BarrierID, BarrierPosition, Capabilities,
    CreateSessionOptions, InputCapture,
};
use cursio_protocol::{Edge, EdgeSet, InputEvent, MouseButton};
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
pub fn spawn_wayland_capture() -> Result<CaptureHandle, InputError> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (edges_tx, edges_rx) = mpsc::unbounded_channel();
    let (grab, grab_rx) = crate::GrabControl::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    // Subscribed before the thread starts, so an edge set while the portal
    // handshake is still running wakes the loop rather than being missed. On a
    // first run the shared value is empty and the first barriers go up when a
    // peer greets us; on a capture restarted mid-mesh it already holds the
    // right edges.
    let edge_rx = watched_edges().subscribe();
    let edges = *edge_rx.borrow();

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
                match setup_portal(edges).await {
                    Ok(portal) => {
                        let _ = ready_tx.send(Ok(()));
                        capture_loop(portal, tx, edges_tx, grab_rx, edge_rx).await;
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

/// Everything the capture loop needs from a portal session that is up.
///
/// The zone geometry is kept rather than dropped after the first placement:
/// the barriers are rebuilt whenever the watched edge changes, and asking the
/// portal for zones again on every change would be three round trips where one
/// suffices — and would race the zone set the barrier ids are scoped to.
struct Portal {
    ic: InputCapture,
    session: ashpd::desktop::Session<InputCapture>,
    event_stream: reis::tokio::EiConvertEventStream,
    context: ei::Context,
    barrier_info: HashMap<u32, BarrierInfo>,
    /// The desktop regions, as `(x, y, width, height)`.
    rects: Vec<Rect>,
    /// Which zone set the current `rects` (and so the barrier ids) belong to.
    zone_set: u32,
}

/// Open the InputCapture portal session, connect libei, and place outer-edge
/// barriers. Returns the pieces the capture loop drives. Runs on the Wayland
/// thread's LocalSet.
async fn setup_portal(edges: EdgeSet) -> Result<Portal, String> {
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
    let (rects, zone_set) = read_zones(&ic, &session).await?;
    let barrier_info = place_barriers(&ic, &session, &rects, zone_set, edges).await;

    Ok(Portal {
        ic,
        session,
        event_stream,
        context,
        barrier_info,
        rects,
        zone_set,
    })
}

/// A desktop region as `(x, y, width, height)`. The portal's own `Region` has
/// no public constructor, so the geometry is carried in a shape a test can
/// build.
type Rect = (i32, i32, i32, i32);

/// Ask the portal for the current zones, flattened to rectangles.
async fn read_zones(
    ic: &InputCapture,
    session: &ashpd::desktop::Session<InputCapture>,
) -> Result<(Vec<Rect>, u32), String> {
    let zones = ic
        .zones(session, Default::default())
        .await
        .and_then(|r| r.response())
        .map_err(|e| format!("zones: {e}"))?;
    let rects = zones
        .regions()
        .iter()
        .map(|r| {
            (
                r.x_offset(),
                r.y_offset(),
                r.width() as i32,
                r.height() as i32,
            )
        })
        .collect();
    Ok((rects, zones.zone_set()))
}

/// Put barriers on each of `edges` of every region and report the id map
/// needed to attribute an activation back to an edge.
///
/// Best-effort by design: a rejected barrier is one that fell on an internal
/// seam between two monitors, which is normal on a multi-monitor desktop and
/// leaves the barrier on the true outer edge in place.
async fn place_barriers(
    ic: &InputCapture,
    session: &ashpd::desktop::Session<InputCapture>,
    rects: &[Rect],
    zone_set: u32,
    edges: EdgeSet,
) -> HashMap<u32, BarrierInfo> {
    let (barriers, barrier_info) = outer_edge_barriers(rects, edges);
    tracing::info!(count = barriers.len(), edges = ?edges.iter().collect::<Vec<_>>(), "placing edge barriers");
    match ic
        .set_pointer_barriers(session, &barriers, zone_set, Default::default())
        .await
        .and_then(|r| r.response())
    {
        Ok(resp) => {
            let failed = resp.failed_barriers();
            if !failed.is_empty() {
                tracing::warn!(?failed, "some barriers were rejected (internal seams)");
            }
        }
        // Reported rather than swallowed: with no barriers the cursor simply
        // stops at the edge, and "edge crossing does nothing" is otherwise
        // indistinguishable from a topology that says there is no peer there.
        Err(e) => {
            tracing::warn!(error = %e, "the portal refused the barriers; edge crossing is off")
        }
    }
    barrier_info
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

/// Take capture down, put a fresh barrier list up, and restore whatever state
/// capture was in.
///
/// The disable is not a precaution against a stale grab so much as against the
/// portal's own bookkeeping: replacing barriers while it is holding input means
/// the cursor is somewhere the new barrier list does not describe, and the
/// compositor is under no obligation to resolve that the way we would like.
/// Dropping to a known state and building back up is cheap — this runs once per
/// peer, not once per motion event.
async fn replace_barriers(
    ic: &InputCapture,
    session: &ashpd::desktop::Session<InputCapture>,
    rects: &[Rect],
    zone_set: u32,
    edges: EdgeSet,
    state: &mut CaptureState,
) -> HashMap<u32, BarrierInfo> {
    let was_enabled = state.enabled;
    if was_enabled {
        let _ = ic.disable(session, Default::default()).await;
        state.enabled = false;
        state.released();
    }

    let info = place_barriers(ic, session, rects, zone_set, edges).await;

    // Only re-arm if it was armed. Coming back enabled from an Idle session
    // would put barriers up with no peer to hand anything to, and a barrier
    // that fires there grabs the user's input for nothing.
    if was_enabled {
        match ic.enable(session, Default::default()).await {
            Ok(()) => state.enabled = true,
            Err(e) => tracing::warn!(error = %e, "re-arm after moving the barriers failed"),
        }
    }
    info
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

/// Whether `edge` of this region faces off the desktop rather than another
/// monitor.
///
/// A barrier is a line the pointer cannot cross, so it belongs only where the
/// desktop ends. Put one on the seam between two monitors and the portal
/// refuses it — correctly, since stopping the pointer mid-desktop is not
/// something a compositor will do.
///
/// "Another monitor is there" means one that starts exactly where this edge
/// ends *and* overlaps it along the edge's own axis. Monitors of different
/// heights make that overlap partial: a short screen beside a tall one leaves
/// the tall one's edge exposed above and below it. Treating such an edge as
/// outer keeps the barrier, which is the safe way to be wrong — the portal
/// rejects a barrier it dislikes, while a missing one silently stops edge
/// crossing from working at all.
fn is_outer_edge(rect: Rect, edge: Edge, rects: &[Rect]) -> bool {
    let (x, y, w, h) = rect;
    rects.iter().all(|&(ox, oy, ow, oh)| {
        if (ox, oy, ow, oh) == rect {
            return true; // a region cannot enclose itself
        }
        // Does the neighbour sit flush against this edge, and cover it whole?
        match edge {
            Edge::Left => !(ox + ow == x && oy <= y && oy + oh >= y + h),
            Edge::Right => !(ox == x + w && oy <= y && oy + oh >= y + h),
            Edge::Top => !(oy + oh == y && ox <= x && ox + ow >= x + w),
            Edge::Bottom => !(oy == y + h && ox <= x && ox + ow >= x + w),
        }
    })
}

/// Build barriers on ONLY the `edges` sides of the desktop — the edges that
/// border a peer. A barrier on any other edge is actively harmful: crossing it
/// would still make the portal *grab* input (locking the user), even though
/// nothing then takes the crossing because no peer is there. So we arm exactly
/// the edges we're willing to hand off across, and every other edge stays a
/// normal wall the cursor just stops at.
///
/// With no edges, no barriers are placed at all (edge-crossing off).
///
/// Only outer edges get one (see [`is_outer_edge`]); every barrier remembers
/// its own edge, so an activation says which way the cursor left. Returns the
/// id -> [`BarrierInfo`] map used to recover the edge + normalized position on
/// activation.
fn outer_edge_barriers(
    rects: &[Rect],
    edges: EdgeSet,
) -> (Vec<Barrier>, HashMap<u32, BarrierInfo>) {
    let mut barriers = Vec::new();
    let mut info = HashMap::new();
    let mut next_id = 1u32;

    for (peer_edge, &(x, y, w, h)) in edges
        .iter()
        .flat_map(|edge| rects.iter().map(move |rect| (edge, rect)))
    {
        // Outer edges only, as the name says. A barrier on the seam between two
        // of this desktop's own monitors is one the portal rejects — it logged
        // `some barriers were rejected` on every start of a two-monitor
        // desktop, which is noise that hides a barrier genuinely refused.
        if !is_outer_edge((x, y, w, h), peer_edge, rects) {
            continue;
        }

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
async fn capture_loop(
    portal: Portal,
    tx: mpsc::UnboundedSender<InputEvent>,
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
    } = portal;
    // The edges the barriers currently sit on. Read from the shared value
    // rather than from a parameter: it may already have moved while the portal
    // handshake was running.
    let mut edges = *edge_rx.borrow_and_update();

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
                    if wanted != edges {
                        edges = wanted;
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

/// `at`, moved [`EDGE_NUDGE_PX`] inward from `peer_edge`, in the portal's
/// absolute coordinates — where to leave the pointer when handing input back.
fn inside_edge(at: (f32, f32), peer_edge: Option<Edge>) -> (f64, f64) {
    let (x, y) = (f64::from(at.0), f64::from(at.1));
    let inset = f64::from(EDGE_NUDGE_PX);
    match peer_edge {
        Some(Edge::Right) => (x - inset, y),
        Some(Edge::Left) => (x + inset, y),
        Some(Edge::Bottom) => (x, y - inset),
        Some(Edge::Top) => (x, y + inset),
        None => (x, y),
    }
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

/// Neutral scroll units per logical pixel of libei's smooth scroll: ten pixels
/// to a notch, the step a desktop toolkit scrolls one wheel click by.
const SCROLL_PER_PIXEL: f32 = cursio_protocol::SCROLL_NOTCH as f32 / 10.0;

/// Smooth scroll from libei, held until its frame ends.
///
/// A touchpad has no notches, so its two-finger scroll arrives only as smooth
/// deltas — which used to be dropped, leaving it unable to scroll a peer. A
/// compositor may describe one wheel click both ways in the same frame, so a
/// frame that carried a discrete step drops its delta rather than scroll twice.
#[derive(Default)]
struct ScrollFrame {
    pending: MotionRemainder,
    dx: i32,
    dy: i32,
    discrete: bool,
}

impl ScrollFrame {
    fn delta(&mut self, dx: f32, dy: f32) {
        let (x, y) = self
            .pending
            .take(dx * SCROLL_PER_PIXEL, dy * SCROLL_PER_PIXEL);
        self.dx += x;
        self.dy += y;
    }

    /// The frame is over: what it scrolled, if a delta is all it carried.
    fn finish(&mut self) -> Option<InputEvent> {
        let (dx, dy, discrete) = (self.dx, self.dy, self.discrete);
        self.dx = 0;
        self.dy = 0;
        self.discrete = false;
        // libei's vertical axis points down; the neutral one points up.
        (!discrete && (dx != 0 || dy != 0)).then_some(InputEvent::PointerScroll { dx, dy: -dy })
    }
}

/// Translate one libei event to a neutral [`InputEvent`] and forward it (binding
/// seat capabilities the first time a seat appears).
fn handle_ei_event(
    event: EiEvent,
    tx: &mpsc::UnboundedSender<InputEvent>,
    context: &ei::Context,
    remainder: &mut MotionRemainder,
    scroll: &mut ScrollFrame,
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
        EiEvent::KeyboardKey(k) => {
            let pressed = matches!(k.state, KeyState::Press);
            // The capture end of the key trace, as for buttons below: a peer
            // left holding Alt and Shift after both were let go here shows as
            // a release logged here and never injected there.
            tracing::debug!(key = k.key, pressed, "captured key");
            InputEvent::Key {
                // libei key codes are evdev keycodes — our neutral namespace.
                key: k.key as u16,
                pressed,
            }
        }
        EiEvent::Button(b) => {
            let pressed = matches!(b.state, ButtonState::Press);
            // Buttons are logged at both ends of the wire (capture here, inject
            // on the peer) so a "click made the cursor stick" report can be
            // traced instead of guessed at: if a release is captured here but
            // never injected there, the loss is in between.
            tracing::debug!(button = b.button, pressed, "captured pointer button");
            match b.button {
                BTN_LEFT => InputEvent::PointerButton {
                    button: MouseButton::Left,
                    pressed,
                },
                BTN_RIGHT => InputEvent::PointerButton {
                    button: MouseButton::Right,
                    pressed,
                },
                BTN_MIDDLE => InputEvent::PointerButton {
                    button: MouseButton::Middle,
                    pressed,
                },
                BTN_SIDE => InputEvent::PointerButton {
                    button: MouseButton::Back,
                    pressed,
                },
                BTN_EXTRA => InputEvent::PointerButton {
                    button: MouseButton::Forward,
                    pressed,
                },
                other => {
                    tracing::trace!(button = other, "dropping unmapped button");
                    return;
                }
            }
        }
        EiEvent::ScrollDiscrete(s) => {
            // libei discrete scroll is in 120-unit steps, the neutral unit.
            // libei's vertical axis points down (positive = scroll down) while
            // the neutral event follows evdev REL_WHEEL (positive = up), so dy
            // flips. Horizontal agrees (positive = right) and is passed as-is.
            scroll.discrete = true;
            let dx = s.discrete_dx;
            let dy = -s.discrete_dy;
            if dx == 0 && dy == 0 {
                return;
            }
            InputEvent::PointerScroll { dx, dy }
        }
        EiEvent::ScrollDelta(s) => {
            scroll.delta(s.dx, s.dy);
            return;
        }
        EiEvent::Frame(_) => match scroll.finish() {
            Some(event) => event,
            None => return,
        },
        _ => return,
    };
    let _ = tx.send(neutral);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(edge: Edge) -> EdgeSet {
        EdgeSet::EMPTY.with(edge)
    }

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
    fn auto_release_leaves_the_pointer_off_the_barrier_it_crossed() {
        let at = (3840.0, 1061.5);
        assert_eq!(inside_edge(at, Some(Edge::Right)), (3828.0, 1061.5));
        assert_eq!(inside_edge(at, Some(Edge::Left)), (3852.0, 1061.5));
        assert_eq!(inside_edge(at, Some(Edge::Bottom)), (3840.0, 1049.5));
        assert_eq!(inside_edge(at, None), (3840.0, 1061.5));
    }

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

    /// A single screen with the peer on the right: one barrier, on the right
    /// hand side, spanning the full height.
    #[test]
    fn a_barrier_lands_on_the_edge_that_borders_the_peer() {
        let (barriers, info) = outer_edge_barriers(&[(0, 0, 1920, 1080)], only(Edge::Right));
        assert_eq!(barriers.len(), 1);
        assert_eq!(barriers[0].position().as_tuple(), (1920, 0, 1920, 1079));
        let bi = info.values().next().unwrap();
        assert_eq!(bi.edge, Edge::Right);
        assert_eq!((bi.origin, bi.length), (0, 1080));
    }

    /// A peer to the right and another below: both edges are armed at once,
    /// and each barrier remembers which one it is, so a crossing says which
    /// way — and so to whom — the cursor left.
    #[test]
    fn two_neighbours_arm_two_edges_that_each_name_themselves() {
        let edges: EdgeSet = [Edge::Right, Edge::Bottom].into_iter().collect();
        let (barriers, info) = outer_edge_barriers(&[(0, 0, 1920, 1080)], edges);
        assert_eq!(barriers.len(), 2);
        let mut placed: Vec<Edge> = info.values().map(|bi| bi.edge).collect();
        placed.sort_by_key(|edge| format!("{edge:?}"));
        assert_eq!(placed, vec![Edge::Bottom, Edge::Right]);
        let lines: Vec<_> = barriers.iter().map(|b| b.position().as_tuple()).collect();
        assert!(lines.contains(&(1920, 0, 1920, 1079)));
        assert!(lines.contains(&(0, 1080, 1919, 1080)));
    }

    /// Every other edge stays an ordinary wall. A barrier there would still
    /// make the portal grab input when the cursor hit it — locking the user out
    /// for a crossing the session then discards as "no peer that way".
    #[test]
    fn no_barrier_is_placed_without_an_edge_to_watch() {
        let (barriers, info) = outer_edge_barriers(&[(0, 0, 1920, 1080)], EdgeSet::EMPTY);
        assert!(barriers.is_empty());
        assert!(info.is_empty());
    }

    /// Two monitors of equal height, side by side: only the outer edge gets a
    /// barrier.
    ///
    /// The seam between them used to get one too, and the portal rejected it on
    /// every start — `some barriers were rejected` in the log of any
    /// two-monitor desktop, which is exactly the noise that hides a barrier
    /// refused for a reason worth knowing.
    #[test]
    fn a_seam_between_two_monitors_gets_no_barrier() {
        let desktop = [(0, 0, 1920, 1080), (1920, 0, 1920, 1080)];
        let (barriers, info) = outer_edge_barriers(&desktop, only(Edge::Right));
        assert_eq!(barriers.len(), 1, "the seam must not be offered");
        assert_eq!(info.len(), 1);
        let line = barriers[0].position().as_tuple().0;
        assert_eq!(line, 3840, "the barrier belongs on the desktop's own edge");
    }

    /// A neighbour that does not cover the whole edge leaves it partly facing
    /// off the desktop, so the barrier stays.
    ///
    /// A 1080-tall screen beside a 1440-tall one covers only part of the taller
    /// one's edge; the rest is outer. Dropping the barrier there would leave
    /// the pointer able to leave the desktop with nothing to notice it, which
    /// is worse than offering one the portal might reject.
    #[test]
    fn a_partly_covered_edge_keeps_its_barrier() {
        let desktop = [(0, 0, 2560, 1440), (2560, 0, 1920, 1080)];
        let (barriers, _) = outer_edge_barriers(&desktop, only(Edge::Right));
        let lines: Vec<i32> = barriers.iter().map(|b| b.position().as_tuple().0).collect();
        assert!(
            lines.contains(&2560),
            "the taller screen's edge is exposed above the shorter one"
        );
        assert!(lines.contains(&4480), "and the outer edge is still there");
    }

    /// Stacked monitors, with the peer to the right: neither edge is a seam,
    /// because the seam between them is horizontal.
    #[test]
    fn a_seam_on_another_axis_does_not_suppress_a_barrier() {
        let desktop = [(0, 0, 1920, 1080), (0, 1080, 1920, 1080)];
        let (barriers, _) = outer_edge_barriers(&desktop, only(Edge::Right));
        assert_eq!(barriers.len(), 2, "both right edges face off the desktop");
    }

    /// A horizontal edge varies along x, a vertical one along y. Getting this
    /// backwards puts the crossing at the wrong end of the peer's screen.
    #[test]
    fn a_horizontal_edge_spans_the_width_and_a_vertical_one_the_height() {
        let screen = [(0, 0, 1920, 1080)];
        let (_, top) = outer_edge_barriers(&screen, only(Edge::Top));
        assert_eq!(top.values().next().unwrap().length, 1920);
        let (_, left) = outer_edge_barriers(&screen, only(Edge::Left));
        assert_eq!(left.values().next().unwrap().length, 1080);
    }

    /// A monitor whose origin is not (0, 0) — the second screen of any
    /// multi-monitor desktop — must normalize against its own span, or a
    /// crossing halfway down it reports a position off the end of the edge.
    #[test]
    fn an_offset_monitor_normalizes_against_its_own_span() {
        let (_, info) = outer_edge_barriers(&[(1920, 200, 2560, 1440)], only(Edge::Right));
        let bi = *info.values().next().unwrap();
        assert_eq!((bi.origin, bi.length), (200, 1440));
    }

    #[test]
    fn a_touchpads_smooth_scroll_reaches_the_peer_flipped_upward() {
        let mut frame = ScrollFrame::default();
        frame.delta(0.0, 5.0);
        assert_eq!(
            frame.finish(),
            Some(InputEvent::PointerScroll { dx: 0, dy: -60 }),
            "five pixels down is half a notch down"
        );
        assert_eq!(frame.finish(), None, "a frame is reported once");
    }

    #[test]
    fn a_wheel_click_described_both_ways_scrolls_once() {
        let mut frame = ScrollFrame::default();
        frame.delta(0.0, 10.0);
        frame.discrete = true;
        assert_eq!(frame.finish(), None);
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
