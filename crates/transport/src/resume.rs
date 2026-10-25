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
//! A path is let go of when it falls silent: three seconds for the one
//! carrying the session, which hears an `ACK` every second, and longer for a
//! standby that hears one rarely. A path that is alive but not delivering —
//! frames sent on it stay unacknowledged for seconds while another path
//! exists — is *degraded*: the session moves to the other path and comes back
//! only after a while, so a marginal link does not make it flap. That is what
//! Linux's multipath TCP learned to do with its stale subflows.
//!
//! With no interactive path left, the link holds the session for [`HOLD`] and ends
//! it, as the old link ended on a dead connection.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

// Tokio's clock, so a test can run the hold out without waiting a minute.
use tokio::time::Instant;

use async_trait::async_trait;
use cursio_protocol::{Message, SessionTag};
use tokio::sync::{mpsc, Notify};

use crate::crypto::{link_tag, open_frame, part_message_id, seal_frame, Reassembler, SharedNoise};
use crate::lane::Lane;
use crate::priority::{FrameSink, OutFrame, PrioritizedTx, QueueKick};
use crate::{Transport, TransportError, TransportRx, TransportTx};

/// How long a session waits, with no path at all, for one to come back.
pub const HOLD: Duration = Duration::from_secs(60);

/// How often an `ACK` goes out on the path carrying the session even with
/// nothing new to acknowledge: the path's keepalive.
const ACK_EVERY: Duration = Duration::from_secs(1);
/// Soonest an `ACK` follows delivered frames.
const ACK_SOON: Duration = Duration::from_millis(50);
/// How often every path, standby included, hears a keepalive.
const STANDBY_KEEPALIVE: Duration = Duration::from_secs(15);

/// Silence after which the path carrying the session is let go of.
const ACTIVE_SILENCE: Duration = Duration::from_secs(3);
/// Silence after which a standby path is let go of: three keepalives.
const STANDBY_SILENCE: Duration = Duration::from_secs(45);

/// A frame unacknowledged this long on the carrying path, with another path
/// alive, marks the carrying path degraded.
const STALL: Duration = Duration::from_secs(3);
/// How long a degraded path is passed over.
const DEGRADED_FOR: Duration = Duration::from_secs(10);

/// How long a joining path waits for the other side's `RESUME`.
const RESUME_WAIT: Duration = Duration::from_secs(5);

/// Reliable bytes retained per lane. The lane waits for ACKs at this bound;
/// other lane workers keep running.
const RESEND_CAP: usize = 16 * 1024 * 1024;

/// How often the supervisor looks at the paths.
const TICK: Duration = Duration::from_millis(100);

/// `ACK` flag: this one is a keepalive for every path, not only the carrying one.
const ACK_ALL_PATHS: u8 = 0x01;
/// `SEQ` flag: the frame is from a lossy message.
const SEQ_LOSSY: u8 = 0x01;

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
}

impl Path {
    pub fn kind(&self) -> PathKind {
        self.kind
    }
}

/// How many paths of each kind carry a session now; reported whenever it
/// changes so the mesh can dial a path back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Paths {
    pub network: bool,
    pub bluetooth: bool,
}

/// A frame kept until the peer acknowledges it.
struct Kept {
    seq: u64,
    /// The whole `SEQ` frame, ready to seal again for whichever path resends it.
    frame: Vec<u8>,
    sent_at: Option<Instant>,
    sent_on: Option<u64>,
}

struct PathSlot {
    id: u64,
    kind: PathKind,
    writer: mpsc::Sender<Vec<u8>>,
    last_heard: Instant,
    degraded_until: Option<Instant>,
    attached_at: Instant,
    probe: Option<(u64, Instant)>,
    last_probe: Instant,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for PathSlot {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Default)]
struct Delivery {
    next_seq: u64,
    unacked: VecDeque<Kept>,
    unacked_bytes: usize,
    peer_delivered: u64,
    delivered: u64,
}

