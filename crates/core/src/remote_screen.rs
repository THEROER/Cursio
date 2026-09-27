//! Remote-screen views: a window here that shows an app or a monitor of a peer.
//!
//! The use it exists for is screen-sharing software. Discord, OBS or a browser
//! call can only offer what this machine has, and a window is the one thing
//! every one of them lists on every platform. So the view is an ordinary,
//! visible top-level window titled `Cursio — <peer>`, and what it shows is one
//! source on the peer — any of its windows, or a whole monitor — chosen from a
//! selector that never draws into the picture.
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
//! ## One window across every switch
//!
//! Screen-sharing software captures a *window*; recreate the window and the
//! share ends. So a source switch never closes the view. The host swaps the
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

type OpenResult = Result<Box<dyn CaptureSession + Send>, WindowError>;

/// A source being opened on its own thread.
struct Opening {
    source: ScreenSource,
    result: std_mpsc::Receiver<OpenResult>,
}

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
    /// At most one: a newer request replaces an older one still opening. The
    /// abandoned thread's send then fails and its capture is dropped with it.
    opening: Option<Opening>,
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
        // Resolved against a fresh list, not the published one: the viewer's
        // menu may be a poll behind, and a window that closed since must fail
        // here with its name rather than inside a capture backend.
        let source = match sources.list() {
            Ok(list) => list.into_iter().find(|s| s.id == id),
            Err(e) => {
                self.outbox.push(WindowMsg::SourceEnded {
                    reason: format!("could not list sources: {e}"),
                });
                return;
            }
        };
        let Some(source) = source else {
            self.outbox.push(WindowMsg::SourceEnded {
                reason: format!("source {id} no longer exists on this machine"),
            });
            return;
        };
        if source.picked_on_host {
            tracing::info!(
                title = %source.title,
                "a peer asked to see this machine; pick what to show in the dialog here"
            );
        }
        let (tx, rx) = std_mpsc::channel();
        let thread_source = source.clone();
        let spawned = std::thread::Builder::new()
            .name("cursio-screen-open".into())
            .spawn(move || {
                let _ = tx.send(sources.open(SCREEN_STREAM_ID, &thread_source));
            });
        match spawned {
            Ok(_) => {
                self.opening = Some(Opening { source, result: rx });
            }
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
        let Some(sources) = self.sources.as_ref() else {
            if self.refusal_sent {
                return None;
            }
            self.refusal_sent = true;
            return Some(WindowMsg::SourcesRefused {
                reason: self.refusal.clone(),
            });
        };
        let now = Instant::now();
        if now < self.next_poll {
            return None;
        }
        self.next_poll = now + SOURCE_POLL;
        match sources.list() {
            Ok(list) if self.published.as_ref() != Some(&list) => {
                self.published = Some(list.clone());
                Some(WindowMsg::Sources(list))
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "could not list shareable sources for the peer");
                None
            }
        }
    }

    /// A source that finished opening, swapped in for the one on screen.
    fn poll_opening(&mut self) -> Option<WindowMsg> {
        let opening = self.opening.as_ref()?;
        let result = match opening.result.try_recv() {
            Ok(result) => result,
            Err(std_mpsc::TryRecvError::Empty) => return None,
            Err(std_mpsc::TryRecvError::Disconnected) => {
                Err(WindowError::Backend("the capture thread died".into()))
            }
        };
        let source = self.opening.take()?.source;
        match result {
            Ok(mut session) => {
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
            Err(e) => {
                tracing::warn!(error = %e, title = %source.title, "could not show source to the peer");
                Some(WindowMsg::SourceEnded {
                    reason: format!("{}: {e}", source.title),
                })
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
        if let Some(mut view) = self.view.take() {
            view.close(WindowCloseReason::HostClosed);
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

        /// Exchange until the host has nothing opening, since opening runs on
        /// a thread of its own.
        async fn settle(&mut self) -> Vec<WindowMsg> {
            let mut seen = Vec::new();
            for _ in 0..400 {
                seen.extend(self.exchange().await.control());
                if self.host.opening.is_none() && self.viewer.outbox.is_empty() {
                    let extra = self.exchange().await.control();
                    seen.extend(extra);
                    if self.host.opening.is_none() {
                        return seen;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("the host never finished opening a source");
        }
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
        host.pump(&mut tx).await.unwrap();
        host.next_poll = Instant::now();
        host.pump(&mut tx).await.unwrap();
        let lists = |tx: &RecordingTx| {
            tx.control()
                .iter()
                .filter(|m| matches!(m, WindowMsg::Sources(_)))
                .count()
        };
        assert_eq!(lists(&tx), 1, "an unchanged list must not be resent");

        sources.set_sources(vec![source(9, SourceKind::Window, "Discord", false)]);
        host.next_poll = Instant::now();
        host.pump(&mut tx).await.unwrap();
        assert_eq!(lists(&tx), 2, "a window opening must reach the viewer");
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
        host.pump(&mut tx).await.unwrap();

        assert!(matches!(
            tx.control().as_slice(),
            [WindowMsg::SourceEnded { reason }] if reason.contains("no longer exists")
        ));
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
}
