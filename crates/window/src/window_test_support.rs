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

use tokio::sync::mpsc;

use cursio_protocol::{
    WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput, WindowMeta,
};

use crate::{CaptureSession, Surrogate, WindowCapture, WindowError, WindowRenderer};

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
    /// window.
    _input_tx: mpsc::UnboundedSender<WindowInput>,
    pub closed: Option<WindowCloseReason>,
}

impl Surrogate for FakeSurrogate {
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
}

impl FakeRenderer {
    /// A renderer that refuses to open, as a real one would with no display.
    pub fn failing() -> Self {
        Self { fail: true }
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
            _input_tx: tx,
            closed: None,
        })
    }
}
