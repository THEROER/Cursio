//! Window transfer as a [`SessionModule`], sharing the session's one channel.
//!
//! Rides alongside input, audio, and clipboard rather than owning a session of
//! its own — the same arrangement as [`crate::audio_session`]. The transport
//! already schedules our messages sensibly: [`Message::WindowFrame`] is
//! `Priority::Video` (the bulkiest, most droppable traffic) while
//! [`Message::WindowInput`] is `Priority::Input`, so a burst of pixels can never
//! delay a click travelling the other way.
//!
//! One engine serves **both roles at once**, because either machine may host a
//! window while viewing another:
//!
//! - **Host** — drives a [`CaptureSession`]: sends [`Message::WindowOffer`] once,
//!   then pumps [`WindowFrame`]s from the capture channel; feeds inbound
//!   [`WindowInput`] to the real window and inbound [`WindowFrameAck`] to the
//!   encoder's rate control.
//! - **Viewer** — on an inbound offer, opens a [`Surrogate`]; blits inbound
//!   frames into it, and pumps the surrogate's input and acks back to the host.
//!
//! Capture starts only after an explicit transfer request, normally a window
//! crossing the peer-facing screen edge.

use async_trait::async_trait;
use cursio_protocol::{Message, ModuleId, Priority, WindowCloseReason, WindowId, WindowMsg};
use cursio_transport::{TransportError, TransportTx};
use cursio_window::{CaptureSession, Surrogate, WindowCapture, WindowRenderer};

use crate::module::SessionModule;

/// Put one window *control* message on the wire, enveloped for this module.
///
/// Only control traffic goes through here. Frames and surrogate input stay
/// first-class `Message` variants and are sent directly — see
/// [`WindowMsg`](cursio_protocol::WindowMsg) for why that split exists.
async fn send_control(tx: &mut dyn TransportTx, msg: WindowMsg) -> Result<(), TransportError> {
    let framed = Message::module(ModuleId::WINDOW, WindowMsg::PRIORITY, &msg)?;
    tx.send(&framed).await
}

/// Most frames to forward in one [`SessionModule::pump`] call.
///
/// The session cannot reach its inbound branch while a module pumps, so a module
/// that drains a busy channel dry starves the peer's messages — the discipline
/// the trait documents. A capture at 60 fps produces roughly one frame per
/// 16 ms while `pump` runs far more often than that, so a small bound keeps
/// latency low and still never monopolises the loop.
const MAX_FRAMES_PER_PUMP: usize = 4;

/// Stop pulling capture packets once this much video is waiting for the socket.
///
/// `PriorityTransport::send` only enqueues, so without this gate the session
/// drains one queue into another and can still accumulate seconds of stale
/// video. Backpressure now reaches the bounded capture channel instead.
const VIDEO_QUEUE_HIGH_WATER_BYTES: usize = 2 * 1024 * 1024;

/// Most input events to forward per pump, viewer→host.
///
/// Pointer motion is chatty; the same starvation rule applies, and stale motion
/// is worthless anyway.
const MAX_INPUT_PER_PUMP: usize = 32;

/// Window transfer for one session, in whichever role(s) it ends up playing.
pub struct WindowEngine<C: WindowCapture, R: WindowRenderer> {
    capture: C,
    renderer: R,
    /// The window we are hosting, if any. `None` until [`share_window`] starts one.
    hosted: Option<C::Session>,
    /// The window we are viewing, if any — opened when an offer arrives.
    viewed: Option<R::Surrogate>,
    /// The id of the viewed window. Kept beside the surrogate because
    /// [`Surrogate`] deliberately doesn't carry its meta — the id is session
    /// bookkeeping, not something a renderer backend should have to store.
    viewed_id: Option<WindowId>,
    /// Next [`WindowId`] to allocate as host. Ids only need to be unique within
    /// a session, and only the host allocates, so a counter suffices.
    next_id: WindowId,
    /// Whether the offer for the currently hosted window has gone out. The
    /// offer must precede any frame, but sending needs a transport, so
    /// `share_window` only starts the capture and the first `pump` sends this.
    offer_sent: bool,
    /// Set by `handle_message` when a close arrives, acted on in `pump`
    /// (`handle_message` is sync and cannot touch the transport).
    pending_close: Option<(WindowId, WindowCloseReason)>,
    peer_name: String,
    /// Transfers requested from outside the session — by the drag trigger,
    /// which lives on the input side and cannot reach into this module.
    ///
    /// A channel rather than a direct call because the trigger fires from its
    /// own task while the engine is owned by the session loop; this hands the
    /// request over without sharing the engine behind a lock.
    requests: Option<tokio::sync::mpsc::UnboundedReceiver<TransferRequest>>,
}

