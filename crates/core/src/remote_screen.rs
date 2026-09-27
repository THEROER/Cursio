//! Remote-screen views: an app or a monitor of a peer, offered to this
//! machine's screen-sharing software without being drawn on its desktop.
//!
//! Discord, OBS or a browser call can only offer what this machine has. What
//! "has" means differs, so the view does too:
//!
//! - **Windows**: those apps list windows, so the view is one — titled
//!   `Cursio — <peer>`, one per peer, kept for the life of the link, but
//!   never on screen (see the Windows surrogate's `unseen` module for the
//!   measurements behind "listed and recorded, yet unseen"). Its source is
//!   chosen from a notification-area icon.
//! - **Linux (GNOME)**: those apps take pixels only from the ScreenCast
//!   portal, so the view is a PipeWire node, opened when a capture app picks
//!   the peer's source in Cursio's portal backend dialog and closed when it is
//!   done. Nothing is kept open per peer; the request travels through
//!   [`ScreenHub`], because the portal runs apart from the session loops
//!   ([`ScreenViewer::on_demand`]).
//!
//! The first version drew a visible window per peer on the viewing desktop,
//! purely so it could be captured — rendering for nobody, which is what the
//! user asked not to have.
//!
//! ## Pulled by the viewer, unlike a transfer
//!
//! A window transfer is pushed: dragging it across the edge is the host's user
//! saying "show this over there". Here the request comes from the viewer. In a
//! mesh of strangers that would be a spyware primitive; in Cursio's it is not a
//! new grant. Every peer is one of the user's own machines, admitted by a
//! password typed on both, and each of them can already drive the other's
//! keyboard and mouse and read its disks — watching its screen is strictly
//! less. Hosting still stays a choice: a node started with `--no-screen-host`
//! answers every request with [`WindowMsg::SourcesRefused`] and a reason, so
//! the viewer can say why nothing appears instead of waiting on silence.
//! Viewing stays passive in the other sense — nothing here opens unless the
//! user asked for views with `--remote-screen`.
//!
//! ## One window across every switch (Windows)
//!
//! Screen-sharing software captures a *window*; recreate the window and the
//! share ends. So a source switch never closes the view. (On Linux a switch
//! is a new pick in the capture app, which asks the portal afresh, so each
//! pick gets its own node; one per peer at a time, a newer pick taking the
//! stream from an older one.) The host swaps the
//! capture underneath one reserved stream id ([`SCREEN_STREAM_ID`]) and sends
//! [`WindowMsg::SourceShown`] ahead of the new source's first frame; the view
//! restarts its decoder and picture on that marker and letterboxes whatever
//! size arrives. The old capture keeps streaming until the new one is actually
//! up, because on GNOME "up" can mean a portal picker waiting on the host.
//!
//! The host's half is [`ScreenHost`], the viewer's [`ScreenViewer`]; the window
//! module owns one of each and hands them the messages that are theirs.

use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cursio_protocol::{
    Message, ModuleId, Priority, ScreenSource, SourceId, SourceKind, VideoCaps, WindowCloseReason,
    WindowFrame, WindowFrameAck, WindowMeta, WindowMsg, SCREEN_STREAM_ID,
};
use cursio_transport::{TransportError, TransportTx};
use cursio_window::{
    CaptureSession, ScreenSources, SourcePick, Surrogate, WindowError, WindowRenderer,
};
use tokio::sync::oneshot;

use crate::screen_hub::{HubRequest, ScreenHub, Shown};
use crate::window_session::KeyframeRequests;

/// How often a subscribed host re-reads its source list.
///
/// A window list changes at human pace; two seconds keeps the viewer's menu
/// current without the enumeration showing up anywhere.
const SOURCE_POLL: Duration = Duration::from_secs(2);

/// Frames forwarded per pump — the same starvation bound transfers use.
const MAX_FRAMES_PER_PUMP: usize = 4;

/// Stop pulling frames once this much video waits for the socket; see the
/// window module's identical gate.
const VIDEO_QUEUE_HIGH_WATER_BYTES: usize = 2 * 1024 * 1024;

/// Size a new view opens at, before any source has told it otherwise.
///
/// The view letterboxes rather than following its source, so this is only a
/// starting size the user is free to change.
const VIEW_SIZE: (u32, u32) = (1280, 720);

async fn send_control(tx: &mut dyn TransportTx, msg: WindowMsg) -> Result<(), TransportError> {
    let framed = Message::module(ModuleId::WINDOW, msg.priority(), &msg)?;
    tx.send(&framed).await
}

/// The source that opened and its capture, or why the viewer gets nothing.
type OpenResult = Result<(ScreenSource, Box<dyn CaptureSession + Send>), String>;

type ListResult = Result<Vec<ScreenSource>, WindowError>;

/// The host's half: publish the source list, stream the chosen source.
pub struct ScreenHost {
    /// `None` when this node does not serve views; `refusal` says why.
    sources: Option<Arc<dyn ScreenSources>>,
    refusal: String,
    subscribed: bool,
    refusal_sent: bool,
    /// What the viewer was last told, so an unchanged list is not resent.
    published: Option<Vec<ScreenSource>>,
    next_poll: Instant,
    current: Option<(SourceId, Box<dyn CaptureSession + Send>)>,
    /// A source being resolved and opened on its own thread. At most one: a
    /// newer request replaces an older one still opening. The abandoned
    /// thread's send then fails and its capture is dropped with it.
    opening: Option<std_mpsc::Receiver<OpenResult>>,
    /// A list being read on its own thread. Measured on Windows at 50–220 ms
    /// every poll while a view was streaming: a window's title is answered by
    /// the thread that owns the window, and this process's own view window is
    /// owned by a thread busy decoding the stream. The mouse rides the same
    /// loop. At most one in flight, so a list stuck on a hung application
    /// stops the refreshes instead of piling up threads behind it.
    listing: Option<std_mpsc::Receiver<ListResult>>,
    /// Control owed to the viewer that `handle` could not send itself.
    outbox: Vec<WindowMsg>,
    /// What the viewer's decoder takes, handed to every capture opened here.
    peer_video: VideoCaps,
}

