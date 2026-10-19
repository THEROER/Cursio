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
//! At most one network and one Bluetooth path at once; a newer one of a kind
//! replaces the older (nginx's QUIC rework bounds tracked paths the same way).
//! The network carries the session whenever it is up and healthy; Bluetooth is
//! the standby, kept alive by a sparse keepalive because a busy Bluetooth radio
//! costs a phone's Wi-Fi on the shared 2.4 GHz antenna and its battery. Over
//! Bluetooth, pixels are not sent at all.
//!
//! A path is let go of when it falls silent: three seconds for the one
//! carrying the session, which hears an `ACK` every second, and longer for a
//! standby that hears one rarely. A path that is alive but not delivering —
//! frames sent on it stay unacknowledged for seconds while another path
//! exists — is *degraded*: the session moves to the other path and comes back
//! only after a while, so a marginal link does not make it flap. That is what
//! Linux's multipath TCP learned to do with its stale subflows.
//!
//! With no path left, the link holds the session for [`HOLD`] and then ends
//! it, as the old link ended on a dead connection.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

// Tokio's clock, so a test can run the hold out without waiting a minute.
use tokio::time::Instant;

use async_trait::async_trait;
use cursio_protocol::{Message, Priority, SessionTag};
use tokio::sync::{mpsc, Notify};

use crate::crypto::{link_tag, open_frame, part_message_id, seal_frame, Reassembler, SharedNoise};
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

/// Most bytes kept for resending. Past this the peer has stopped
/// acknowledging for far longer than any path survives, and the session can
/// no longer be resumed faithfully: it ends with its path, as before.
const RESEND_CAP: usize = 64 * 1024 * 1024;

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
struct State {
    next_seq: u64,
    unacked: VecDeque<Kept>,
    unacked_bytes: usize,
    /// False once the resend buffer overflowed: the session can no longer
    /// survive losing its path.
    resendable: bool,
    peer_delivered: u64,
    delivered: u64,
    ack_due: bool,
    paths: Vec<PathSlot>,
    next_path_id: u64,
    pathless_since: Option<Instant>,
    ended: Option<String>,
    kick: Option<QueueKick>,
    reported: Paths,
}

impl State {
    /// The path the session is sent on: healthy before degraded, the network
    /// before Bluetooth, the newest of a kind first.
    fn carrying(&self, now: Instant) -> Option<&PathSlot> {
        self.paths.iter().min_by_key(|p| {
            let degraded = p.degraded_until.is_some_and(|until| until > now);
            let bluetooth = p.kind == PathKind::Bluetooth;
            (degraded, bluetooth, std::cmp::Reverse(p.attached_at))
        })
    }

    fn summary(&self) -> Paths {
        Paths {
            network: self.paths.iter().any(|p| p.kind == PathKind::Network),
            bluetooth: self.paths.iter().any(|p| p.kind == PathKind::Bluetooth),
        }
    }

    /// The peer delivered everything up to `delivered`: stop keeping it.
    fn acknowledged(&mut self, delivered: u64) {
        if delivered <= self.peer_delivered {
            return;
        }
        self.peer_delivered = delivered;
        while self.unacked.front().is_some_and(|k| k.seq <= delivered) {
            if let Some(kept) = self.unacked.pop_front() {
                self.unacked_bytes -= kept.frame.len();
            }
        }
    }
}

/// Told the paths whenever they change.
type PathsObserver = Box<dyn Fn(Paths) + Send + Sync>;

struct Inner {
    state: Mutex<State>,
    /// Woken when the path set changes or the link ends.
    changed: Notify,
    /// Every path's reader feeds this; the session's receiver drains it.
    inbound: mpsc::UnboundedSender<(u64, Vec<u8>)>,
    tag: SessionTag,
    observer: Mutex<Option<PathsObserver>>,
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn kick(&self, flags: u8) {
        let kick = self.lock().kick.clone();
        if let Some(kick) = kick {
            kick.push_control(ack_frame(0, flags));
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
            if removed && state.paths.is_empty() && state.ended.is_none() {
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
                        if inbound.send((id, tagged)).is_err() {
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
                tasks: vec![write.abort_handle(), read.abort_handle()],
            });
            state.pathless_since = None;
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
        tracing::info!(path = id, ?kind, "a path joined the session");
        self.changed.notify_waiters();
        self.kick(0);
        self.report();
    }
}

fn seq_frame(seq: u64, lossy: bool, tagged: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(10 + tagged.len());
    frame.push(link_tag::SEQ);
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.push(if lossy { SEQ_LOSSY } else { 0 });
    frame.extend_from_slice(tagged);
    frame
}

fn ack_frame(delivered: u64, flags: u8) -> Vec<u8> {
    let mut frame = Vec::with_capacity(10);
    frame.push(link_tag::ACK);
    frame.extend_from_slice(&delivered.to_be_bytes());
    frame.push(flags);
    frame
}

fn resume_frame(delivered: u64) -> Vec<u8> {
    let mut frame = Vec::with_capacity(9);
    frame.push(link_tag::RESUME);
    frame.extend_from_slice(&delivered.to_be_bytes());
    frame
}

fn read_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?))
}

