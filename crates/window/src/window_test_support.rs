//! In-memory [`WindowCapture`] / [`WindowRenderer`] backends for tests.
//!
//! The session-level logic — offer before frames, bounded pumping, who claims
//! which message, teardown on either side — is the part most worth testing, and
//! none of it should require a compositor. The real backends do: the host one
//! opens a portal dialog, the viewer one opens a GTK window, neither of which
//! exists in CI or in a `cargo test` run.
//!
//! So these fakes stand in. They implement the same traits over plain channels,
//! letting a test drive the exchange deterministically: push a frame, pump, and
//! assert on what went out. Shipped in the crate (not behind `#[cfg(test)]`)
//! because the tests that need them live in `cursio-core`, a different crate.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use cursio_protocol::{
    ScreenSource, SourceId, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput,
    WindowMeta,
};

use crate::{
    CaptureSession, ScreenSources, SourcePick, Surrogate, WindowCapture, WindowError,
    WindowRenderer,
};

/// A capture whose frames a test supplies over a channel.
pub struct FakeCaptureSession {
    meta: WindowMeta,
    frames: mpsc::Receiver<WindowFrame>,
    /// Input the host was asked to replay, for assertions.
    pub applied_input: Vec<WindowInput>,
    /// Acks fed back to the encoder, for assertions.
    pub acks: Vec<WindowFrameAck>,
    /// Set once `close` has been called.
    pub closed: Option<WindowCloseReason>,
}

impl CaptureSession for FakeCaptureSession {
    fn meta(&self) -> &WindowMeta {
        &self.meta
    }

    fn frames(&mut self) -> &mut mpsc::Receiver<WindowFrame> {
        &mut self.frames
    }

    fn apply_input(&mut self, input: WindowInput) -> Result<(), WindowError> {
        self.applied_input.push(input);
        Ok(())
    }

    fn on_ack(&mut self, ack: WindowFrameAck) {
        self.acks.push(ack);
    }

    fn close(&mut self, reason: WindowCloseReason) {
        self.closed = Some(reason);
    }
}

/// Hands out a [`FakeCaptureSession`] fed by a test-owned channel.
pub struct FakeCapture {
    frames: Option<mpsc::Receiver<WindowFrame>>,
    /// Senders for the extra sessions, kept alive so their receivers stay open.
    spare_senders: Vec<mpsc::Sender<WindowFrame>>,
    /// The title `aim_at` last named, mirroring a real backend: the session
    /// dedupes by title, so a fake that ignored the aim would report every
    /// window as the same one.
    aimed_title: Option<String>,
    width: u32,
    height: u32,
}

impl FakeCapture {
    /// Frames sent on `frames` appear as if captured, at the given geometry.
    pub fn new(frames: mpsc::Receiver<WindowFrame>, width: u32, height: u32) -> Self {
        Self {
            frames: Some(frames),
            spare_senders: Vec::new(),
            aimed_title: None,
            width,
            height,
        }
    }
}

impl WindowCapture for FakeCapture {
    type Session = FakeCaptureSession;

    fn aim_at(&mut self, title: &str) {
        self.aimed_title = Some(title.to_string());
    }

    fn begin(&mut self, id: WindowId) -> Result<Self::Session, WindowError> {
        // The first session gets the test's channel; later ones get a channel
        // of their own that never yields. A host can have several windows open
        // at once, and a fake that refused the second could only test the
        // single-window case it was written for — but a test still drives
        // frames through one known channel, so that one keeps it.
        let frames = self.frames.take().unwrap_or_else(|| {
            let (tx, rx) = mpsc::channel(1);
            // Held so the receiver reads as open (empty) rather than closed,
            // which the engine treats as the capture having ended.
            self.spare_senders.push(tx);
            rx
        });
        Ok(FakeCaptureSession {
            meta: WindowMeta {
                id,
                title: self
                    .aimed_title
                    .clone()
                    .unwrap_or_else(|| "fake window".into()),
                width: self.width,
                height: self.height,
            },
            frames,
            applied_input: Vec::new(),
            acks: Vec::new(),
            closed: None,
        })
    }
}

/// A surrogate that records what it was shown instead of drawing it.
pub struct FakeSurrogate {
    /// Frames handed to `apply_frame`, for assertions.
    pub applied: Vec<WindowFrame>,
    input: mpsc::UnboundedReceiver<WindowInput>,
    /// Kept alive so `input()` reports `Empty` rather than `Disconnected` —
    /// a dropped sender would look to the session like the user closed the
    /// window. [`user_closes`](Self::user_closes) drops it on purpose.
    input_tx: Option<mpsc::UnboundedSender<WindowInput>>,
    pub closed: Option<WindowCloseReason>,
    /// Titles passed to `restart_stream`, in order.
    pub restarts: Vec<String>,
    /// The source list last handed to the selector.
    pub sources: Vec<ScreenSource>,
    /// Picks the test makes on the user's behalf, taken front first.
    pub picks: VecDeque<SourcePick>,
}

