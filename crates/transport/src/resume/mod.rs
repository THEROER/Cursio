//! A session that outlives its connection.
//!
//! A link used to be one TCP connection, and a session one link: Wi-Fi gone for
//! two seconds meant fifteen seconds of silence before the session noticed,
//! then every module torn down — disks unmounted, streams closed, the cursor
//! sent home — and all of it rebuilt once the mesh dialled again. A laptop on a
//! flaky radio paid that once an hour for a blip that healed itself in eight
//! seconds.
//!
//! Here the session rides a [`ResumableLink`] instead, and connections are
//! only *paths* under it. When a path dies the session keeps running; a new
//! connection that offers the session's [`SessionTag`] in its greeting joins it
//! as another path, and the session carries on where it stopped.
//!
//! ## How nothing is lost or doubled
//!
//! XMPP's stream management (XEP-0198) is the model. Every frame the session
//! sends is numbered (`SEQ`), and each side keeps saying, in `ACK`, the highest
//! number it has delivered. A sender keeps every frame not yet acknowledged; a
//! path joining the session opens with `RESUME` and the number delivered, and
//! each side resends the rest on it. The receiver delivers a frame only if its
//! number is past the last one delivered, so the copies a resend produces fall
//! away and every frame reaches the session at most once — which matters most
//! for a key press: delivered twice it types twice, lost it stays held.
//!
//! ## What is not resent
//!
//! Pointer motion, sound and pixels describe the present
//! ([`Message::is_lossy`](cursio_protocol::Message::is_lossy)). They are
//! numbered but never kept: after a five-second gap, replaying them would jump
//! the cursor by everything it moved meanwhile and play old sound over new,
//! which is Mosh's point that a mobile link should deliver the latest state
//! rather than the history. A chunked lossy message cut off mid-way is dropped
//! by the receiver once frames start arriving on another path.
//!
//! ## Paths
//!
//! Each lane has one network path, with a shared Bluetooth standby. Each
//! lane numbers and acknowledges its own frames so an independent socket can
//! make progress without waiting for another lane. A newer path replaces the
//! older one for that lane. Bluetooth does not carry pixels.
//!
//! A path carrying the session is passed over when it falls silent for three
//! seconds, since it hears an `ACK` every second. A network path is then kept,
//! suspect, while the mesh places a replacement, and let go of only after
//! fifteen: a blip shorter than that resumes on the old connection, and a
//! replacement that joins first replaces it. Bluetooth and standby paths are
//! let go of at their own silences. A path that is alive but not delivering —
//! frames sent on it stay unacknowledged for seconds while another path
//! exists — is *degraded*: the session moves to the other path and comes back
//! only after a while, so a marginal link does not make it flap. That is what
//! Linux's multipath TCP learned to do with its stale subflows.
//!
//! With no interactive path left, the link holds the session for [`HOLD`] and ends
//! it, as the old link ended on a dead connection.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

// Tokio's clock, so a test can run the hold out without waiting a minute.
use tokio::time::Instant;

use async_trait::async_trait;
use cursio_protocol::diag::{PathChoice, PathReport};
use cursio_protocol::{Message, SessionTag};
use tokio::sync::{mpsc, Notify};

use crate::crypto::{link_tag, open_frame, seal_frame, Reassembler, SharedNoise};
use crate::lane::Lane;
use crate::priority::{FrameSink, PrioritizedTx};
use crate::resume::frame::read_delivered;
use crate::resume::frame::resume_frame;
use crate::resume::inner::Inner;
use crate::resume::send::ResumableSink;
use crate::resume::state::State;
use crate::resume::supervise::supervise;
use crate::{Transport, TransportError, TransportRx, TransportTx};

mod frame;
mod inner;
mod receive;
mod send;
mod state;
mod supervise;

pub use receive::ResumableRx;

/// How long a session waits, with no path at all, for one to come back.
pub const HOLD: Duration = Duration::from_secs(60);

/// How often an `ACK` goes out on the path carrying the session even with
/// nothing new to acknowledge: the path's keepalive.
const ACK_EVERY: Duration = Duration::from_secs(1);
/// Soonest an `ACK` follows delivered frames.
const ACK_SOON: Duration = Duration::from_millis(50);
/// How often every path, standby included, hears a keepalive.
const STANDBY_KEEPALIVE: Duration = Duration::from_secs(15);