#[derive(Default)]
struct State {
    lanes: [Delivery; Lane::COUNT],
    ack_due: bool,
    paths: Vec<PathSlot>,
    next_path_id: u64,
    pathless_since: Option<Instant>,
    ended: Option<String>,
    kick: Option<QueueKick>,
    reported: Paths,
    bulk_active: Option<Instant>,
    next_probe: u64,
}

impl State {
    fn carrying(&self, lane: Lane, now: Instant) -> Option<&PathSlot> {
        self.paths
            .iter()
            .filter(|p| p.kind.lane() == Some(lane) || p.kind == PathKind::Bluetooth)
            .min_by_key(|p| {
                let degraded = p.degraded_until.is_some_and(|until| until > now);
                (
                    degraded,
                    p.kind == PathKind::Bluetooth,
                    std::cmp::Reverse(p.attached_at),
                )
            })
    }

    fn summary(&self) -> Paths {
        Paths {
            network: self.paths.iter().any(|p| p.kind == PathKind::Network),
            bluetooth: self.paths.iter().any(|p| p.kind == PathKind::Bluetooth),
        }
    }

    fn acknowledged(&mut self, lane: Lane, delivered: u64) -> Option<Duration> {
        let delivery = &mut self.lanes[lane as usize];
        // An ACK must never acknowledge data this end has not numbered.
        if delivered <= delivery.peer_delivered || delivered > delivery.next_seq {
            return None;
        }
        delivery.peer_delivered = delivered;
        let mut rtt = None;
        while delivery.unacked.front().is_some_and(|k| k.seq <= delivered) {
            if let Some(kept) = delivery.unacked.pop_front() {
                delivery.unacked_bytes -= kept.frame.len();
                if let Some(at) = kept.sent_at {
                    rtt = Some(at.elapsed());
                }
            }
        }
        rtt
    }

    fn delivered(&self) -> [u64; Lane::COUNT] {
        std::array::from_fn(|i| self.lanes[i].delivered)
    }

    fn missing_lane(&self) -> Option<Lane> {
        Lane::ALL.into_iter().find(|lane| {
            !self.paths.iter().any(|path| {
                path.kind.lane() == Some(*lane)
                    && path
                        .degraded_until
                        .is_none_or(|until| until <= Instant::now())
            })
        })
    }
}

type PathsObserver = Box<dyn Fn(Paths) + Send + Sync>;

struct Inner {
    state: Mutex<State>,
    /// Woken when the path set changes or the link ends.
    changed: Notify,
    /// Every path's reader feeds this; the session's receiver drains it.
    inbound: [mpsc::Sender<(u64, Vec<u8>)>; Lane::COUNT],
    tag: SessionTag,
    observer: Mutex<Option<PathsObserver>>,
    congestion: Mutex<crate::congestion::BackgroundRate>,
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn kick(&self, flags: u8) {
        let kick = self.lock().kick.clone();
        if let Some(kick) = kick {
            for lane in Lane::ALL {
                kick.push_lane_control(ack_frame([0; Lane::COUNT], flags), lane);
            }
        }
    }

    /// Report the path set if it changed since last reported.
    fn report(&self) {
        let changed = {
            let mut state = self.lock();
            let now = state.summary();
            (now != state.reported).then(|| {
                state.reported = now;
                now
            })
        };
        if let Some(paths) = changed {
            tracing::info!(
                network = paths.network,
                bluetooth = paths.bluetooth,
                "the session's paths changed"
            );
            if let Some(observer) = self
                .observer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
            {
                observer(paths);
            }
        }
    }

    fn path_failed(&self, id: u64, why: &str) {
        let removed = {
            let mut state = self.lock();
            let before = state.paths.len();
            state.paths.retain(|p| p.id != id);
            let removed = state.paths.len() != before;
            if removed
                && state.carrying(Lane::Interactive, Instant::now()).is_none()
                && state.ended.is_none()
                && state.pathless_since.is_none()
            {
                state.pathless_since = Some(Instant::now());
            }
            removed
        };
        if removed {
            tracing::info!(path = id, %why, "a path of the session ended");
            self.changed.notify_waiters();
            // The carrying path may have changed: resend on the new one now,
            // not when the session next has something to say.
            self.kick(0);
            self.report();
        }
    }

