//! A phone's whole screen, shown on a computer as a window it can be driven
//! from — what scrcpy does over ADB, done over the mesh.
//!
//! ## Why this is a window transfer
//!
//! A phone hosting its screen is the AirPlay case turned around: the stream is
//! already H.264 when it exists (the phone's hardware encoder produces it from
//! the screen), [`TileCodec::LossyVideo`] already carries H.264 between Cursio
//! machines, and every viewer's surrogate already decodes it. So the access
//! units go out exactly as the encoder made them, the computer opens an
//! ordinary surrogate titled `<phone> — screen`, and its pointer comes back
//! as ordinary [`WindowInput`] — which [`GestureMapper`] turns into touch.
//! Nothing new crosses the link except the request that starts it
//! ([`WindowMsg::RequestScreen`]), because a viewer cannot otherwise ask a
//! host for a window.
//!
//! ## Who starts it
//!
//! Either side. The phone's user can share from the app; a computer asks with
//! `RequestScreen`, and the phone then asks its user — Android lets nothing
//! capture the screen without a consent shown on the phone, so a request
//! from a computer is a notification there, not a stream. Once the screen is
//! being captured, every computer that asked gets a window; when the last one
//! closes its window, capture stops and the system's recording indicator goes
//! with it.
//!
//! ## What stays on the phone
//!
//! [`ScreenBackend`] is the phone's half: the projection, the encoder and the
//! accessibility service that dispatches gestures. This file only decides,
//! which is why it builds and is tested on any machine.
//!
//! [`TileCodec::LossyVideo`]: cursio_protocol::TileCodec::LossyVideo
//! [`WindowMsg::RequestScreen`]: cursio_protocol::WindowMsg::RequestScreen
//! [`GestureMapper`]: crate::phone_gesture::GestureMapper

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cursio_protocol::{
    Rect, TileCodec, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput,
    WindowMeta,
};
use cursio_window::{CaptureSession, WindowCapture, WindowError, CAPTURE_FRAME_QUEUE_CAPACITY};
use tokio::sync::{broadcast, mpsc, watch};

use crate::phone_gesture::{Gesture, GestureMapper};

/// The phone's half: capture, encoding and touch.
///
/// Implemented over JNI on Android, by a fake in tests.
pub trait ScreenBackend: Send + Sync {
    /// A computer wants the screen and none is being captured: ask the user.
    fn ask(&self, computer: &str);
    /// Nobody is watching any more: end the capture.
    fn stop(&self);
    /// Send a whole picture next; a viewer has nothing to build on.
    fn keyframe(&self);
    /// Aim the encoder at this many bits per second.
    fn set_bitrate(&self, bits_per_second: u32);
    /// Carry out a gesture on the screen.
    fn gesture(&self, gesture: Gesture);
}

/// One encoded piece of the stream.
#[derive(Debug, Clone)]
struct Unit {
    data: Arc<[u8]>,
    /// The picture can be decoded from here without anything before it.
    key: bool,
}

/// What a running capture looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Running {
    width: u32,
    height: u32,
    /// Bumped each time the picture changes size (the phone turned), which
    /// ends every window showing the old size so a new one opens at the new.
    generation: u64,
}

struct Feed {
    backend: Option<Arc<dyn ScreenBackend>>,
    running: Option<Running>,
    /// The encoder's SPS/PPS, which every decoder needs before any picture
    /// and which the encoder emits only once.
    parameters: Option<Arc<[u8]>>,
    /// Computers that asked to see the screen and have not closed it.
    viewers: HashSet<String>,
    /// Computers linked to this phone right now, which the phone's own
    /// "show on computers" shares with.
    linked: HashSet<String>,
    generation: u64,
}

struct Shared {
    feed: Mutex<Feed>,
    units: broadcast::Sender<Unit>,
    /// Woken whenever the running state or the viewer set changes.
    changed: watch::Sender<u64>,
}

/// How many units a slow window may fall behind before it skips to the next
/// keyframe; a second of 60 fps.
const UNIT_BACKLOG: usize = 64;

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        feed: Mutex::new(Feed {
            backend: None,
            running: None,
            parameters: None,
            viewers: HashSet::new(),
            linked: HashSet::new(),
            generation: 0,
        }),
        units: broadcast::channel(UNIT_BACKLOG).0,
        changed: watch::channel(0).0,
    })
}

