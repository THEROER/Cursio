//! Bounded priority queues for encrypted delivery.
//!
//! Messages are chunked and queued atomically by class. Reliable producers
//! wait when their class is full. A link over one connection has one drainer;
//! a resumable session has one per independent lane. Within a lane the next
//! frame is selected by class, so a lower class cannot sit ahead of a higher
//! one.
//!
//! ## Why chunk-then-queue plaintext (not sealed frames)
//!
//! Noise nonces must advance in exactly the order ciphertext hits the wire. So
//! we queue *tagged plaintext* pieces (via [`chunk`]) and let the
//! drainer seal each one the instant before it sends it — never sooner. Sealing
//! at `send` time would fix the nonce order at enqueue time and interleaving
//! would corrupt the stream.
//!
//! ## Starvation
//!
//! On a link over one connection, strict priority is broken in one place:
//! when the lowest class has waited out `BULK_STARVATION_LIMIT` frames of the
//! classes above it, one whole message of it goes next. Classes declared
//! [`urgent`](crate::Class::urgent) are exempt and still go first; see
//! `Queues::pop_highest`.
//!
//! ## Latency vs. buffering
//!
//! Each class has a byte budget. Reliable producers await space; messages are
//! admitted atomically so cancellation never leaves half a message queued.
//! One oversized message may occupy an otherwise empty class. Independent
//! session lanes have independent drainers, so a blocked bulk socket cannot
//! hold the input drainer.
//!
//! ## Where priority stops
//!
//! Ordering here is only worth as much as the queue *after* it is shallow.
//! Two things bound what an input frame can find in front of it once chosen:
//! the frame the drainer is part-way through writing, at most one Noise frame
//! (64 KiB — a frame, once started, has to finish or the stream is corrupt),
//! and whatever the kernel still holds unsent, which
//! the TCP carrier keeps to a few tens of KiB ([`crate::tcp`]). Without that second
//! bound the socket buffer autotuned to megabytes and the scheduling above
//! was decided long before the bytes actually left.
//!
//! Frames stay at the Noise maximum rather than being cut smaller for the
//! lower classes: at gigabit one is half a millisecond, and smaller parts
//! would push a 64 MiB message past the part count every receiver accepts.
//!
//! ## Dropping what is too old
//!
//! A message whose kind says it is only worth sending fresh
//! ([`Delivery::Fresh`] with a time) is dropped here, whole, when the drainer
//! reaches it too late — the only place a sender can still choose to, since
//! once a frame is handed to the socket TCP will deliver it however late.
//! A stall otherwise left a queue of sound and pictures that the link spent
//! its recovery sending and the receiver then threw away for age. Only a
//! message's first frame is judged: a message part-way out is finished, or
//! the receiver would reassemble its tail onto the next one.
//!
//! ## Measuring it
//!
//! Every frame carries the instant it was queued, and the drainer records how
//! long it took to reach the socket, per class, over a [`DELAY_WINDOW`]. Each
//! finished window is handed to the channel's report
//! ([`Classes::with_window_report`]), so "the mouse lags" can be answered
//! with a number, and read back through [`PrioritizedTx::queue_delay`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Notify;
use tracing::Instrument;

use crate::seal::{chunk, chunk_within, has_more_parts, link_tag, Seal};
use crate::{
    CarrierTx, ChannelError, ClassId, Classes, Delivery, Envelope, LaneId, QueueDelay, QueueDepth,
    Window,
};

/// Frames of higher-priority traffic that may go ahead of a waiting frame of
/// the lowest class before one is let through regardless.
///
/// Sized so the guarantee is cheap for everything above: at 32, a transfer gets
/// at least one message through per 32 frames of the classes above it, and
/// those wait out one bulk message when it happens. Urgent classes never do,
/// since a forced frame does not overtake them. Lower would spend more of a
/// busy link on bulk than a latency-sensitive session can afford; much higher
/// stops rescuing the transfer at all.
const BULK_STARVATION_LIMIT: u32 = 32;

/// How long a queue-delay measurement runs before it is logged and published.
///
/// Long enough that one window of ordinary pointer motion holds thousands of
/// input frames, so a p99 is a p99; short enough that a lag the user just
/// noticed shows up in the log while they are still looking at it.
const DELAY_WINDOW: Duration = Duration::from_secs(10);

/// Quarter-octave buckets of microseconds, up to 2^27 µs (over two minutes);
/// anything longer lands in the last one.
const DELAY_BUCKETS: usize = 4 * 27;

/// Wait times of one class, bucketed so recording is a single increment and
/// the drainer — which records every frame it writes — never sorts or
/// allocates. The quarter-octave resolution (about 19%) is far finer than the
/// question asked of it, which is whether input waits microseconds or
/// milliseconds.
#[derive(Clone)]
struct DelayHistogram {
    counts: [u32; DELAY_BUCKETS],
    frames: u32,
    max: Duration,
}

impl Default for DelayHistogram {
    fn default() -> Self {
        Self {
            counts: [0; DELAY_BUCKETS],
            frames: 0,
            max: Duration::ZERO,
        }
    }
}

impl DelayHistogram {
    fn bucket(wait: Duration) -> usize {
        let micros = u64::try_from(wait.as_micros()).unwrap_or(u64::MAX).max(1);
        let octave = 63 - micros.leading_zeros() as usize;
        // The two bits below the leading one pick the quarter of the octave.
        let quarter = if octave >= 2 {
            (micros >> (octave - 2)) & 0b11
        } else {
            (micros << (2 - octave)) & 0b11
        } as usize;
        (octave * 4 + quarter).min(DELAY_BUCKETS - 1)
    }

    /// The top of bucket `idx`, which is what a percentile falling in it is
    /// reported as: an honest upper bound rather than a flattering midpoint.
    fn upper_bound(idx: usize) -> Duration {
        let (octave, quarter) = (idx / 4, (idx % 4) as u64);
        Duration::from_micros(((4 + quarter + 1) << octave) / 4)
    }

    fn record(&mut self, wait: Duration) {
        self.counts[Self::bucket(wait)] += 1;
        self.frames += 1;
        self.max = self.max.max(wait);
    }

    fn percentile(&self, fraction: f64) -> Duration {
        let target = ((f64::from(self.frames) * fraction).ceil() as u32).max(1);
        let mut seen = 0;
        for (idx, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= target {
                return Self::upper_bound(idx).min(self.max);
            }
        }
        self.max
    }

    fn summary(&self) -> QueueDelay {
        if self.frames == 0 {
            return QueueDelay::default();
        }
        QueueDelay {
            frames: self.frames,
            p50: self.percentile(0.50),
            p99: self.percentile(0.99),
            max: self.max,
        }
    }
}

/// One tagged-plaintext frame awaiting sealing, and when `send` queued it.
struct Queued<E> {
    frame: Vec<u8>,
    queued_at: Instant,
    /// From a message that only matters fresh ([`Delivery::Fresh`]).
    lossy: bool,
    /// The first frame of its message. Only here may a message be dropped:
    /// once a part has gone out, the receiver is reassembling it, and the
    /// rest must follow.
    starts_message: bool,
    /// When its message stops being worth sending ([`Delivery::Fresh`]).
    expires: Option<Instant>,
    /// A message still waiting here whole, in one frame, that a later one may
    /// fold into rather than queue behind; see [`PrioritizedTx::send`].
    held: Option<E>,
}