    fn end(&self, why: String) {
        {
            let mut state = self.lock();
            if state.ended.is_some() {
                return;
            }
            tracing::info!(%why, "the session can no longer be resumed; ending it");
            state.ended = Some(why);
            state.paths.clear();
        }
        self.changed.notify_waiters();
        self.report();
    }

    fn probe(&self, id: u64, tagged: &[u8]) {
        let Some(nonce) = tagged
            .get(1..)
            .filter(|bytes| bytes.len() == 8)
            .and_then(read_u64)
        else {
            return;
        };
        let mut state = self.lock();
        let active = state
            .bulk_active
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1));
        let Some(path) = state.paths.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if tagged[0] == link_tag::PING {
            let mut pong = vec![link_tag::PONG];
            pong.extend_from_slice(&nonce.to_be_bytes());
            let _ = path.writer.try_send(pong);
        } else if let Some((expected, at)) = path.probe {
            if expected == nonce {
                path.probe = None;
                self.congestion
                    .lock()
                    .unwrap()
                    .observe(at.elapsed(), Instant::now(), active);
            }
        }
    }

    fn acknowledge(&self, bytes: &[u8]) -> Result<(), TransportError> {
        let delivered = read_delivered(bytes)?;
        {
            let mut state = self.lock();
            for lane in Lane::ALL {
                state.acknowledged(lane, delivered[lane as usize]);
            }
        }
        self.changed.notify_waiters();
        Ok(())
    }

    /// Start a path's reader and writer and add it to the session.
    fn install(self: &Arc<Self>, path: Path) {
        let Path {
            mut tx,
            mut rx,
            noise,
            kind,
        } = path;
        let (writer, mut frames) = mpsc::channel::<Vec<u8>>(2);
        let id = {
            let mut state = self.lock();
            state.next_path_id += 1;
            state.next_path_id
        };

        let weak = Arc::downgrade(self);
        let write_noise = noise.clone();
        let write = tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                let sent = match seal_frame(&write_noise, &frame) {
                    Ok(sealed) => tx.send(&sealed).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = sent {
                    if let Some(inner) = weak.upgrade() {
                        inner.path_failed(id, &format!("could not write: {e}"));
                    }
                    return;
                }
            }
        });

        let weak = Arc::downgrade(self);
        let inbound = self.inbound.clone();
        let read = tokio::spawn(async move {
            loop {
                let result = match rx.recv().await {
                    Ok(msg) => open_frame(&noise, msg),
                    Err(e) => Err(e),
                };
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(tagged) => {
                        if let Some(slot) = inner.lock().paths.iter_mut().find(|p| p.id == id) {
                            slot.last_heard = Instant::now();
                        }
                        if matches!(
                            tagged.first(),
                            Some(&link_tag::PING) | Some(&link_tag::PONG)
                        ) {
                            inner.probe(id, &tagged);
                            continue;
                        }
                        if tagged.first() == Some(&link_tag::ACK) {
                            if let Err(error) = inner.acknowledge(&tagged[1..]) {
                                inner.path_failed(id, &error.to_string());
                                return;
                            }
                            continue;
                        }
                        let Some(lane) = tagged.get(1).and_then(|v| Lane::decode(*v)) else {
                            inner.path_failed(id, "invalid delivery lane");
                            return;
                        };
                        if kind.lane().is_some_and(|expected| expected != lane) {
                            inner.path_failed(id, "frame arrived on the wrong lane");
                            return;
                        }
                        if inbound[lane as usize].send((id, tagged)).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        inner.path_failed(id, &format!("could not read: {e}"));
                        return;
                    }
                }
            }
        });

        let now = Instant::now();
        let replaced: Vec<PathSlot> = {
            let mut state = self.lock();
            let (old, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut state.paths)
                .into_iter()
                .partition(|p| p.kind == kind);
            state.paths = keep;
            state.paths.push(PathSlot {
                id,
                kind,
                writer,
                last_heard: now,
                degraded_until: None,
                attached_at: now,
                probe: None,
                last_probe: now,
                tasks: vec![write.abort_handle(), read.abort_handle()],
            });
            if state.carrying(Lane::Interactive, now).is_some() {
                state.pathless_since = None;
            }
            old
        };
        for old in &replaced {
            tracing::info!(
                path = old.id,
                ?kind,
                "a newer path of the same kind replaced this one"
            );
        }
        drop(replaced);
        if matches!(kind, PathKind::Network | PathKind::Bluetooth) {
            *self.congestion.lock().unwrap() = crate::congestion::BackgroundRate::default();
        }
        tracing::info!(path = id, ?kind, "a path joined the session");
        self.changed.notify_waiters();
        self.kick(0);
        self.report();
    }
}