impl ScreenHost {
    /// Record what the viewer's decoder takes; applies to the source on
    /// screen now and to every one opened after.
    pub fn set_peer_video(&mut self, caps: VideoCaps) {
        self.peer_video = caps;
        if let Some((_, session)) = self.current.as_mut() {
            session.set_peer_video(caps);
        }
    }

    /// A host that serves views from `sources`.
    pub fn serving(sources: Arc<dyn ScreenSources>) -> Self {
        Self::new(Some(sources), String::new())
    }

    /// A host that refuses every view, telling the viewer `reason`.
    pub fn refusing(reason: impl Into<String>) -> Self {
        Self::new(None, reason.into())
    }

    fn new(sources: Option<Arc<dyn ScreenSources>>, refusal: String) -> Self {
        Self {
            sources,
            refusal,
            subscribed: false,
            refusal_sent: false,
            published: None,
            next_poll: Instant::now(),
            current: None,
            opening: None,
            listing: None,
            outbox: Vec::new(),
            peer_video: VideoCaps::default(),
        }
    }

    /// Take a message meant for the host's half. Returns whether it was one.
    pub fn handle(&mut self, msg: &WindowMsg) -> bool {
        match msg {
            WindowMsg::ListSources => {
                self.subscribed = true;
                self.published = None;
                self.next_poll = Instant::now();
            }
            WindowMsg::ShowSource { source } => self.show(*source),
            WindowMsg::StopSource => {
                self.opening = None;
                if let Some((_, mut session)) = self.current.take() {
                    session.close(WindowCloseReason::ViewerClosed);
                    self.outbox.push(WindowMsg::SourceEnded {
                        reason: "the viewer stopped it".into(),
                    });
                }
            }
            WindowMsg::FrameAck(ack) if ack.id == SCREEN_STREAM_ID => self.on_ack(*ack),
            WindowMsg::KeyframeRequest { id } if *id == SCREEN_STREAM_ID => {
                if let Some((_, session)) = self.current.as_mut() {
                    session.request_keyframe();
                }
            }
            _ => return false,
        }
        true
    }

    fn on_ack(&mut self, ack: WindowFrameAck) {
        if let Some((_, session)) = self.current.as_mut() {
            session.on_ack(ack);
        }
    }

    fn show(&mut self, id: SourceId) {
        let Some(sources) = self.sources.clone() else {
            self.refusal_sent = false;
            return;
        };
        let (tx, rx) = std_mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("cursio-screen-open".into())
            .spawn(move || {
                let _ = tx.send(open_source(sources.as_ref(), id));
            });
        match spawned {
            Ok(_) => self.opening = Some(rx),
            Err(e) => self.outbox.push(WindowMsg::SourceEnded {
                reason: format!("could not start the capture thread: {e}"),
            }),
        }
    }