fn feed() -> std::sync::MutexGuard<'static, Feed> {
    shared().feed.lock().unwrap_or_else(|p| p.into_inner())
}

fn notify_changed() {
    shared().changed.send_modify(|n| *n = n.wrapping_add(1));
}

/// Install the phone's half. Without one, requests are answered by nothing.
pub fn set_backend(backend: Arc<dyn ScreenBackend>) {
    feed().backend = Some(backend);
}

fn backend() -> Option<Arc<dyn ScreenBackend>> {
    feed().backend.clone()
}

/// The capture is up and pictures are `width` x `height`. Called again with a
/// new size when the phone turns.
pub fn started(width: u32, height: u32) {
    {
        let mut feed = feed();
        let same = feed
            .running
            .is_some_and(|r| r.width == width && r.height == height);
        if !same {
            feed.generation += 1;
            // A new size means a new encoder, whose parameter sets will come.
            feed.parameters = None;
        }
        let generation = feed.generation;
        feed.running = Some(Running {
            width,
            height,
            generation,
        });
    }
    tracing::info!(width, height, "this phone's screen is being captured");
    notify_changed();
}

/// One unit from the encoder. `config` marks the parameter sets.
pub fn unit(data: &[u8], config: bool, key: bool) {
    if config {
        feed().parameters = Some(Arc::from(data));
        return;
    }
    // A decoder joining, or restarting after an error, drops everything
    // before a keyframe — parameter sets travelling alone included — and then
    // cannot decode that keyframe either, forever, since the encoder emits
    // them once. So every keyframe carries them, as AirPlay's tap does.
    let data: Arc<[u8]> = match (key, feed().parameters.clone()) {
        (true, Some(parameters)) => [&parameters[..], data].concat().into(),
        _ => Arc::from(data),
    };
    // No receiver is the ordinary case before anyone watches.
    let _ = shared().units.send(Unit { data, key });
}

/// The capture ended on the phone (stopped there, or the system took it).
pub fn stopped() {
    {
        let mut feed = feed();
        feed.running = None;
        feed.parameters = None;
    }
    tracing::info!("this phone's screen is no longer captured");
    notify_changed();
}

/// A computer asked to see the screen, or to stop.
pub fn requested(computer: &str, show: bool) {
    let ask = {
        let mut feed = feed();
        if show {
            feed.viewers.insert(computer.to_string());
            feed.running.is_none()
        } else {
            feed.viewers.remove(computer);
            false
        }
    };
    tracing::info!(%computer, show, "a computer asked about this phone's screen");
    if ask {
        match backend() {
            Some(backend) => backend.ask(computer),
            None => tracing::warn!("this phone cannot capture its screen"),
        }
    }
    release_if_unwatched();
    notify_changed();
}

/// Share with every linked computer, from the phone's own side, once the
/// capture is up. Answers how many that is.
pub fn share_with_linked() -> usize {
    let count = {
        let mut feed = feed();
        let linked: Vec<String> = feed.linked.iter().cloned().collect();
        feed.viewers.extend(linked);
        feed.viewers.len()
    };
    notify_changed();
    count
}

/// The link to `computer` ended: it watches nothing any more.
fn unlinked(computer: &str) {
    {
        let mut feed = feed();
        feed.linked.remove(computer);
        feed.viewers.remove(computer);
    }
    release_if_unwatched();
    notify_changed();
}

/// A computer closed its window: it no longer watches, and with nobody left
/// the capture ends.
fn viewer_closed(computer: &str) {
    feed().viewers.remove(computer);
    release_if_unwatched();
    notify_changed();
}

fn release_if_unwatched() {
    let stop = {
        let feed = feed();
        feed.viewers.is_empty() && feed.running.is_some()
    };
    if stop {
        if let Some(backend) = backend() {
            backend.stop();
        }
    }
}

/// Whether `computer` should be showing the screen now, and at which size.
fn wanted_by(computer: &str) -> Option<Running> {
    let feed = feed();
    feed.running.filter(|_| feed.viewers.contains(computer))
}

/// The title a computer's window for this screen carries.
pub fn title(phone: &str) -> String {
    format!("{phone} — screen")
}