fn seq_frame(lane: Lane, seq: u64, lossy: bool, tagged: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(link_tag::SEQ_HEADER_LEN + tagged.len());
    frame.push(link_tag::SEQ);
    frame.push(lane as u8);
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.push(if lossy { SEQ_LOSSY } else { 0 });
    frame.extend_from_slice(tagged);
    frame
}

fn snapshot_frame(tag: u8, delivered: [u64; Lane::COUNT], flag: u8) -> Vec<u8> {
    let mut frame = vec![tag];
    for seq in delivered {
        frame.extend_from_slice(&seq.to_be_bytes());
    }
    frame.push(flag);
    frame
}

fn ack_frame(delivered: [u64; Lane::COUNT], flags: u8) -> Vec<u8> {
    snapshot_frame(link_tag::ACK, delivered, flags)
}

fn resume_frame(delivered: [u64; Lane::COUNT], lane: Option<Lane>) -> Vec<u8> {
    snapshot_frame(
        link_tag::RESUME,
        delivered,
        lane.map_or(u8::MAX, |lane| lane as u8),
    )
}

fn read_delivered(bytes: &[u8]) -> Result<[u64; Lane::COUNT], TransportError> {
    if bytes.len() != Lane::COUNT * 8 + 1 {
        return Err(TransportError::Other("invalid delivery snapshot".into()));
    }
    Ok(std::array::from_fn(|i| read_u64(&bytes[i * 8..]).unwrap()))
}

fn read_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?))
}

/// The drainer's end of a resumable link: numbers each frame, keeps the
/// reliable ones, and writes on whichever path carries the session.
struct ResumableSink {
    inner: Arc<Inner>,
    lane: Lane,
    last_path: Option<u64>,
    next_bulk: Instant,
}