/// A frame the drainer chose, on its way out.
pub(crate) struct OutFrame {
    pub tagged: Vec<u8>,
    pub class: ClassId,
    pub lossy: bool,
}

/// Where the drainer writes, one frame at a time, in the order it chose them.
///
/// Sealing a frame and writing it are one step here because Noise nonces must
/// advance in exactly the order ciphertext reaches the wire. A plain link seals
/// with its one connection's keys ([`Sealing`]); a resumable one numbers the
/// frame, keeps it for resending, and seals it for whichever path is carrying
/// the session at that moment ([`crate::session`]).
#[async_trait]
pub(crate) trait FrameSink: Send {
    async fn send_frame(&mut self, frame: OutFrame) -> Result<(), ChannelError>;
}

/// The sink of a link over one connection.
struct Sealing {
    inner: Box<dyn CarrierTx>,
    seal: Seal,
}

#[async_trait]
impl FrameSink for Sealing {
    async fn send_frame(&mut self, frame: OutFrame) -> Result<(), ChannelError> {
        let sealed = self.seal.seal(&frame.tagged)?;
        self.inner.send(sealed).await
    }
}

/// The per-class frame queues, shared between the [`PrioritizedTx`] handles and
/// the drainer task.
struct Queues<E> {
    spec: Arc<Classes>,
    classes: Vec<VecDeque<Queued<E>>>,
    in_flight: usize,
    in_flight_at: Vec<Option<Instant>>,
    /// Bytes waiting in each class, kept alongside the queues so a producer can
    /// read its own backlog without walking a `VecDeque` of frames.
    queued_bytes: Vec<usize>,
    /// Bytes of each class actually written to the socket, ever. Paired with
    /// `queued_bytes` in [`QueueDepth`]; see there for what it is for.
    sent_bytes: Vec<u64>,
    /// Frames sent from a class above the lowest since its last frame went
    /// out, counted only while the lowest class has something waiting.
    ///
    /// Strict priority alone starves the bottom class whenever the one above it
    /// is never empty, and file traffic makes that ordinary rather than
    /// exceptional: serving a download, four of every five messages are
    /// metadata — an `open`, a `stat` and a `close` around each chunk — so
    /// `Metadata` refilled faster than it drained and the chunks themselves sat
    /// behind it. Measured against Steam: 3355 metadata messages in a
    /// half-minute carried 1 write, and windows with 800-odd carried none at
    /// all, while the loop's own pace allowed two hundred a second.
    starved_bulk: u32,
    /// Mid-message: a forced bulk frame was a part, so the rest of that message
    /// goes out before higher-priority traffic resumes.
    releasing_bulk: bool,
    /// Set once the sender side is gone *and* all queues are empty, so the
    /// drainer knows to exit cleanly rather than park forever.
    closed: bool,
    /// Fatal drainer error. Once set, the socket can never accept another
    /// frame; producers must see an error instead of filling dead queues.
    terminal_error: Option<String>,
    /// Waits recorded in the window now running, per class.
    delay: Vec<DelayHistogram>,
    /// When the running window began; `None` until the first frame is sent.
    window_start: Option<Instant>,
    /// The last complete window, per class — what `queue_delay` reports, so
    /// every reader sees the same finished figures rather than a window that
    /// is still filling.
    last_window: Vec<QueueDelay>,
    /// Messages dropped unsent in the running window because they outlived
    /// their [`Delivery::Fresh`] time, per class.
    expired: Vec<u32>,
}

impl<E> Queues<E> {
    fn new(spec: Arc<Classes>) -> Self {
        let n = spec.classes().len();
        Self {
            spec,
            classes: (0..n).map(|_| VecDeque::new()).collect(),
            in_flight: 0,
            in_flight_at: vec![None; n],
            queued_bytes: vec![0; n],
            sent_bytes: vec![0; n],
            starved_bulk: 0,
            releasing_bulk: false,
            closed: false,
            terminal_error: None,
            delay: vec![DelayHistogram::default(); n],
            window_start: None,
            last_window: vec![QueueDelay::default(); n],
            expired: vec![0; n],
        }
    }

    /// Queue one frame in `class`, keeping the byte count with it.
    fn push(&mut self, class: usize, frame: Vec<u8>) {
        self.push_message(class, vec![frame], Delivery::Reliable);
    }

    /// Queue every frame of one message, contiguously.
    fn push_message(&mut self, class: usize, frames: Vec<Vec<u8>>, delivery: Delivery) {
        let queued_at = Instant::now();
        let (lossy, expires) = match delivery {
            Delivery::Reliable => (false, None),
            Delivery::Fresh { within } => (true, within.and_then(|w| queued_at.checked_add(w))),
        };
        for (n, frame) in frames.into_iter().enumerate() {
            self.queued_bytes[class] += frame.len();
            self.classes[class].push_back(Queued {
                frame,
                queued_at,
                lossy,
                starts_message: n == 0,
                expires,
                held: None,
            });
        }
    }

    /// Take the front of `class`, first dropping whole every message at the
    /// front whose time ran out before it could start.
    fn pop_class(&mut self, class: usize, now: Instant) -> Option<Queued<E>> {
        loop {
            let front = self.classes[class].pop_front()?;
            self.queued_bytes[class] -= front.frame.len();
            let expired = front.starts_message && front.expires.is_some_and(|at| now >= at);
            if !expired {
                return Some(front);
            }
            self.expired[class] += 1;
            let mut more = has_more_parts(&front.frame);
            while more {
                let Some(part) = self.classes[class].pop_front() else {
                    break;
                };
                self.queued_bytes[class] -= part.frame.len();
                more = has_more_parts(&part.frame);
            }
        }
    }

    /// Record that a frame of `class` queued at `queued_at` reached the socket
    /// at `now`. Returns the window just completed, when this closes one.
    fn record_sent(&mut self, class: usize, queued_at: Instant, now: Instant) -> Option<Window> {
        self.delay[class].record(now.saturating_duration_since(queued_at));
        let start = *self.window_start.get_or_insert(now);
        if now.duration_since(start) < DELAY_WINDOW {
            return None;
        }
        let finished: Vec<QueueDelay> = self.delay.iter().map(DelayHistogram::summary).collect();
        self.last_window = finished.clone();
        self.delay.fill(DelayHistogram::default());
        self.window_start = Some(now);
        let expired = vec![0; self.expired.len()];
        Some(Window {
            delay: finished,
            expired: std::mem::replace(&mut self.expired, expired),
        })
    }

    /// Pop the next frame to send: the front of the highest-priority non-empty
    /// class. `None` when everything is empty.
    ///
    /// Returns the class alongside the frame so the drainer can credit the
    /// bytes to it once they are on the wire.
    fn pop_lane(&mut self, lane: Option<LaneId>) -> Option<(usize, Queued<E>)> {
        let Some(lane) = lane else {
            return self.pop_highest();
        };
        let now = Instant::now();
        let spec = self.spec.clone();
        (0..self.classes.len())
            .filter(|&idx| spec.lane_of(idx) == lane)
            .find_map(|idx| self.pop_class(idx, now).map(|frame| (idx, frame)))
    }