/// Open and close one computer's window as the capture and its wishes change.
///
/// The engine for a link takes transfers only as requests, so this turns the
/// shared state into them: a window when the computer wants the screen and it
/// is up, a close when either stops, and a close-then-open when the phone
/// turns, because a transferred window's size is fixed by its offer.
pub fn spawn_trigger(
    computer: String,
    phone: String,
) -> mpsc::UnboundedReceiver<crate::window_session::TransferRequest> {
    use crate::window_session::TransferRequest;

    let (tx, rx) = mpsc::unbounded_channel();
    let mut changed = shared().changed.subscribe();
    feed().linked.insert(computer.clone());
    tokio::spawn(async move {
        let mut showing: Option<u64> = None;
        // The engine dropping its end is the link ending; whatever the
        // computer was watching ends with it, or the capture would outlive
        // everyone who could see it.
        let _gone = Unlink(computer.clone());
        loop {
            let want = wanted_by(&computer).map(|r| r.generation);
            if want != showing {
                if showing.is_some() && tx.send(TransferRequest::BringBack).is_err() {
                    return;
                }
                if want.is_some()
                    && tx
                        .send(TransferRequest::Send {
                            title: title(&phone),
                        })
                        .is_err()
                {
                    return;
                }
                showing = want;
            }
            tokio::select! {
                result = changed.changed() => if result.is_err() { return },
                () = tx.closed() => return,
            }
        }
    });
    rx
}

/// Forgets a computer when its link's trigger ends, however it ends.
struct Unlink(String);

impl Drop for Unlink {
    fn drop(&mut self) {
        unlinked(&self.0);
    }
}

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
    /// The seq of the newest frame handed to the session, for the gap the
    /// viewer's acks are measured against.
    sent: Arc<AtomicU32>,
}

impl PhoneScreenSession {
    fn open(id: WindowId, title: String, running: Running, computer: Option<String>) -> Self {
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
            sent,
        }
    }
}