#[async_trait]
impl FrameSink for ResumableSink {
    async fn send_frame(&mut self, out: OutFrame) -> Result<(), TransportError> {
        debug_assert_eq!(Lane::for_priority(out.priority), self.lane);
        let ack = out.tagged.first() == Some(&link_tag::ACK);
        if !ack && !out.lossy {
            loop {
                let changed = self.inner.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let state = self.inner.lock();
                    if let Some(why) = &state.ended {
                        return Err(TransportError::Other(why.clone()));
                    }
                    if state.lanes[self.lane as usize].unacked_bytes
                        + out.tagged.len()
                        + link_tag::SEQ_HEADER_LEN
                        <= RESEND_CAP
                    {
                        break;
                    }
                }
                changed.await;
            }
        }
        let now = Instant::now();
        let mut writes = Vec::new();
        {
            let mut state = self.inner.lock();
            if let Some(why) = &state.ended {
                return Err(TransportError::Other(why.clone()));
            }
            let carrying = state
                .carrying(self.lane, now)
                .map(|p| (p.id, p.kind, p.writer.clone()));
            if let Some((id, _, writer)) = &carrying {
                if self.last_path != Some(*id) {
                    self.last_path = Some(*id);
                    for kept in &mut state.lanes[self.lane as usize].unacked {
                        kept.sent_at = None;
                        kept.sent_on = None;
                        writes.push((writer.clone(), *id, kept.frame.clone()));
                    }
                }
            } else {
                self.last_path = None;
            }
            if ack {
                if self.lane == Lane::Interactive {
                    let flags = out.tagged.get(1 + Lane::COUNT * 8).copied().unwrap_or(0);
                    let frame = ack_frame(state.delivered(), flags);
                    state.ack_due = false;
                    let bluetooth_active = Lane::ALL.into_iter().any(|lane| {
                        state
                            .carrying(lane, now)
                            .is_some_and(|p| p.kind == PathKind::Bluetooth)
                    });
                    // Cumulative snapshots may be retried, without waiting on a
                    // full data writer. A Bluetooth standby stays quiet.
                    for path in &state.paths {
                        if path.kind != PathKind::Bluetooth
                            || bluetooth_active
                            || flags & ACK_ALL_PATHS != 0
                        {
                            let _ = path.writer.try_send(frame.clone());
                        }
                    }
                }
            } else {
                if self.lane == Lane::Bulk {
                    state.bulk_active = Some(now);
                }
                let delivery = &mut state.lanes[self.lane as usize];
                delivery.next_seq += 1;
                let frame = seq_frame(self.lane, delivery.next_seq, out.lossy, &out.tagged);
                if !out.lossy {
                    delivery.unacked_bytes += frame.len();
                    delivery.unacked.push_back(Kept {
                        seq: delivery.next_seq,
                        frame: frame.clone(),
                        sent_at: None,
                        sent_on: None,
                    });
                }
                match carrying {
                    Some((_, PathKind::Bluetooth, _)) if out.lossy && self.lane == Lane::Video => {}
                    Some((id, _, writer)) => writes.push((writer, id, frame)),
                    None => {}
                }
            }
        }
        for (writer, id, frame) in writes {
            if self.lane == Lane::Bulk {
                tokio::time::sleep(self.next_bulk.saturating_duration_since(Instant::now())).await;
                let rate = self.inner.congestion.lock().unwrap().bytes_per_second();
                self.next_bulk =
                    Instant::now() + Duration::from_secs_f64(frame.len() as f64 / rate);
                self.inner.lock().bulk_active = Some(Instant::now());
            }
            if let Some(seq) = frame.get(2..).and_then(read_u64) {
                if let Some(kept) = self.inner.lock().lanes[self.lane as usize]
                    .unacked
                    .iter_mut()
                    .find(|kept| kept.seq == seq)
                {
                    kept.sent_at = Some(Instant::now());
                    kept.sent_on = Some(id);
                }
            }
            if writer.send(frame).await.is_err() {
                self.inner.path_failed(id, "its writer stopped");
            }
        }
        Ok(())
    }
}

/// The session's receiving end: frames from every path, in order, once each.
pub struct ResumableRx {
    inner: Arc<Inner>,
    inbound: [mpsc::Receiver<(u64, Vec<u8>)>; Lane::COUNT],
    reasm: [Reassembler; Lane::COUNT],
    /// Chunked lossy messages part-way through reassembly.
    lossy_parts: [HashSet<u32>; Lane::COUNT],
    /// The path the last numbered frame came on.
    last_path: [Option<u64>; Lane::COUNT],
}