    /// The source list, re-read when due and sent only when it changed.
    fn poll_sources(&mut self) -> Option<WindowMsg> {
        if !self.subscribed {
            return None;
        }
        let Some(sources) = self.sources.clone() else {
            if self.refusal_sent {
                return None;
            }
            self.refusal_sent = true;
            return Some(WindowMsg::SourcesRefused {
                reason: self.refusal.clone(),
            });
        };
        if let Some(listing) = self.listing.as_ref() {
            let result = match listing.try_recv() {
                Ok(result) => result,
                Err(std_mpsc::TryRecvError::Empty) => return None,
                Err(std_mpsc::TryRecvError::Disconnected) => Err(WindowError::Backend(
                    "the source-listing thread died".into(),
                )),
            };
            self.listing = None;
            return match result {
                Ok(list) if self.published.as_ref() != Some(&list) => {
                    self.published = Some(list.clone());
                    Some(WindowMsg::Sources(list))
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "could not list shareable sources for the peer");
                    None
                }
            };
        }
        let now = Instant::now();
        if now < self.next_poll {
            return None;
        }
        self.next_poll = now + SOURCE_POLL;
        let (tx, rx) = std_mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("cursio-screen-list".into())
            .spawn(move || {
                let _ = tx.send(sources.list());
            });
        match spawned {
            Ok(_) => self.listing = Some(rx),
            Err(e) => tracing::warn!(error = %e, "could not start the source-listing thread"),
        }
        None
    }

    /// A source that finished opening, swapped in for the one on screen.
    fn poll_opening(&mut self) -> Option<WindowMsg> {
        let opening = self.opening.as_ref()?;
        let result = match opening.try_recv() {
            Ok(result) => result,
            Err(std_mpsc::TryRecvError::Empty) => return None,
            Err(std_mpsc::TryRecvError::Disconnected) => Err("the capture thread died".into()),
        };
        self.opening = None;
        match result {
            Ok((source, mut session)) => {
                session.set_peer_video(self.peer_video);
                if let Some((_, mut old)) = self.current.take() {
                    old.close(WindowCloseReason::Returned);
                }
                let meta = session.meta().clone();
                tracing::info!(title = %source.title, "showing a source in the peer's view");
                self.current = Some((source.id, session));
                Some(WindowMsg::SourceShown {
                    source: source.id,
                    title: source.title,
                    width: meta.width,
                    height: meta.height,
                })
            }
            Err(reason) => {
                tracing::warn!(%reason, "could not show a source to the peer");
                Some(WindowMsg::SourceEnded { reason })
            }
        }
    }

    /// Send what is owed: list updates, a finished switch, then frames.
    pub async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        for msg in std::mem::take(&mut self.outbox) {
            send_control(tx, msg).await?;
        }
        if let Some(msg) = self.poll_sources() {
            send_control(tx, msg).await?;
        }
        if let Some(msg) = self.poll_opening() {
            send_control(tx, msg).await?;
        }
        let mut ended = false;
        if let Some((_, session)) = self.current.as_mut() {
            for _ in 0..MAX_FRAMES_PER_PUMP {
                if tx.queue_depth(Priority::Video).queued_bytes >= VIDEO_QUEUE_HIGH_WATER_BYTES {
                    break;
                }
                match session.frames().try_recv() {
                    Ok(frame) => tx.send(&Message::WindowFrame(frame)).await?,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        ended = true;
                        break;
                    }
                }
            }
        }
        if ended {
            self.current = None;
            send_control(
                tx,
                WindowMsg::SourceEnded {
                    reason: "the source closed on this machine".into(),
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Stop whatever is being shown; the link is going away.
    pub fn shutdown(&mut self) {
        self.opening = None;
        if let Some((_, mut session)) = self.current.take() {
            session.close(WindowCloseReason::HostClosed);
        }
    }

    /// Whether a source is streaming, for tests and logs.
    pub fn showing(&self) -> Option<SourceId> {
        self.current.as_ref().map(|(id, _)| *id)
    }
}

/// Find `id` and start capturing it; run on the opening thread.
///
/// Resolved against a fresh list, not the published one: the viewer's menu
/// may be a poll behind, and a window that closed since must fail here with
/// its name rather than inside a capture backend.
fn open_source(sources: &dyn ScreenSources, id: SourceId) -> OpenResult {
    let list = sources
        .list()
        .map_err(|e| format!("could not list sources: {e}"))?;
    let source = list
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("source {id} no longer exists on this machine"))?;
    if source.picked_on_host {
        tracing::info!(
            title = %source.title,
            "a peer asked to see this machine; pick what to show in the dialog here"
        );
    }
    match sources.open(SCREEN_STREAM_ID, &source) {
        Ok(session) => Ok((source, session)),
        Err(e) => Err(format!("{}: {e}", source.title)),
    }
}

/// The viewer's half: one view of the peer, and what the user picks in it.
pub struct ScreenViewer<S: Surrogate> {
    peer_name: String,
    subscribed: bool,
    view: Option<S>,
    sources: Vec<ScreenSource>,
    /// What the view is showing, per the host's last `SourceShown`.
    showing: Option<SourceId>,
    /// What was last asked for — the reference point for next/previous while
    /// a switch is still opening.
    wanted: Option<SourceId>,
    /// Whether a first source has been chosen automatically. Only once: after
    /// that the choice is the user's, and a refreshed list must not undo it.
    auto_picked: bool,
    /// The user closed the view. It stays closed for this link, or the next
    /// list refresh would pop it straight back up.
    dismissed: bool,
    outbox: Vec<WindowMsg>,
    /// Set where views are asked for by capture apps (Linux, through the
    /// portal) instead of kept open by the link: the hub, and this peer's
    /// key in it.
    demand: Option<(ScreenHub, String)>,
    /// A capture app's request waiting for the host's `SourceShown`.
    pending: Option<PendingShow>,
    /// The showing a capture app holds now.
    active: Option<ActiveShow>,
    keyframe_asks: KeyframeRequests,
}

struct PendingShow {
    token: u64,
    source: SourceId,
    reply: oneshot::Sender<Result<Shown, String>>,
}

struct ActiveShow {
    token: u64,
    ended: oneshot::Sender<()>,
}

impl<S: Surrogate> ScreenViewer<S> {
    pub fn new(peer_name: String) -> Self {
        Self {
            peer_name,
            subscribed: false,
            view: None,
            sources: Vec::new(),
            showing: None,
            wanted: None,
            auto_picked: false,
            dismissed: false,
            outbox: Vec::new(),
            demand: None,
            pending: None,
            active: None,
            keyframe_asks: KeyframeRequests::default(),
        }
    }

    /// Open views only when a capture app asks for one through `hub`, where
    /// this peer is `peer_key`.
    pub fn on_demand(mut self, hub: ScreenHub, peer_key: String) -> Self {
        self.demand = Some((hub, peer_key));
        self
    }

    /// The host's answer to a capture app's request: open the node now that
    /// its size is known, and tell the app where it is.
    fn fulfil<R>(
        &mut self,
        source: SourceId,
        title: &str,
        width: u32,
        height: u32,
        renderer: &mut R,
    ) where
        R: WindowRenderer<Surrogate = S>,
    {
        let Some(pending) = self.pending.take_if(|p| p.source == source) else {
            // Not what anyone is waiting for: the host restarted the stream a
            // capture app already holds.
            if let Some(view) = self.view.as_mut() {
                view.restart_stream(title);
            }
            return;
        };
        if let Some(mut old) = self.view.take() {
            old.close(WindowCloseReason::Returned);
        }
        let meta = WindowMeta {
            id: SCREEN_STREAM_ID,
            title: format!("{} · {title}", view_title(&self.peer_name)),
            width,
            height,
        };
        let opened = renderer.open_screen_view(&meta).and_then(|view| {
            view.node_id()
                .map(|node_id| (view, node_id))
                .ok_or(WindowError::Unsupported)
        });
        match opened {
            Ok((view, node_id)) => {
                let kind = self
                    .sources
                    .iter()
                    .find(|s| s.id == source)
                    .map_or(SourceKind::Monitor, |s| s.kind);
                let (ended_tx, ended_rx) = oneshot::channel();
                let shown = Shown {
                    node_id,
                    width,
                    height,
                    kind,
                    token: pending.token,
                    ended: ended_rx,
                };
                if pending.reply.send(Ok(shown)).is_err() {
                    // The app gave up while the host was opening.
                    self.outbox.push(WindowMsg::StopSource);
                    return;
                }
                self.view = Some(view);
                self.active = Some(ActiveShow {
                    token: pending.token,
                    ended: ended_tx,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, peer = %self.peer_name, "could not publish the peer's screen here");
                let _ = pending.reply.send(Err(e.to_string()));
                self.outbox.push(WindowMsg::StopSource);
            }
        }
    }

    /// End the capture app's showing: its node goes, and the app is told.
    fn end_active(&mut self) {
        if let Some(mut view) = self.view.take() {
            view.close(WindowCloseReason::HostClosed);
        }
        if let Some(active) = self.active.take() {
            let _ = active.ended.send(());
        }
    }

    /// Act on what capture apps asked of this peer since the last pump.
    fn apply_demand(&mut self) {
        let Some((hub, key)) = self.demand.as_ref() else {
            return;
        };
        for request in hub.take_requests(key) {
            match request {
                HubRequest::Show {
                    source,
                    token,
                    reply,
                } => {
                    // One stream per direction per link: a newer request
                    // takes it over, and whoever held it is told so.
                    if let Some(old) = self.pending.take() {
                        let _ = old.reply.send(Err("replaced by a newer request".into()));
                    }
                    self.end_active();
                    self.pending = Some(PendingShow {
                        token,
                        source,
                        reply,
                    });
                    self.request(source);
                }
                HubRequest::Stop { token } => {
                    if self.active.as_ref().is_some_and(|a| a.token == token) {
                        self.end_active();
                        self.outbox.push(WindowMsg::StopSource);
                    } else if self.pending.as_ref().is_some_and(|p| p.token == token) {
                        self.pending = None;
                        self.outbox.push(WindowMsg::StopSource);
                    }
                }
            }
        }
    }

    /// Take a message meant for the viewer's half. Returns whether it was one.
    ///
    /// Needs the renderer because the view opens on the first source list:
    /// a host that refuses, or has nothing, should not leave an empty window.
    pub fn handle<R>(&mut self, msg: &WindowMsg, renderer: &mut R) -> bool
    where
        R: WindowRenderer<Surrogate = S>,
    {
        if self.demand.is_some() {
            return self.handle_on_demand(msg, renderer);
        }
        match msg {
            WindowMsg::Sources(list) => {
                self.sources = list.clone();
                self.ensure_view(renderer);
                if let Some(view) = self.view.as_mut() {
                    view.set_sources(list);
                }
                if !self.auto_picked && self.view.is_some() {
                    self.auto_picked = true;
                    if let Some(first) = default_source(list) {
                        self.request(first);
                    }
                }
            }
            WindowMsg::SourcesRefused { reason } => {
                tracing::info!(peer = %self.peer_name, %reason, "peer does not share its screen");
            }
            WindowMsg::SourceShown { source, title, .. } => {
                self.showing = Some(*source);
                self.ask_for_keyframe();
                if let Some(view) = self.view.as_mut() {
                    view.restart_stream(title);
                }
            }
            WindowMsg::SourceEnded { reason } => {
                tracing::info!(peer = %self.peer_name, %reason, "remote-screen source ended");
                self.showing = None;
                if let Some(view) = self.view.as_mut() {
                    view.restart_stream("");
                }
            }
            _ => return false,
        }
        true
    }

    fn handle_on_demand<R>(&mut self, msg: &WindowMsg, renderer: &mut R) -> bool
    where
        R: WindowRenderer<Surrogate = S>,
    {
        let Some((hub, key)) = self.demand.clone() else {
            return false;
        };
        match msg {
            WindowMsg::Sources(list) => {
                self.sources = list.clone();
                hub.publish(&key, &self.peer_name, list);
            }
            WindowMsg::SourcesRefused { reason } => {
                tracing::info!(peer = %self.peer_name, %reason, "peer does not share its screen");
                hub.withdraw(&key);
            }
            WindowMsg::SourceShown {
                source,
                title,
                width,
                height,
            } => {
                self.showing = Some(*source);
                self.ask_for_keyframe();
                self.fulfil(*source, title, *width, *height, renderer);
            }
            WindowMsg::SourceEnded { reason } => {
                tracing::info!(peer = %self.peer_name, %reason, "remote-screen source ended");
                self.showing = None;
                if let Some(pending) = self.pending.take() {
                    let _ = pending.reply.send(Err(reason.clone()));
                }
                self.end_active();
            }
            _ => return false,
        }
        true
    }

    /// A frame of the peer's screen, for the view.
    pub fn apply_frame(&mut self, frame: &WindowFrame) {
        if let Some(view) = self.view.as_mut() {
            if let Err(e) = view.apply_frame(frame) {
                tracing::warn!(error = %e, seq = frame.seq, "dropping unusable remote-screen frame");
            }
        }
    }

    fn ensure_view<R>(&mut self, renderer: &mut R)
    where
        R: WindowRenderer<Surrogate = S>,
    {
        if self.view.is_some() || self.dismissed {
            return;
        }
        let meta = WindowMeta {
            id: SCREEN_STREAM_ID,
            title: view_title(&self.peer_name),
            width: VIEW_SIZE.0,
            height: VIEW_SIZE.1,
        };
        match renderer.open_screen_view(&meta) {
            Ok(view) => {
                tracing::info!(title = %meta.title, "opened remote-screen view");
                self.view = Some(view);
            }
            Err(e) => {
                // Once per link: the list refreshes every few seconds, and a
                // platform with no viewer will not grow one in between.
                self.dismissed = true;
                tracing::warn!(error = %e, peer = %self.peer_name, "could not open a remote-screen view");
            }
        }
    }

    /// A new stream starts here: ask for its whole picture at once, rather
    /// than trusting that its first frame is one and arrives whole.
    fn ask_for_keyframe(&mut self) {
        self.keyframe_asks.note_sent(Instant::now());
        self.outbox.push(WindowMsg::KeyframeRequest {
            id: SCREEN_STREAM_ID,
        });
    }

    fn request(&mut self, source: SourceId) {
        self.wanted = Some(source);
        self.outbox.push(WindowMsg::ShowSource { source });
    }

    /// Next or previous source, wrapping, among those that need no picker.
    ///
    /// Picker entries are left out of cycling: stepping through the list with
    /// a key must not pop a dialog up on the other machine each time. They
    /// stay reachable from the menu, where choosing one is deliberate.
    fn step(&self, forward: bool) -> Option<SourceId> {
        let direct: Vec<SourceId> = self
            .sources
            .iter()
            .filter(|s| !s.picked_on_host)
            .map(|s| s.id)
            .collect();
        if direct.is_empty() {
            return None;
        }
        let at = self
            .wanted
            .or(self.showing)
            .and_then(|id| direct.iter().position(|&d| d == id));
        let len = direct.len();
        let next = match (at, forward) {
            (None, _) => 0,
            (Some(i), true) => (i + 1) % len,
            (Some(i), false) => (i + len - 1) % len,
        };
        Some(direct[next])
    }

    /// Subscribe once, act on the user's picks, and forward the view's acks.
    pub async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if !self.subscribed {
            self.subscribed = true;
            send_control(tx, WindowMsg::ListSources).await?;
        }
        self.apply_demand();

        let mut closed = false;
        let mut picks = Vec::new();
        let mut ack = None;
        if let Some(view) = self.view.as_mut() {
            // View-only: whatever arrives is discarded, and the channel is
            // drained only to learn whether the user closed the window.
            loop {
                match view.input().try_recv() {
                    Ok(_) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
            while let Some(pick) = view.take_source_pick() {
                picks.push(pick);
            }
            ack = view.take_ack();
            if self
                .keyframe_asks
                .poll(std::time::Instant::now(), || view.take_keyframe_request())
            {
                self.outbox.push(WindowMsg::KeyframeRequest {
                    id: SCREEN_STREAM_ID,
                });
            }
        }
        for pick in picks {
            match pick {
                SourcePick::Source(id) => self.request(id),
                SourcePick::Next | SourcePick::Previous => {
                    match self.step(matches!(pick, SourcePick::Next)) {
                        Some(id) => self.request(id),
                        None => tracing::info!(
                            peer = %self.peer_name,
                            "nothing to step to; the peer only offers its own picker (right-click)"
                        ),
                    }
                }
                SourcePick::Stop => {
                    self.wanted = None;
                    self.outbox.push(WindowMsg::StopSource);
                }
            }
        }
        if closed {
            tracing::info!(peer = %self.peer_name, "remote-screen view closed by the user");
            self.view = None;
            self.dismissed = true;
            self.showing = None;
            self.wanted = None;
            self.outbox.push(WindowMsg::StopSource);
        }
        if let Some(ack) = ack {
            self.outbox.push(WindowMsg::FrameAck(ack));
        }
        for msg in std::mem::take(&mut self.outbox) {
            send_control(tx, msg).await?;
        }
        Ok(())
    }

    /// Close the view; the link is going away.
    pub fn shutdown(&mut self) {
        if let Some(pending) = self.pending.take() {
            let _ = pending.reply.send(Err("the link to the peer ended".into()));
        }
        self.end_active();
        if let Some((hub, key)) = self.demand.as_ref() {
            hub.withdraw(key);
        }
    }

    pub fn view(&self) -> Option<&S> {
        self.view.as_ref()
    }

    pub fn view_mut(&mut self) -> Option<&mut S> {
        self.view.as_mut()
    }
}

/// What a new view shows without being asked: the first whole monitor that
/// needs no picker, which is what "show me that machine" means before the user
/// has said anything more specific. A host that only offers pickers gets no
/// default — opening a dialog on the other machine unasked would be rude.
fn default_source(list: &[ScreenSource]) -> Option<SourceId> {
    list.iter()
        .find(|s| s.kind == SourceKind::Monitor && !s.picked_on_host)
        .map(|s| s.id)
}

/// The view's window title, which is what a screen-sharing picker lists.
///
/// Fixed per peer, never the current source: the application lists windows by
/// title, and one that renamed itself on every switch would be hard to find
/// again in that list.
pub fn view_title(peer_name: &str) -> String {
    format!("Cursio — {peer_name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cursio_protocol::{Rect, TileCodec};
    use cursio_transport::QueueDepth;
    use cursio_window::window_test_support::{FakeRenderer, FakeScreenSources, FakeSurrogate};

    #[derive(Default)]
    struct RecordingTx {
        sent: Vec<Message>,
    }

    #[async_trait]
    impl TransportTx for RecordingTx {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            self.sent.push(msg.clone());
            Ok(())
        }

        fn queue_depth(&self, _priority: Priority) -> QueueDepth {
            QueueDepth::default()
        }
    }

    impl RecordingTx {
        fn control(&self) -> Vec<WindowMsg> {
            self.sent
                .iter()
                .filter_map(|m| m.decode_module::<WindowMsg>(ModuleId::WINDOW).unwrap())
                .collect()
        }
    }

    fn source(id: SourceId, kind: SourceKind, title: &str, picked_on_host: bool) -> ScreenSource {
        ScreenSource {
            id,
            kind,
            title: title.into(),
            width: 800,
            height: 600,
            picked_on_host,
        }
    }

    fn lightmoon() -> FakeScreenSources {
        FakeScreenSources::new(vec![
            source(1, SourceKind::Window, "Firefox", false),
            source(0x8000_0000, SourceKind::Monitor, "Screen 1", false),
            source(2, SourceKind::Window, "Terminal", false),
        ])
    }

    fn frame(seq: u32) -> WindowFrame {
        WindowFrame {
            id: SCREEN_STREAM_ID,
            seq,
            dirty: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            codec: TileCodec::RawBgra8888,
            data: vec![0; 4],
        }
    }

    /// A host and a viewer joined back to back, standing in for two machines.
    struct Pair {
        host: ScreenHost,
        viewer: ScreenViewer<FakeSurrogate>,
        renderer: FakeRenderer,
    }

    impl Pair {
        fn new(sources: &FakeScreenSources) -> Self {
            Self {
                host: ScreenHost::serving(Arc::new(sources.clone())),
                viewer: ScreenViewer::new("Lightmoon".into()),
                renderer: FakeRenderer::default(),
            }
        }

        /// One round trip: the viewer pumps, the host takes what it sent and
        /// pumps, the viewer takes the host's answer. Returns the host's traffic.
        async fn exchange(&mut self) -> RecordingTx {
            let mut up = RecordingTx::default();
            self.viewer.pump(&mut up).await.unwrap();
            for msg in up.control() {
                self.host.handle(&msg);
            }
            let mut down = RecordingTx::default();
            self.host.pump(&mut down).await.unwrap();
            for msg in &down.sent {
                match msg {
                    Message::WindowFrame(f) => self.viewer.apply_frame(f),
                    other => {
                        if let Some(control) =
                            other.decode_module::<WindowMsg>(ModuleId::WINDOW).unwrap()
                        {
                            self.viewer.handle(&control, &mut self.renderer);
                        }
                    }
                }
            }
            down
        }

        /// Exchange until the host has nothing listing or opening, since both
        /// run on threads of their own.
        async fn settle(&mut self) -> Vec<WindowMsg> {
            let mut seen = Vec::new();
            for _ in 0..400 {
                seen.extend(self.exchange().await.control());
                if host_idle(&self.host) && self.viewer.outbox.is_empty() {
                    let extra = self.exchange().await.control();
                    seen.extend(extra);
                    if host_idle(&self.host) {
                        return seen;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("the host never finished listing or opening a source");
        }
    }

    fn host_idle(host: &ScreenHost) -> bool {
        host.opening.is_none() && host.listing.is_none()
    }

    /// Pump a lone host until its listing and opening threads have answered.
    async fn pump_settled(host: &mut ScreenHost, tx: &mut RecordingTx) {
        for _ in 0..400 {
            host.pump(tx).await.unwrap();
            if host_idle(host) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the host never finished listing or opening a source");
    }

    #[tokio::test]
    async fn the_view_opens_on_the_first_list_and_shows_the_first_monitor() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);

        pair.settle().await;

        assert_eq!(pair.renderer.views_opened, vec!["Cursio — Lightmoon"]);
        assert_eq!(pair.host.showing(), Some(0x8000_0000));
        let view = pair.viewer.view().unwrap();
        assert_eq!(view.sources.len(), 3);
        assert_eq!(view.restarts, vec!["Screen 1"]);
    }

    #[tokio::test]
    async fn the_list_is_resent_only_when_it_changes() {
        let sources = lightmoon();
        let mut host = ScreenHost::serving(Arc::new(sources.clone()));
        host.handle(&WindowMsg::ListSources);

        let mut tx = RecordingTx::default();
        pump_settled(&mut host, &mut tx).await;
        host.next_poll = Instant::now();
        pump_settled(&mut host, &mut tx).await;
        let lists = |tx: &RecordingTx| {
            tx.control()
                .iter()
                .filter(|m| matches!(m, WindowMsg::Sources(_)))
                .count()
        };
        assert_eq!(lists(&tx), 1, "an unchanged list must not be resent");

        sources.set_sources(vec![source(9, SourceKind::Window, "Discord", false)]);
        host.next_poll = Instant::now();
        pump_settled(&mut host, &mut tx).await;
        assert_eq!(lists(&tx), 2, "a window opening must reach the viewer");
    }

    /// Enumerating sources never holds up the loop the cursor rides on.
    ///
    /// On Windows a window's title is answered by the thread that owns the
    /// window, and a view's own window thread is busy decoding: listing on the
    /// session loop stalled it 50–220 ms every two seconds while a view ran.
    #[tokio::test]
    async fn a_slow_source_list_does_not_hold_up_the_pump() {
        let sources = lightmoon();
        let mut host = ScreenHost::serving(Arc::new(sources.clone()));
        host.handle(&WindowMsg::ListSources);
        sources.set_list_stalled(true);

        let mut tx = RecordingTx::default();
        let started = std::time::Instant::now();
        for _ in 0..3 {
            host.pump(&mut tx).await.unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "the pump waited on the enumeration"
        );
        assert!(tx.control().is_empty());

        sources.set_list_stalled(false);
        pump_settled(&mut host, &mut tx).await;
        assert!(matches!(
            tx.control().as_slice(),
            [WindowMsg::Sources(list)] if list.len() == 3
        ));
    }

    #[tokio::test]
    async fn a_host_that_opted_out_refuses_with_a_reason_and_no_view_opens() {
        let mut host = ScreenHost::refusing("started with --no-screen-host");
        let mut viewer: ScreenViewer<FakeSurrogate> = ScreenViewer::new("Nocturne".into());
        let mut renderer = FakeRenderer::default();

        let mut up = RecordingTx::default();
        viewer.pump(&mut up).await.unwrap();
        for msg in up.control() {
            host.handle(&msg);
        }
        let mut down = RecordingTx::default();
        host.pump(&mut down).await.unwrap();
        let answers = down.control();
        assert!(matches!(
            answers.as_slice(),
            [WindowMsg::SourcesRefused { reason }] if reason.contains("--no-screen-host")
        ));
        for msg in &answers {
            viewer.handle(msg, &mut renderer);
        }
        assert!(renderer.views_opened.is_empty());
    }

    #[tokio::test]
    async fn switching_source_keeps_the_view_and_restarts_its_stream() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        let monitor_frames = sources.frames_for(0x8000_0000).unwrap();

        pair.viewer
            .view_mut()
            .unwrap()
            .picks
            .push_back(SourcePick::Source(2));
        let traffic = pair.settle().await;

        assert_eq!(pair.host.showing(), Some(2));
        assert_eq!(
            pair.renderer.views_opened.len(),
            1,
            "a switch must never recreate the window a screen share is capturing"
        );
        assert_eq!(
            pair.viewer.view().unwrap().restarts,
            vec!["Screen 1", "Terminal"]
        );
        assert!(traffic
            .iter()
            .any(|m| matches!(m, WindowMsg::SourceShown { source: 2, .. })));
        assert!(
            monitor_frames.is_closed(),
            "the previous source's capture must stop once the new one is up"
        );
    }

    #[tokio::test]
    async fn frames_after_a_switch_reach_the_same_view() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        sources
            .frames_for(0x8000_0000)
            .unwrap()
            .try_send(frame(1))
            .unwrap();
        pair.exchange().await;

        pair.viewer
            .view_mut()
            .unwrap()
            .picks
            .push_back(SourcePick::Source(1));
        pair.settle().await;
        sources.frames_for(1).unwrap().try_send(frame(1)).unwrap();
        pair.exchange().await;

        assert_eq!(pair.viewer.view().unwrap().applied.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_open_says_why_and_keeps_showing_the_old_source() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        sources.fail_next("the user cancelled the dialog");
        pair.viewer
            .view_mut()
            .unwrap()
            .picks
            .push_back(SourcePick::Source(1));
        let traffic = pair.settle().await;

        assert!(traffic.iter().any(|m| matches!(
            m,
            WindowMsg::SourceEnded { reason } if reason.contains("cancelled")
        )));
        assert_eq!(pair.host.showing(), Some(0x8000_0000));
    }

    #[tokio::test]
    async fn stopping_ends_the_stream_but_keeps_the_view() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        pair.viewer
            .view_mut()
            .unwrap()
            .picks
            .push_back(SourcePick::Stop);
        pair.settle().await;

        assert_eq!(pair.host.showing(), None);
        let view = pair.viewer.view().unwrap();
        assert!(view.closed.is_none());
        assert_eq!(view.restarts.last().map(String::as_str), Some(""));
    }

    #[tokio::test]
    async fn closing_the_view_stops_the_host_and_it_stays_closed() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        pair.viewer.view_mut().unwrap().user_closes();
        pair.settle().await;
        pair.host.next_poll = Instant::now();
        sources.set_sources(vec![source(5, SourceKind::Window, "New", false)]);
        pair.settle().await;

        assert_eq!(pair.host.showing(), None);
        assert!(pair.viewer.view().is_none());
        assert_eq!(
            pair.renderer.views_opened.len(),
            1,
            "a list refresh must not reopen a view the user closed"
        );
    }

    #[tokio::test]
    async fn stepping_skips_sources_that_open_a_picker_on_the_host() {
        let sources = FakeScreenSources::new(vec![
            source(1, SourceKind::Window, "Choose a window…", true),
            source(0x8000_0000, SourceKind::Monitor, "Screen 1", false),
            source(0x8000_0001, SourceKind::Monitor, "Screen 2", false),
        ]);
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        for _ in 0..2 {
            pair.viewer
                .view_mut()
                .unwrap()
                .picks
                .push_back(SourcePick::Next);
            pair.settle().await;
        }

        assert_eq!(
            sources.opened(),
            vec![0x8000_0000, 0x8000_0001, 0x8000_0000]
        );
    }

    #[tokio::test]
    async fn a_host_offering_only_pickers_shows_nothing_until_asked() {
        let sources = FakeScreenSources::new(vec![
            source(1, SourceKind::Window, "Choose a window…", true),
            source(2, SourceKind::Monitor, "Choose a screen…", true),
        ]);
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        assert_eq!(pair.renderer.views_opened.len(), 1);
        assert!(
            sources.opened().is_empty(),
            "a dialog must not pop up on the other machine unasked"
        );
    }

    #[tokio::test]
    async fn a_source_gone_since_the_list_was_sent_is_refused_by_name() {
        let sources = lightmoon();
        let mut host = ScreenHost::serving(Arc::new(sources.clone()));
        sources.set_sources(Vec::new());

        host.handle(&WindowMsg::ShowSource { source: 1 });
        let mut tx = RecordingTx::default();
        pump_settled(&mut host, &mut tx).await;

        assert!(matches!(
            tx.control().as_slice(),
            [WindowMsg::SourceEnded { reason }] if reason.contains("no longer exists")
        ));
    }

    fn on_demand(sources: &FakeScreenSources) -> (Pair, ScreenHub) {
        let hub = ScreenHub::default();
        let mut pair = Pair::new(sources);
        pair.viewer = ScreenViewer::new("Lightmoon".into()).on_demand(hub.clone(), "lm".into());
        (pair, hub)
    }

    #[tokio::test]
    async fn on_demand_the_peers_sources_reach_the_hub_and_nothing_opens_unasked() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);

        pair.settle().await;

        assert_eq!(hub.peers().len(), 1);
        assert_eq!(hub.peers()[0].2.len(), 3);
        assert!(pair.renderer.views_opened.is_empty());
        assert!(
            sources.opened().is_empty(),
            "no capture may start until a capture app asks"
        );
    }

    #[tokio::test]
    async fn a_capture_apps_request_publishes_a_node_once_the_host_shows_the_source() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;

        let answer = hub.show("lm", 2);
        pair.settle().await;

        let shown = answer.await.unwrap().expect("the source is shown");
        assert_eq!(shown.node_id, 100);
        assert_eq!((shown.width, shown.height), (800, 600));
        assert_eq!(shown.kind, SourceKind::Window);
        assert_eq!(pair.host.showing(), Some(2));
        assert_eq!(
            pair.renderer.views_opened,
            ["Cursio — Lightmoon · Terminal"]
        );
    }

    #[tokio::test]
    async fn the_capture_app_stopping_ends_the_node_and_the_hosts_capture() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;
        let shown = {
            let answer = hub.show("lm", 1);
            pair.settle().await;
            answer.await.unwrap().unwrap()
        };

        hub.stop("lm", shown.token);
        pair.settle().await;

        assert_eq!(pair.host.showing(), None);
        assert!(pair.viewer.view().is_none());
    }

    #[tokio::test]
    async fn a_newer_request_takes_the_stream_and_the_old_holder_is_told() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;
        let first = {
            let answer = hub.show("lm", 1);
            pair.settle().await;
            answer.await.unwrap().unwrap()
        };

        let second = hub.show("lm", 0x8000_0000);
        pair.settle().await;

        assert!(
            first.ended.await.is_ok(),
            "the first app must learn its share ended"
        );
        let second = second.await.unwrap().unwrap();
        assert_eq!(second.kind, SourceKind::Monitor);
        assert_eq!(pair.host.showing(), Some(0x8000_0000));
    }

    #[tokio::test]
    async fn a_source_the_host_cannot_open_fails_the_request_with_its_reason() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;

        sources.fail_next("the user cancelled the dialog");
        let answer = hub.show("lm", 1);
        pair.settle().await;

        let err = answer.await.unwrap().unwrap_err();
        assert!(err.contains("cancelled"));
        assert!(pair.renderer.views_opened.is_empty());
    }

    #[tokio::test]
    async fn the_link_ending_withdraws_the_peer_and_ends_its_share() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;
        let shown = {
            let answer = hub.show("lm", 1);
            pair.settle().await;
            answer.await.unwrap().unwrap()
        };

        pair.viewer.shutdown();

        assert!(shown.ended.await.is_ok());
        assert!(hub.peers().is_empty());
    }

    #[tokio::test]
    async fn the_link_ending_closes_the_view_and_stops_the_capture() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        let frames = sources.frames_for(0x8000_0000).unwrap();

        pair.host.shutdown();
        pair.viewer.shutdown();

        assert!(frames.is_closed());
        assert!(pair.viewer.view().is_none());
    }

    #[tokio::test]
    async fn a_shown_source_asks_its_capture_for_a_whole_picture() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        pair.exchange().await;
        // Asked on `SourceShown`, before any frame could say whether the first
        // one arrived whole.
        assert_eq!(sources.keyframe_requests(), 1);
    }

    #[tokio::test]
    async fn a_view_stuck_without_a_keyframe_asks_at_most_once_a_second() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        pair.exchange().await;
        let before = sources.keyframe_requests();

        // The decoder raises its want for every packet it drops.
        for _ in 0..20 {
            pair.viewer.view_mut().unwrap().wants_keyframe = true;
            pair.exchange().await;
        }
        assert_eq!(
            sources.keyframe_requests(),
            before,
            "the ask on SourceShown was just now; the next waits its second"
        );
        assert!(
            pair.viewer.view_mut().unwrap().wants_keyframe,
            "a want raised while waiting is kept for when a request may go"
        );

        pair.viewer.keyframe_asks = KeyframeRequests::default();
        pair.exchange().await;
        assert_eq!(sources.keyframe_requests(), before + 1);
    }
}