/// The drainer's end of a resumable link: numbers each frame, keeps the
/// reliable ones, and writes on whichever path carries the session.
struct ResumableSink {
    inner: Arc<Inner>,
    /// The path the last frame went out on; a different one now means the
    /// session moved, and what the peer has not acknowledged goes again.
    last_path: Option<u64>,
}

#[async_trait]
impl FrameSink for ResumableSink {
    async fn send_frame(&mut self, out: OutFrame) -> Result<(), TransportError> {
        let now = Instant::now();
        let mut writes: Vec<(mpsc::Sender<Vec<u8>>, u64, Vec<u8>)> = Vec::new();
        {
            let mut state = self.inner.lock();
            if let Some(why) = &state.ended {
                return Err(TransportError::Other(why.clone()));
            }

            let ack = out.tagged.first() == Some(&link_tag::ACK);
            let carrying = state
                .carrying(now)
                .map(|p| (p.id, p.kind, p.writer.clone()));

            // Moved to another path: resend what the peer has not delivered.
            if let Some((id, _, writer)) = &carrying {
                if self.last_path != Some(*id) {
                    self.last_path = Some(*id);
                    for kept in state.unacked.iter_mut() {
                        kept.sent_at = Some(now);
                        kept.sent_on = Some(*id);
                        writes.push((writer.clone(), *id, kept.frame.clone()));
                    }
                    if !writes.is_empty() {
                        tracing::info!(
                            path = id,
                            frames = writes.len(),
                            "resending what the peer has not delivered"
                        );
                    }
                }
            }

            if ack {
                let flags = out.tagged.get(9).copied().unwrap_or(0);
                let frame = ack_frame(state.delivered, flags);
                state.ack_due = false;
                if flags & ACK_ALL_PATHS != 0 {
                    for path in &state.paths {
                        writes.push((path.writer.clone(), path.id, frame.clone()));
                    }
                } else if let Some((id, _, writer)) = &carrying {
                    writes.push((writer.clone(), *id, frame));
                }
            } else {
                state.next_seq += 1;
                let seq = state.next_seq;
                let frame = seq_frame(seq, out.lossy, &out.tagged);
                if !out.lossy && state.resendable {
                    state.unacked_bytes += frame.len();
                    state.unacked.push_back(Kept {
                        seq,
                        frame: frame.clone(),
                        sent_at: carrying.as_ref().map(|_| now),
                        sent_on: carrying.as_ref().map(|(id, _, _)| *id),
                    });
                    if state.unacked_bytes > RESEND_CAP {
                        tracing::warn!(
                            bytes = state.unacked_bytes,
                            "the peer has stopped acknowledging; this session can no longer survive losing its connection"
                        );
                        state.resendable = false;
                        state.unacked.clear();
                        state.unacked_bytes = 0;
                    }
                }
                match &carrying {
                    // Pixels do not fit through Bluetooth; the window waits
                    // for the network and asks for a whole picture then.
                    Some((_, PathKind::Bluetooth, _))
                        if out.lossy && out.priority == Priority::Video => {}
                    Some((id, _, writer)) => writes.push((writer.clone(), *id, frame)),
                    // No path: a reliable frame waits in the resend buffer,
                    // a lossy one is gone.
                    None => {}
                }
            }
        }

        for (writer, id, frame) in writes {
            if writer.send(frame).await.is_err() {
                // The writer ended with its path; what it was given is kept
                // and goes again on the next one.
                self.inner.path_failed(id, "its writer stopped");
            }
        }
        Ok(())
    }
}