    fn pop_highest(&mut self) -> Option<(usize, Queued<E>)> {
        // A bulk frame forced through after `BULK_STARVATION_LIMIT` frames of
        // higher-priority traffic went ahead of it. One in every few dozen is
        // far too little to be felt by anything above, and it is the difference
        // between a transfer that progresses and one that does not.
        //
        // Once a bulk frame is forced through, the rest of its message follows
        // before the count starts again. A logical message is chunked to the
        // Noise plaintext limit, so one `FilesMsg::MAX_CHUNK` read reply leaves
        // here as five frames; charging each of them separately meant a single
        // reply needed five turns of the quota, with a hundred and sixty
        // metadata frames passing in between. Releasing the run keeps the
        // guarantee at one *message* rather than one frame.
        //
        // Urgent classes stay above a forced frame and above a release in
        // progress. They cannot starve anything — none stays non-empty — and a
        // logical message may be up to 64 MiB, so a release that held them
        // back would stall heartbeats and the cursor for the whole of it.
        let bulk = self.classes.len() - 1;
        let spec = &self.spec;
        let urgent_waiting = self
            .classes
            .iter()
            .enumerate()
            .any(|(idx, q)| spec.class(idx).urgent && !q.is_empty());
        let forced = !urgent_waiting
            && !self.classes[bulk].is_empty()
            && (self.releasing_bulk || self.starved_bulk >= BULK_STARVATION_LIMIT);
        let now = Instant::now();
        let (idx, frame) = if forced {
            self.pop_class(bulk, now).map(|f| (bulk, f))?
        } else {
            (0..self.classes.len()).find_map(|idx| self.pop_class(idx, now).map(|f| (idx, f)))?
        };
        if idx == bulk {
            self.starved_bulk = 0;
            // `send` queues a message's parts contiguously under one lock, so
            // the frame behind a part that is not the last belongs to the same
            // message. Its last part ends the run.
            self.releasing_bulk = (forced || self.releasing_bulk) && has_more_parts(&frame.frame);
        } else if !self.classes[bulk].is_empty() {
            // Counted only against a queue that actually has something waiting:
            // metadata flowing while nothing bulk is pending is not starvation,
            // and must not bank credit for a transfer that starts later.
            self.starved_bulk += 1;
        }
        Some((idx, frame))
    }

    /// True when every class is empty — used by `flush` to know the drainer has
    /// nothing left to write.
    fn pop_is_empty(&self) -> bool {
        self.classes.iter().all(VecDeque::is_empty)
    }

    fn fail(&mut self, error: String) {
        for class in &mut self.classes {
            class.clear();
        }
        self.queued_bytes.fill(0);
        self.terminal_error = Some(error);
        self.closed = true;
    }
}

/// Shared scheduler state: the queues plus notifiers for both directions —
/// `wake` rouses the drainer when work arrives or the sender closes; `drained`
/// lets `flush` wait until the drainer has emptied the queues.
struct Shared<E> {
    spec: Arc<Classes>,
    queues: Mutex<Queues<E>>,
    wake: Vec<Notify>,
    drained: Notify,
}

/// Which kind of path carries the interactive lane now, for a link with more
/// than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrying {
    /// A lane's own carrier, through the router.
    Network,
    /// A lane's own carrier over a shorter route that may go away.
    Direct,
    /// The standby carrier that can take every lane.
    Standby,
}

type Holding = Arc<dyn Fn() -> bool + Send + Sync>;
type CarryingPath = Arc<dyn Fn() -> Option<Carrying> + Send + Sync>;
type FrameLimit = Arc<dyn Fn() -> usize + Send + Sync>;
type InputSeen = Arc<dyn Fn() + Send + Sync>;

/// A sender that queues by class instead of writing synchronously.
///
/// Cloneable, because each producer may send from its own thread: a clone is
/// another handle onto the same queues, and all real work lives in the drainer
/// task spawned by [`Self::spawn`]. The queue closes when the *last* handle
/// goes, so a producer finishing early cannot cut off another's final message.
pub struct PrioritizedTx<E> {
    shared: Arc<Shared<E>>,
    _open: Arc<OpenHandle<E>>,
    /// Answers [`Self::holding`] for a sink that can lose its connection and
    /// wait for another.
    holding: Option<Holding>,
    /// Answers [`Self::carrying_path`] for a sink with several paths.
    carrying_path: Option<CarryingPath>,
    /// The largest frame to cut a message into, for a sink whose connection
    /// may be slow; the full Noise budget without it.
    frame_limit: Option<FrameLimit>,
    /// Told of each activity message sent and each one the caller says it
    /// received; see [`Self::note_input`].
    input_seen: Option<InputSeen>,
}

impl<E> Clone for PrioritizedTx<E> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            _open: self._open.clone(),
            holding: self.holding.clone(),
            carrying_path: self.carrying_path.clone(),
            frame_limit: self.frame_limit.clone(),
            input_seen: self.input_seen.clone(),
        }
    }
}

/// Closes the queues when dropped, which happens once, with the last clone.
struct OpenHandle<E>(Arc<Shared<E>>);

/// Queueing a lane's own maintenance, whatever the envelope.
trait LaneControl: Send + Sync {
    fn push_lane_control(&self, tagged: Vec<u8>, lane: LaneId) -> bool;
}

impl<E: Send> LaneControl for Shared<E> {
    fn push_lane_control(&self, tagged: Vec<u8>, lane: LaneId) -> bool {
        let index = self.spec.maintenance_class(lane);
        {
            let mut q = self.queues.lock().unwrap();
            if q.closed || q.terminal_error.is_some() {
                return false;
            }
            // Maintenance is cumulative; don't grow it behind a blocked path.
            if q.queued_bytes[index] >= self.spec.class(index).budget {
                return false;
            }
            q.push(index, tagged);
        }
        self.wake[lane.index()].notify_one();
        true
    }
}

/// Queues control frames of the link's own without holding the queue open:
/// a resumable link's acknowledgements and keepalives, which must stop when
/// the session drops its sender rather than keep the drainer alive forever.
#[derive(Clone)]
pub(crate) struct QueueKick(std::sync::Weak<dyn LaneControl>);

impl QueueKick {
    /// Queue a lane's maintenance without keeping the scheduler alive.
    pub(crate) fn push_lane_control(&self, tagged: Vec<u8>, lane: LaneId) -> bool {
        self.0
            .upgrade()
            .is_some_and(|shared| shared.push_lane_control(tagged, lane))
    }
}

impl<E: Envelope> PrioritizedTx<E> {
    /// Schedule `classes` onto one sealed connection, spawning the drainer
    /// task that owns it. The drainer runs until the last handle is dropped
    /// and the queues are empty, or the connection fails.
    pub fn spawn(inner: Box<dyn CarrierTx>, seal: Seal, classes: Arc<Classes>) -> Self {
        Self::spawn_sink(Box::new(Sealing { inner, seal }), classes)
    }