/// Silence after which the path carrying the session is passed over: a
/// network path is then kept, suspect, until [`NETWORK_GIVE_UP`]; any other
/// is let go of.
const ACTIVE_SILENCE: Duration = Duration::from_secs(3);
/// Silence after which a suspect network path is let go of. Closing it at
/// [`ACTIVE_SILENCE`], as before, turned every Wi-Fi blip into four new
/// connections and a resend of everything unacknowledged — 150 to 240 times
/// a day on a laptop with a weak card — even when the old connections would
/// have carried on a second later.
const NETWORK_GIVE_UP: Duration = Duration::from_secs(15);
/// As [`ACTIVE_SILENCE`], for a Bluetooth path. The radio pauses for seconds
/// while it pages another device or opens a profile, and a Bluetooth path is
/// usually the only one left: letting it go at three seconds ended it in the
/// middle of every session's opening burst, and each new path then resent
/// everything unacknowledged into the next pause. A dead link still ends at
/// the radio's own supervision timeout, as a read error.
const BLUETOOTH_SILENCE: Duration = Duration::from_secs(10);

/// The largest frame while Bluetooth carries the interactive lane. A frame
/// cannot be interrupted once on the wire, and at the radio's speed a full
/// 64 KiB one held every pointer move behind it for most of a second; 4 KiB
/// is tens of milliseconds there, for about 1% more framing.
const BLUETOOTH_FRAME: usize = 4 * 1024;
/// Silence after which a standby path is let go of: three keepalives.
const STANDBY_SILENCE: Duration = Duration::from_secs(45);

/// A frame unacknowledged this long on the carrying path, with another path
/// alive, marks the carrying path degraded.
const STALL: Duration = Duration::from_secs(3);
/// As [`STALL`], for input on a network path while a Bluetooth path stands
/// by. Input is what the person is waiting on, and a LAN answers in
/// milliseconds: three seconds of a pointer going nowhere before the standby
/// took over was most of what a Wi-Fi drop felt like.
const INPUT_STALL: Duration = Duration::from_secs(1);
/// How long a degraded path is passed over.
const DEGRADED_FOR: Duration = Duration::from_secs(10);

/// How long a joining path waits for the other side's `RESUME`.
const RESUME_WAIT: Duration = Duration::from_secs(5);

/// Reliable bytes retained per lane. The lane waits for ACKs at this bound;
/// other lane workers keep running.
const RESEND_CAP: usize = 16 * 1024 * 1024;

/// How often the supervisor looks at the paths.
const TICK: Duration = Duration::from_millis(100);

/// What a path runs over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// TCP over Wi-Fi or Ethernet.
    Network,
    /// RFCOMM to a paired device.
    Bluetooth,
    NetworkAudio,
    NetworkVideo,
    NetworkBulk,
}

impl PathKind {
    fn lane(self) -> Option<Lane> {
        match self {
            Self::Network => Some(Lane::Interactive),
            Self::NetworkAudio => Some(Lane::Audio),
            Self::NetworkVideo => Some(Lane::Video),
            Self::NetworkBulk => Some(Lane::Bulk),
            Self::Bluetooth => None,
        }
    }

    fn for_lane(lane: Lane) -> Self {
        match lane {
            Lane::Interactive => Self::Network,
            Lane::Audio => Self::NetworkAudio,
            Lane::Video => Self::NetworkVideo,
            Lane::Bulk => Self::NetworkBulk,
        }
    }
}

/// One connection, Noise already established, ready to carry a session.
pub struct Path {
    pub(crate) tx: Box<dyn TransportTx>,
    pub(crate) rx: Box<dyn TransportRx>,
    pub(crate) noise: SharedNoise,
    pub(crate) kind: PathKind,
    /// Over a direct Wi-Fi link rather than through the router.
    pub(crate) direct: bool,
}

impl Path {
    pub fn kind(&self) -> PathKind {
        self.kind
    }

    pub fn is_direct(&self) -> bool {
        self.direct
    }
}

/// How many paths of each kind carry a session now; reported whenever it
/// changes so the mesh can dial a path back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Paths {
    pub network: bool,
    pub bluetooth: bool,
    /// A network path over a direct Wi-Fi link is among them.
    pub direct: bool,
}

/// Every resumable link in the process, for [`network_lost`].
fn live_links() -> &'static Mutex<Vec<Weak<Inner>>> {
    static LINKS: std::sync::OnceLock<Mutex<Vec<Weak<Inner>>>> = std::sync::OnceLock::new();
    LINKS.get_or_init(|| Mutex::new(Vec::new()))
}

/// The system reports the network gone (Android's `NetworkCallback.onLost`,
/// NetworkManager's state): every session lets its network path go at once,
/// moving to a Bluetooth standby where it has one, instead of waiting out
/// the path's silence. A connection that survives the change is redialled
/// within one sweep, so a false alarm costs a reconnect, not the session.
pub fn network_lost() {
    let links: Vec<Arc<Inner>> = {
        let mut all = live_links().lock().unwrap_or_else(|p| p.into_inner());
        all.retain(|link| link.strong_count() > 0);
        all.iter().filter_map(Weak::upgrade).collect()
    };
    for link in links {
        ResumeHandle(link).network_lost();
    }
}

