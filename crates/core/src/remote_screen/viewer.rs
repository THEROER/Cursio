//! The viewer half: a peer's sources asked for, shown in one unseen view, or
//! handed to a capture app on demand.

use crate::remote_screen::view_title;
use crate::screen_hub::{HubRequest, ScreenHub, Shown};
use crate::window_session::send_control;
use crate::window_session::KeyframeRequests;
use cursio_protocol::{
    ScreenSource, SourceId, SourceKind, WindowCloseReason, WindowFrame, WindowMeta, WindowMsg,
    SCREEN_STREAM_ID,
};
use cursio_transport::{TransportError, TransportTx};
use cursio_window::{SourcePick, Surrogate, WindowError, WindowRenderer};
use std::time::Instant;
use tokio::sync::oneshot;

/// Size a new view opens at, before any source has told it otherwise.
///
/// The view letterboxes rather than following its source, so this is only a
/// starting size the user is free to change.
pub(super) const VIEW_SIZE: (u32, u32) = (1280, 720);

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
    pub(super) outbox: Vec<WindowMsg>,
    /// Set where views are asked for by capture apps (Linux, through the
    /// portal) instead of kept open by the link: the hub, and this peer's
    /// key in it.
    demand: Option<(ScreenHub, String)>,
    /// A capture app's request waiting for the host's `SourceShown`.
    pending: Option<PendingShow>,
    /// The showing a capture app holds now.
    active: Option<ActiveShow>,
    pub(super) keyframe_asks: KeyframeRequests,
    /// What the Cursio app's tray picks for this view; see
    /// [`crate::screen_views`]. Set while the view is open.
    tray_picks: Option<tokio::sync::mpsc::UnboundedReceiver<crate::screen_views::TrayPick>>,
}

pub(super) struct PendingShow {
    token: u64,
    source: SourceId,
    reply: oneshot::Sender<Result<Shown, String>>,
}

pub(super) struct ActiveShow {
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
            tray_picks: None,
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
        if self.view.is_some() {
            crate::screen_views::update(&self.peer_name, &self.sources, self.showing);
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
                self.tray_picks = Some(crate::screen_views::open(&self.peer_name));
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
            if let Some(tray) = self.tray_picks.as_mut() {
                while let Ok(pick) = tray.try_recv() {
                    picks.push(pick.map_or(SourcePick::Stop, SourcePick::Source));
                }
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
            self.tray_picks = None;
            crate::screen_views::close(&self.peer_name);
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
        if self.tray_picks.take().is_some() {
            crate::screen_views::close(&self.peer_name);
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
pub(super) fn default_source(list: &[ScreenSource]) -> Option<SourceId> {
    list.iter()
        .find(|s| s.kind == SourceKind::Monitor && !s.picked_on_host)
        .map(|s| s.id)
}