/// A transfer asked for from outside the session loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferRequest {
    /// Send a window to the peer. The title is what the drag detector saw; the
    /// capture backend uses it to pick the same window.
    Send { title: String },
    /// Bring the transferred window back to this machine.
    BringBack,
}

impl<C: WindowCapture, R: WindowRenderer> WindowEngine<C, R> {
    /// Create the module. Nothing is captured or shown until a window is shared
    /// locally or offered by the peer.
    pub fn new(capture: C, renderer: R, peer_name: String) -> Self {
        Self {
            capture,
            renderer,
            hosted: None,
            viewed: None,
            viewed_id: None,
            next_id: 1,
            offer_sent: false,
            pending_close: None,
            peer_name,
            requests: None,
        }
    }

    /// Accept transfer requests from the drag trigger.
    ///
    /// Optional: without this the engine only shares when told directly by its
    /// caller.
    pub fn with_requests(
        mut self,
        requests: tokio::sync::mpsc::UnboundedReceiver<TransferRequest>,
    ) -> Self {
        self.requests = Some(requests);
        self
    }

    /// Act on anything the drag trigger asked for since the last pump.
    ///
    /// Failures are logged, not propagated: a window that can't be captured
    /// must not take the session down with it.
    fn apply_requests(&mut self) {
        // Drain rather than take one: the trigger is latched per drag, so a
        // backlog here would mean stale requests acting on later drags.
        //
        // Collected before acting because handling a request calls back into
        // `self`, which cannot be borrowed again while the receiver is held.
        let pending: Vec<TransferRequest> = match self.requests.as_mut() {
            Some(rx) => std::iter::from_fn(|| rx.try_recv().ok()).collect(),
            None => return,
        };

        for request in pending {
            match request {
                TransferRequest::Send { title } => {
                    if self.hosted.is_some() {
                        tracing::debug!("already sharing a window; ignoring drag request");
                        continue;
                    }
                    tracing::info!(%title, "window dragged to the peer edge");
                    if let Err(e) = self.share_window() {
                        tracing::warn!(error = %e, "could not start the dragged transfer");
                    }
                }
                TransferRequest::BringBack => {
                    if self.hosted.is_none() {
                        tracing::debug!("nothing is being shared; ignoring return request");
                        continue;
                    }
                    tracing::info!("window dragged back from the peer");
                    self.stop_sharing(WindowCloseReason::Returned);
                }
            }
        }
    }