    /// A handle for queueing the link's own control frames; see [`QueueKick`].
    pub(crate) fn kick_handle(&self) -> QueueKick {
        let weak: std::sync::Weak<dyn LaneControl> = Arc::downgrade(&self.shared) as _;
        QueueKick(weak)
    }

    /// As [`Self::spawn`], writing through `sink` rather than one connection.
    pub(crate) fn spawn_sink(sink: Box<dyn FrameSink>, classes: Arc<Classes>) -> Self {
        Self::spawn_workers(vec![(None, sink)], classes)
    }

    pub(crate) fn spawn_lanes(sinks: Vec<Box<dyn FrameSink>>, classes: Arc<Classes>) -> Self {
        let workers = classes
            .lane_ids()
            .zip(sinks)
            .map(|(lane, sink)| (Some(lane), sink))
            .collect();
        Self::spawn_workers(workers, classes)
    }

    fn spawn_workers(
        workers: Vec<(Option<LaneId>, Box<dyn FrameSink>)>,
        classes: Arc<Classes>,
    ) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::new(classes.clone())),
            wake: classes.lanes().iter().map(|_| Notify::new()).collect(),
            drained: Notify::new(),
            spec: classes,
        });
        for (lane, mut sink) in workers {
            let drain_shared = shared.clone();
            let span = tracing::Span::current();
            let drainer = async move {
                let wake_index = lane.unwrap_or(LaneId::INTERACTIVE).index();
                loop {
                    let wake = drain_shared.wake[wake_index].notified();
                    let (frame, closed) = {
                        let mut q = drain_shared.queues.lock().unwrap();
                        let frame = q.pop_lane(lane);
                        if let Some((class, queued)) = &frame {
                            q.in_flight += 1;
                            q.in_flight_at[*class] = Some(queued.queued_at);
                        }
                        (frame, q.closed)
                    };
                    if let Some((class, queued)) = frame {
                        drain_shared.drained.notify_waiters();
                        let len = queued.frame.len();
                        let maintenance = queued.frame.first() == Some(&link_tag::ACK);
                        let out = OutFrame {
                            tagged: queued.frame,
                            class,
                            lossy: queued.lossy,
                        };
                        let result = sink.send_frame(out).await;
                        let finished = {
                            let mut q = drain_shared.queues.lock().unwrap();
                            q.in_flight -= 1;
                            q.in_flight_at[class] = None;
                            match result {
                                Ok(()) if !maintenance => {
                                    q.sent_bytes[class] += len as u64;
                                    q.record_sent(class, queued.queued_at, Instant::now())
                                }
                                Ok(()) => None,
                                Err(ref error) => {
                                    q.fail(error.to_string());
                                    None
                                }
                            }
                        };
                        drain_shared.drained.notify_waiters();
                        if result.is_err() {
                            for wake in &drain_shared.wake {
                                wake.notify_one();
                            }
                            break;
                        }
                        if let Some(window) = finished {
                            drain_shared.spec.report(&window);
                        }
                    } else {
                        drain_shared.drained.notify_waiters();
                        if closed {
                            break;
                        }
                        wake.await;
                    }
                }
            };
            tokio::spawn(drainer.instrument(span));
        }
        PrioritizedTx {
            _open: Arc::new(OpenHandle(shared.clone())),
            shared,
            holding: None,
            carrying_path: None,
            input_seen: None,
            frame_limit: None,
        }
    }

    /// Answer [`Self::holding`] with `holding`.
    pub(crate) fn with_holding(mut self, holding: Holding) -> Self {
        self.holding = Some(holding);
        self
    }

    /// Tell `seen` of every activity message that crosses this link.
    pub(crate) fn with_input_seen(mut self, seen: InputSeen) -> Self {
        self.input_seen = Some(seen);
        self
    }

    /// Answer [`Self::carrying_path`] with `carrying`.
    pub(crate) fn with_carrying_path(mut self, carrying: CarryingPath) -> Self {
        self.carrying_path = Some(carrying);
        self
    }

    /// Cut each message larger than [`crate::seal::MIN_FRAME`] into frames
    /// of at most what `limit` answers when it is queued; see
    /// [`crate::seal::chunk_within`].
    pub(crate) fn with_frame_limit(mut self, limit: FrameLimit) -> Self {
        self.frame_limit = Some(limit);
        self
    }
}

impl<E: Envelope> Queues<E> {
    /// Fold `next` into the message waiting last in `class`, if that one is
    /// held whole for folding. The folded message keeps the older one's place
    /// and age.
    fn fold_into_last(&mut self, class: usize, next: &E) -> Result<bool, ChannelError> {
        let Some(last) = self.classes[class].back_mut() else {
            return Ok(false);
        };
        let Some(mut folded) = last.held.clone() else {
            return Ok(false);
        };
        if !folded.fold(next) {
            return Ok(false);
        }
        let mut frames = chunk(&folded.encode()?)?;
        let Some(frame) = frames.pop().filter(|_| frames.is_empty()) else {
            return Ok(false);
        };
        self.queued_bytes[class] = self.queued_bytes[class] - last.frame.len() + frame.len();
        last.frame = frame;
        last.held = Some(folded);
        Ok(true)
    }
}

impl<E: Envelope> PrioritizedTx<E> {
    /// Queue `msg` in its class, waiting while a reliable class is full.
    ///
    /// Admission is atomic: cancelled while waiting, nothing of the message
    /// is queued.
    pub async fn send(&mut self, msg: &E) -> Result<(), ChannelError> {
        if msg.is_activity() {
            self.note_input();
        }
        let idx = msg.class();
        let delivery = msg.delivery();
        // A message that finds a foldable one still unsent joins it. A link
        // that keeps up never sees this; one that cannot send a frame per
        // pointer move — Bluetooth carries well under a thousand packets a
        // second, which a 1000 Hz mouse alone asks for — would otherwise
        // deliver the backlog as a burst of tiny steps after a pause, felt as
        // the pointer jerking. A Bluetooth mouse does the same: it adds up its
        // motion and reports once per connection interval.
        let folds = msg.folds();
        if folds {
            let mut q = self.shared.queues.lock().unwrap();
            if q.terminal_error.is_none() && !q.closed && q.fold_into_last(idx, msg)? {
                return Ok(());
            }
        }
        let body = msg.encode()?;
        let frames = match &self.frame_limit {
            Some(limit) => chunk_within(&body, || limit())?,
            None => chunk(&body)?,
        };
        let size: usize = frames.iter().map(Vec::len).sum();
        loop {
            let available = self.shared.drained.notified();
            tokio::pin!(available);
            available.as_mut().enable();
            {
                let mut q = self.shared.queues.lock().unwrap();
                if let Some(error) = &q.terminal_error {
                    return Err(ChannelError::Other(error.clone()));
                }
                if q.closed {
                    return Err(ChannelError::Closed);
                }
                if q.queued_bytes[idx] + size <= self.shared.spec.class(idx).budget.max(size) {
                    let single = frames.len() == 1;
                    q.push_message(idx, frames, delivery);
                    if single && folds {
                        if let Some(last) = q.classes[idx].back_mut() {
                            last.held = Some(msg.clone());
                        }
                    }
                    // A link over one connection has only the interactive
                    // worker; a session also wakes the lane's worker.
                    self.shared.wake[0].notify_one();
                    self.shared.wake[self.shared.spec.lane_of(idx).index()].notify_one();
                    return Ok(());
                }
            }
            available.await;
        }
    }
}

