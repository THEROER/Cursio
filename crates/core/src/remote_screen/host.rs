//! The host half: this machine's screens and windows listed for a peer, and
//! the one it asked for captured and streamed to it.

use crate::window_session::{send_control, MAX_FRAMES_PER_PUMP, VIDEO_QUEUE_HIGH_WATER_BYTES};
use cursio_protocol::{
    Message, Priority, ScreenSource, SourceId, VideoCaps, WindowCloseReason, WindowFrameAck,
    WindowMsg, SCREEN_STREAM_ID,
};
use cursio_transport::{TransportError, TransportTx};
use cursio_window::{CaptureSession, ScreenSources, WindowError};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often a subscribed host re-reads its source list.
///
/// A window list changes at human pace; two seconds keeps the viewer's menu
/// current without the enumeration showing up anywhere.
pub(super) const SOURCE_POLL: Duration = Duration::from_secs(2);

/// The source that opened and its capture, or why the viewer gets nothing.
pub(super) type OpenResult = Result<(ScreenSource, Box<dyn CaptureSession + Send>), String>;

pub(super) type ListResult = Result<Vec<ScreenSource>, WindowError>;

/// The host's half: publish the source list, stream the chosen source.
pub struct ScreenHost {
    /// `None` when this node does not serve views; `refusal` says why.
    sources: Option<Arc<dyn ScreenSources>>,
    refusal: String,
    subscribed: bool,
    refusal_sent: bool,
    /// What the viewer was last told, so an unchanged list is not resent.
    published: Option<Vec<ScreenSource>>,
    pub(super) next_poll: Instant,
    current: Option<(SourceId, Box<dyn CaptureSession + Send>)>,
    /// A source being resolved and opened on its own thread. At most one: a
    /// newer request replaces an older one still opening. The abandoned
    /// thread's send then fails and its capture is dropped with it.
    pub(super) opening: Option<std_mpsc::Receiver<OpenResult>>,
    /// A list being read on its own thread. Measured on Windows at 50–220 ms
    /// every poll while a view was streaming: a window's title is answered by
    /// the thread that owns the window, and this process's own view window is
    /// owned by a thread busy decoding the stream. The mouse rides the same
    /// loop. At most one in flight, so a list stuck on a hung application
    /// stops the refreshes instead of piling up threads behind it.
    pub(super) listing: Option<std_mpsc::Receiver<ListResult>>,
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
        // A phone's source spawns its forwarding task while opening; on a bare
        // thread that `tokio::spawn` panicked, the viewer heard only "the
        // capture thread died", and picking a phone never showed anything.
        let runtime = tokio::runtime::Handle::try_current().ok();
        let spawned = std::thread::Builder::new()
            .name("cursio-screen-open".into())
            .spawn(move || {
                let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
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
    pub(crate) fn has_capture(&self) -> bool {
        self.current.is_some()
    }

    pub(crate) fn set_network_budget(&mut self, bits_per_second: u32, congested: bool) {
        if let Some((_, session)) = &mut self.current {
            session.set_network_budget(bits_per_second, congested);
        }
    }

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
pub(super) fn open_source(sources: &dyn ScreenSources, id: SourceId) -> OpenResult {
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