    /// Start hosting a window: pick it (the backend prompts if it must) and
    /// begin capturing.
    ///
    /// The offer itself goes out from the next `pump`, since sending needs the
    /// transport the session lends us. Returns the id assigned to the transfer.
    pub fn share_window(&mut self) -> Result<WindowId, cursio_window::WindowError> {
        if self.hosted.is_some() {
            return Err(cursio_window::WindowError::Backend(
                "already sharing a window in this session".into(),
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        let session = self.capture.begin(id)?;
        tracing::info!(
            id,
            peer = %self.peer_name,
            width = session.meta().width,
            height = session.meta().height,
            "sharing a window with the peer"
        );
        self.hosted = Some(session);
        self.offer_sent = false;
        Ok(id)
    }

    /// Stop hosting the current window, if any.
    pub fn stop_sharing(&mut self, reason: WindowCloseReason) {
        if let Some(mut session) = self.hosted.take() {
            let id = session.meta().id;
            session.close(reason);
            self.offer_sent = false;
            self.pending_close = Some((id, reason));
        }
    }

    /// The offer to send, if one is still owed. The offer must precede any
    /// frame, so this is checked before frames are pumped.
    fn take_unsent_offer(&mut self) -> Option<cursio_protocol::WindowMeta> {
        let session = self.hosted.as_ref()?;
        if self.offer_sent {
            return None;
        }
        self.offer_sent = true;
        Some(session.meta().clone())
    }

    /// Apply one control message from the peer: start a transfer, take rate
    /// feedback, or end a transfer.
    ///
    /// Split out from `handle_message` because control now arrives enveloped
    /// while frames and input do not — keeping the two decode paths separate is
    /// clearer than one match over both.
    fn handle_control(&mut self, msg: WindowMsg) {
        match msg {
            WindowMsg::Offer(meta) => match self.renderer.open(&meta) {
                Ok(surrogate) => {
                    tracing::info!(
                        id = meta.id,
                        width = meta.width,
                        height = meta.height,
                        "opened surrogate for the peer's window"
                    );
                    self.viewed = Some(surrogate);
                    self.viewed_id = Some(meta.id);
                }
                Err(e) => {
                    tracing::error!(error = %e, id = meta.id, "could not open surrogate window");
                    // Tell the host not to keep streaming into the void.
                    self.pending_close = Some((meta.id, WindowCloseReason::ViewerClosed));
                }
            },
            WindowMsg::FrameAck(ack) => {
                if let Some(session) = self.hosted.as_mut() {
                    session.on_ack(ack);
                }
            }
            WindowMsg::Close { id, reason } => {
                tracing::info!(id, ?reason, "peer ended a window transfer");
                // Whichever side we were playing for this id, wind it down.
                if self.viewed_id == Some(id) {
                    if let Some(mut s) = self.viewed.take() {
                        s.close(reason);
                    }
                    self.viewed_id = None;
                }
                if self.hosted.as_ref().map(|s| s.meta().id) == Some(id) {
                    if let Some(mut s) = self.hosted.take() {
                        s.close(reason);
                    }
                    self.offer_sent = false;
                }
            }
        }
    }

    /// Host → peer: forward captured frames, bounded per call.
    async fn pump_frames(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let Some(session) = self.hosted.as_mut() else {
            return Ok(());
        };
        for _ in 0..MAX_FRAMES_PER_PUMP {
            if tx.queue_depth(Priority::Video).queued_bytes >= VIDEO_QUEUE_HIGH_WATER_BYTES {
                break;
            }
            // `try_recv`, never `recv`: the capture channel going quiet is the
            // normal case (a static window emits nothing), and awaiting it here
            // would block the whole session loop on an idle window.
            match session.frames().try_recv() {
                Ok(frame) => tx.send(&Message::WindowFrame(frame)).await?,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    // The capture backend ended on its own (window closed,
                    // portal revoked). Tell the peer so its surrogate goes away.
                    tracing::info!("window capture ended; telling the peer");
                    let id = session.meta().id;
                    self.hosted = None;
                    self.offer_sent = false;
                    send_control(
                        tx,
                        WindowMsg::Close {
                            id,
                            reason: WindowCloseReason::HostClosed,
                        },
                    )
                    .await?;
                    break;
                }
            }
        }
        Ok(())
    }

    /// Viewer → host: forward the surrogate's input, then an ack if one is due.
    async fn pump_viewer(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let Some(surrogate) = self.viewed.as_mut() else {
            return Ok(());
        };
        for _ in 0..MAX_INPUT_PER_PUMP {
            match surrogate.input().try_recv() {
                Ok(input) => tx.send(&Message::WindowInput(input)).await?,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    // The user closed the surrogate window.
                    tracing::info!("surrogate window closed by the user");
                    let id = self.viewed_id.take();
                    self.viewed = None;
                    if let Some(id) = id {
                        send_control(
                            tx,
                            WindowMsg::Close {
                                id,
                                reason: WindowCloseReason::ViewerClosed,
                            },
                        )
                        .await?;
                    }
                    return Ok(());
                }
            }
        }
        // Rate-control feedback, self-limited by the surrogate to a few per
        // second — see `Surrogate::take_ack`.
        if let Some(surrogate) = self.viewed.as_mut() {
            if let Some(ack) = surrogate.take_ack() {
                send_control(tx, WindowMsg::FrameAck(ack)).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl<C, R> SessionModule for WindowEngine<C, R>
where
    C: WindowCapture + Send,
    C::Session: Send,
    R: WindowRenderer + Send,
    R::Surrogate: Send,
{
    fn name(&self) -> &'static str {
        "window"
    }

    /// Nothing to announce up front: unlike audio (which advertises its devices)
    /// a window transfer only exists once someone shares a window, so the wire
    /// stays silent until then.
    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        tracing::info!(
            peer = %self.peer_name,
            "window transfer ready — share a window to stream it to the peer"
        );
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        // Drag requests first, so a window dragged to the edge starts its
        // transfer on this pump rather than waiting for the next one.
        self.apply_requests();

        // A close deferred from `handle_message` goes first: the peer should
        // learn a transfer ended before it sees anything else.
        if let Some((id, reason)) = self.pending_close.take() {
            send_control(tx, WindowMsg::Close { id, reason }).await?;
        }
        // The offer must precede the first frame.
        if let Some(meta) = self.take_unsent_offer() {
            tracing::info!(id = meta.id, "offering window to the peer");
            send_control(tx, WindowMsg::Offer(meta)).await?;
        }
        self.pump_frames(tx).await?;
        self.pump_viewer(tx).await?;
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        // Control traffic arrives enveloped; the two hot paths below stay
        // first-class. Checked first because it is the cheap discriminant.
        match msg.decode_module::<WindowMsg>(ModuleId::WINDOW) {
            Ok(Some(control)) => {
                self.handle_control(control);
                return true;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, "undecodable window message from peer");
                return true;
            }
        }
        match msg {
            Message::WindowFrame(frame) => {
                if let Some(surrogate) = self.viewed.as_mut() {
                    if let Err(e) = surrogate.apply_frame(frame) {
                        tracing::warn!(error = %e, seq = frame.seq, "dropping unusable frame");
                    }
                } else {
                    tracing::debug!(id = frame.id, "frame for a window we aren't showing");
                }
                true
            }
            Message::WindowInput(input) => {
                if let Some(session) = self.hosted.as_mut() {
                    if let Err(e) = session.apply_input(*input) {
                        // Expected until the RemoteDesktop portal is wired in;
                        // logged at debug so it doesn't drown the session.
                        tracing::debug!(error = %e, "could not replay viewer input");
                    }
                }
                true
            }
            _ => false,
        }
    }

    async fn shutdown(&mut self, tx: &mut dyn TransportTx) {
        // Best-effort: the link may already be dead, so a failed send is not
        // worth reporting — the peer tears its side down on disconnect anyway.
        if let Some(mut session) = self.hosted.take() {
            let id = session.meta().id;
            session.close(WindowCloseReason::HostClosed);
            let _ = send_control(
                tx,
                WindowMsg::Close {
                    id,
                    reason: WindowCloseReason::HostClosed,
                },
            )
            .await;
        }
        if let Some(mut surrogate) = self.viewed.take() {
            surrogate.close(WindowCloseReason::ViewerClosed);
        }
    }
}

/// Builds [`WindowEngine`] when window transfer is enabled *and* this platform
/// has a capture/render backend.
///
/// This is where window transfer's `#[cfg]` lives. `main` used to carry three
/// platform blocks to construct the same module three ways; now the platform
/// question is answered once, here, and a machine with no backend simply
/// advertises nothing.
pub struct WindowFactory {
    /// When set, this node shares whatever an iPhone is mirroring to it instead
    /// of one of its own windows. The two are alternatives rather than both at
    /// once: an engine hosts a single window per session.
    ///
    /// The only option there is. A window of this machine's own transfers by
    /// being dragged across the peer-facing edge — the drag is the request, and
    /// a flag that meant the same thing was a second way to say it that had to
    /// agree with the first.
    pub airplay: Option<AirPlayShare>,
}

/// The receiver stream this node can offer through the window module.
#[derive(Clone)]
pub struct AirPlayShare {
    pub tap: cursio_airplay::MirrorTap,
    pub state: crate::service_session::ServiceState,
    pub local: cursio_protocol::PeerId,
    /// Preserve `--share-airplay`: offer to every peer, not only peers that
    /// explicitly asked for a local AirPlay window.
    pub force: bool,
}

/// Whether this build has a window-transfer backend at all.
///
/// macOS counts even though it can only host: a Mac that shares its screen with
/// a Windows peer is the whole point of the backend, and negotiation would skip
/// the module entirely if this said otherwise. Its renderer refuses politely,
/// so a peer offering *it* a window gets a clear error rather than silence.
const SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos"
));