/// A direct Wi-Fi link went away: every session lets the paths over it go at
/// once, so the paths through the router carry on instead of waiting out
/// the direct ones' silence.
pub fn direct_lost(why: &str) {
    let links: Vec<Arc<Inner>> = {
        let mut all = live_links().lock().unwrap_or_else(|p| p.into_inner());
        all.retain(|link| link.strong_count() > 0);
        all.iter().filter_map(Weak::upgrade).collect()
    };
    for link in links {
        ResumeHandle(link).release_direct(why);
    }
}

/// Where the mesh reaches a resumable session: to name it in greetings, give
/// it new paths, and hear when it has none.
#[derive(Clone)]
pub struct ResumeHandle(Arc<Inner>);

impl ResumeHandle {
    /// The tag both ends know this session by.
    pub fn tag(&self) -> SessionTag {
        self.0.tag
    }

    /// How the transport's log lines name this session.
    pub fn label(&self) -> String {
        self.0.label()
    }

    /// The paths carrying the session now.
    pub fn paths(&self) -> Paths {
        self.0.lock().summary()
    }

    pub fn network_complete(&self) -> bool {
        self.0.lock().missing_lane(None).is_none()
    }

    /// Whether every lane has a healthy path over the direct Wi-Fi link, so
    /// the side that dials over it has nothing left to place.
    pub fn direct_complete(&self) -> bool {
        self.0.lock().missing_lane(Some(true)).is_none()
    }

    pub fn is_ended(&self) -> bool {
        self.0.lock().ended.is_some()
    }

    /// End the session: its peer no longer has it, or the user removed it.
    pub fn end(&self, why: &str) {
        self.0.end(why.to_string());
    }

    /// Carry what this end sends as `choice` says, for the rest of the
    /// session; see [`PathChoice`]. Frames already numbered are resent on the
    /// newly chosen path, as after any change of path.
    pub fn choose(&self, choice: PathChoice) {
        {
            let mut state = self.0.lock();
            if state.choice == choice {
                return;
            }
            state.choice = choice;
        }
        tracing::info!(session = %self.0.label(), ?choice, "the session's path was chosen by hand");
        self.0.changed.notify_waiters();
        self.0.kick(0);
    }

    /// This end's view of its paths, for a person diagnosing them.
    pub fn report(&self) -> PathReport {
        let state = self.0.lock();
        let paths = state.summary();
        PathReport {
            network: paths.network,
            bluetooth: paths.bluetooth,
            carrying: state
                .carrying(Lane::Interactive, Instant::now())
                .map(|path| match path.kind {
                    PathKind::Bluetooth => "bluetooth".to_string(),
                    _ if path.direct => "direct".to_string(),
                    _ => "network".to_string(),
                }),
            choice: state.choice,
        }
    }

    /// Be told whenever the set of paths changes.
    pub fn on_paths(&self, observer: impl Fn(Paths) + Send + Sync + 'static) {
        *self.0.observer.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(observer));
    }

    /// The system says the network went away: let the network path go now
    /// instead of after its silence, so a standby takes over at once.
    pub fn network_lost(&self) {
        for lane in Lane::ALL {
            self.release(
                PathKind::for_lane(lane),
                "the system reported the network gone",
            );
        }
    }

    /// Let every path over a direct Wi-Fi link go.
    pub fn release_direct(&self, why: &str) {
        let ids: Vec<u64> = self
            .0
            .lock()
            .paths
            .iter()
            .filter(|p| p.direct)
            .map(|p| p.id)
            .collect();
        for id in ids {
            self.0.path_failed(id, why);
        }
    }

    /// Let every path of `kind` go: a Bluetooth path once the network carries
    /// the session again, unless it is kept as a standby.
    pub fn release(&self, kind: PathKind, why: &str) {
        let ids: Vec<u64> = self
            .0
            .lock()
            .paths
            .iter()
            .filter(|p| p.kind == kind)
            .map(|p| p.id)
            .collect();
        for id in ids {
            self.0.path_failed(id, why);
        }
    }

    /// Add a connection to the session: say how far each side has got, then
    /// carry on over it. Both ends call this on the same connection.
    pub async fn attach(&self, path: Path) -> Result<(), TransportError> {
        let Path {
            mut tx,
            mut rx,
            noise,
            kind,
            direct,
        } = path;
        // Both ends mark the connection alike (each sees its own address on
        // the group), so both propose a lane missing from the same set.
        let (delivered, proposed) = {
            let state = self.0.lock();
            if let Some(why) = &state.ended {
                return Err(TransportError::Other(format!("the session is over: {why}")));
            }
            let class = (kind != PathKind::Bluetooth).then_some(direct);
            (state.delivered(), state.missing_lane(class))
        };
        let mut resume = resume_frame(delivered, proposed);
        resume.extend_from_slice(&self.tag());
        tx.send(&seal_frame(&noise, &resume)?).await?;
        let answer = tokio::time::timeout(RESUME_WAIT, rx.recv())
            .await
            .map_err(|_| {
                TransportError::Other("the peer did not say where it had got to".into())
            })??;
        let tagged = open_frame(&noise, answer)?;
        if tagged.first() != Some(&link_tag::RESUME) {
            return Err(TransportError::Other(
                "the joining path did not open with RESUME".into(),
            ));
        }
        const SNAPSHOT_LEN: usize = 1 + Lane::COUNT * 8 + 1;
        if tagged.len() != SNAPSHOT_LEN + self.tag().len() || tagged[SNAPSHOT_LEN..] != self.tag() {
            return Err(TransportError::Other(
                "the path belongs to a different session".into(),
            ));
        }
        let peer_delivered = read_delivered(&tagged[1..SNAPSHOT_LEN])?;
        let peer_proposed = tagged[SNAPSHOT_LEN - 1];
        if peer_proposed != u8::MAX && Lane::decode(peer_proposed).is_none() {
            return Err(TransportError::Other("invalid resume lane".into()));
        }
        // A complete peer proposes 255, not lane zero: when just one end
        // noticed a failed bulk path, that must still select the bulk lane.
        let selected = proposed
            .map_or(u8::MAX, |lane| lane as u8)
            .min(peer_proposed);
        let lane = Lane::decode(selected).unwrap_or(Lane::Interactive);
        let kind = if kind == PathKind::Bluetooth {
            kind
        } else {
            PathKind::for_lane(lane)
        };
        {
            let mut state = self.0.lock();
            for lane in Lane::ALL {
                state.acknowledged(lane, peer_delivered[lane as usize]);
            }
        }
        self.0.install(Path {
            tx,
            rx,
            noise,
            kind,
            direct,
        });
        Ok(())
    }
}

