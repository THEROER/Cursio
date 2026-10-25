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
//! crossing the peer-facing screen edge — or, for a remote-screen view, after
//! the viewing peer picks a source; that half lives in
//! [`crate::remote_screen`], and this engine only routes its messages.

use std::collections::HashMap;

use async_trait::async_trait;
use cursio_protocol::{
    Message, ModuleId, Priority, VideoCaps, WindowCloseReason, WindowId, WindowMsg,
};
use cursio_transport::{TransportError, TransportTx};
use cursio_window::{CaptureSession, Surrogate, WindowCapture, WindowRenderer};

use crate::module::SessionModule;
use crate::remote_screen::{ScreenHost, ScreenViewer};
use crate::screen_hub::ScreenHub;
pub(super) use crate::window_session::pump::KeyframeRequests;
use cursio_protocol::SCREEN_STREAM_ID;

mod factory;
mod pump;

#[cfg(feature = "airplay")]
pub use factory::AirPlayShare;
pub use factory::WindowFactory;

/// Put one window *control* message on the wire, enveloped for this module.
///
/// Only control traffic goes through here. Frames and surrogate input stay
/// first-class `Message` variants and are sent directly — see
/// [`WindowMsg`](cursio_protocol::WindowMsg) for why that split exists.
async fn send_control(tx: &mut dyn TransportTx, msg: WindowMsg) -> Result<(), TransportError> {
    let framed = Message::module(ModuleId::WINDOW, msg.priority(), &msg)?;
    tx.send(&framed).await
}

/// One window this machine is hosting, and whether the peer knows about it yet.
struct HostedWindow<S> {
    session: S,
    /// Whether this window's offer has gone out. The offer must precede any of
    /// its frames, but sending needs a transport, so `share_window` only starts
    /// the capture and the next `pump` sends this.
    offer_sent: bool,
    last_frame: Option<std::time::Instant>,
}