impl crate::module::ModuleFactory for WindowFactory {
    fn id(&self) -> ModuleId {
        ModuleId::WINDOW
    }

    /// Advertised by any machine that *could* show a window, not only by one
    /// that wants to send one.
    ///
    /// Negotiation is symmetric: a module runs only when both sides advertise
    /// it. Receiving is passive and costs an idle engine; offering stays the
    /// deliberate act of dragging a window across an edge.
    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        SUPPORTED.then(|| cursio_protocol::ModuleAdvert::symmetric(ModuleId::WINDOW, VERSION))
    }

    #[allow(unused_variables)]
    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        if let Some(share) = &self.airplay {
            share.state.note_offer(ctx.peer.peer_id, ctx.peer.service);
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(airplay) = self
                .airplay
                .clone()
                .filter(|share| share.force || ctx.peer.service.show_window)
            {
                return Ok(Box::new(airplay_engine(
                    airplay,
                    cursio_window::GtkWindowRenderer,
                    ctx,
                )));
            }
            // On Linux the portal runs its own window picker, so there is no
            // title to match and no window of ours to hide — both options are
            // Windows-only and deliberately ignored here.
            let engine = WindowEngine::new(
                cursio_window::PortalWindowCapture,
                cursio_window::GtkWindowRenderer,
                ctx.peer_name().to_string(),
            )
            .with_requests(spawn_drag_trigger(ctx.peer_edge));
            Ok(Box::new(engine))
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(airplay) = self
                .airplay
                .clone()
                .filter(|share| share.force || ctx.peer.service.show_window)
            {
                return Ok(Box::new(airplay_engine(
                    airplay,
                    cursio_window::Win32WindowRenderer,
                    ctx,
                )));
            }
            // Hiding the source is what makes a transfer feel like the window
            // moved rather than been copied; the drag that started it is the
            // user saying they want it over there.
            let engine = WindowEngine::new(
                cursio_window::WgcWindowCapture {
                    title_hint: String::new(),
                    hide_source: true,
                },
                cursio_window::Win32WindowRenderer,
                ctx.peer_name().to_string(),
            )
            .with_requests(spawn_drag_trigger(ctx.peer_edge));
            Ok(Box::new(engine))
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(airplay) = self
                .airplay
                .clone()
                .filter(|share| share.force || ctx.peer.service.show_window)
            {
                return Ok(Box::new(airplay_engine(
                    airplay,
                    cursio_window::MacWindowRenderer,
                    ctx,
                )));
            }
            // A Mac can view a peer's window but not start a transfer of its
            // own: there is no drag detector on this platform, and the flag
            // that used to stand in for one is gone. The engine is built anyway
            // so the module is symmetric and a peer's window still arrives.
            let engine = WindowEngine::new(
                cursio_window::MacWindowCapture::default(),
                cursio_window::MacWindowRenderer,
                ctx.peer_name().to_string(),
            );
            Ok(Box::new(engine))
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            // Unreachable in practice: `advert` returns `None` here, so
            // negotiation never agrees on this module.
            anyhow::bail!("window transfer has no backend on this platform")
        }
    }
}

