//! One computer's view of the phone's screen: the encoder's units forwarded as
//! the window's frames, its input turned to touch, and a bitrate that follows
//! how well that viewer keeps up.

use crate::phone_gesture::GestureMapper;
use crate::phone_screen::backend;
use crate::phone_screen::feed;
use crate::phone_screen::set_rate_target;
use crate::phone_screen::shared;
use crate::phone_screen::title;
use crate::phone_screen::viewer_closed;
use crate::phone_screen::Running;
use crate::phone_screen::Unit;
use cursio_protocol::{
    Rect, TileCodec, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput,
    WindowMeta,
};
use cursio_window::{CaptureSession, WindowCapture, WindowError, CAPTURE_FRAME_QUEUE_CAPACITY};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// The phone's screen as a window to transfer to one computer.
pub struct PhoneScreenCapture {
    computer: String,
    phone: String,
}

impl PhoneScreenCapture {
    pub fn new(computer: String, phone: String) -> Self {
        Self { computer, phone }
    }
}

impl WindowCapture for PhoneScreenCapture {
    type Session = PhoneScreenSession;

    fn begin(&mut self, id: WindowId) -> Result<Self::Session, WindowError> {
        let running = feed().running.ok_or_else(|| {
            WindowError::WindowGone("this phone's screen is not being captured".into())
        })?;
        Ok(PhoneScreenSession::open(
            id,
            title(&self.phone),
            running,
            Some(self.computer.clone()),
        ))
    }
}

/// One computer's view of the phone's screen.
pub struct PhoneScreenSession {
    meta: WindowMeta,
    frames: mpsc::Receiver<WindowFrame>,
    forward: tokio::task::JoinHandle<()>,
    mapper: GestureMapper,
    /// Whose window this is, so closing it there stops their share; `None`
    /// for a remote-screen view, which a capture app opens and closes.
    computer: Option<String>,
    bitrate: Bitrate,
    network_budget: u32,
    rate_id: u64,
    /// The seq of the newest frame handed to the session, for the gap the
    /// viewer's acks are measured against.
    sent: Arc<AtomicU32>,
}

impl PhoneScreenSession {
    pub(super) fn open(
        id: WindowId,
        title: String,
        running: Running,
        computer: Option<String>,
    ) -> Self {
        let (frames_tx, frames_rx) = mpsc::channel(CAPTURE_FRAME_QUEUE_CAPACITY);
        let units = shared().units.subscribe();
        let dirty = Rect {
            x: 0,
            y: 0,
            width: running.width,
            height: running.height,
        };
        let sent = Arc::new(AtomicU32::new(0));
        let forward = tokio::spawn(forward(units, frames_tx, id, dirty, sent.clone()));
        // A new viewer has nothing to decode from until the next keyframe,
        // which on a still screen would be never.
        if let Some(backend) = backend() {
            backend.keyframe();
        }
        let rate_id = {
            let mut feed = feed();
            feed.next_rate_id += 1;
            feed.next_rate_id
        };
        set_rate_target(rate_id, Some(BITRATE_START));
        Self {
            meta: WindowMeta {
                id,
                title,
                width: running.width,
                height: running.height,
            },
            frames: frames_rx,
            forward,
            mapper: GestureMapper::new(running.width, running.height),
            computer,
            bitrate: Bitrate::default(),
            network_budget: u32::MAX,
            rate_id,
            sent,
        }
    }
}

