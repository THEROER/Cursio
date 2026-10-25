//! The InputCapture portal session: creating it, reading the monitor zones, and
//! placing (and replacing) the barriers on them.

use crate::wl_capture::barriers::outer_edge_barriers;
use crate::wl_capture::barriers::BarrierInfo;
use crate::wl_capture::barriers::Rect;
use crate::wl_capture::CaptureState;
use ashpd::desktop::input_capture::{Capabilities, CreateSessionOptions, InputCapture};
use cursio_protocol::EdgeSet;
use reis::ei::{self};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;

/// How long starting capture waits for the portal before carrying on without
/// its answer. A session with a saved grant answers well within this; only a
/// dialog waiting on the user takes longer.
pub(super) const PORTAL_ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Everything the capture loop needs from a portal session that is up.
///
/// The zone geometry is kept rather than dropped after the first placement:
/// the barriers are rebuilt whenever the watched edge changes, and asking the
/// portal for zones again on every change would be three round trips where one
/// suffices — and would race the zone set the barrier ids are scoped to.
pub(super) struct Portal {
    pub(super) ic: InputCapture,
    pub(super) session: ashpd::desktop::Session<InputCapture>,
    pub(super) event_stream: reis::tokio::EiConvertEventStream,
    pub(super) context: ei::Context,
    pub(super) barrier_info: HashMap<u32, BarrierInfo>,
    /// The desktop regions, as `(x, y, width, height)`.
    pub(super) rects: Vec<Rect>,
    /// Which zone set the current `rects` (and so the barrier ids) belong to.
    pub(super) zone_set: u32,
    /// The edges the barriers were actually placed on. Not re-read from the
    /// shared value by the loop: setup can sit on the permission dialog for
    /// hours, peers greet meanwhile, and a loop that took the current value as
    /// "already placed" kept the empty set it started with — no barrier at all
    /// until some peer moved.
    pub(super) edges: EdgeSet,
}

/// Open the InputCapture portal session, connect libei, and place outer-edge
/// barriers. Returns the pieces the capture loop drives. Runs on the Wayland
/// thread's LocalSet.
pub(super) async fn setup_portal(
    edge_rx: &mut tokio::sync::watch::Receiver<EdgeSet>,
) -> Result<Portal, String> {
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
    // Read only now, not when the thread started: a change from here on wakes
    // the capture loop, which compares against what was placed.
    let edges = *edge_rx.borrow_and_update();
    let barrier_info = place_barriers(&ic, &session, &rects, zone_set, edges).await;

    Ok(Portal {
        ic,
        session,
        event_stream,
        context,
        barrier_info,
        rects,
        zone_set,
        edges,
    })
}

/// Ask the portal for the current zones, flattened to rectangles.
pub(super) async fn read_zones(
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
pub(super) async fn place_barriers(
    ic: &InputCapture,
    session: &ashpd::desktop::Session<InputCapture>,
    rects: &[Rect],
    zone_set: u32,
    edges: EdgeSet,
) -> HashMap<u32, BarrierInfo> {
    let (barriers, barrier_info) = outer_edge_barriers(rects, edges, &crate::crossing_spans());
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

/// Take capture down, put a fresh barrier list up, and restore whatever state
/// capture was in.
///
/// The disable is not a precaution against a stale grab so much as against the
/// portal's own bookkeeping: replacing barriers while it is holding input means
/// the cursor is somewhere the new barrier list does not describe, and the
/// compositor is under no obligation to resolve that the way we would like.
/// Dropping to a known state and building back up is cheap — this runs once per
/// peer, not once per motion event.
pub(super) async fn replace_barriers(
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