/// Version of this module's wire vocabulary ([`WindowMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 1;

/// Watch for windows dragged toward the peer's edge and turn that into
/// [`TransferRequest`]s.
///
/// Lives here rather than in the binary because it is part of what window
/// transfer *is* on each platform — the same reason the capture backends do.
/// Returns an empty-forever channel if the platform can't detect drags, which
/// the engine handles as "no drag trigger attached".
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn spawn_drag_trigger(
    peer_edge: Option<cursio_protocol::Edge>,
) -> tokio::sync::mpsc::UnboundedReceiver<TransferRequest> {
    use cursio_window::drag_trigger::{DragTrigger, TriggerAction};

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let (mut watcher, screen) = (
            cursio_window::linux_drag::DragWatcher::spawn(),
            cursio_window::linux_drag::screen_size().await,
        );
        #[cfg(target_os = "windows")]
        let (mut watcher, screen) = (
            cursio_window::windows_drag::DragWatcher::spawn(),
            cursio_window::windows_drag::screen_size(),
        );

        // Without a screen size there is no edge to compare against. Bail
        // rather than guess: a wrong bound would fire mid-screen.
        let Some(screen) = screen else {
            tracing::warn!("could not determine screen size; window drag detection is off");
            return;
        };
        if peer_edge.is_none() {
            tracing::info!(
                "this peer borders no edge of this screen, so dragging a window off one does \
                 nothing; configure distinct --at positions to enable window transfer"
            );
        }
        tracing::info!(
            ?peer_edge,
            width = screen.width,
            height = screen.height,
            "watching for windows dragged toward the peer"
        );

        let mut trigger = DragTrigger::new(peer_edge, screen);
        while let Some(event) = watcher.next().await {
            let Some(action) = trigger.on_drag(&event) else {
                continue;
            };
            let request = match action {
                TriggerAction::SendToPeer { title } => TransferRequest::Send { title },
                TriggerAction::BringBack => TransferRequest::BringBack,
            };
            if tx.send(request).is_err() {
                break; // the session ended
            }
        }
    });

    rx
}

