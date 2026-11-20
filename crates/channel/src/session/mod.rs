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
//! ([`Delivery::Fresh`](crate::Delivery::Fresh)). They are
//! numbered but never kept: after a five-second gap, replaying them would jump
//! the cursor by everything it moved meanwhile and play old sound over new,
//! which is Mosh's point that a mobile link should deliver the latest state
//! rather than the history. A chunked lossy message cut off mid-way is dropped
//! by the receiver once frames start arriving on another path.
//!
//! ## Paths
//!
//! Each lane has one path of its own ([`PathKind::Lane`]), with a shared
//! standby that can carry every lane ([`PathKind::Standby`]; Cursio's is
//! Bluetooth). Each lane numbers and acknowledges its own frames so an
//! independent socket can make progress without waiting for another lane. A
//! newer path replaces the older one for that lane. A standby does not carry
//! the fresh frames of a lane declared
//! [`standby_skips_fresh`](crate::Lane::standby_skips_fresh) — pixels.
//! A lane path may also be marked *direct*, over a shorter route that may go
//! away (a Wi-Fi Direct group beside the router): it carries while it is
//! healthy, and the routed one stands beside it.
//!
//! A path carrying the session is passed over when it falls silent for three
//! seconds, since it hears an `ACK` every second. A lane path is then kept,
//! suspect, while the caller places a replacement, and let go of only after
//! fifteen: a blip shorter than that resumes on the old connection, and a
//! replacement that joins first replaces it. Standby paths are let go of at
//! their own silences. A path that is alive but not delivering —
//! frames sent on it stay unacknowledged for seconds while another path
//! exists — is *degraded*: the session moves to the other path and comes back
//! only after a while, so a marginal link does not make it flap. That is what
//! Linux's multipath TCP learned to do with its stale subflows.
//!
//! With no interactive path left, the link holds the session for [`HOLD`] and ends
//! it, as the old link ended on a dead connection.
//!
//! ## One door in, one way out
//!
//! Every connection joins through [`ResumeHandle::attach`], where a session
//! bound to an identity re-proves the joining connection (`bind`); every end
//! goes through one place that carries its reason (`Inner::end`), whether the
//! hold ran out, the caller ended it, or a trust withdrew, and it is reported
//! to the application as [`SessionEvent::Ended`].

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

// Tokio's clock, so a test can run the hold out without waiting a minute.
use tokio::time::Instant;

use tokio::sync::{mpsc, Notify};

use crate::scheduler::{Carrying, FrameSink, PrioritizedTx};
use crate::seal::{link_tag, Seal};
use crate::session::frame::read_delivered;
use crate::session::frame::resume_frame;
use crate::session::inner::Inner;
use crate::session::send::ResumableSink;
use crate::session::state::State;
use crate::session::supervise::supervise;
use crate::{
    CarrierRx, CarrierTx, ChannelError, Classes, Envelope, LaneId, PathChoice, PathReport,
    SessionTag,
};

mod authorize;
mod bind;
mod congestion;
mod events;
mod frame;
mod inner;
mod ledger;
mod receive;
mod send;
mod state;
mod supervise;

pub use authorize::Authorization;
pub use bind::{offered_tag, SessionOptions, CHECK_EVERY};
pub use events::{SessionEvent, SessionEvents, Unauthorized};
pub use receive::ResumableRx;

use crate::carrier::TlsInfo;
use crate::session::bind::Binding;
use crate::session::ledger::Ledger;
use crate::trust::Verified;

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
/// As [`ACTIVE_SILENCE`], for a standby path while it carries. Cursio's is
/// Bluetooth, whose radio pauses for seconds while it pages another device or
/// opens a profile, and a standby is usually the only path left: letting it
/// go at three seconds ended it in the middle of every session's opening
/// burst, and each new path then resent everything unacknowledged into the
/// next pause. A dead link still ends at the radio's own supervision timeout,
/// as a read error.
const STANDBY_CARRYING_SILENCE: Duration = Duration::from_secs(10);