impl ResumableRx {
    /// Handle one frame; `Some` when it completes a message for the session.
    fn accept(&mut self, path: u64, tagged: &[u8]) -> Result<Option<Message>, TransportError> {
        let Some((&tag, rest)) = tagged.split_first() else {
            return Err(TransportError::Other(
                "empty frame on a resumable link".into(),
            ));
        };
        match tag {
            link_tag::SEQ => {
                let lane = rest
                    .first()
                    .and_then(|v| Lane::decode(*v))
                    .ok_or_else(|| TransportError::Other("invalid SEQ lane".into()))?;
                if rest.len() < 10 {
                    return Err(TransportError::Other("truncated SEQ".into()));
                }
                let seq = read_u64(&rest[1..]).unwrap();
                let lossy = rest[9] & SEQ_LOSSY != 0;
                let frame = &rest[10..];
                let index = lane as usize;
                {
                    let mut state = self.inner.lock();
                    let delivery = &mut state.lanes[index];
                    if seq <= delivery.delivered {
                        return Ok(None);
                    }
                    delivery.delivered = seq;
                    state.ack_due = true;
                }
                if self.last_path[index].is_some_and(|last| last != path) {
                    for id in self.lossy_parts[index].drain() {
                        self.reasm[index].forget(id);
                    }
                }
                self.last_path[index] = Some(path);
                let part = part_message_id(frame);
                if lossy {
                    if let Some(id) = part {
                        self.lossy_parts[index].insert(id);
                    }
                }
                let message = self.reasm[index].accept(frame)?;
                if message.is_some() {
                    if let Some(id) = part {
                        self.lossy_parts[index].remove(&id);
                    }
                }
                Ok(message)
            }
            link_tag::ACK | link_tag::RESUME => {
                self.inner.acknowledge(rest)?;
                Ok(None)
            }
            other => Err(TransportError::Other(format!(
                "unknown frame {other:#x} on a resumable link"
            ))),
        }
    }
}

#[async_trait]
impl TransportRx for ResumableRx {
    async fn recv(&mut self) -> Result<Message, TransportError> {
        loop {
            let inner = self.inner.clone();
            let changed = inner.changed.notified();
            let item = self
                .inbound
                .iter_mut()
                .find_map(|queue| queue.try_recv().ok());
            if let Some((path, tagged)) = item {
                if let Some(msg) = self.accept(path, &tagged)? {
                    return Ok(msg);
                }
                continue;
            }
            if self.inner.lock().ended.is_some() {
                return Err(TransportError::Closed);
            }
            let [interactive, audio, video, bulk] = &mut self.inbound;
            let item = tokio::select! {
                biased;
                item = interactive.recv() => item,
                item = audio.recv() => item,
                item = video.recv() => item,
                item = bulk.recv() => item,
                () = changed => continue,
            };
            if let Some((path, tagged)) = item {
                if let Some(msg) = self.accept(path, &tagged)? {
                    return Ok(msg);
                }
            } else {
                return Err(TransportError::Closed);
            }
        }
    }
}