/// The session's receiving end: frames from every path, in order, once each.
pub struct ResumableRx {
    inner: Arc<Inner>,
    inbound: mpsc::UnboundedReceiver<(u64, Vec<u8>)>,
    reasm: Reassembler,
    /// Chunked lossy messages part-way through reassembly.
    lossy_parts: HashSet<u32>,
    /// The path the last numbered frame came on.
    last_path: Option<u64>,
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
                let seq =
                    read_u64(rest).ok_or_else(|| TransportError::Other("truncated SEQ".into()))?;
                let lossy = rest.get(8).is_some_and(|f| f & SEQ_LOSSY != 0);
                let frame = rest.get(9..).unwrap_or_default();
                {
                    let mut state = self.inner.lock();
                    if seq <= state.delivered {
                        return Ok(None);
                    }
                    state.delivered = seq;
                    state.ack_due = true;
                }
                if self.last_path.is_some_and(|last| last != path) {
                    // Whatever lossy message was mid-way on the old path
                    // will never be finished.
                    for id in self.lossy_parts.drain() {
                        self.reasm.forget(id);
                    }
                }
                self.last_path = Some(path);
                let part = part_message_id(frame);
                if lossy {
                    if let Some(id) = part {
                        self.lossy_parts.insert(id);
                    }
                }
                let message = self.reasm.accept(frame)?;
                if message.is_some() {
                    if let Some(id) = part {
                        self.lossy_parts.remove(&id);
                    }
                }
                Ok(message)
            }
            link_tag::ACK | link_tag::RESUME => {
                if let Some(delivered) = read_u64(rest) {
                    self.inner.lock().acknowledged(delivered);
                }
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
            match self.inbound.try_recv() {
                Ok((path, tagged)) => {
                    if let Some(msg) = self.accept(path, &tagged)? {
                        return Ok(msg);
                    }
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => return Err(TransportError::Closed),
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            if self.inner.lock().ended.is_some() {
                return Err(TransportError::Closed);
            }
            tokio::select! {
                item = self.inbound.recv() => match item {
                    Some((path, tagged)) => {
                        if let Some(msg) = self.accept(path, &tagged)? {
                            return Ok(msg);
                        }
                    }
                    None => return Err(TransportError::Closed),
                },
                () = changed => {}
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
            let carrying = state.carrying(now).map(|p| p.id);
            for path in &state.paths {
                let limit = if Some(path.id) == carrying {
                    ACTIVE_SILENCE
                } else {
                    STANDBY_SILENCE
                };
                let quiet = now.duration_since(path.last_heard);
                if quiet > limit {
                    silent.push((path.id, quiet));
                }
            }
            if let Some(id) = carrying {
                let stalled = state.paths.len() > 1
                    && state.unacked.iter().any(|k| {
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
            if state.paths.is_empty() {
                if !state.resendable {
                    end = Some(
                        "its connection dropped with more unacknowledged than can be kept"
                            .to_string(),
                    );
                } else if state
                    .pathless_since
                    .is_some_and(|since| now.duration_since(since) > HOLD)
                {
                    end = Some(format!(
                        "no connection came back within {} s",
                        HOLD.as_secs()
                    ));
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
        self.release(PathKind::Network, "the system reported the network gone");
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
        let delivered = {
            let state = self.0.lock();
            if let Some(why) = &state.ended {
                return Err(TransportError::Other(format!("the session is over: {why}")));
            }
            state.delivered
        };
        tx.send(&seal_frame(&noise, &resume_frame(delivered))?)
            .await?;
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
        let peer_delivered = read_u64(&tagged[1..])
            .ok_or_else(|| TransportError::Other("truncated RESUME".into()))?;
        tracing::info!(
            delivered,
            peer_delivered,
            ?kind,
            "resuming the session on a new path"
        );
        self.0.lock().acknowledged(peer_delivered);
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
        let (inbound_tx, inbound) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                resendable: true,
                ..State::default()
            }),
            changed: Notify::new(),
            inbound: inbound_tx,
            tag,
            observer: Mutex::new(None),
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
            reasm: Reassembler::default(),
            lossy_parts: HashSet::new(),
            last_path: None,
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
        let sink = ResumableSink {
            inner: self.inner.clone(),
            last_path: None,
        };
        let weak = Arc::downgrade(&self.inner);
        let holding = Arc::new(move || {
            weak.upgrade().is_some_and(|inner| {
                let state = inner.lock();
                state.ended.is_none() && state.paths.is_empty()
            })
        });
        let tx = PrioritizedTx::spawn_sink(Box::new(sink)).with_holding(holding);
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
    }

    async fn next(end: &mut End) -> Message {
        tokio::time::timeout(Duration::from_secs(5), end.rx.recv())
            .await
            .expect("a message within five seconds")
            .unwrap()
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
        let (_tx, inbound) = mpsc::unbounded_channel();
        let mut rx = ResumableRx {
            inner: handle.0.clone(),
            inbound,
            reasm: Reassembler::default(),
            lossy_parts: HashSet::new(),
            last_path: None,
        };
        let whole = |msg: &Message| {
            let mut tagged = vec![0u8];
            tagged.extend(msg.to_bytes().unwrap());
            tagged
        };
        let frame = seq_frame(1, false, &whole(&key(1)));
        assert_eq!(rx.accept(1, &frame).unwrap(), Some(key(1)));
        assert_eq!(
            rx.accept(2, &frame).unwrap(),
            None,
            "the copy was delivered too"
        );
    }
}