impl<E> PrioritizedTx<E> {
    /// Age of the oldest queued or currently sending frame of `class`, for
    /// live control.
    pub fn queue_age(&self, class: ClassId) -> Duration {
        let q = self.shared.queues.lock().unwrap();
        let index = class;
        q.classes[index]
            .front()
            .map(|f| f.queued_at)
            .into_iter()
            .chain(q.in_flight_at[index])
            .min()
            .map_or(Duration::ZERO, |at| at.elapsed())
    }

    /// How much of `class` is queued here and how much has left.
    pub fn queue_depth(&self, class: ClassId) -> QueueDepth {
        let q = self.shared.queues.lock().unwrap();
        QueueDepth {
            queued_bytes: q.queued_bytes[class],
            sent_bytes: q.sent_bytes[class],
        }
    }

    /// How long `class`'s frames waited in the last complete window.
    pub fn queue_delay(&self, class: ClassId) -> QueueDelay {
        self.shared.queues.lock().unwrap().last_window[class]
    }

    /// Whether the link has lost its connection and is holding the session
    /// open for a new one to resume it. Always `false` for a link over one
    /// connection, which cannot be resumed.
    pub fn holding(&self) -> bool {
        self.holding.as_ref().is_some_and(|holding| holding())
    }

    /// Which kind of path carries the interactive lane now; `None` for a link
    /// over one connection.
    pub fn carrying_path(&self) -> Option<Carrying> {
        self.carrying_path.as_ref().and_then(|carrying| carrying())
    }

    /// A person acted through this link just now, either way: background
    /// traffic yields while that goes on; see `session/congestion.rs`.
    pub fn note_input(&self) {
        if let Some(seen) = &self.input_seen {
            seen();
        }
    }

    /// Wait until every frame queued so far has reached the carrier, so a
    /// final message leaves before the caller drops the sender. Returns early
    /// once the link has failed.
    pub async fn flush(&mut self) {
        loop {
            // Register interest *before* checking, so a drain that completes
            // between the check and the await can't slip past us.
            let drained = self.shared.drained.notified();
            let done = {
                let q = self.shared.queues.lock().unwrap();
                (q.pop_is_empty() && q.in_flight == 0) || q.terminal_error.is_some()
            };
            if done {
                return;
            }
            // Make sure the drainer is running, then wait for it to empty the
            // queues (or for the link to die, which also ends draining).
            for wake in &self.shared.wake {
                wake.notify_one();
            }
            drained.await;
        }
    }
}