/// Watches the paths: keepalives and acknowledgements, silence, stalls, and
/// the hold running out.
async fn supervise(weak: Weak<Inner>) {
    let mut tick = tokio::time::interval(TICK);
    let mut last_ack = Instant::now();
    let mut last_keepalive = Instant::now();
    loop {
        tick.tick().await;
        let Some(inner) = weak.upgrade() else { return };
        let now = Instant::now();
        let mut silent: Vec<(u64, Duration)> = Vec::new();
        let mut degraded = None;
        let mut end = None;
        let ack_due;
        {
            let mut state = inner.lock();
            if state.ended.is_some() {
                return;
            }
            let carrying: Vec<u64> = Lane::ALL
                .into_iter()
                .filter_map(|lane| state.carrying(lane, now).map(|p| p.id))
                .collect();
            for path in &state.paths {
                let limit = if carrying.contains(&path.id) {
                    ACTIVE_SILENCE
                } else {
                    STANDBY_SILENCE
                };
                let quiet = now.duration_since(path.last_heard);
                if quiet > limit {
                    silent.push((path.id, quiet));
                }
            }
            for id in carrying {
                let stalled = state.paths.len() > 1
                    && state.lanes.iter().flat_map(|d| d.unacked.iter()).any(|k| {
                        k.sent_on == Some(id)
                            && k.sent_at.is_some_and(|at| now.duration_since(at) > STALL)
                    });
                if stalled {
                    if let Some(path) = state.paths.iter_mut().find(|p| p.id == id) {
                        path.degraded_until = Some(now + DEGRADED_FOR);
                        degraded = Some(id);
                    }
                }
            }
            if state.carrying(Lane::Interactive, now).is_none()
                && state
                    .pathless_since
                    .is_some_and(|since| now.duration_since(since) > HOLD)
            {
                end = Some(format!(
                    "no connection came back within {} s",
                    HOLD.as_secs()
                ));
            }
            let interval = if state
                .bulk_active
                .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
            {
                Duration::from_millis(200)
            } else {
                ACK_EVERY
            };
            let probe_path = state.carrying(Lane::Interactive, now).map(|p| p.id);
            state.next_probe += 1;
            let nonce = state.next_probe;
            if let Some(path) = state.paths.iter_mut().find(|p| Some(p.id) == probe_path) {
                if let Some((_, at)) = path.probe {
                    if interval < ACK_EVERY && now.duration_since(at) > interval {
                        inner
                            .congestion
                            .lock()
                            .unwrap()
                            .observe(now.duration_since(at), now, true);
                    }
                }
                if now.duration_since(path.last_probe) >= interval
                    && path
                        .probe
                        .is_none_or(|(_, at)| now.duration_since(at) > STALL)
                {
                    let mut ping = vec![link_tag::PING];
                    ping.extend_from_slice(&nonce.to_be_bytes());
                    if path.writer.try_send(ping).is_ok() {
                        path.probe = Some((nonce, now));
                        path.last_probe = now;
                    }
                }
            }
            ack_due = state.ack_due;
        }

        for (id, quiet) in silent {
            inner.path_failed(id, &format!("silent for {} ms", quiet.as_millis()));
        }
        if let Some(id) = degraded {
            tracing::info!(
                path = id,
                "the carrying path stopped delivering; moving the session to another"
            );
            inner.kick(0);
        }
        if let Some(why) = end {
            inner.end(why);
            return;
        }
        if now.duration_since(last_keepalive) >= STANDBY_KEEPALIVE {
            last_keepalive = now;
            last_ack = now;
            inner.kick(ACK_ALL_PATHS);
        } else if (ack_due && now.duration_since(last_ack) >= ACK_SOON)
            || now.duration_since(last_ack) >= ACK_EVERY
        {
            last_ack = now;
            inner.kick(0);
        }
    }
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

/// Where the mesh reaches a resumable session: to name it in greetings, give
/// it new paths, and hear when it has none.
#[derive(Clone)]
pub struct ResumeHandle(Arc<Inner>);

impl ResumeHandle {
    /// The tag both ends know this session by.
    pub fn tag(&self) -> SessionTag {
        self.0.tag
    }

    /// The paths carrying the session now.
    pub fn paths(&self) -> Paths {
        self.0.lock().summary()
    }

    pub fn network_complete(&self) -> bool {
        let state = self.0.lock();
        Lane::ALL.into_iter().all(|lane| {
            state.paths.iter().any(|path| {
                path.kind.lane() == Some(lane)
                    && path
                        .degraded_until
                        .is_none_or(|until| until <= Instant::now())
            })
        })
    }

    pub fn is_ended(&self) -> bool {
        self.0.lock().ended.is_some()
    }

    /// End the session: its peer no longer has it, or the user removed it.
    pub fn end(&self, why: &str) {
        self.0.end(why.to_string());
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
        } = path;
        let (delivered, proposed) = {
            let state = self.0.lock();
            if let Some(why) = &state.ended {
                return Err(TransportError::Other(format!("the session is over: {why}")));
            }
            (state.delivered(), state.missing_lane())
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
        let tx = PrioritizedTx::spawn_lanes(sinks).with_holding(holding);
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
                bluetooth: false
            }
        );
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
                bluetooth: true
            }
        );
        a.handle.network_lost();
        b.handle.network_lost();
        a.tx.send(&key(3)).await.unwrap();
        assert_eq!(next(&mut b).await, key(3), "the standby did not take over");
        assert!(!a.tx.holding());
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