/// The largest frame while a standby carries the interactive lane. A frame
/// cannot be interrupted once on the wire, and at Bluetooth's speed a full
/// 64 KiB one held every pointer move behind it for most of a second; 4 KiB
/// is tens of milliseconds there, for about 1% more framing.
const STANDBY_FRAME: usize = 4 * 1024;
/// Silence after which a standby path is let go of: three keepalives.
const STANDBY_SILENCE: Duration = Duration::from_secs(45);

/// A frame unacknowledged this long on the carrying path, with another path
/// alive, marks the carrying path degraded.
const STALL: Duration = Duration::from_secs(3);
/// As [`STALL`], for input on the interactive lane's path while a standby or
/// a routed path stands by. Input is what the person is waiting on, and a LAN answers in
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

/// What a path carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// One lane's own carrier.
    Lane(LaneId),
    /// A carrier that can take every lane, standing by while the lanes' own
    /// carriers are there.
    Standby,
}

impl PathKind {
    /// The interactive lane's own carrier.
    pub const INTERACTIVE: Self = Self::Lane(LaneId::INTERACTIVE);

    pub fn lane(self) -> Option<LaneId> {
        match self {
            Self::Lane(lane) => Some(lane),
            Self::Standby => None,
        }
    }
}

/// One connection, its seal already established, ready to carry a session.
pub struct Path {
    pub tx: Box<dyn CarrierTx>,
    pub rx: Box<dyn CarrierRx>,
    pub seal: Seal,
    /// What it offers to carry. A lane path that joins a running session may
    /// be given another lane: the one both ends find missing.
    pub kind: PathKind,
    /// Over a shorter route that may go away, rather than through the router.
    pub direct: bool,
    /// Who the connection's opening proved, for a session bound to an
    /// identity; see [`Self::proving`].
    pub peer: Option<Verified>,
    /// What the TLS under the connection verified, taken from the carrier
    /// before it was split.
    pub tls: Option<Arc<TlsInfo>>,
}

impl Path {
    pub fn new(carrier: Box<dyn crate::Carrier>, seal: Seal, kind: PathKind, direct: bool) -> Self {
        let tls = carrier.tls().cloned().map(Arc::new);
        let (tx, rx) = carrier.split();
        Self {
            tx,
            rx,
            seal,
            kind,
            direct,
            peer: None,
            tls,
        }
    }

    /// The connection proved `peer`; a bound session admits it only as the
    /// identity the session was opened by.
    pub fn proving(mut self, peer: Verified) -> Self {
        self.peer = Some(peer);
        self
    }

    pub fn kind(&self) -> PathKind {
        self.kind
    }

    pub fn is_direct(&self) -> bool {
        self.direct
    }
}