/// A [`Transport`] for a session that can change its connection.
pub struct ResumableLink {
    inner: Arc<Inner>,
    rx: Option<ResumableRx>,
    tx: Option<Box<dyn TransportTx>>,
}

impl ResumableLink {
    /// Start a session over `first`, known to both ends as `tag`. Spawns its
    /// tasks, so it must run inside the runtime the session will.
    pub fn new(first: Path, tag: SessionTag) -> (Self, ResumeHandle) {
        let channels = std::array::from_fn::<_, { Lane::COUNT }, _>(|_| mpsc::channel(16));
        let (senders, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
        let inbound_tx = senders.try_into().ok().unwrap();
        let inbound = receivers.try_into().ok().unwrap();
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            inbound: inbound_tx,
            tag,
            observer: Mutex::new(None),
            congestion: Mutex::new(crate::congestion::BackgroundRate::default()),
        });
        inner.install(first);
        live_links()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::downgrade(&inner));
        tokio::spawn(supervise(Arc::downgrade(&inner)));
        let rx = ResumableRx {
            inner: inner.clone(),
            inbound,
            reasm: std::array::from_fn(|_| Reassembler::default()),
            lossy_parts: std::array::from_fn(|_| HashSet::new()),
            last_path: [None; Lane::COUNT],
        };
        let handle = ResumeHandle(inner.clone());
        (
            Self {
                inner,
                rx: Some(rx),
                tx: None,
            },
            handle,
        )
    }

    fn sender(&mut self) -> Box<dyn TransportTx> {
        let sinks: Vec<Box<dyn FrameSink>> = Lane::ALL
            .into_iter()
            .map(|lane| {
                Box::new(ResumableSink {
                    inner: self.inner.clone(),
                    lane,
                    last_path: None,
                    next_bulk: Instant::now(),
                }) as Box<dyn FrameSink>
            })
            .collect();
        let weak = Arc::downgrade(&self.inner);
        let holding = Arc::new(move || {
            weak.upgrade().is_some_and(|inner| {
                let state = inner.lock();
                state.ended.is_none() && state.carrying(Lane::Interactive, Instant::now()).is_none()
            })
        });
        let weak = Arc::downgrade(&self.inner);
        let frame_limit = Arc::new(move || {
            let on_bluetooth = weak.upgrade().is_some_and(|inner| {
                inner
                    .lock()
                    .carrying(Lane::Interactive, Instant::now())
                    .is_some_and(|path| path.kind == PathKind::Bluetooth)
            });
            if on_bluetooth {
                BLUETOOTH_FRAME
            } else {
                usize::MAX
            }
        });
        let weak = Arc::downgrade(&self.inner);
        let carrying = Arc::new(move || {
            let inner = weak.upgrade()?;
            let state = inner.lock();
            let path = state.carrying(Lane::Interactive, Instant::now())?;
            Some(match path.kind {
                PathKind::Bluetooth => crate::stream::BLUETOOTH,
                _ => "network",
            })
        });
        let weak = Arc::downgrade(&self.inner);
        let input_seen = Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.congestion.lock().unwrap().note_input(Instant::now());
            }
        });
        let tx = PrioritizedTx::spawn_lanes(sinks)
            .with_holding(holding)
            .with_carrying_path(carrying)
            .with_input_seen(input_seen)
            .with_frame_limit(frame_limit);
        self.inner.lock().kick = Some(tx.kick_handle());
        Box::new(tx)
    }
}