impl FakeSurrogate {
    /// Behave as if the user closed the window.
    pub fn user_closes(&mut self) {
        self.input_tx = None;
    }
}

impl Surrogate for FakeSurrogate {
    fn restart_stream(&mut self, title: &str) {
        self.restarts.push(title.to_string());
    }

    fn set_sources(&mut self, sources: &[ScreenSource]) {
        self.sources = sources.to_vec();
    }

    fn take_source_pick(&mut self) -> Option<SourcePick> {
        self.picks.pop_front()
    }

    fn apply_frame(&mut self, frame: &WindowFrame) -> Result<(), WindowError> {
        self.applied.push(frame.clone());
        Ok(())
    }

    fn input(&mut self) -> &mut mpsc::UnboundedReceiver<WindowInput> {
        &mut self.input
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        None
    }

    fn close(&mut self, reason: WindowCloseReason) {
        self.closed = Some(reason);
    }
}

/// Opens [`FakeSurrogate`]s, or fails on demand to exercise the error path.
#[derive(Default)]
pub struct FakeRenderer {
    fail: bool,
    /// Titles of every remote-screen view opened, so a test can tell a view
    /// that was kept from one that was recreated.
    pub views_opened: Vec<String>,
}

impl FakeRenderer {
    /// A renderer that refuses to open, as a real one would with no display.
    pub fn failing() -> Self {
        Self {
            fail: true,
            views_opened: Vec::new(),
        }
    }
}

impl WindowRenderer for FakeRenderer {
    type Surrogate = FakeSurrogate;

    fn open(&mut self, _meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        if self.fail {
            return Err(WindowError::Backend("fake renderer refuses".into()));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(FakeSurrogate {
            applied: Vec::new(),
            input: rx,
            input_tx: Some(tx),
            closed: None,
            restarts: Vec::new(),
            sources: Vec::new(),
            picks: VecDeque::new(),
        })
    }

    fn open_screen_view(&mut self, meta: &WindowMeta) -> Result<Self::Surrogate, WindowError> {
        let view = self.open(meta)?;
        self.views_opened.push(meta.title.clone());
        Ok(view)
    }
}

#[derive(Default)]
struct FakeSourcesState {
    sources: Vec<ScreenSource>,
    /// The frame sender behind each source opened, newest open wins.
    senders: HashMap<SourceId, mpsc::Sender<WindowFrame>>,
    /// Every `open`, in order.
    opened: Vec<SourceId>,
    /// When set, the next `open` fails with this reason.
    fail_next: Option<String>,
}

/// A host's source list and captures, driven by a test.
///
/// Shared behind a lock because the engine opens sources on a thread of its
/// own, exactly as it does with a real backend.
#[derive(Clone, Default)]
pub struct FakeScreenSources {
    state: Arc<Mutex<FakeSourcesState>>,
}

impl FakeScreenSources {
    pub fn new(sources: Vec<ScreenSource>) -> Self {
        let fake = Self::default();
        fake.set_sources(sources);
        fake
    }

    /// Replace what `list` reports, as windows opening and closing would.
    pub fn set_sources(&self, sources: Vec<ScreenSource>) {
        self.lock().sources = sources;
    }

    /// The channel that feeds frames into the capture of `source`, once opened.
    pub fn frames_for(&self, source: SourceId) -> Option<mpsc::Sender<WindowFrame>> {
        self.lock().senders.get(&source).cloned()
    }

    /// Every source opened so far, in order.
    pub fn opened(&self) -> Vec<SourceId> {
        self.lock().opened.clone()
    }

    /// Make the next `open` fail, as a declined picker would.
    pub fn fail_next(&self, reason: &str) {
        self.lock().fail_next = Some(reason.to_string());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeSourcesState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ScreenSources for FakeScreenSources {
    fn list(&self) -> Result<Vec<ScreenSource>, WindowError> {
        Ok(self.lock().sources.clone())
    }

    fn open(
        &self,
        id: WindowId,
        source: &ScreenSource,
    ) -> Result<Box<dyn CaptureSession + Send>, WindowError> {
        let mut state = self.lock();
        state.opened.push(source.id);
        if let Some(reason) = state.fail_next.take() {
            return Err(WindowError::Backend(reason));
        }
        let (tx, rx) = mpsc::channel(64);
        state.senders.insert(source.id, tx);
        Ok(Box::new(FakeCaptureSession {
            meta: WindowMeta {
                id,
                title: source.title.clone(),
                width: source.width,
                height: source.height,
            },
            frames: rx,
            applied_input: Vec::new(),
            acks: Vec::new(),
            closed: None,
        }))
    }
}
