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
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{broadcast, mpsc, watch};

use crate::phone_gesture::Gesture;

mod asking;
mod session;

pub use asking::{ask, register_phone, PhoneScreenSources};
pub use session::{PhoneScreenCapture, PhoneScreenSession};

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
    /// Computers linked to this phone right now, with how many links each:
    /// while a better link replaces a worse one both are briefly up, and the
    /// older one ending must not take the computer with it.
    linked: HashMap<String, usize>,
    /// Whom the phone's own "show on computers" shares with once the user
    /// allows the capture, chosen in the window before the consent opens.
    chosen: Option<Vec<String>>,
    generation: u64,
    rate_targets: HashMap<u64, u32>,
    next_rate_id: u64,
    applied_rate: Option<u32>,
}

struct Shared {
    feed: Mutex<Feed>,
    rate_update: Mutex<()>,
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
            linked: HashMap::new(),
            chosen: None,
            generation: 0,
            rate_targets: HashMap::new(),
            next_rate_id: 0,
            applied_rate: None,
        }),
        rate_update: Mutex::new(()),
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

fn set_rate_target(id: u64, target: Option<u32>) {
    let _update = shared()
        .rate_update
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let change = {
        let mut feed = feed();
        if let Some(target) = target {
            feed.rate_targets.insert(id, target);
        } else {
            feed.rate_targets.remove(&id);
        }
        let rate = feed.rate_targets.values().copied().min();
        if feed.applied_rate == rate {
            None
        } else {
            feed.applied_rate = rate;
            rate.and_then(|rate| feed.backend.clone().map(|backend| (backend, rate)))
        }
    };
    if let Some((backend, rate)) = change {
        backend.set_bitrate(rate);
    }
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

/// The computers linked now, and the one the user was at last — the one to
/// offer first. Sorted by name.
pub fn targets() -> (Vec<String>, Option<String>) {
    let mut linked: Vec<String> = feed().linked.keys().cloned().collect();
    linked.sort_unstable();
    let last = crate::notify_hub::hub()
        .last_used_desktop()
        .filter(|name| linked.contains(name));
    (linked, last)
}

/// Whom the next share from the phone goes to, picked before Android's
/// consent is asked for.
pub fn choose(computers: Vec<String>) {
    feed().chosen = Some(computers);
}

/// Share from the phone's own side, once the capture is up: with the
/// computers [`choose`] named, else the one the user was at last, else —
/// with nothing known of where the user is — every linked one. Answers how
/// many computers watch now.
///
/// It was every linked computer always, so the screen opened on whichever
/// machines happened to be up rather than the one the user was looking at.
pub fn share_with_chosen() -> usize {
    let last = crate::notify_hub::hub().last_used_desktop();
    let count = {
        let mut feed = feed();
        let wanted = match feed.chosen.take() {
            Some(chosen) => chosen,
            None => match last.filter(|name| feed.linked.contains_key(name)) {
                Some(last) => vec![last],
                None => feed.linked.keys().cloned().collect(),
            },
        };
        let wanted: Vec<String> = wanted
            .into_iter()
            .filter(|name| feed.linked.contains_key(name))
            .collect();
        tracing::info!(?wanted, "showing this phone's screen");
        feed.viewers.extend(wanted);
        feed.viewers.len()
    };
    notify_changed();
    count
}

/// One link to `computer` ended: with none left it watches nothing any more.
fn unlinked(computer: &str) {
    {
        let mut feed = feed();
        let left = feed.linked.get_mut(computer).map(|links| {
            *links = links.saturating_sub(1);
            *links
        });
        if left.is_none_or(|left| left == 0) {
            feed.linked.remove(computer);
            feed.viewers.remove(computer);
        }
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
    *feed().linked.entry(computer.clone()).or_default() += 1;
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

#[cfg(test)]
pub(crate) fn reset_for_test() {
    let mut feed = feed();
    feed.running = None;
    feed.parameters = None;
    feed.viewers.clear();
    feed.linked.clear();
    feed.chosen = None;
    feed.backend = None;
    feed.rate_targets.clear();
    feed.applied_rate = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phone_screen::session::BITRATE_START;
    use crate::window_session::TransferRequest;
    use cursio_protocol::TileCodec;
    use cursio_protocol::WindowCloseReason;
    use cursio_protocol::WindowInput;
    use cursio_window::CaptureSession;
    use cursio_window::WindowCapture;

    /// The feed is process-wide, so tests that touch it take turns.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Default)]
    struct FakeBackend {
        asked: Mutex<Vec<String>>,
        stops: Mutex<u32>,
        gestures: Mutex<Vec<Gesture>>,
        rates: Mutex<Vec<u32>>,
    }

    impl ScreenBackend for FakeBackend {
        fn ask(&self, computer: &str) {
            self.asked.lock().unwrap().push(computer.to_string());
        }
        fn stop(&self) {
            *self.stops.lock().unwrap() += 1;
        }
        fn keyframe(&self) {}
        fn set_bitrate(&self, bits_per_second: u32) {
            self.rates.lock().unwrap().push(bits_per_second);
        }
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
    async fn the_shared_encoder_obeys_the_slowest_view_and_releases_its_limit() {
        let _turn = SERIAL.lock().await;
        let backend = fake();
        started(100, 200);
        let running = feed().running.unwrap();
        let mut slow = PhoneScreenSession::open(1, "slow".into(), running, None);
        let mut fast = PhoneScreenSession::open(2, "fast".into(), running, None);
        slow.set_network_budget(1_500_000, true);
        fast.set_network_budget(8_000_000, false);
        assert_eq!(backend.rates.lock().unwrap().last(), Some(&1_500_000));
        drop(slow);
        assert_eq!(backend.rates.lock().unwrap().last(), Some(&BITRATE_START));
        fast.set_network_budget(3_000_000, false);
        assert_eq!(backend.rates.lock().unwrap().last(), Some(&3_000_000));
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
    async fn sharing_from_the_phone_goes_only_to_the_computer_chosen() {
        let _turn = SERIAL.lock().await;
        let _backend = fake();
        let mut chosen = spawn_trigger("Nocturne".into(), "POCO".into());
        let mut other = spawn_trigger("Lightmoon".into(), "POCO".into());
        assert_eq!(
            targets().0,
            vec!["Lightmoon".to_string(), "Nocturne".to_string()]
        );
        choose(vec!["Nocturne".into()]);
        started(100, 200);
        assert_eq!(share_with_chosen(), 1);
        assert!(matches!(
            chosen.recv().await,
            Some(TransferRequest::Send { .. })
        ));
        tokio::task::yield_now().await;
        assert!(
            other.try_recv().is_err(),
            "an unchosen computer was sent the screen"
        );
    }

    #[tokio::test]
    async fn a_replaced_link_ending_leaves_its_computer_watching() {
        let _turn = SERIAL.lock().await;
        let _backend = fake();
        let older = spawn_trigger("Nocturne".into(), "POCO".into());
        let _newer = spawn_trigger("Nocturne".into(), "POCO".into());
        choose(vec!["Nocturne".into()]);
        started(100, 200);
        assert_eq!(share_with_chosen(), 1);
        drop(older);
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            wanted_by("Nocturne").is_some(),
            "the computer stopped watching"
        );
    }

    #[tokio::test]
    async fn sharing_with_nothing_known_of_the_user_reaches_every_linked_computer() {
        let _turn = SERIAL.lock().await;
        let _backend = fake();
        let mut first = spawn_trigger("screen-test-first".into(), "POCO".into());
        let mut second = spawn_trigger("screen-test-second".into(), "POCO".into());
        started(100, 200);
        assert_eq!(share_with_chosen(), 2);
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
}