/// Which kinds of path carry a session now; reported whenever it changes so
/// the caller can dial a path back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Paths {
    /// The interactive lane has a path of its own.
    pub network: bool,
    /// A standby is among them; named after Cursio's, as in [`PathReport`].
    pub bluetooth: bool,
    /// The interactive lane has a direct path.
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

    /// Whether every lane has a healthy path through the router. Direct
    /// paths do not count: they last only while the two machines are near,
    /// and a session whose lanes were all direct had nothing to fall back
    /// on but Bluetooth when the group went (seen: the router's paths were
    /// never placed again after a restart, since every lane looked filled).
    pub fn network_complete(&self) -> bool {
        self.0.lock().missing_lane(Some(false)).is_none()
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
                .carrying(LaneId::INTERACTIVE, Instant::now())
                .map(|path| match path.kind {
                    PathKind::Standby => self.0.classes.standby_name().to_lowercase(),
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

    /// The system says the network went away: let every lane path go now
    /// instead of after its silence, so a standby takes over at once.
    pub fn network_lost(&self) {
        for lane in self.0.classes.lane_ids() {
            self.release(PathKind::Lane(lane), "the system reported the network gone");
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

    /// Let every path of `kind` go: a standby once the lanes' own carriers
    /// carry the session again, unless it is kept standing by.
    pub fn release(&self, kind: impl Into<PathKind>, why: &str) {
        let kind = kind.into();
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
    pub async fn attach(&self, path: Path) -> Result<(), ChannelError> {
        let Path {
            mut tx,
            mut rx,
            seal,
            kind,
            direct,
            peer,
            tls,
        } = path;
        if let Some(binding) = &self.0.binding {
            let now = std::time::SystemTime::now();
            if let Err(why) = binding.admit(peer.as_ref(), now) {
                tracing::warn!(session = %self.0.label(), %why, "a connection was refused the session");
                return Err(ChannelError::Other(format!(
                    "the connection may not join: {why}"
                )));
            }
            if !self.0.verify_peer() {
                return Err(ChannelError::Other("the session is over".into()));
            }
        }
        let offered = offered_tag(&self.tag(), tls.as_ref().and_then(|t| t.exporter.as_ref()));
        let classes = &self.0.classes;
        // Both ends mark the connection alike (each sees its own address on
        // the group), so both propose a lane missing from the same set.
        let (delivered, proposed) = {
            let state = self.0.lock();
            if let Some(why) = &state.ended {
                return Err(ChannelError::Other(format!("the session is over: {why}")));
            }
            let class = (kind != PathKind::Standby).then_some(direct);
            (state.delivered(), state.missing_lane(class))
        };
        let mut resume = resume_frame(&delivered, proposed);
        resume.extend_from_slice(&offered);
        tx.send(seal.seal(&resume)?).await?;
        let answer = tokio::time::timeout(RESUME_WAIT, rx.recv())
            .await
            .map_err(|_| {
                ChannelError::Other("the peer did not say where it had got to".into())
            })??;
        let tagged = seal.open(&answer)?;
        if tagged.first() != Some(&link_tag::RESUME) {
            return Err(ChannelError::Other(
                "the joining path did not open with RESUME".into(),
            ));
        }
        let snapshot_len = 1 + classes.lanes().len() * 8 + 1;
        if tagged.len() != snapshot_len + offered.len() || tagged[snapshot_len..] != offered {
            return Err(ChannelError::Other(
                "the path belongs to a different session".into(),
            ));
        }
        let peer_delivered = read_delivered(&tagged[1..snapshot_len], classes.lanes().len())?;
        let peer_proposed = tagged[snapshot_len - 1];
        if peer_proposed != u8::MAX && classes.decode_lane(peer_proposed).is_none() {
            return Err(ChannelError::Other("invalid resume lane".into()));
        }
        // A complete peer proposes 255, not lane zero: when just one end
        // noticed a failed bulk path, that must still select the bulk lane.
        let selected = proposed.map_or(u8::MAX, LaneId::get).min(peer_proposed);
        let lane = classes.decode_lane(selected).unwrap_or(LaneId::INTERACTIVE);
        let kind = match kind {
            PathKind::Standby => kind,
            PathKind::Lane(_) => PathKind::Lane(lane),
        };
        self.0.acknowledged(&peer_delivered);
        self.0.install(Path {
            tx,
            rx,
            seal,
            kind,
            direct,
            peer: None,
            tls,
        });
        Ok(())
    }

    /// Whether `offer`, which a connection's opening exchange named, names
    /// this session over `path`; see [`offered_tag`].
    pub fn named_by(&self, offer: &SessionTag, path: &Path) -> bool {
        let expected = offered_tag(
            &self.tag(),
            path.tls.as_ref().and_then(|t| t.exporter.as_ref()),
        );
        // Not a secret comparison worth hiding the timing of: a wrong offer
        // only makes this end refuse, and the join itself re-proves both ends.
        expected == *offer
    }

    /// When the session ends unless the peer proves itself again; `None` for
    /// a session no trust binds, or one whose proofs do not expire.
    pub fn deadline(&self) -> Option<std::time::SystemTime> {
        self.0.binding.as_ref().and_then(Binding::deadline)
    }

    /// Send the peer a fresh proof of this end's identity (`REAUTH`), to
    /// carry the session past the deadline the last one set. Over the path
    /// carrying the interactive lane; an error when there is none, so the
    /// caller tries again once a path is back.
    pub async fn reauthenticate(&self, proof: Vec<u8>) -> Result<(), ChannelError> {
        let writer = {
            let state = self.0.lock();
            if let Some(why) = &state.ended {
                return Err(ChannelError::Other(format!("the session is over: {why}")));
            }
            state
                .carrying(LaneId::INTERACTIVE, Instant::now())
                .map(|path| path.control().clone())
        };
        let writer = writer.ok_or_else(|| {
            ChannelError::Other("no path carries the session to send a fresh proof on".into())
        })?;
        let mut frame = Vec::with_capacity(1 + proof.len());
        frame.push(link_tag::REAUTH);
        frame.extend_from_slice(&proof);
        writer.send(frame).await.map_err(|_| ChannelError::Closed)
    }
}

/// A session that can change its connection.
pub struct ResumableLink<E> {
    inner: Arc<Inner>,
    rx: Option<ResumableRx<E>>,
    tx: Option<PrioritizedTx<E>>,
    events: Option<SessionEvents<E>>,
}

impl<E: Envelope> ResumableLink<E> {
    /// Start a session over `first`, known to both ends as `tag`. Spawns its
    /// tasks, so it must run inside the runtime the session will.
    pub fn new(first: Path, tag: SessionTag, classes: Arc<Classes>) -> (Self, ResumeHandle) {
        Self::with_options(first, tag, classes, SessionOptions::default())
            .expect("a session no trust binds asks nothing of its first path")
    }

    /// As [`Self::new`], run as `options` say. A session bound to a trust
    /// refuses a first path that proved no identity, or one the trust no
    /// longer stands behind.
    pub fn with_options(
        first: Path,
        tag: SessionTag,
        classes: Arc<Classes>,
        options: SessionOptions,
    ) -> Result<(Self, ResumeHandle), ChannelError> {
        let binding = match options.trust {
            Some(trust) => {
                let peer = first.peer.clone().ok_or_else(|| {
                    ChannelError::Other("a bound session's first path proved no identity".into())
                })?;
                let binding = Binding::new(trust, peer, options.check_every);
                binding
                    .verify(std::time::SystemTime::now())
                    .map_err(ChannelError::Other)?;
                Some(binding)
            }
            None => None,
        };
        if options.authorization.is_some() && binding.is_none() {
            return Err(ChannelError::Other(
                "an authorization policy needs a session bound to an identity".into(),
            ));
        }
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (inbound_tx, inbound): (Vec<_>, Vec<_>) =
            classes.lanes().iter().map(|_| mpsc::channel(16)).unzip();
        let inner = Arc::new(Inner {
            state: Mutex::new(State::new(classes.lanes().len())),
            classes,
            changed: Notify::new(),
            inbound: inbound_tx,
            tag,
            observer: Mutex::new(None),
            congestion: Mutex::new(congestion::BackgroundRate::default()),
            binding,
            ledger: Arc::new(Ledger::new(options.hand_back, events_tx.clone())),
            events: events_tx,
            authorization: options.authorization,
        });
        inner.install(first);
        live_links()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::downgrade(&inner));
        tokio::spawn(supervise(Arc::downgrade(&inner)));
        let rx = ResumableRx::new(inner.clone(), inbound);
        let handle = ResumeHandle(inner.clone());
        Ok((
            Self {
                inner,
                rx: Some(rx),
                tx: None,
                events: Some(SessionEvents {
                    rx: events_rx,
                    envelope: std::marker::PhantomData,
                }),
            },
            handle,
        ))
    }

    /// What the session reports besides its messages; see [`SessionEvent`].
    /// Taken once, before [`Self::split`], which drops it otherwise.
    pub fn events(&mut self) -> Option<SessionEvents<E>> {
        self.events.take()
    }

    fn sender(&mut self) -> PrioritizedTx<E> {
        let sinks: Vec<Box<dyn FrameSink>> = self
            .inner
            .classes
            .lane_ids()
            .map(|lane| {
                Box::new(ResumableSink {
                    inner: self.inner.clone(),
                    lane,
                    last_path: None,
                    mirror: None,
                    next_bulk: Instant::now(),
                }) as Box<dyn FrameSink>
            })
            .collect();
        let weak = Arc::downgrade(&self.inner);
        let holding = Arc::new(move || {
            weak.upgrade().is_some_and(|inner| {
                let state = inner.lock();
                state.ended.is_none()
                    && state
                        .carrying(LaneId::INTERACTIVE, Instant::now())
                        .is_none()
            })
        });
        let weak = Arc::downgrade(&self.inner);
        let frame_limit = Arc::new(move || {
            let on_standby = weak.upgrade().is_some_and(|inner| {
                inner
                    .lock()
                    .carrying(LaneId::INTERACTIVE, Instant::now())
                    .is_some_and(|path| path.kind == PathKind::Standby)
            });
            if on_standby {
                STANDBY_FRAME
            } else {
                usize::MAX
            }
        });
        let weak = Arc::downgrade(&self.inner);
        let carrying = Arc::new(move || {
            let inner = weak.upgrade()?;
            let state = inner.lock();
            let path = state.carrying(LaneId::INTERACTIVE, Instant::now())?;
            Some(match path.kind {
                PathKind::Standby => Carrying::Standby,
                _ if path.direct => Carrying::Direct,
                _ => Carrying::Network,
            })
        });
        let weak = Arc::downgrade(&self.inner);
        let input_seen = Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.congestion.lock().unwrap().note_input(Instant::now());
            }
        });
        let tx = PrioritizedTx::spawn_lanes(sinks, self.inner.classes.clone())
            .with_holding(holding)
            .with_carrying_path(carrying)
            .with_input_seen(input_seen)
            .with_frame_limit(frame_limit)
            .with_ledger(self.inner.ledger.clone());
        self.inner.lock().kick = Some(tx.kick_handle());
        tx
    }

    pub async fn send(&mut self, msg: &E) -> Result<(), ChannelError> {
        if self.tx.is_none() {
            self.tx = Some(self.sender());
        }
        match self.tx.as_mut() {
            Some(tx) => tx.send(msg).await,
            None => Err(ChannelError::Closed),
        }
    }

    pub async fn recv(&mut self) -> Result<E, ChannelError> {
        match self.rx.as_mut() {
            Some(rx) => rx.recv().await,
            None => Err(ChannelError::Closed),
        }
    }

    /// The scheduled sender and the receiver, to run apart.
    pub fn split(mut self) -> (PrioritizedTx<E>, ResumableRx<E>) {
        let tx = match self.tx.take() {
            Some(tx) => tx,
            None => self.sender(),
        };
        let rx = self.rx.take().expect("a resumable link is split once");
        (tx, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::mem::MemCarrier;
    use crate::session::frame::seq_frame;
    use crate::test_util::*;
    use crate::{Carrier, NoisePsk, Role, Trust};
    use async_trait::async_trait;

    async fn path_pair(kind: PathKind) -> (Path, Path, SessionTag) {
        let (mut a, mut b) = MemCarrier::pair();
        let trust = NoisePsk::from_secret(b"test", b"a shared secret both ends hold");
        let (oa, ob) = tokio::join!(
            trust.open(&mut a, Role::Initiator),
            trust.open(&mut b, Role::Responder),
        );
        let (oa, ob) = (oa.unwrap(), ob.unwrap());
        assert_eq!(
            oa.session_tag, ob.session_tag,
            "both ends must name the session alike"
        );
        let tag = oa.session_tag;
        let a: Box<dyn Carrier> = Box::new(a);
        let b: Box<dyn Carrier> = Box::new(b);
        (
            Path::new(a, oa.seal, kind, false),
            Path::new(b, ob.seal, kind, false),
            tag,
        )
    }

    struct End {
        tx: PrioritizedTx<Msg>,
        rx: ResumableRx<Msg>,
        handle: ResumeHandle,
    }

    async fn session() -> (End, End) {
        let (a, b, tag) = path_pair(PathKind::INTERACTIVE).await;
        let (link_a, handle_a) = ResumableLink::new(a, tag, classes());
        let (link_b, handle_b) = ResumableLink::new(b, tag, classes());
        for _ in 1..4 {
            let (pa, pb, _) = path_pair(PathKind::INTERACTIVE).await;
            let (ra, rb) = tokio::join!(handle_a.attach(pa), handle_b.attach(pb));
            ra.unwrap();
            rb.unwrap();
        }
        let (tx_a, rx_a) = link_a.split();
        let (tx_b, rx_b) = link_b.split();
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

    fn key(n: u16) -> Msg {
        Msg::Key(n)
    }

    /// Cut every path of both ends, as a radio dropping would.
    async fn cut(a: &End, b: &End) {
        a.handle.network_lost();
        b.handle.network_lost();
        tokio::task::yield_now().await;
        assert_eq!(a.handle.paths(), Paths::default());
    }

    async fn rejoin(a: &End, b: &End) {
        let (pa, pb, _) = path_pair(PathKind::INTERACTIVE).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        ra.unwrap();
        rb.unwrap();
        while !a.handle.network_complete() {
            let (pa, pb, _) = path_pair(PathKind::INTERACTIVE).await;
            let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
            ra.unwrap();
            rb.unwrap();
        }
    }

    async fn next(end: &mut End) -> Msg {
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
        assert_eq!(a.tx.queue_depth(VIDEO).sent_bytes, 0);
        assert_eq!(a.tx.queue_depth(BULK).sent_bytes, 0);
    }

    struct BlockedTx;

    #[async_trait]
    impl CarrierTx for BlockedTx {
        async fn send(&mut self, _: Vec<u8>) -> Result<(), ChannelError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_blocked_bulk_socket_does_not_block_input_audio_or_video() {
        let (mut a, mut b) = session().await;
        let (mut pa, pb, _) = path_pair(PathKind::Lane(BULK_LANE)).await;
        pa.tx = Box::new(BlockedTx);
        a.handle.0.install(pa);
        b.handle.0.install(pb);
        a.tx.send(&bulk(1_000_000)).await.unwrap();
        // Enough frames to block both the socket writer and its bounded inbox.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let audio = Msg::Blob(AUDIO, 42u32.to_be_bytes().to_vec());
        let video = Msg::Blob(VIDEO, 43u32.to_be_bytes().to_vec());
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
        a.handle.release(PathKind::Lane(BULK_LANE), "test");
        b.handle.release(PathKind::Lane(BULK_LANE), "test");
        assert!(!a.handle.network_complete());
        assert!(!a.tx.holding(), "the interactive path is still up");
        let data = Msg::Blob(BULK, vec![9; 200_000]);
        a.tx.send(&data).await.unwrap();
        a.tx.send(&key(11)).await.unwrap();
        assert_eq!(next(&mut b).await, key(11));
        rejoin(&a, &b).await;
        assert_eq!(next(&mut b).await, data);
        assert!(a.handle.network_complete());
    }

    #[tokio::test(start_paused = true)]
    async fn a_resend_buffer_filled_while_held_is_written_on_the_path_that_comes_back() {
        let (a, mut b) = session().await;
        cut(&a, &b).await;
        let blob = |n: u8| Msg::Blob(BULK, vec![n; 1024 * 1024]);
        let mut tx = a.tx.clone();
        let upload = tokio::spawn(async move {
            for n in 0..24u8 {
                tx.send(&blob(n)).await.unwrap();
            }
        });
        // Well past the resend cap: the lane's sink now waits for room.
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        rejoin(&a, &b).await;
        for n in 0..24u8 {
            let got = tokio::time::timeout(Duration::from_secs(60), b.rx.recv())
                .await
                .expect("the held upload moves once a path is back")
                .unwrap();
            assert_eq!(got, blob(n));
        }
        upload.await.unwrap();
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
            .find(|p| p.kind == PathKind::INTERACTIVE)
            .unwrap()
            .id;
        // Keep the old far path alive so only the near side knows it is gone.
        let old = {
            let mut state = a.handle.0.lock();
            let index = state
                .paths
                .iter()
                .position(|p| p.kind == PathKind::Lane(BULK_LANE))
                .unwrap();
            state.paths.remove(index)
        };
        let (pa, pb, _) = path_pair(PathKind::INTERACTIVE).await;
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
                .find(|p| p.kind == PathKind::INTERACTIVE)
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
        let (pa, pb, _) = path_pair(PathKind::INTERACTIVE).await;
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
        let small = Msg::Blob(BULK, vec![1, 2, 3]);
        a.tx.send(&small).await.unwrap();
        assert_eq!(next(&mut b).await, small);
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
        b.tx.send(&Msg::Heartbeat(0)).await.unwrap();
        assert_eq!(next(&mut a).await, Msg::Heartbeat(0));
    }

    #[tokio::test]
    async fn a_message_cut_into_full_frames_crosses_and_is_resent_whole() {
        let (mut a, mut b) = session().await;
        let big = Msg::Blob(BULK, vec![0x5A; 300_000]);
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
            a.tx.send(&Msg::Motion(100, 0)).await.unwrap();
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

    async fn attach_direct(a: &ResumeHandle, b: &ResumeHandle) {
        let (mut pa, mut pb, _) = path_pair(PathKind::INTERACTIVE).await;
        pa.direct = true;
        pb.direct = true;
        let (ra, rb) = tokio::join!(a.attach(pa), b.attach(pb));
        ra.unwrap();
        rb.unwrap();
    }

    #[tokio::test]
    async fn a_direct_path_carries_beside_the_router_and_hands_back_when_lost() {
        let (mut a, mut b) = session().await;
        for _ in 0..4 {
            assert!(!a.handle.direct_complete());
            attach_direct(&a.handle, &b.handle).await;
        }
        assert!(a.handle.direct_complete());
        assert!(a.handle.network_complete());
        assert_eq!(a.handle.0.lock().paths.len(), 2 * 4);
        assert_eq!(a.handle.report().carrying.as_deref(), Some("direct"));
        assert_eq!(a.tx.carrying_path(), Some(Carrying::Direct));
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
    async fn a_session_on_direct_paths_alone_still_wants_the_routers() {
        let (mut first_a, mut first_b, tag) = path_pair(PathKind::INTERACTIVE).await;
        first_a.direct = true;
        first_b.direct = true;
        let (_link_a, a) = ResumableLink::<Msg>::new(first_a, tag, classes());
        let (_link_b, b) = ResumableLink::<Msg>::new(first_b, tag, classes());
        for _ in 1..4 {
            attach_direct(&a, &b).await;
        }
        assert!(a.direct_complete());
        assert!(
            !a.network_complete(),
            "the router's paths are the standby and must still be placed"
        );
    }

    #[tokio::test]
    async fn input_copied_onto_the_routers_path_arrives_once_and_in_order() {
        let (mut a, mut b) = session().await;
        for _ in 0..4 {
            attach_direct(&a.handle, &b.handle).await;
        }
        for n in 0..200u16 {
            a.tx.send(&key(n)).await.unwrap();
        }
        for n in 0..200u16 {
            assert_eq!(next(&mut b).await, key(n));
        }
        a.tx.send(&key(999)).await.unwrap();
        assert_eq!(next(&mut b).await, key(999), "no copy left over");
    }

    async fn attach_standby(a: &End, b: &End) {
        let (pa, pb, _) = path_pair(PathKind::Standby).await;
        let (ra, rb) = tokio::join!(a.handle.attach(pa), b.handle.attach(pb));
        ra.unwrap();
        rb.unwrap();
    }

    #[tokio::test]
    async fn the_network_carries_the_session_while_a_bluetooth_standby_waits() {
        let (mut a, mut b) = session().await;
        attach_standby(&a, &b).await;
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
        assert_eq!(a.tx.carrying_path(), Some(Carrying::Standby));
    }

    #[tokio::test]
    async fn choosing_bluetooth_by_hand_carries_input_there_while_the_network_stands_by() {
        let (mut a, mut b) = session().await;
        attach_standby(&a, &b).await;
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
        attach_standby(&a, &b).await;
        a.handle.network_lost();
        b.handle.network_lost();
        let image = Msg::Blob(BULK, (0..300_000u32).map(|i| (i % 251) as u8).collect());
        a.tx.send(&image).await.unwrap();
        a.tx.send(&key(4)).await.unwrap();
        let received = [next(&mut b).await, next(&mut b).await];
        assert!(received.contains(&image), "the image did not arrive whole");
        assert!(received.contains(&key(4)));
    }

    #[tokio::test]
    async fn a_standby_does_not_carry_the_fresh_frames_of_a_lane_that_skips_them() {
        let (mut a, mut b) = session().await;
        attach_standby(&a, &b).await;
        a.handle.network_lost();
        b.handle.network_lost();
        a.tx.send(&Msg::Pixels(vec![1; 100])).await.unwrap();
        let reliable = Msg::Blob(VIDEO, vec![2; 100]);
        a.tx.send(&reliable).await.unwrap();
        assert_eq!(next(&mut b).await, reliable);
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
        let inbound = (0..4).map(|_| mpsc::channel(16).1).collect();
        let mut rx = ResumableRx::<Msg>::new(handle.0.clone(), inbound);
        let mut whole = vec![0u8];
        whole.extend(key(1).encode().unwrap());
        let frame = seq_frame(INTERACTIVE_LANE, 1, false, &whole);
        assert_eq!(rx.accept(1, &frame).unwrap(), Some(key(1)));
        assert_eq!(
            rx.accept(2, &frame).unwrap(),
            None,
            "the copy was delivered too"
        );
    }

    /// Holds back what is sent while muted and lets it through afterwards,
    /// as a TCP connection does across a radio blip.
    struct BlipTx {
        inner: Box<dyn CarrierTx>,
        muted: Arc<std::sync::atomic::AtomicBool>,
        held: Vec<Vec<u8>>,
    }

    #[async_trait]
    impl CarrierTx for BlipTx {
        async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
            if self.muted.load(std::sync::atomic::Ordering::Relaxed) {
                self.held.push(frame);
                return Ok(());
            }
            for held in std::mem::take(&mut self.held) {
                self.inner.send(held).await?;
            }
            self.inner.send(frame).await
        }
    }

    /// A session whose `b` end can be muted on every lane path.
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
        let (a, b, tag) = path_pair(PathKind::INTERACTIVE).await;
        let (link_a, handle_a) = ResumableLink::new(a, tag, classes());
        let (link_b, handle_b) = ResumableLink::new(gate(b), tag, classes());
        for lane in [AUDIO_LANE, VIDEO_LANE, BULK_LANE] {
            let (pa, pb, _) = path_pair(PathKind::Lane(lane)).await;
            let (ra, rb) = tokio::join!(handle_a.attach(pa), handle_b.attach(gate(pb)));
            ra.unwrap();
            rb.unwrap();
        }
        let (tx_a, rx_a) = link_a.split();
        let (tx_b, rx_b) = link_b.split();
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
        let (mut a, mut b, muted) = blip_session().await;
        muted.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(ACTIVE_SILENCE + Duration::from_secs(3)).await;
        assert!(
            a.handle.paths().network,
            "a quiet lane path is kept while a replacement is placed"
        );
        assert!(
            !a.handle.network_complete(),
            "and counts as missing, so the caller places one"
        );
        muted.store(false, std::sync::atomic::Ordering::Relaxed);
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
}