/// Build an engine that hosts the mirrored iPhone screen.
///
/// Sharing follows mesh placement rather than a drag: only the selected host
/// offers a surrogate, and a host or quality change closes and recreates it.
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn airplay_engine<R: cursio_window::WindowRenderer>(
    share: AirPlayShare,
    renderer: R,
    ctx: &crate::module::SessionCtx,
) -> WindowEngine<crate::airplay_window::AirPlayWindowCapture, R> {
    let requests = spawn_airplay_trigger(share.state.clone(), share.local);
    WindowEngine::new(
        crate::airplay_window::AirPlayWindowCapture::for_mesh(share.tap, share.state),
        renderer,
        ctx.peer_name().to_string(),
    )
    .with_requests(requests)
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn spawn_airplay_trigger(
    state: crate::service_session::ServiceState,
    local: cursio_protocol::PeerId,
) -> tokio::sync::mpsc::UnboundedReceiver<TransferRequest> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut last: Option<Option<cursio_protocol::Quality>> = None;
        let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            poll.tick().await;
            let placement = state.placement();
            let selected = placement.is_some_and(|placement| placement.host == local)
                || (state.was_asked_to_host(cursio_protocol::ServiceId::AIRPLAY)
                    && state.is_hosted_here(cursio_protocol::ServiceId::AIRPLAY));
            let wanted = selected.then(|| placement.and_then(|placement| placement.quality));
            if wanted == last {
                continue;
            }
            if last.is_some() && tx.send(TransferRequest::BringBack).is_err() {
                return;
            }
            if wanted.is_some()
                && tx
                    .send(TransferRequest::Send {
                        title: "Cursio (iPhone)".into(),
                    })
                    .is_err()
            {
                return;
            }
            last = wanted;
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::{Rect, TileCodec, WindowFrame, WindowInput, WindowMeta};
    use cursio_transport::QueueDepth;
    use cursio_window::{window_test_support::*, WindowError};
    use tokio::sync::mpsc;

    fn airplay_offer(
        fitness: cursio_protocol::Fitness,
        wanted: Option<cursio_protocol::Quality>,
    ) -> cursio_protocol::ServiceOffer {
        cursio_protocol::ServiceOffer {
            fitness,
            wanted,
            show_window: false,
        }
    }

    #[tokio::test]
    async fn airplay_window_transfer_follows_host_and_quality_changes() {
        let local = cursio_protocol::PeerId::from_u128(1);
        let peer = cursio_protocol::PeerId::from_u128(2);
        let state = crate::service_session::ServiceState::new();
        state.note_offer(local, airplay_offer(cursio_protocol::Fitness::Full, None));
        let mut requests = spawn_airplay_trigger(state.clone(), local);

        assert!(matches!(
            requests.recv().await,
            Some(TransferRequest::Send { .. })
        ));

        state.note_offer(
            peer,
            airplay_offer(
                cursio_protocol::Fitness::Full,
                Some(cursio_protocol::Quality::new(2560, 1440, 30)),
            ),
        );
        assert_eq!(requests.recv().await, Some(TransferRequest::BringBack));
        assert!(matches!(
            requests.recv().await,
            Some(TransferRequest::Send { .. })
        ));

        state.note_offer(local, airplay_offer(cursio_protocol::Fitness::No, None));
        assert_eq!(requests.recv().await, Some(TransferRequest::BringBack));
    }

    /// Collects what the module sends, standing in for the transport.
    #[derive(Default)]
    struct RecordingTx {
        sent: Vec<Message>,
        video_depth: QueueDepth,
    }

    #[async_trait]
    impl TransportTx for RecordingTx {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            self.sent.push(msg.clone());
            Ok(())
        }

        fn queue_depth(&self, priority: Priority) -> QueueDepth {
            if priority == Priority::Video {
                self.video_depth
            } else {
                QueueDepth::default()
            }
        }
    }

    impl RecordingTx {
        /// The control messages sent, decoded out of their envelopes — the same
        /// way the peer's module sees them.
        fn control(&self) -> Vec<WindowMsg> {
            self.sent
                .iter()
                .filter_map(|m| m.decode_module::<WindowMsg>(ModuleId::WINDOW).unwrap())
                .collect()
        }
    }

    /// Wrap a control message the way the peer would send it, for feeding to
    /// `handle_message`.
    fn control_frame(msg: WindowMsg) -> Message {
        Message::module(ModuleId::WINDOW, WindowMsg::PRIORITY, &msg).unwrap()
    }

    fn engine(
        capture: FakeCapture,
        renderer: FakeRenderer,
    ) -> WindowEngine<FakeCapture, FakeRenderer> {
        WindowEngine::new(capture, renderer, "test-peer".into())
    }

    fn frame(id: WindowId, seq: u32) -> WindowFrame {
        WindowFrame {
            id,
            seq,
            dirty: Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            codec: TileCodec::RawBgra8888,
            data: vec![0; 2 * 2 * 4],
        }
    }

    fn frame_channel() -> (mpsc::Sender<WindowFrame>, mpsc::Receiver<WindowFrame>) {
        mpsc::channel(64)
    }

    #[tokio::test]
    async fn a_drag_request_starts_the_transfer_on_the_next_pump() {
        let (_frames_tx, frames_rx) = frame_channel();
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        )
        .with_requests(req_rx);

        req_tx
            .send(TransferRequest::Send {
                title: "Editor".into(),
            })
            .unwrap();

        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();

        // Dragging a window to the edge must offer it, with no separate call.
        assert!(matches!(tx.control().first(), Some(WindowMsg::Offer(_))));
    }

    #[tokio::test]
    async fn dragging_back_closes_the_transfer_as_returned() {
        let (_frames_tx, frames_rx) = frame_channel();
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        )
        .with_requests(req_rx);

        req_tx
            .send(TransferRequest::Send {
                title: "Editor".into(),
            })
            .unwrap();
        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();

        req_tx.send(TransferRequest::BringBack).unwrap();
        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();

        // `Returned` (not `Closed`) is what tells the peer the window went
        // home rather than died, so its surrogate disappears quietly.
        assert!(matches!(
            tx.control().first(),
            Some(WindowMsg::Close {
                reason: WindowCloseReason::Returned,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_second_send_while_hosting_is_ignored() {
        // The trigger latches per drag, but a repeat must not restart capture
        // or allocate a second id for the window already on the peer.
        let (_frames_tx, frames_rx) = frame_channel();
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        )
        .with_requests(req_rx);

        for _ in 0..2 {
            req_tx
                .send(TransferRequest::Send {
                    title: "Editor".into(),
                })
                .unwrap();
        }

        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();

        let offers = tx
            .sent
            .iter()
            .filter_map(|m| m.decode_module::<WindowMsg>(ModuleId::WINDOW).unwrap())
            .filter(|m| matches!(m, WindowMsg::Offer(_)))
            .count();
        assert_eq!(offers, 1, "a repeated drag must not re-offer the window");
    }

    #[tokio::test]
    async fn requests_are_ignored_without_a_trigger_attached() {
        // An engine without an external trigger must pump normally rather than
        // assume one exists.
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );

        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();
        assert!(tx.sent.is_empty());
    }

    #[tokio::test]
    async fn offer_precedes_the_first_frame() {
        let (frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let id = eng.share_window().unwrap();
        frames_tx.try_send(frame(id, 1)).unwrap();

        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();

        // The viewer cannot size a surrogate without the offer, so it must come
        // first — this ordering is a protocol requirement, not a nicety. It also
        // spans the envelope boundary: the offer is enveloped control, the frame
        // is a first-class variant, and they must still be ordered relative to
        // each other on the wire.
        assert!(matches!(
            tx.sent
                .first()
                .unwrap()
                .decode_module::<WindowMsg>(ModuleId::WINDOW)
                .unwrap(),
            Some(WindowMsg::Offer(_))
        ));
        assert!(matches!(tx.sent.get(1), Some(Message::WindowFrame(_))));
    }

    #[tokio::test]
    async fn offer_is_sent_only_once() {
        let (frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let id = eng.share_window().unwrap();

        let mut tx = RecordingTx::default();
        frames_tx.try_send(frame(id, 1)).unwrap();
        eng.pump(&mut tx).await.unwrap();
        frames_tx.try_send(frame(id, 2)).unwrap();
        eng.pump(&mut tx).await.unwrap();

        let offers = tx
            .sent
            .iter()
            .filter_map(|m| m.decode_module::<WindowMsg>(ModuleId::WINDOW).unwrap())
            .filter(|m| matches!(m, WindowMsg::Offer(_)))
            .count();
        assert_eq!(offers, 1);
    }

    #[tokio::test]
    async fn frames_are_bounded_per_pump() {
        let (frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let id = eng.share_window().unwrap();
        // Far more than one pump may take: draining them all would starve the
        // session's inbound branch.
        for seq in 0..50 {
            frames_tx.try_send(frame(id, seq)).unwrap();
        }

        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();

        let frames = tx
            .sent
            .iter()
            .filter(|m| matches!(m, Message::WindowFrame(_)))
            .count();
        assert_eq!(frames, MAX_FRAMES_PER_PUMP);
    }

    #[tokio::test]
    async fn backed_up_transport_leaves_capture_frame_queued() {
        let (frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let id = eng.share_window().unwrap();
        frames_tx.try_send(frame(id, 1)).unwrap();

        let mut tx = RecordingTx {
            sent: Vec::new(),
            video_depth: QueueDepth {
                queued_bytes: VIDEO_QUEUE_HIGH_WATER_BYTES,
                sent_bytes: 0,
            },
        };
        eng.pump(&mut tx).await.unwrap();
        assert!(tx
            .sent
            .iter()
            .all(|message| !matches!(message, Message::WindowFrame(_))));

        tx.video_depth = QueueDepth::default();
        eng.pump(&mut tx).await.unwrap();
        assert!(tx
            .sent
            .iter()
            .any(|message| matches!(message, Message::WindowFrame(_))));
    }

    #[test]
    fn an_offer_opens_a_surrogate() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let meta = WindowMeta {
            id: 7,
            title: "peer window".into(),
            width: 320,
            height: 240,
        };
        assert!(eng.handle_message(&control_frame(WindowMsg::Offer(meta))));
        assert_eq!(eng.viewed_id, Some(7));
    }

    #[test]
    fn a_failed_surrogate_tells_the_host_to_stop() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::failing(),
        );
        let meta = WindowMeta {
            id: 7,
            title: "peer window".into(),
            width: 320,
            height: 240,
        };
        eng.handle_message(&control_frame(WindowMsg::Offer(meta)));
        // No surrogate, and a close queued so the host stops streaming pixels
        // nobody is showing.
        assert!(eng.viewed.is_none());
        assert_eq!(
            eng.pending_close,
            Some((7, WindowCloseReason::ViewerClosed))
        );
    }

    #[test]
    fn window_messages_are_claimed_and_others_are_not() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        assert!(eng.handle_message(&Message::WindowFrame(frame(1, 1))));
        assert!(eng.handle_message(&control_frame(WindowMsg::FrameAck(
            cursio_protocol::WindowFrameAck {
                id: 1,
                acked_seq: 1
            }
        ))));
        assert!(
            eng.handle_message(&Message::WindowInput(WindowInput::PointerMove {
                id: 1,
                x: 0,
                y: 0
            }))
        );
        // Another module's message must fall through to it.
        assert!(!eng.handle_message(&Message::Heartbeat { nonce: 1 }));
    }

    #[tokio::test]
    async fn a_dead_capture_closes_the_transfer() {
        let (frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        eng.share_window().unwrap();
        // The backend going away (window closed, portal revoked) must reach the
        // peer rather than leaving its surrogate frozen forever.
        drop(frames_tx);

        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();

        assert!(tx.control().iter().any(|m| matches!(
            m,
            WindowMsg::Close {
                reason: WindowCloseReason::HostClosed,
                ..
            }
        )));
        assert!(eng.hosted.is_none());
    }

    #[test]
    fn peer_close_tears_down_our_surrogate() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        eng.handle_message(&control_frame(WindowMsg::Offer(WindowMeta {
            id: 7,
            title: "w".into(),
            width: 320,
            height: 240,
        })));
        eng.handle_message(&control_frame(WindowMsg::Close {
            id: 7,
            reason: WindowCloseReason::HostClosed,
        }));
        assert!(eng.viewed.is_none());
        assert_eq!(eng.viewed_id, None);
    }

    #[test]
    fn sharing_twice_is_refused() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        eng.share_window().unwrap();
        assert!(matches!(eng.share_window(), Err(WindowError::Backend(_))));
    }
}