/// Stream units as frames from a keyframe on.
///
/// A window that falls behind skips to the next keyframe rather than
/// showing pictures built on ones it never got.
async fn forward(
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

    fn on_ack(&mut self, ack: WindowFrameAck) {
        let sent = self.sent.load(Ordering::Relaxed);
        if let Some(rate) = self
            .bitrate
            .on_ack(ack.acked_seq, sent, std::time::Instant::now())
        {
            if let Some(backend) = backend() {
                backend.set_bitrate(rate);
            }
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
struct Bitrate {
    current: u32,
    last_change: Option<std::time::Instant>,
    healthy_since: Option<std::time::Instant>,
}

/// Where the encoder starts, and the bounds it moves within.
const BITRATE_START: u32 = 6_000_000;
const BITRATE_MIN: u32 = 1_000_000;
const BITRATE_MAX: u32 = 12_000_000;
/// Frames in flight beyond which the link is behind: half a second at 60 fps.
const BEHIND: u32 = 30;
/// Frames in flight below which it keeps up.
const KEEPING_UP: u32 = 6;

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
        self.forward.abort();
    }
}

/// The phone's screen as a source for a computer's capture apps (Discord,
/// OBS), alongside the window a computer can open.
pub struct PhoneScreenSources {
    pub phone: String,
}

/// How long a capture app's pick waits for the phone's user to allow it.
const CONSENT_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

impl cursio_window::ScreenSources for PhoneScreenSources {
    fn list(&self) -> Result<Vec<cursio_protocol::ScreenSource>, WindowError> {
        let size = feed().running.map_or((0, 0), |r| (r.width, r.height));
        Ok(vec![cursio_protocol::ScreenSource {
            id: 1,
            kind: cursio_protocol::SourceKind::Monitor,
            title: title(&self.phone),
            width: size.0,
            height: size.1,
            // The phone asks its user before anything streams.
            picked_on_host: true,
        }])
    }

    /// Blocks until the phone's user allows the capture, or gives up.
    fn open(
        &self,
        id: WindowId,
        _source: &cursio_protocol::ScreenSource,
    ) -> Result<Box<dyn CaptureSession + Send>, WindowError> {
        const VIEW: &str = "a capture app";
        if feed().running.is_none() {
            requested(VIEW, true);
            let deadline = std::time::Instant::now() + CONSENT_WAIT;
            while feed().running.is_none() {
                if std::time::Instant::now() >= deadline {
                    requested(VIEW, false);
                    return Err(WindowError::WindowGone(
                        "the phone's user did not allow sharing its screen".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        } else {
            feed().viewers.insert(VIEW.to_string());
        }
        let running = feed().running.ok_or_else(|| {
            WindowError::WindowGone("this phone's screen is not being captured".into())
        })?;
        Ok(Box::new(PhoneScreenSession::open(
            id,
            title(&self.phone),
            running,
            Some(VIEW.to_string()),
        )))
    }
}

/// The computer's side: each link to a phone, by the phone's name, and the
/// channel that link's window engine sends [`WindowMsg::RequestScreen`] from.
///
/// [`WindowMsg::RequestScreen`]: cursio_protocol::WindowMsg::RequestScreen
fn viewers() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<bool>>> {
    static VIEWERS: OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<bool>>>> = OnceLock::new();
    VIEWERS.get_or_init(Default::default)
}

/// Register a link to `phone`; its engine sends what the receiver yields.
pub fn register_phone(phone: &str) -> mpsc::UnboundedReceiver<bool> {
    let (tx, rx) = mpsc::unbounded_channel();
    viewers()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(phone.to_string(), tx);
    rx
}

/// Ask `phone` to show its screen here, or to stop. Answers with the phone's
/// name as it is known, or why it cannot be asked.
pub fn ask(phone: &str, show: bool) -> Result<String, String> {
    let mut viewers = viewers().lock().unwrap_or_else(|p| p.into_inner());
    // A link that ended leaves a closed sender behind.
    viewers.retain(|_, tx| !tx.is_closed());
    let found = viewers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(phone))
        .map(|(name, tx)| (name.clone(), tx.clone()));
    match found {
        Some((name, tx)) => {
            let _ = tx.send(show);
            Ok(name)
        }
        None => {
            let mut known: Vec<&str> = viewers.keys().map(String::as_str).collect();
            known.sort_unstable();
            Err(if known.is_empty() {
                format!("{phone} cannot show its screen here: no phone is connected with screen sharing")
            } else {
                format!(
                    "{phone} cannot show its screen here; phones that can: {}",
                    known.join(", ")
                )
            })
        }
    }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    let mut feed = feed();
    feed.running = None;
    feed.parameters = None;
    feed.viewers.clear();
    feed.linked.clear();
    feed.backend = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_session::TransferRequest;
    use std::time::{Duration, Instant};

    /// The feed is process-wide, so tests that touch it take turns.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Default)]
    struct FakeBackend {
        asked: Mutex<Vec<String>>,
        stops: Mutex<u32>,
        gestures: Mutex<Vec<Gesture>>,
    }

    impl ScreenBackend for FakeBackend {
        fn ask(&self, computer: &str) {
            self.asked.lock().unwrap().push(computer.to_string());
        }
        fn stop(&self) {
            *self.stops.lock().unwrap() += 1;
        }
        fn keyframe(&self) {}
        fn set_bitrate(&self, _bits_per_second: u32) {}
        fn gesture(&self, gesture: Gesture) {
            self.gestures.lock().unwrap().push(gesture);
        }
    }

    fn fake() -> Arc<FakeBackend> {
        reset_for_test();
        let backend = Arc::new(FakeBackend::default());
        set_backend(backend.clone());
        backend
    }

    #[tokio::test]
    async fn a_computer_asking_while_nothing_is_captured_asks_the_phones_user() {
        let _turn = SERIAL.lock().await;
        let backend = fake();
        requested("Nocturne", true);
        assert_eq!(*backend.asked.lock().unwrap(), vec!["Nocturne".to_string()]);
    }

    #[tokio::test]
    async fn the_window_opens_once_the_capture_is_up_and_closes_when_it_stops() {
        let _turn = SERIAL.lock().await;
        let _backend = fake();
        let mut requests = spawn_trigger("Nocturne".into(), "POCO".into());
        requested("Nocturne", true);
        started(1080, 2400);
        assert_eq!(
            requests.recv().await,
            Some(TransferRequest::Send {
                title: "POCO — screen".into()
            })
        );
        stopped();
        assert_eq!(requests.recv().await, Some(TransferRequest::BringBack));
    }

    #[tokio::test]
    async fn turning_the_phone_reopens_the_window_at_the_new_size() {
        let _turn = SERIAL.lock().await;
        let _backend = fake();
        let mut requests = spawn_trigger("Nocturne".into(), "POCO".into());
        requested("Nocturne", true);
        started(1080, 2400);
        assert!(matches!(
            requests.recv().await,
            Some(TransferRequest::Send { .. })
        ));
        started(2400, 1080);
        assert_eq!(requests.recv().await, Some(TransferRequest::BringBack));
        assert!(matches!(
            requests.recv().await,
            Some(TransferRequest::Send { .. })
        ));
    }

    #[tokio::test]
    async fn the_last_window_closing_ends_the_capture_but_a_reopen_does_not() {
        let _turn = SERIAL.lock().await;
        let backend = fake();
        requested("Nocturne", true);
        started(100, 200);
        let mut capture = PhoneScreenCapture::new("Nocturne".into(), "POCO".into());
        let mut session = capture.begin(1).unwrap();
        session.close(WindowCloseReason::Returned);
        assert_eq!(*backend.stops.lock().unwrap(), 0);
        let mut session = capture.begin(2).unwrap();
        session.close(WindowCloseReason::ViewerClosed);
        assert_eq!(*backend.stops.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_viewer_gets_pictures_from_a_keyframe_carrying_the_parameter_sets() {
        let _turn = SERIAL.lock().await;
        let _backend = fake();
        started(100, 200);
        unit(&[0, 0, 0, 1, 0x67], true, false);
        let mut capture = PhoneScreenCapture::new("Nocturne".into(), "POCO".into());
        let mut session = capture.begin(1).unwrap();
        tokio::task::yield_now().await;
        unit(&[0, 0, 0, 1, 0x41], false, false);
        unit(&[0, 0, 0, 1, 0x65], false, true);
        unit(&[0, 0, 0, 1, 0x41], false, false);
        let first = session.frames().recv().await.unwrap();
        assert_eq!(first.data, [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x65]);
        assert_eq!(first.codec, TileCodec::LossyVideo);
        let second = session.frames().recv().await.unwrap();
        assert_eq!(second.data, [0, 0, 0, 1, 0x41]);
    }

    #[tokio::test]
    async fn the_viewers_clicks_reach_the_phone_as_touch() {
        let _turn = SERIAL.lock().await;
        let backend = fake();
        started(100, 200);
        let mut capture = PhoneScreenCapture::new("Nocturne".into(), "POCO".into());
        let mut session = capture.begin(1).unwrap();
        session
            .apply_input(WindowInput::PointerButton {
                id: 1,
                x: 50,
                y: 100,
                button: cursio_protocol::MouseButton::Left,
                pressed: true,
            })
            .unwrap();
        assert!(matches!(
            backend.gestures.lock().unwrap().as_slice(),
            [Gesture::Down(_)]
        ));
    }

    #[tokio::test]
    async fn the_link_ending_stops_a_capture_nobody_else_watches() {
        let _turn = SERIAL.lock().await;
        let backend = fake();
        let requests = spawn_trigger("Nocturne".into(), "POCO".into());
        requested("Nocturne", true);
        started(100, 200);
        drop(requests);
        for _ in 0..20 {
            if *backend.stops.lock().unwrap() > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*backend.stops.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn sharing_from_the_phone_reaches_every_linked_computer() {
        let _turn = SERIAL.lock().await;
        let _backend = fake();
        let mut first = spawn_trigger("Nocturne".into(), "POCO".into());
        let mut second = spawn_trigger("Lightmoon".into(), "POCO".into());
        started(100, 200);
        assert_eq!(share_with_linked(), 2);
        assert!(matches!(
            first.recv().await,
            Some(TransferRequest::Send { .. })
        ));
        assert!(matches!(
            second.recv().await,
            Some(TransferRequest::Send { .. })
        ));
    }

    #[test]
    fn asking_a_phone_that_is_not_linked_names_the_ones_that_are() {
        let _rx = register_phone("Redmi");
        let error = ask("Pixel", true).unwrap_err();
        assert!(error.contains("Redmi"), "{error}");
        assert_eq!(ask("redmi", true).unwrap(), "Redmi");
    }

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