/// Stream units as frames from a keyframe on.
///
/// A window that falls behind skips to the next keyframe rather than
/// showing pictures built on ones it never got.
pub(super) async fn forward(
    mut units: broadcast::Receiver<Unit>,
    frames: mpsc::Sender<WindowFrame>,
    id: WindowId,
    dirty: Rect,
    sent: Arc<AtomicU32>,
) {
    let mut seq = 0_u32;
    let mut frame = |data: &[u8]| {
        let f = WindowFrame {
            id,
            seq,
            dirty,
            codec: TileCodec::LossyVideo,
            data: data.to_vec(),
        };
        sent.store(seq, Ordering::Relaxed);
        seq = seq.wrapping_add(1);
        f
    };
    let mut synced = false;
    loop {
        match units.recv().await {
            Ok(unit) => {
                synced |= unit.key;
                if !synced {
                    continue;
                }
                if frames.send(frame(&unit.data)).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::debug!(
                    skipped,
                    "a screen viewer fell behind; waiting for a keyframe"
                );
                synced = false;
                if let Some(backend) = backend() {
                    backend.keyframe();
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

impl CaptureSession for PhoneScreenSession {
    fn meta(&self) -> &WindowMeta {
        &self.meta
    }

    fn frames(&mut self) -> &mut mpsc::Receiver<WindowFrame> {
        &mut self.frames
    }

    fn apply_input(&mut self, input: WindowInput) -> Result<(), WindowError> {
        if let Some(gesture) = self.mapper.map(&input) {
            match backend() {
                Some(backend) => backend.gesture(gesture),
                None => {
                    return Err(WindowError::Backend(
                        "this phone cannot be driven from a computer".into(),
                    ))
                }
            }
        }
        Ok(())
    }

    fn set_network_budget(&mut self, bits_per_second: u32, _congested: bool) {
        self.network_budget = bits_per_second.max(64_000);
        set_rate_target(
            self.rate_id,
            Some(self.bitrate.current.min(self.network_budget)),
        );
    }

    fn on_ack(&mut self, ack: WindowFrameAck) {
        let sent = self.sent.load(Ordering::Relaxed);
        if self
            .bitrate
            .on_ack(ack.acked_seq, sent, std::time::Instant::now())
            .is_some()
        {
            set_rate_target(
                self.rate_id,
                Some(self.bitrate.current.min(self.network_budget)),
            );
        }
    }

    fn request_keyframe(&mut self) {
        if let Some(backend) = backend() {
            backend.keyframe();
        }
    }

    fn close(&mut self, reason: WindowCloseReason) {
        self.forward.abort();
        // `Returned` is this side closing the window to reopen it at a new
        // size; the computer still wants the screen.
        if reason != WindowCloseReason::Returned {
            if let Some(computer) = self.computer.take() {
                viewer_closed(&computer);
            }
        }
    }
}

/// The encoder's bitrate, moved by how far the viewer's acks trail.
///
/// The link's own budget would be the better signal, but a phone's encoder is
/// a black box that takes only a target; frames in flight is what it can be
/// steered by. Halved quickly when the gap grows, raised slowly while it stays
/// small, the way the desktop encoders back off.
#[derive(Debug)]
pub(super) struct Bitrate {
    current: u32,
    last_change: Option<std::time::Instant>,
    healthy_since: Option<std::time::Instant>,
}

/// Where the encoder starts, and the bounds it moves within.
pub(super) const BITRATE_START: u32 = 6_000_000;
pub(super) const BITRATE_MIN: u32 = 1_000_000;
pub(super) const BITRATE_MAX: u32 = 12_000_000;
/// Frames in flight beyond which the link is behind: half a second at 60 fps.
pub(super) const BEHIND: u32 = 30;
/// Frames in flight below which it keeps up.
pub(super) const KEEPING_UP: u32 = 6;

impl Default for Bitrate {
    fn default() -> Self {
        Self {
            current: BITRATE_START,
            last_change: None,
            healthy_since: None,
        }
    }
}

impl Bitrate {
    /// A new target when the gap between `sent` and `acked` calls for one.
    fn on_ack(&mut self, acked: u32, sent: u32, now: std::time::Instant) -> Option<u32> {
        let in_flight = sent.wrapping_sub(acked);
        let settled = self
            .last_change
            .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(1));
        if in_flight > BEHIND {
            self.healthy_since = None;
            if settled && self.current > BITRATE_MIN {
                self.current = (self.current / 2).max(BITRATE_MIN);
                self.last_change = Some(now);
                return Some(self.current);
            }
            return None;
        }
        if in_flight >= KEEPING_UP {
            self.healthy_since = None;
            return None;
        }
        let since = *self.healthy_since.get_or_insert(now);
        if now.duration_since(since) >= std::time::Duration::from_secs(5)
            && self.current < BITRATE_MAX
        {
            self.current = (self.current / 4 * 5).min(BITRATE_MAX);
            self.healthy_since = Some(now);
            self.last_change = Some(now);
            return Some(self.current);
        }
        None
    }
}

impl Drop for PhoneScreenSession {
    fn drop(&mut self) {
        set_rate_target(self.rate_id, None);
        self.forward.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn the_bitrate_halves_when_the_viewer_falls_behind_and_creeps_up_when_it_keeps_up() {
        let mut rate = Bitrate::default();
        let start = Instant::now();
        assert_eq!(rate.on_ack(0, 40, start), Some(BITRATE_START / 2));
        // Not again within a second.
        assert_eq!(rate.on_ack(0, 40, start + Duration::from_millis(500)), None);
        assert_eq!(rate.on_ack(40, 41, start + Duration::from_secs(2)), None);
        assert_eq!(
            rate.on_ack(80, 81, start + Duration::from_secs(8)),
            Some(BITRATE_START / 2 / 4 * 5)
        );
    }
}