#[async_trait]
impl Transport for ResumableLink {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        if self.tx.is_none() {
            self.tx = Some(self.sender());
        }
        match self.tx.as_mut() {
            Some(tx) => tx.send(msg).await,
            None => Err(TransportError::Closed),
        }
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        match self.rx.as_mut() {
            Some(rx) => rx.recv().await,
            None => Err(TransportError::Closed),
        }
    }

    fn kind(&self) -> &'static str {
        "resumable"
    }

    fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        self.split_prioritized()
    }

    fn split_prioritized(mut self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        let tx = match self.tx.take() {
            Some(tx) => tx,
            None => self.sender(),
        };
        let rx = self.rx.take().expect("a resumable link is split once");
        (tx, Box::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::crypto::EncryptedTransport;
    use crate::pairing::Role;

    use crate::resume::frame::seq_frame;
    use crate::test_util::MemTransport;
    use cursio_protocol::InputEvent;

    const KEY: &[u8] = b"a shared secret both ends hold";

    async fn path_pair(kind: PathKind) -> (Path, Path, SessionTag) {
        let (a, b) = MemTransport::pair();
        let (a, b) = tokio::join!(
            EncryptedTransport::establish(a, KEY, Role::Initiator),
            EncryptedTransport::establish(b, KEY, Role::Responder),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        let tag = a.session_tag();
        assert_eq!(
            tag,
            b.session_tag(),
            "both ends must name the session alike"
        );
        (a.into_path(kind), b.into_path(kind), tag)
    }

    struct End {
        tx: Box<dyn TransportTx>,
        rx: Box<dyn TransportRx>,
        handle: ResumeHandle,
    }

    async fn session() -> (End, End) {
        let (a, b, tag) = path_pair(PathKind::Network).await;
        let (link_a, handle_a) = ResumableLink::new(a, tag);
        let (link_b, handle_b) = ResumableLink::new(b, tag);
        for _ in 1..Lane::COUNT {
            let (pa, pb, _) = path_pair(PathKind::Network).await;
            let (ra, rb) = tokio::join!(handle_a.attach(pa), handle_b.attach(pb));
            ra.unwrap();
            rb.unwrap();
        }
        let (tx_a, rx_a) = Box::new(link_a).split_prioritized();
        let (tx_b, rx_b) = Box::new(link_b).split_prioritized();
        (
            End {
                tx: tx_a,
                rx: rx_a,
                handle: handle_a,
            },
            End {
                tx: tx_b,
                rx: rx_b,
                handle: handle_b,
            },
        )
    }

    fn key(n: u16) -> Message {
        Message::Input(InputEvent::Key {
            key: n,
            pressed: true,
        })
    }

    /// Cut every path of both ends, as a radio dropping would.
    async fn cut(a: &End, b: &End) {
        a.handle.network_lost();
        b.handle.network_lost();
        tokio::task::yield_now().await;
        assert_eq!(a.handle.paths(), Paths::default());
    }

    async fn rejoin(a: &End, b: &End) {
        let (pa, pb, _) = path_pair(PathKind::Network).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        ra.unwrap();
        rb.unwrap();
        while !a.handle.network_complete() {
            let (pa, pb, _) = path_pair(PathKind::Network).await;
            let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
            ra.unwrap();
            rb.unwrap();
        }
    }

    async fn next(end: &mut End) -> Message {
        tokio::time::timeout(Duration::from_secs(5), end.rx.recv())
            .await
            .expect("a message within five seconds")
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn idle_maintenance_does_not_look_like_video_or_bulk_payload() {
        let (a, _b) = session().await;
        tokio::time::sleep(ACK_EVERY + TICK).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            a.tx.queue_depth(cursio_protocol::Priority::Video)
                .sent_bytes,
            0
        );
        assert_eq!(
            a.tx.queue_depth(cursio_protocol::Priority::Bulk).sent_bytes,
            0
        );
    }

    struct BlockedTx;

    #[async_trait]
    impl TransportTx for BlockedTx {
        async fn send(&mut self, _: &Message) -> Result<(), TransportError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_blocked_bulk_socket_does_not_block_input_audio_or_video() {
        let (mut a, mut b) = session().await;
        let (mut pa, pb, _) = path_pair(PathKind::NetworkBulk).await;
        pa.tx = Box::new(BlockedTx);
        a.handle.0.install(pa);
        b.handle.0.install(pb);
        a.tx.send(&Message::Opaque(vec![7; 1_000_000]))
            .await
            .unwrap();
        // Enough frames to block both the socket writer and its bounded inbox.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let audio = Message::module(
            cursio_protocol::ModuleId::AUDIO,
            cursio_protocol::Priority::Audio,
            &42u32,
        )
        .unwrap();
        let video = Message::module(
            cursio_protocol::ModuleId::WINDOW,
            cursio_protocol::Priority::Video,
            &43u32,
        )
        .unwrap();
        a.tx.send(&key(8)).await.unwrap();
        a.tx.send(&audio).await.unwrap();
        a.tx.send(&video).await.unwrap();
        let mut received = Vec::new();
        for _ in 0..3 {
            received.push(
                tokio::time::timeout(Duration::from_millis(500), b.rx.recv())
                    .await
                    .expect("another lane waited on the bulk socket")
                    .unwrap(),
            );
        }
        assert!(received.contains(&key(8)));
        assert!(received.contains(&audio));
        assert!(received.contains(&video));
        assert!(!a.handle.is_ended());
    }

    #[tokio::test]
    async fn losing_only_the_bulk_path_preserves_and_resumes_its_data() {
        let (mut a, mut b) = session().await;
        a.handle.release(PathKind::NetworkBulk, "test");
        b.handle.release(PathKind::NetworkBulk, "test");
        assert!(!a.handle.network_complete());
        assert!(!a.tx.holding(), "the interactive path is still up");
        let data = Message::Opaque(vec![9; 200_000]);
        a.tx.send(&data).await.unwrap();
        a.tx.send(&key(11)).await.unwrap();
        assert_eq!(next(&mut b).await, key(11));
        rejoin(&a, &b).await;
        assert_eq!(next(&mut b).await, data);
        assert!(a.handle.network_complete());
    }

    #[tokio::test]
    async fn a_lane_missing_at_only_one_end_is_selected_without_replacing_control() {
        let (a, b) = session().await;
        let control = b
            .handle
            .0
            .lock()
            .paths
            .iter()
            .find(|p| p.kind == PathKind::Network)
            .unwrap()
            .id;
        // Keep the old far path alive so only the near side knows it is gone.
        let old = {
            let mut state = a.handle.0.lock();
            let index = state
                .paths
                .iter()
                .position(|p| p.kind == PathKind::NetworkBulk)
                .unwrap();
            state.paths.remove(index)
        };
        let (pa, pb, _) = path_pair(PathKind::Network).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        ra.unwrap();
        rb.unwrap();
        assert!(a.handle.network_complete() && b.handle.network_complete());
        assert_eq!(
            b.handle
                .0
                .lock()
                .paths
                .iter()
                .find(|p| p.kind == PathKind::Network)
                .unwrap()
                .id,
            control
        );
        drop(old);
    }

    #[tokio::test]
    async fn an_authenticated_path_cannot_join_a_different_session() {
        let (a, _) = session().await;
        let (b, _) = session().await;
        let (pa, pb, _) = path_pair(PathKind::Network).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        assert!(ra.is_err() && rb.is_err());
    }

    #[tokio::test]
    async fn independent_sequence_spaces_do_not_discard_a_slower_lane() {
        let (mut a, mut b) = session().await;
        for n in 0..100 {
            a.tx.send(&key(n)).await.unwrap();
        }
        for n in 0..100 {
            assert_eq!(next(&mut b).await, key(n));
        }
        let bulk = Message::Opaque(vec![1, 2, 3]);
        a.tx.send(&bulk).await.unwrap();
        assert_eq!(next(&mut b).await, bulk);
    }

    #[tokio::test]
    async fn messages_cross_a_resumable_link_in_order() {
        let (mut a, mut b) = session().await;
        for n in 0..50 {
            a.tx.send(&key(n)).await.unwrap();
        }
        for n in 0..50 {
            assert_eq!(next(&mut b).await, key(n));
        }
        b.tx.send(&Message::Bye).await.unwrap();
        assert_eq!(next(&mut a).await, Message::Bye);
    }

    #[tokio::test]
    async fn a_message_cut_into_full_frames_crosses_and_is_resent_whole() {
        let (mut a, mut b) = session().await;
        let big = Message::Opaque(vec![0x5A; 300_000]);
        a.tx.send(&big).await.unwrap();
        assert_eq!(next(&mut b).await, big);
        cut(&a, &b).await;
        a.tx.send(&big).await.unwrap();
        rejoin(&a, &b).await;
        assert_eq!(next(&mut b).await, big);
        a.tx.send(&key(1)).await.unwrap();
        assert_eq!(next(&mut b).await, key(1));
    }

    #[tokio::test]
    async fn a_session_continues_on_a_new_path_with_every_press_once_and_in_order() {
        let (mut a, mut b) = session().await;
        for n in 0..20 {
            a.tx.send(&key(n)).await.unwrap();
        }
        cut(&a, &b).await;
        // Sent with no path at all: kept, and resent once one joins.
        for n in 20..40 {
            a.tx.send(&key(n)).await.unwrap();
        }
        assert!(a.tx.holding(), "a link with no path holds the session");
        rejoin(&a, &b).await;
        for n in 40..60 {
            a.tx.send(&key(n)).await.unwrap();
        }
        for n in 0..60 {
            assert_eq!(next(&mut b).await, key(n));
        }
        assert!(!a.tx.holding());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), b.rx.recv())
                .await
                .is_err(),
            "a resend delivered something twice"
        );
    }

    #[tokio::test]
    async fn motion_from_while_the_path_was_gone_is_not_replayed() {
        let (mut a, mut b) = session().await;
        cut(&a, &b).await;
        for _ in 0..10 {
            a.tx.send(&Message::Input(InputEvent::PointerMotion {
                dx: 100,
                dy: 0,
            }))
            .await
            .unwrap();
        }
        a.tx.send(&key(1)).await.unwrap();
        rejoin(&a, &b).await;
        assert_eq!(
            next(&mut b).await,
            key(1),
            "stale motion arrived before the press"
        );
    }

    #[tokio::test]
    async fn a_newer_path_of_a_kind_replaces_the_older() {
        let (mut a, mut b) = session().await;
        rejoin(&a, &b).await;
        a.tx.send(&key(7)).await.unwrap();
        assert_eq!(next(&mut b).await, key(7));
        assert_eq!(
            a.handle.paths(),
            Paths {
                network: true,
                bluetooth: false,
                direct: false,
            }
        );
    }

    #[tokio::test]
    async fn a_direct_path_carries_beside_the_router_and_hands_back_when_lost() {
        let (mut a, mut b) = session().await;
        for _ in 0..Lane::COUNT {
            assert!(!a.handle.direct_complete());
            let (mut pa, mut pb, _) = path_pair(PathKind::Network).await;
            pa.direct = true;
            pb.direct = true;
            let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
            ra.unwrap();
            rb.unwrap();
        }
        assert!(a.handle.direct_complete());
        assert!(a.handle.network_complete());
        assert_eq!(a.handle.0.lock().paths.len(), 2 * Lane::COUNT);
        assert_eq!(a.handle.report().carrying.as_deref(), Some("direct"));
        a.tx.send(&key(3)).await.unwrap();
        assert_eq!(next(&mut b).await, key(3));

        a.handle.release_direct("the group went away");
        b.handle.release_direct("the group went away");
        assert!(!a.handle.paths().direct);
        assert!(a.handle.network_complete());
        assert_eq!(a.handle.report().carrying.as_deref(), Some("network"));
        a.tx.send(&key(4)).await.unwrap();
        assert_eq!(next(&mut b).await, key(4));
    }

    #[tokio::test]
    async fn the_network_carries_the_session_while_a_bluetooth_standby_waits() {
        let (mut a, mut b) = session().await;
        let (pa, pb, _) = path_pair(PathKind::Bluetooth).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        ra.unwrap();
        rb.unwrap();
        assert_eq!(
            a.handle.paths(),
            Paths {
                network: true,
                bluetooth: true,
                direct: false,
            }
        );
        a.handle.network_lost();
        b.handle.network_lost();
        a.tx.send(&key(3)).await.unwrap();
        assert_eq!(next(&mut b).await, key(3), "the standby did not take over");
        assert!(!a.tx.holding());
    }

    /// Holds back what is sent while muted and lets it through afterwards,
    /// as a TCP connection does across a radio blip.
    struct BlipTx {
        inner: Box<dyn TransportTx>,
        muted: Arc<std::sync::atomic::AtomicBool>,
        held: Vec<Message>,
    }

    #[async_trait]
    impl TransportTx for BlipTx {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            if self.muted.load(std::sync::atomic::Ordering::Relaxed) {
                self.held.push(msg.clone());
                return Ok(());
            }
            for held in std::mem::take(&mut self.held) {
                self.inner.send(&held).await?;
            }
            self.inner.send(msg).await
        }
    }

    /// A session whose `b` end can be muted on every network path.
    async fn blip_session() -> (End, End, Arc<std::sync::atomic::AtomicBool>) {
        let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = |mut path: Path| {
            path.tx = Box::new(BlipTx {
                inner: path.tx,
                muted: muted.clone(),
                held: Vec::new(),
            });
            path
        };
        let (a, b, tag) = path_pair(PathKind::Network).await;
        let (link_a, handle_a) = ResumableLink::new(a, tag);
        let (link_b, handle_b) = ResumableLink::new(gate(b), tag);
        for lane in [Lane::Audio, Lane::Video, Lane::Bulk] {
            let (pa, pb, _) = path_pair(PathKind::for_lane(lane)).await;
            let (ra, rb) = tokio::join!(handle_a.attach(pa), handle_b.attach(gate(pb)));
            ra.unwrap();
            rb.unwrap();
        }
        let (tx_a, rx_a) = Box::new(link_a).split_prioritized();
        let (tx_b, rx_b) = Box::new(link_b).split_prioritized();
        (
            End {
                tx: tx_a,
                rx: rx_a,
                handle: handle_a,
            },
            End {
                tx: tx_b,
                rx: rx_b,
                handle: handle_b,
            },
            muted,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_blip_resumes_on_the_same_connection_instead_of_closing_it() {
        let (mut a, b, muted) = blip_session().await;
        muted.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(ACTIVE_SILENCE + Duration::from_secs(3)).await;
        assert!(
            a.handle.paths().network,
            "a quiet network path is kept while a replacement is placed"
        );
        assert!(
            !a.handle.network_complete(),
            "and counts as missing, so the mesh places one"
        );
        muted.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut b = b;
        b.tx.send(&key(6)).await.unwrap();
        assert_eq!(next(&mut a).await, key(6));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            a.handle.network_complete(),
            "the old connection carries again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_network_path_silent_past_the_give_up_is_let_go_of() {
        let (a, _b, muted) = blip_session().await;
        muted.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(ACTIVE_SILENCE + NETWORK_GIVE_UP + Duration::from_secs(1)).await;
        assert!(!a.handle.paths().network);
    }

    #[tokio::test]
    async fn choosing_bluetooth_by_hand_carries_input_there_while_the_network_stands_by() {
        let (mut a, mut b) = session().await;
        let (pa, pb, _) = path_pair(PathKind::Bluetooth).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        ra.unwrap();
        rb.unwrap();
        assert_eq!(a.handle.report().carrying.as_deref(), Some("network"));

        a.handle.choose(PathChoice::Bluetooth);
        let report = a.handle.report();
        assert_eq!(report.carrying.as_deref(), Some("bluetooth"));
        assert_eq!(report.choice, PathChoice::Bluetooth);
        assert!(report.network && report.bluetooth);
        a.tx.send(&key(5)).await.unwrap();
        assert_eq!(next(&mut b).await, key(5));

        a.handle.choose(PathChoice::Auto);
        assert_eq!(a.handle.report().carrying.as_deref(), Some("network"));
    }

    #[tokio::test]
    async fn a_large_message_crosses_bluetooth_in_small_frames_and_arrives_whole() {
        let (mut a, mut b) = session().await;
        let (pa, pb, _) = path_pair(PathKind::Bluetooth).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        ra.unwrap();
        rb.unwrap();
        a.handle.network_lost();
        b.handle.network_lost();

        let image = Message::Opaque((0..300_000u32).map(|i| (i % 251) as u8).collect());
        a.tx.send(&image).await.unwrap();
        a.tx.send(&key(4)).await.unwrap();
        let received = [next(&mut b).await, next(&mut b).await];
        assert!(received.contains(&image), "the image did not arrive whole");
        assert!(received.contains(&key(4)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_with_no_path_ends_once_the_hold_runs_out() {
        let (a, mut b) = session().await;
        cut(&a, &b).await;
        tokio::time::sleep(HOLD + Duration::from_secs(1)).await;
        assert!(a.handle.is_ended());
        assert!(
            b.rx.recv().await.is_err(),
            "the receiver must end with the session"
        );
    }

    #[tokio::test]
    async fn a_resend_that_crosses_a_live_frame_is_delivered_once() {
        let (_a, b) = session().await;
        let End { rx, handle, .. } = b;
        drop(rx);
        let inbound = std::array::from_fn(|_| mpsc::channel(16).1);
        let mut rx = ResumableRx {
            inner: handle.0.clone(),
            inbound,
            reasm: std::array::from_fn(|_| Reassembler::default()),
            lossy_parts: std::array::from_fn(|_| HashSet::new()),
            last_path: [None; Lane::COUNT],
        };
        let whole = |msg: &Message| {
            let mut tagged = vec![0u8];
            tagged.extend(msg.to_bytes().unwrap());
            tagged
        };
        let frame = seq_frame(Lane::Interactive, 1, false, &whole(&key(1)));
        assert_eq!(rx.accept(1, &frame).unwrap(), Some(key(1)));
        assert_eq!(
            rx.accept(2, &frame).unwrap(),
            None,
            "the copy was delivered too"
        );
    }
}