impl<E> Drop for OpenHandle<E> {
    fn drop(&mut self) {
        // Mark closed so the drainer exits once it has flushed what's queued,
        // then wake it in case it's parked.
        if let Ok(mut q) = self.0.queues.lock() {
            q.closed = true;
        }
        for wake in &self.0.wake {
            wake.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    fn queues() -> Queues<Msg> {
        Queues::new(classes())
    }

    #[test]
    fn pop_highest_prefers_lower_index() {
        let mut q = queues();
        q.push(BULK, vec![0xBB]);
        q.push(INPUT, vec![0x11]);
        q.push(AUDIO, vec![0xAA]);
        assert_eq!(q.pop_highest().map(|(_, f)| f.frame), Some(vec![0x11]));
        assert_eq!(q.pop_highest().map(|(_, f)| f.frame), Some(vec![0xAA]));
        assert_eq!(q.pop_highest().map(|(_, f)| f.frame), Some(vec![0xBB]));
        assert!(q.pop_highest().is_none());
    }

    /// A busy class above must not stop bulk traffic entirely.
    ///
    /// Strict priority assumed the upper class drains; file traffic breaks that
    /// assumption, since serving a download queues an `open`, a `stat` and a
    /// `close` around every chunk. Live against Steam, half-minute windows
    /// carrying 800 metadata messages carried no writes at all.
    #[test]
    fn endless_metadata_still_lets_bulk_through() {
        let mut q = queues();
        for _ in 0..8 {
            q.push(BULK, vec![0u8; 4]);
        }
        let mut sent_bulk = 0;
        for _ in 0..(BULK_STARVATION_LIMIT * 4) {
            q.push(METADATA, vec![0u8; 1]);
            if let Some((idx, _)) = q.pop_highest() {
                if idx == BULK {
                    sent_bulk += 1;
                }
            }
        }
        assert!(
            sent_bulk > 0,
            "a transfer makes progress however busy the class above it is"
        );
    }

    /// The tagged frames of one message the size of a file read reply.
    fn chunked_message() -> Vec<Vec<u8>> {
        chunk(&[0u8; 256 * 1024]).unwrap()
    }

    /// A forced bulk frame releases the rest of its message with it.
    ///
    /// The quota counts frames, but a bulk message is many: a 1.2 MB write is
    /// chunked to the Noise plaintext limit and arrives here as thirteen.
    /// Charged one at a time, that write needed thirteen turns of the quota
    /// and four hundred metadata frames could pass in between.
    #[test]
    fn a_forced_bulk_message_goes_out_whole() {
        let mut q = queues();
        let parts = chunked_message();
        let part_count = parts.len();
        assert!(part_count > 1, "the test message must need several frames");
        for f in parts {
            q.push(BULK, f);
        }
        for _ in 0..64 {
            q.push(METADATA, vec![0u8; 1]);
        }
        let mut seen_bulk = 0;
        let mut run_broken = false;
        while let Some((idx, _)) = q.pop_highest() {
            if idx == BULK {
                seen_bulk += 1;
            } else if seen_bulk > 0 && seen_bulk < part_count {
                run_broken = true;
            }
        }
        assert_eq!(seen_bulk, part_count, "every part of the message went out");
        assert!(
            !run_broken,
            "the parts of one message are not split by other traffic"
        );
    }

    /// A release ends with its message, not with the bulk queue: every piece
    /// is tagged `PART`, the last included, so keying the release on the tag
    /// alone let one forced frame carry every transfer queued behind it.
    #[test]
    fn a_release_stops_at_the_end_of_its_message() {
        let mut q = queues();
        let part_count = chunked_message().len();
        for _ in 0..3 {
            for f in chunked_message() {
                q.push(BULK, f);
            }
        }
        q.starved_bulk = BULK_STARVATION_LIMIT;
        q.push(METADATA, vec![0u8; 1]);
        for _ in 0..part_count {
            assert_eq!(q.pop_highest().map(|(idx, _)| idx), Some(BULK));
        }
        assert_eq!(
            q.pop_highest().map(|(idx, _)| idx),
            Some(METADATA),
            "the next message waits for its own turn of the quota"
        );
    }

    /// Urgent classes still go first while a bulk message is released: a
    /// logical message may run to 64 MiB, and holding the cursor and
    /// heartbeats back for all of it would be felt as a frozen pointer or a
    /// dropped link.
    #[test]
    fn input_overtakes_a_bulk_release_in_progress() {
        let mut q = queues();
        for f in chunked_message() {
            q.push(BULK, f);
        }
        q.starved_bulk = BULK_STARVATION_LIMIT;
        assert_eq!(q.pop_highest().map(|(idx, _)| idx), Some(BULK));
        q.push(INPUT, vec![0u8; 1]);
        q.push(CONTROL, vec![0u8; 1]);
        assert_eq!(q.pop_highest().map(|(idx, _)| idx), Some(CONTROL));
        assert_eq!(q.pop_highest().map(|(idx, _)| idx), Some(INPUT));
        assert_eq!(
            q.pop_highest().map(|(idx, _)| idx),
            Some(BULK),
            "the release resumes once the urgent frames are out"
        );
    }

    const STALE: Delivery = Delivery::Fresh {
        within: Some(Duration::ZERO),
    };

    #[test]
    fn a_fresh_message_past_its_time_is_dropped_whole_and_counted() {
        let mut q = queues();
        let parts = chunked_message();
        assert!(parts.len() > 1);
        q.push_message(VIDEO, parts, STALE);
        q.push_message(VIDEO, vec![vec![1]], Delivery::Reliable);
        let (class, next) = q.pop_highest().expect("the reliable message");
        assert_eq!((class, next.frame), (VIDEO, vec![1]));
        assert!(q.pop_highest().is_none());
        assert_eq!(q.queued_bytes[VIDEO], 0);
        assert_eq!(q.expired[VIDEO], 1);
    }

    #[test]
    fn a_fresh_message_already_started_is_finished() {
        let mut q = queues();
        let parts = chunked_message();
        let count = parts.len();
        q.push_message(
            VIDEO,
            parts,
            Delivery::Fresh {
                within: Some(Duration::from_secs(60)),
            },
        );
        q.pop_highest().expect("the first part");
        for part in &mut q.classes[VIDEO] {
            part.expires = Some(Instant::now());
        }
        let rest = std::iter::from_fn(|| q.pop_highest()).count();
        assert_eq!(rest, count - 1);
        assert_eq!(q.expired[VIDEO], 0);
    }

    #[test]
    fn reliable_and_untimed_fresh_messages_never_expire() {
        let mut q = queues();
        q.push_message(INPUT, vec![vec![1]], Delivery::Reliable);
        q.push_message(INPUT, vec![vec![2]], Delivery::Fresh { within: None });
        let order: Vec<_> = std::iter::from_fn(|| q.pop_highest().map(|(_, f)| f.frame)).collect();
        assert_eq!(order, vec![vec![1], vec![2]]);
    }

    #[test]
    fn a_lane_skips_past_an_expired_class_to_the_next() {
        let mut q = queues();
        q.push_message(AUDIO, vec![vec![1]], STALE);
        assert!(q.pop_lane(Some(AUDIO_LANE)).is_none());
        assert_eq!(q.expired[AUDIO], 1);
    }

    /// Credit is not banked while nothing bulk is waiting, or a quiet period
    /// of metadata would buy a later transfer the right to jump the queue.
    #[test]
    fn metadata_alone_banks_no_credit_against_bulk() {
        let mut q = queues();
        for _ in 0..(BULK_STARVATION_LIMIT * 2) {
            q.push(METADATA, vec![0u8; 1]);
            q.pop_highest();
        }
        assert_eq!(
            q.starved_bulk, 0,
            "no bulk was waiting, so nothing was starved"
        );
    }

    /// The backlog a class reports is its own and falls as the drainer takes
    /// frames: what a bulk producer paces itself against.
    #[test]
    fn a_class_reports_only_its_own_backlog() {
        let mut q = queues();
        q.push(BULK, vec![0u8; 100]);
        q.push(BULK, vec![0u8; 50]);
        q.push(INPUT, vec![0u8; 7]);
        assert_eq!(q.queued_bytes[BULK], 150);
        assert_eq!(q.queued_bytes[INPUT], 7);
        let (class, frame) = q.pop_highest().expect("a frame");
        assert_eq!(class, INPUT);
        assert_eq!(frame.frame.len(), 7);
        assert_eq!(q.queued_bytes[BULK], 150);
        assert_eq!(q.queued_bytes[INPUT], 0);
        q.pop_highest().expect("a bulk frame");
        assert_eq!(q.queued_bytes[BULK], 50);
    }

    #[test]
    fn terminal_failure_discards_every_backlog() {
        let mut q = queues();
        q.push(VIDEO, vec![0; 100]);
        q.push(BULK, vec![0; 200]);
        q.fail("socket closed".into());
        assert!(q.pop_is_empty());
        assert!(q.queued_bytes.iter().all(|&b| b == 0));
        assert_eq!(q.terminal_error.as_deref(), Some("socket closed"));
        assert!(q.closed);
    }

    #[test]
    fn delay_percentiles_bound_the_recorded_waits_from_above() {
        let mut h = DelayHistogram::default();
        for _ in 0..98 {
            h.record(Duration::from_micros(100));
        }
        h.record(Duration::from_millis(5));
        h.record(Duration::from_millis(40));
        let s = h.summary();
        assert_eq!(s.frames, 100);
        assert_eq!(s.max, Duration::from_millis(40));
        assert!(
            s.p50 >= Duration::from_micros(100) && s.p50 < Duration::from_micros(125),
            "p50 is the top of the 100 µs bucket, got {:?}",
            s.p50
        );
        assert!(
            s.p99 >= Duration::from_millis(5) && s.p99 < Duration::from_micros(6250),
            "p99 lands in the 5 ms bucket, got {:?}",
            s.p99
        );
    }

    #[test]
    fn a_delay_window_is_published_only_once_it_completes() {
        let mut q = queues();
        let start = Instant::now();
        assert!(q
            .record_sent(INPUT, start, start + Duration::from_millis(2))
            .is_none());
        assert_eq!(q.last_window[INPUT], QueueDelay::default());
        let closing = start + DELAY_WINDOW + Duration::from_millis(3);
        let window = q
            .record_sent(INPUT, start, closing)
            .expect("the window has run its length");
        assert_eq!(window.delay[INPUT].frames, 2);
        assert_eq!(q.last_window[INPUT], window.delay[INPUT]);
        assert_eq!(q.delay[INPUT].frames, 0, "the next window starts empty");
    }

    // --- End-to-end scheduling and flush. ---
    //
    // The in-memory carrier has no backpressure — the drainer empties the
    // queues into it instantly — so it can show that scheduling keeps the
    // stream intact and that flush works, but not that input overtakes a
    // backlog. That needs a socket whose far end is slower than the near one,
    // which the loopback tests further down arrange.

    use crate::mem::MemCarrier;
    use crate::seal::SealedRx;
    use crate::{Carrier, NoisePsk, Role, Trust};

    struct GatedSink(Arc<tokio::sync::Semaphore>);

    #[async_trait]
    impl FrameSink for GatedSink {
        async fn send_frame(&mut self, _: OutFrame) -> Result<(), ChannelError> {
            self.0.acquire().await.unwrap().forget();
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_full_queue_backpressures_only_its_producer_and_cancellation_is_atomic() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut tx = PrioritizedTx::spawn_sink(Box::new(GatedSink(gate.clone())), classes());
        let message = bulk(1024 * 1024);
        tx.send(&message).await.unwrap();
        // The first frame is now blocked in the sink: eight messages minus
        // that frame fit, a ninth does not.
        tokio::task::yield_now().await;
        for _ in 0..7 {
            tx.send(&message).await.unwrap();
        }
        let before = tx.queue_depth(BULK).queued_bytes;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), tx.send(&message))
                .await
                .is_err()
        );
        assert_eq!(
            tx.queue_depth(BULK).queued_bytes,
            before,
            "a cancelled admission must not queue a partial message"
        );
        // A bulk producer waiting for space must not hold a mutex needed by input.
        tokio::time::timeout(Duration::from_millis(100), tx.send(&Msg::Key(1)))
            .await
            .unwrap()
            .unwrap();
        assert!(tx.queue_age(BULK) >= Duration::from_millis(20));
        // The final frame has been removed from its queue but is still blocked.
        assert!(tokio::time::timeout(Duration::from_millis(20), tx.flush())
            .await
            .is_err());
        gate.add_permits(1000);
        tokio::time::timeout(Duration::from_secs(1), tx.flush())
            .await
            .unwrap();
        assert_eq!(tx.queue_depth(BULK).queued_bytes, 0);
    }