/// Window transfer for one session, in whichever role(s) it ends up playing.
pub struct WindowEngine<C: WindowCapture, R: WindowRenderer> {
    capture: C,
    renderer: R,
    /// Windows we are hosting, keyed by the id we allocated for each.
    ///
    /// A map rather than one slot: the point of the feature is that the two
    /// desktops read as one, and a second monitor does not hold a single
    /// window. When this was an `Option`, dragging a second window across
    /// silently replaced the first — its surrogate was dropped mid-stream and
    /// the transfer looked like it had hung.
    hosted: HashMap<WindowId, HostedWindow<C::Session>>,
    /// Windows we are viewing, keyed by the id the host gave each.
    ///
    /// Keyed rather than paired with an id field, because every inbound frame,
    /// close and ack names the window it belongs to and has to find it.
    viewed: HashMap<WindowId, R::Surrogate>,
    /// Keyframe requests per viewed window.
    keyframe_asks: HashMap<WindowId, KeyframeRequests>,
    /// Next [`WindowId`] to allocate as host. Ids only need to be unique within
    /// a session, and only the host allocates, so a counter suffices.
    next_id: WindowId,
    /// Closes arriving from the peer, acted on in `pump` (`handle_message` is
    /// sync and cannot touch the transport). A queue rather than one slot: two
    /// windows can end in the same batch of messages.
    pending_closes: Vec<(WindowId, WindowCloseReason)>,
    peer_name: String,
    /// Transfers requested from outside the session — by the drag trigger,
    /// which lives on the input side and cannot reach into this module.
    ///
    /// A channel rather than a direct call because the trigger fires from its
    /// own task while the engine is owned by the session loop; this hands the
    /// request over without sharing the engine behind a lock.
    requests: Option<tokio::sync::mpsc::UnboundedReceiver<TransferRequest>>,
    /// Tells the drag trigger a transfer ended without being dragged back.
    ///
    /// The trigger latches "a window is over there" so that the next drag
    /// across the edge is read as a return. Every other way a transfer can end
    /// — the peer closing the surrogate, the real window going away — leaves
    /// that latch set, and the next window dragged at the edge is then taken
    /// for a return of something already home and silently dropped.
    transfer_ended: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// Serving the peer's remote-screen view of this machine.
    screen_host: ScreenHost,
    /// Our remote-screen view of the peer, when the user asked for views.
    screen_view: Option<ScreenViewer<R::Surrogate>>,
    /// What the peer's decoder takes, from its [`WindowMsg::VideoCaps`].
    /// Only the High profile until it says otherwise.
    peer_video: VideoCaps,
    video_budget: crate::video_budget::VideoBudget,
    /// The user here asked to see (or stop seeing) the peer's whole screen;
    /// sent on as [`WindowMsg::RequestScreen`]. Set for a link to a phone.
    screen_asks: Option<tokio::sync::mpsc::UnboundedReceiver<bool>>,
    /// What this host does when the peer asks for its screen. Set on a phone;
    /// elsewhere the request is ignored.
    on_screen_request: Option<Box<dyn FnMut(bool) + Send>>,
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
            hosted: HashMap::new(),
            viewed: HashMap::new(),
            keyframe_asks: HashMap::new(),
            next_id: 1,
            pending_closes: Vec::new(),
            peer_name,
            requests: None,
            transfer_ended: None,
            screen_host: ScreenHost::refusing("this machine has no remote-screen capture backend"),
            screen_view: None,
            peer_video: VideoCaps::default(),
            video_budget: Default::default(),
            screen_asks: None,
            on_screen_request: None,
        }
    }

    /// Send the peer what the user here asks of its screen.
    pub fn with_screen_asks(mut self, asks: tokio::sync::mpsc::UnboundedReceiver<bool>) -> Self {
        self.screen_asks = Some(asks);
        self
    }

    /// Act on the peer asking for this machine's screen.
    pub fn on_screen_request(mut self, act: impl FnMut(bool) + Send + 'static) -> Self {
        self.on_screen_request = Some(Box::new(act));
        self
    }

    /// Serve the peer's remote-screen view with `host` — which may itself be a
    /// refusal, for a node that opted out.
    pub fn with_screen_host(mut self, host: ScreenHost) -> Self {
        self.screen_host = host;
        self
    }

    /// Open a remote-screen view of the peer once it publishes its sources.
    pub fn with_screen_view(mut self) -> Self {
        self.screen_view = Some(ScreenViewer::new(self.peer_name.clone()));
        self
    }

    /// Offer the peer's sources to capture apps through `hub`, opening a view
    /// only when one of them asks (Linux, through the ScreenCast portal).
    pub fn with_screen_view_on_demand(mut self, hub: ScreenHub, peer_key: String) -> Self {
        self.screen_view = Some(ScreenViewer::new(self.peer_name.clone()).on_demand(hub, peer_key));
        self
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

    /// Report back to the drag trigger when a transfer ends by any other route.
    pub fn with_transfer_ended(mut self, ended: tokio::sync::mpsc::UnboundedSender<()>) -> Self {
        self.transfer_ended = Some(ended);
        self
    }

    /// A transfer this machine was hosting is over, and no drag said so.
    fn note_transfer_ended(&self) {
        if let Some(tx) = &self.transfer_ended {
            let _ = tx.send(());
        }
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
                    // The trigger fires per drag, and a window shoved at the
                    // edge twice is still the one window: without this a repeat
                    // would open a second capture of the same thing and offer it
                    // again, so the peer would show it twice.
                    if self
                        .hosted
                        .values()
                        .any(|h| h.session.meta().title == title)
                    {
                        tracing::debug!(%title, "already sharing this window; ignoring drag");
                        continue;
                    }
                    tracing::info!(%title, "window dragged to the peer edge");
                    // Told which window before being asked for one: the drag
                    // detector is the only thing that knows, and a backend that
                    // has to guess picks the foreground window — right on
                    // Windows by coincidence, the whole screen on macOS.
                    self.capture.aim_at(&title);
                    if let Err(e) = self.share_window() {
                        tracing::warn!(error = %e, "could not start the dragged transfer");
                    }
                }
                TransferRequest::BringBack => {
                    if self.hosted.is_empty() {
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
        let id = self.next_id;
        self.next_id += 1;
        let mut session = self.capture.begin(id)?;
        session.set_peer_video(self.peer_video);
        tracing::info!(
            id,
            peer = %self.peer_name,
            width = session.meta().width,
            height = session.meta().height,
            hosted = self.hosted.len() + 1,
            "sharing a window with the peer"
        );
        self.hosted.insert(
            id,
            HostedWindow {
                session,
                offer_sent: false,
                last_frame: None,
            },
        );
        Ok(id)
    }

    /// Stop hosting the window most recently shared, if any.
    ///
    /// Dragging back is undirected — the gesture says "bring one home", not
    /// which — so the newest is returned, which is the one the user just sent
    /// and the only one they can still be thinking of as in flight.
    pub fn stop_sharing(&mut self, reason: WindowCloseReason) {
        let Some(&id) = self.hosted.keys().max() else {
            return;
        };
        self.stop_sharing_id(id, reason);
    }

    /// Stop hosting one specific window.
    fn stop_sharing_id(&mut self, id: WindowId, reason: WindowCloseReason) {
        if let Some(mut hosted) = self.hosted.remove(&id) {
            hosted.session.close(reason);
            self.pending_closes.push((id, reason));
        }
    }

    /// Offers still owed, one per newly shared window. Each must precede that
    /// window's frames, so this is drained before frames are pumped.
    fn take_unsent_offers(&mut self) -> Vec<cursio_protocol::WindowMeta> {
        let mut offers = Vec::new();
        for hosted in self.hosted.values_mut() {
            if !hosted.offer_sent {
                hosted.offer_sent = true;
                offers.push(hosted.session.meta().clone());
            }
        }
        offers
    }

    /// Apply one control message from the peer: start a transfer, take rate
    /// feedback, or end a transfer.
    ///
    /// Split out from `handle_message` because control now arrives enveloped
    /// while frames and input do not — keeping the two decode paths separate is
    /// clearer than one match over both.
    fn handle_control(&mut self, msg: WindowMsg) {
        if self.screen_host.handle(&msg) {
            return;
        }
        if let Some(viewer) = self.screen_view.as_mut() {
            if viewer.handle(&msg, &mut self.renderer) {
                return;
            }
        }
        match msg {
            WindowMsg::VideoCaps(caps) => {
                tracing::info!(peer = %self.peer_name, ?caps, "peer's video decoder");
                self.peer_video = caps;
                for hosted in self.hosted.values_mut() {
                    hosted.session.set_peer_video(caps);
                }
                self.screen_host.set_peer_video(caps);
            }
            WindowMsg::Offer(meta) => match self.renderer.open(&meta) {
                Ok(surrogate) => {
                    tracing::info!(
                        id = meta.id,
                        width = meta.width,
                        height = meta.height,
                        "opened surrogate for the peer's window"
                    );
                    self.viewed.insert(meta.id, surrogate);
                }
                Err(e) => {
                    tracing::error!(error = %e, id = meta.id, "could not open surrogate window");
                    // Tell the host not to keep streaming into the void.
                    self.pending_closes
                        .push((meta.id, WindowCloseReason::ViewerClosed));
                }
            },
            WindowMsg::ResizeRequest { id, width, height } => {
                if let Some(hosted) = self.hosted.get_mut(&id) {
                    if let Err(e) = hosted.session.request_resize(width, height) {
                        // The window refusing a size is ordinary (a fixed
                        // dialog), and the viewer finds out from the frames
                        // that follow, so this is not worth a warning.
                        tracing::debug!(error = %e, id, width, height, "could not resize the hosted window");
                    }
                }
            }
            WindowMsg::FrameAck(ack) => {
                if let Some(hosted) = self.hosted.get_mut(&ack.id) {
                    hosted.session.on_ack(ack);
                }
            }
            WindowMsg::KeyframeRequest { id } => {
                if let Some(hosted) = self.hosted.get_mut(&id) {
                    hosted.session.request_keyframe();
                }
            }
            WindowMsg::Close { id, reason } => {
                tracing::info!(id, ?reason, "peer ended a window transfer");
                // Whichever side we were playing for this id, wind it down.
                self.keyframe_asks.remove(&id);
                if let Some(mut s) = self.viewed.remove(&id) {
                    s.close(reason);
                }
                if let Some(mut hosted) = self.hosted.remove(&id) {
                    hosted.session.close(reason);
                    self.note_transfer_ended();
                }
            }
            WindowMsg::RequestScreen { show } => match self.on_screen_request.as_mut() {
                Some(act) => act(show),
                None => tracing::debug!(
                    peer = %self.peer_name,
                    show,
                    "the peer asked for this machine's screen, which is not offered that way here"
                ),
            },
            other => tracing::debug!(?other, "remote-screen message this node does not serve"),
        }
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

    fn id(&self) -> ModuleId {
        ModuleId::WINDOW
    }

    /// Only what this machine's decoder takes goes out up front — before any
    /// window is offered either way, so a host never opens an encoder the
    /// viewer cannot decode. A transfer itself only exists once someone shares
    /// a window.
    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        send_control(tx, WindowMsg::VideoCaps(cursio_window::LOCAL_VIDEO_CAPS)).await?;
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
        for (id, reason) in std::mem::take(&mut self.pending_closes) {
            send_control(tx, WindowMsg::Close { id, reason }).await?;
        }
        let asks: Vec<bool> = match self.screen_asks.as_mut() {
            Some(rx) => std::iter::from_fn(|| rx.try_recv().ok()).collect(),
            None => Vec::new(),
        };
        for show in asks {
            tracing::info!(peer = %self.peer_name, show, "asking the peer about its screen");
            send_control(tx, WindowMsg::RequestScreen { show }).await?;
        }
        // Each offer must precede that window's first frame.
        for meta in self.take_unsent_offers() {
            tracing::info!(id = meta.id, "offering window to the peer");
            send_control(tx, WindowMsg::Offer(meta)).await?;
        }
        let now = std::time::Instant::now();
        let active: Vec<WindowId> = self
            .hosted
            .iter_mut()
            .filter_map(|(id, hosted)| {
                (!hosted.session.frames().is_empty()
                    || hosted.last_frame.is_some_and(|at| {
                        now.duration_since(at) < std::time::Duration::from_secs(1)
                    }))
                .then_some(*id)
            })
            .collect();
        let screens = usize::from(self.screen_host.has_capture());
        let count = (active.len() + screens).max(1) as u32;
        let age = tx.queue_age(Priority::Video);
        let bits = self.video_budget.observe(
            tx.queue_depth(Priority::Video).sent_bytes,
            age,
            !active.is_empty() || screens > 0,
            now,
        );
        let congested = self.video_budget.congested(now);
        for (id, hosted) in &mut self.hosted {
            let share = if active.contains(id) {
                bits / count
            } else {
                bits
            };
            hosted.session.set_network_budget(share, congested);
        }
        self.screen_host.set_network_budget(bits / count, congested);
        self.pump_frames(tx).await?;
        self.pump_viewer(tx).await?;
        self.screen_host.pump(tx).await?;
        if let Some(viewer) = self.screen_view.as_mut() {
            viewer.pump(tx).await?;
        }
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
            Message::WindowFrame(frame) if frame.id == SCREEN_STREAM_ID => {
                if let Some(viewer) = self.screen_view.as_mut() {
                    viewer.apply_frame(frame);
                }
                true
            }
            Message::WindowFrame(frame) => {
                match self.viewed.get_mut(&frame.id) {
                    Some(surrogate) => {
                        if let Err(e) = surrogate.apply_frame(frame) {
                            tracing::warn!(error = %e, seq = frame.seq, "dropping unusable frame");
                        }
                    }
                    None => tracing::debug!(id = frame.id, "frame for a window we aren't showing"),
                }
                true
            }
            Message::WindowInput(input) => {
                // Routed by the id the event carries: with several windows
                // hosted, replaying onto whichever happened to be first would
                // put the user's clicks in the wrong application.
                if let Some(hosted) = self.hosted.get_mut(&input.id()) {
                    if let Err(e) = hosted.session.apply_input(*input) {
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
        for (id, mut hosted) in std::mem::take(&mut self.hosted) {
            hosted.session.close(WindowCloseReason::HostClosed);
            let _ = send_control(
                tx,
                WindowMsg::Close {
                    id,
                    reason: WindowCloseReason::HostClosed,
                },
            )
            .await;
        }
        self.screen_host.shutdown();
        if let Some(viewer) = self.screen_view.as_mut() {
            viewer.shutdown();
        }
        for (_, mut surrogate) in std::mem::take(&mut self.viewed) {
            surrogate.close(WindowCloseReason::ViewerClosed);
        }
    }
}

/// Version of this module's wire vocabulary ([`WindowMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
/// 6: [`WindowMsg::RequestScreen`], a computer asking a phone for its screen.
pub const VERSION: u16 = 6;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingTx;
    use crate::window_session::pump::MAX_FRAMES_PER_PUMP;
    use crate::window_session::pump::VIDEO_QUEUE_HIGH_WATER_BYTES;
    use cursio_protocol::{Rect, TileCodec, WindowFrame, WindowInput, WindowMeta};
    use cursio_transport::QueueDepth;
    use cursio_window::window_test_support::*;
    use tokio::sync::mpsc;

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
        assert!(matches!(
            tx.decoded::<WindowMsg>(ModuleId::WINDOW).first(),
            Some(WindowMsg::Offer(_))
        ));
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
            tx.decoded::<WindowMsg>(ModuleId::WINDOW).first(),
            Some(WindowMsg::Close {
                reason: WindowCloseReason::Returned,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn the_peer_closing_the_window_tells_the_drag_trigger() {
        // Otherwise the trigger still believes a window is over there, and
        // reads the *next* window dragged at the edge as a return of one that
        // is already home — dropping it silently.
        let (_frames_tx, frames_rx) = frame_channel();
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (ended_tx, mut ended_rx) = mpsc::unbounded_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        )
        .with_requests(req_rx)
        .with_transfer_ended(ended_tx);

        req_tx
            .send(TransferRequest::Send {
                title: "Editor".into(),
            })
            .unwrap();
        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();
        assert!(ended_rx.try_recv().is_err(), "nothing has ended yet");

        eng.handle_message(&control_frame(WindowMsg::Close {
            id: 1,
            reason: WindowCloseReason::ViewerClosed,
        }));

        assert!(ended_rx.try_recv().is_ok());
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
            depth: QueueDepth {
                queued_bytes: VIDEO_QUEUE_HIGH_WATER_BYTES,
                sent_bytes: 0,
            },
        };
        eng.pump(&mut tx).await.unwrap();
        assert!(tx
            .sent
            .iter()
            .all(|message| !matches!(message, Message::WindowFrame(_))));

        tx.depth = QueueDepth::default();
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
        assert!(eng.viewed.contains_key(&7));
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
        assert!(eng.viewed.is_empty());
        assert_eq!(
            eng.pending_closes,
            vec![(7, WindowCloseReason::ViewerClosed)]
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

        assert!(tx
            .decoded::<WindowMsg>(ModuleId::WINDOW)
            .iter()
            .any(|m| matches!(
                m,
                WindowMsg::Close {
                    reason: WindowCloseReason::HostClosed,
                    ..
                }
            )));
        assert!(eng.hosted.is_empty());
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
        assert!(eng.viewed.is_empty());
        assert!(eng.viewed.is_empty());
    }

    #[test]
    fn sharing_a_second_window_keeps_the_first() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );

        let first = eng.share_window().unwrap();
        let second = eng.share_window().unwrap();

        assert_ne!(first, second, "each window gets its own id");
        assert!(
            eng.hosted.contains_key(&first) && eng.hosted.contains_key(&second),
            "a second window must not evict the first"
        );
        // Both are owed an offer, and each is offered exactly once.
        let offered: Vec<WindowId> = eng.take_unsent_offers().iter().map(|m| m.id).collect();
        assert_eq!(offered.len(), 2);
        assert!(eng.take_unsent_offers().is_empty());
    }

    #[tokio::test]
    async fn the_engine_announces_its_decoder_before_anything_else() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let mut tx = RecordingTx::default();
        eng.start(&mut tx).await.unwrap();
        assert_eq!(
            tx.decoded::<WindowMsg>(ModuleId::WINDOW).first(),
            Some(&WindowMsg::VideoCaps(cursio_window::LOCAL_VIDEO_CAPS))
        );
    }

    #[test]
    fn the_peers_decoder_reaches_captures_running_and_started_later() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let early = eng.share_window().unwrap();
        assert_eq!(
            eng.hosted[&early].session.peer_video,
            Some(VideoCaps { lossless: false }),
            "until the viewer speaks, only the High profile is safe"
        );

        let caps = VideoCaps { lossless: true };
        eng.handle_message(&control_frame(WindowMsg::VideoCaps(caps)));
        let late = eng.share_window().unwrap();

        assert_eq!(eng.hosted[&early].session.peer_video, Some(caps));
        assert_eq!(eng.hosted[&late].session.peer_video, Some(caps));
    }

    /// Input names the window it belongs to, so it reaches that one.
    ///
    /// With a single hosted window the id was decoration; with several,
    /// replaying onto whichever the map happened to yield first would deliver
    /// the user's clicks to the wrong application.
    #[test]
    fn input_is_replayed_onto_the_window_it_names() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let first = eng.share_window().unwrap();
        let second = eng.share_window().unwrap();

        eng.handle_message(&Message::WindowInput(WindowInput::PointerMove {
            id: second,
            x: 3,
            y: 4,
        }));

        assert!(
            eng.hosted[&first].session.applied_input.is_empty(),
            "the window that was not named must see nothing"
        );
        assert_eq!(
            eng.hosted[&second].session.applied_input,
            vec![WindowInput::PointerMove {
                id: second,
                x: 3,
                y: 4
            }]
        );
    }

    #[test]
    fn a_keyframe_request_reaches_the_capture_of_that_window() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        let id = eng.share_window().unwrap();
        eng.handle_message(&control_frame(WindowMsg::KeyframeRequest { id }));
        eng.handle_message(&control_frame(WindowMsg::KeyframeRequest { id: id + 1 }));
        let asked = &eng.hosted[&id].session.keyframe_requests;
        assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_surrogate_waiting_for_a_keyframe_asks_the_host() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        eng.handle_message(&control_frame(WindowMsg::Offer(
            cursio_protocol::WindowMeta {
                id: 9,
                title: "peer window".into(),
                width: 640,
                height: 480,
            },
        )));
        eng.viewed.get_mut(&9).unwrap().wants_keyframe = true;

        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();
        assert!(tx
            .decoded::<WindowMsg>(ModuleId::WINDOW)
            .contains(&WindowMsg::KeyframeRequest { id: 9 }));
    }

    #[tokio::test]
    async fn a_window_closed_while_its_input_is_held_still_tells_the_host() {
        let (_frames_tx, frames_rx) = frame_channel();
        let mut eng = engine(
            FakeCapture::new(frames_rx, 640, 480),
            FakeRenderer::default(),
        );
        eng.handle_message(&control_frame(WindowMsg::Offer(
            cursio_protocol::WindowMeta {
                id: 9,
                title: "phone — screen".into(),
                width: 864,
                height: 1920,
            },
        )));
        eng.viewed.get_mut(&9).unwrap().gone = true;

        let mut tx = RecordingTx::default();
        eng.pump(&mut tx).await.unwrap();
        assert!(tx
            .decoded::<WindowMsg>(ModuleId::WINDOW)
            .contains(&WindowMsg::Close {
                id: 9,
                reason: WindowCloseReason::ViewerClosed,
            }));
        assert!(eng.viewed.is_empty());
    }
}