    struct RecordingSink {
        gate: Arc<tokio::sync::Semaphore>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait]
    impl FrameSink for RecordingSink {
        async fn send_frame(&mut self, frame: OutFrame) -> Result<(), ChannelError> {
            self.gate.acquire().await.unwrap().forget();
            self.sent.lock().unwrap().push(frame.tagged);
            Ok(())
        }
    }

    #[tokio::test]
    async fn moves_waiting_behind_a_slow_link_merge_without_crossing_a_click() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut tx = PrioritizedTx::spawn_sink(
            Box::new(RecordingSink {
                gate: gate.clone(),
                sent: sent.clone(),
            }),
            classes(),
        );
        tx.send(&Msg::Key(1)).await.unwrap();
        tokio::task::yield_now().await; // the key is now blocked in the sink
        for message in [
            Msg::Motion(1, 2),
            Msg::Motion(3, 4),
            Msg::Click,
            Msg::Motion(5, 0),
            Msg::Motion(1, 1),
        ] {
            tx.send(&message).await.unwrap();
        }
        gate.add_permits(100);
        tokio::time::timeout(Duration::from_secs(1), tx.flush())
            .await
            .unwrap();

        let expected: Vec<Vec<u8>> = [
            Msg::Key(1),
            Msg::Motion(4, 6),
            Msg::Click,
            Msg::Motion(6, 1),
        ]
        .iter()
        .flat_map(|m| chunk(&m.encode().unwrap()).unwrap())
        .collect();
        assert_eq!(*sent.lock().unwrap(), expected);
        assert_eq!(tx.queue_depth(INPUT).queued_bytes, 0);
    }

    #[tokio::test]
    async fn a_move_on_an_idle_link_goes_out_alone() {
        let gate = Arc::new(tokio::sync::Semaphore::new(100));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut tx = PrioritizedTx::spawn_sink(
            Box::new(RecordingSink {
                gate,
                sent: sent.clone(),
            }),
            classes(),
        );
        for dx in [1, 2] {
            tx.send(&Msg::Motion(dx, 0)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), tx.flush())
                .await
                .unwrap();
        }
        assert_eq!(sent.lock().unwrap().len(), 2);
    }

    /// A sealed pair over `a` and `b`: a scheduled sender on `a`'s side, a
    /// plain receiver on `b`'s, and the halves neither uses, kept alive.
    struct Sealed {
        tx: PrioritizedTx<Msg>,
        rx: SealedRx,
        _unused: (Box<dyn crate::CarrierRx>, Box<dyn crate::CarrierTx>),
    }

    async fn sealed(mut a: Box<dyn Carrier>, mut b: Box<dyn Carrier>) -> Sealed {
        let trust = NoisePsk::from_secret(b"test", b"k");
        let (oa, ob) = tokio::join!(
            trust.open(a.as_mut(), Role::Initiator),
            trust.open(b.as_mut(), Role::Responder)
        );
        let (oa, ob) = (oa.unwrap(), ob.unwrap());
        let (a_tx, a_rx) = a.split();
        let (b_tx, b_rx) = b.split();
        Sealed {
            tx: PrioritizedTx::spawn(a_tx, oa.seal, classes()),
            rx: SealedRx::new(b_rx, ob.seal),
            _unused: (a_rx, b_tx),
        }
    }

    async fn mem_sealed() -> Sealed {
        let (a, b) = MemCarrier::pair();
        sealed(Box::new(a), Box::new(b)).await
    }

    /// A bulk payload and an input event both round-trip intact through the
    /// scheduler, chunking and reassembly.
    #[tokio::test]
    async fn prioritized_sends_roundtrip_intact() {
        let Sealed { mut tx, mut rx, .. } = mem_sealed().await;
        let big = bulk(800_000);
        let input = Msg::Motion(5, -3);
        tx.send(&big).await.unwrap();
        tx.send(&input).await.unwrap();
        // Order isn't asserted: without backpressure the in-memory carrier
        // cannot reproduce the interleave; see the note above.
        let got = [rx.recv::<Msg>().await.unwrap(), rx.recv().await.unwrap()];
        assert!(got.contains(&input), "input must round-trip");
        assert!(got.contains(&big), "bulk must round-trip");
    }

    /// `flush` does not return until queued frames have been sent, so a final
    /// message is not lost when the caller drops the sender.
    #[tokio::test]
    async fn flush_waits_for_queued_frames() {
        let Sealed { mut tx, mut rx, .. } = mem_sealed().await;
        tx.send(&Msg::Heartbeat(9)).await.unwrap();
        tx.flush().await;
        assert_eq!(rx.recv::<Msg>().await.unwrap(), Msg::Heartbeat(9));
    }

    #[tokio::test]
    async fn writer_failure_unblocks_flush_and_rejects_later_sends() {
        let Sealed { mut tx, rx, .. } = mem_sealed().await;
        drop(rx);
        let _ = tx.send(&Msg::Heartbeat(1)).await;
        tokio::time::timeout(Duration::from_secs(1), tx.flush())
            .await
            .expect("flush remained blocked after the writer failed");
        assert!(tx.send(&Msg::Heartbeat(2)).await.is_err());
        assert_eq!(
            tx.queue_depth(CONTROL).queued_bytes,
            0,
            "dead queues must not retain unsendable frames"
        );
    }

    /// A queued bulk payload shows up as a backlog and is credited as sent
    /// once it has left — the two readings a producer paces itself with.
    #[tokio::test]
    async fn the_backlog_a_producer_reads_tracks_the_wire() {
        let Sealed { mut tx, mut rx, .. } = mem_sealed().await;
        assert_eq!(tx.queue_depth(BULK), QueueDepth::default());
        tx.send(&bulk(800_000)).await.unwrap();
        tx.flush().await;
        let _ = rx.recv_body().await.unwrap();
        let depth = tx.queue_depth(BULK);
        assert_eq!(depth.queued_bytes, 0, "nothing is left after a flush");
        assert!(
            depth.sent_bytes >= 800_000,
            "the payload must be credited to its own class, got {depth:?}"
        );
        assert_eq!(tx.queue_depth(AUDIO), QueueDepth::default());
    }

    /// A producer's clone keeps the link open after the first handle is gone,
    /// and what it sends still arrives; the queue closes with the last.
    #[tokio::test]
    async fn a_cloned_sender_shares_the_queue_until_the_last_handle_drops() {
        let Sealed { tx, mut rx, .. } = mem_sealed().await;
        let mut clone = tx.clone();
        drop(tx);
        clone.send(&Msg::Heartbeat(7)).await.unwrap();
        clone.flush().await;
        assert_eq!(rx.recv::<Msg>().await.unwrap(), Msg::Heartbeat(7));
        drop(clone);
        assert!(tokio::time::timeout(Duration::from_secs(1), rx.recv_body())
            .await
            .expect("the link must close once the last handle is gone")
            .is_err());
    }

    /// A scheduled sender and a plain receiver over real loopback TCP.
    ///
    /// `receive_window` caps the receiver's socket buffer, standing in for the
    /// bandwidth-delay product of a real link: left to autotune on loopback it
    /// grows to tens of megabytes and would hold the backlog itself, which no
    /// receiver that reads as fast as the network delivers ever does.
    async fn loopback(bounded: bool, receive_window: Option<u32>) -> Sealed {
        use crate::tcp::TcpCarrier;
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        if let Some(bytes) = receive_window {
            // Set on the listener so the accepted socket starts with it, and
            // the window it advertises never exceeds it.
            socket.set_recv_buffer_size(bytes).unwrap();
        }
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let dialled = tokio::net::TcpStream::connect(addr).await.unwrap();
        let sender = if bounded {
            TcpCarrier::from_stream(dialled)
        } else {
            TcpCarrier::from_stream_unbounded(dialled)
        }
        .unwrap();
        let receiver = TcpCarrier::from_stream(accepted.await.unwrap()).unwrap();
        sealed(Box::new(sender), Box::new(receiver)).await
    }

    /// One file read reply's worth: five Noise frames.
    const BULK_MESSAGE: usize = 256 * 1024;

    /// A gigabit LAN's bandwidth-delay product at about 1 ms.
    const RECEIVE_WINDOW: u32 = 128 * 1024;

    /// Queue sixteen megabytes of bulk, let the drainer push as much of it
    /// into the socket as the socket will take, then send one pointer event,
    /// and count the bulk bytes the receiver had to take before that event.
    ///
    /// Counted in bytes rather than milliseconds because bytes are what the
    /// sender decides; how long they take depends on the receiver, which here
    /// is an unoptimised test build decrypting on a loaded machine. The
    /// receiver pauses between messages so it is slower than the sender, which
    /// is what any real link is to the drainer.
    async fn bulk_ahead_of_input(bounded: bool) -> usize {
        use std::sync::atomic::{AtomicBool, Ordering};

        let Sealed {
            mut tx,
            mut rx,
            _unused,
        } = loopback(bounded, Some(RECEIVE_WINDOW)).await;

        let input = Msg::Motion(1, 1);
        let expected = input.clone();
        let input_sent = Arc::new(AtomicBool::new(false));
        let after_input = input_sent.clone();
        let reader = tokio::spawn(async move {
            let mut ahead = 0;
            loop {
                match rx.recv::<Msg>().await.unwrap() {
                    msg if msg == expected => return ahead,
                    Msg::Blob(_, bytes) if after_input.load(Ordering::SeqCst) => {
                        ahead += bytes.len();
                    }
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        for _ in 0..64 {
            tx.send(&bulk(BULK_MESSAGE)).await.unwrap();
        }
        // Long enough for the drainer to fill whatever the kernel will hold;
        // an autotuned buffer is what this is here to expose.
        tokio::time::sleep(Duration::from_millis(100)).await;

        input_sent.store(true, Ordering::SeqCst);
        tx.send(&input).await.unwrap();
        reader.await.unwrap()
    }

    /// The headline property: a pointer event queued behind a transfer
    /// reaches the peer after the few frames already committed to the wire,
    /// not after the transfer.
    ///
    /// What may legitimately be ahead of it: the bulk message the receiver is
    /// part-way through, the receive window (which Linux doubles), the frame
    /// the drainer is writing and the kernel's unsent allowance. Measured,
    /// that came to 0–256 KiB bounded and 768 KiB–1.5 MiB with the send
    /// buffer left to autotune, even on loopback where autotuning stays
    /// modest; on a real LAN it grows to megabytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_overtakes_a_bulk_backlog_already_handed_to_the_socket() {
        const NOISE_FRAME: usize = 64 * 1024;
        const KERNEL_UNSENT: usize = 128 * 1024;
        let allowed = BULK_MESSAGE + 2 * RECEIVE_WINDOW as usize + NOISE_FRAME + KERNEL_UNSENT;

        let ahead = bulk_ahead_of_input(true).await;
        assert!(
            ahead <= allowed,
            "input arrived behind {} KiB of a transfer queued before it, over the {} KiB bound",
            ahead / 1024,
            allowed / 1024
        );
    }

    /// Bytes a receiver takes as fast as it can, over one link.
    async fn bulk_throughput(bounded: bool, messages: usize) -> f64 {
        let Sealed {
            mut tx,
            mut rx,
            _unused,
        } = loopback(bounded, None).await;

        let reader = tokio::spawn(async move {
            for _ in 0..messages {
                rx.recv_body().await.unwrap();
            }
            Instant::now()
        });
        let started = Instant::now();
        for _ in 0..messages {
            tx.send(&bulk(BULK_MESSAGE)).await.unwrap();
        }
        let finished = reader.await.unwrap();
        (messages * BULK_MESSAGE) as f64 / finished.duration_since(started).as_secs_f64()
    }

    /// Bounding the kernel queue must not cost the transfer its speed.
    ///
    /// Loopback is far faster than any LAN, so the low-water mark drains
    /// quicker here than it ever will on a real link: if the drainer keeps a
    /// loopback socket fed, it keeps a gigabit one fed with room to spare.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg_attr(
        debug_assertions,
        ignore = "an unoptimised build measures its own cipher and serialiser, a few MB/s; \
                  run with --release"
    )]
    async fn a_bounded_backlog_keeps_bulk_throughput() {
        let unbounded = bulk_throughput(false, 128).await;
        let bounded = bulk_throughput(true, 128).await;
        // Both runs are limited by the cipher, not the socket, so they sit
        // within run-to-run noise of each other (about a quarter, measured);
        // a bound that stalled the drainer would show up as a halving.
        assert!(
            bounded >= unbounded * 0.5,
            "bounded {:.0} MB/s against unbounded {:.0} MB/s",
            bounded / 1e6,
            unbounded / 1e6
        );
    }
}
