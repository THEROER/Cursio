//! Priority multiplexing for the shared encrypted channel.
//!
//! Everything — input, audio, window pixels, clipboard, files — rides one Noise
//! link. Without scheduling, a large payload (a clipboard image, a file) sent as
//! many back-to-back frames would sit in front of the next mouse event and the
//! pointer would visibly lag. [`PrioritizedTx`] fixes that: [`send`] does not
//! write to the socket, it *chunks and queues* the message by
//! [`Message::priority`] and returns immediately. A background **drainer** owns
//! the real socket and always emits a frame from the highest non-empty class,
//! re-checking between every frame — so an input frame queued while a bulk
//! transfer is draining jumps ahead of that transfer's remaining chunks.
//!
//! [`send`]: cursio_transport::TransportTx::send
//!
//! ## Why chunk-then-queue plaintext (not sealed frames)
//!
//! Noise nonces must advance in exactly the order ciphertext hits the wire. So
//! we queue *tagged plaintext* pieces (via [`chunk_plaintext`]) and let the
//! drainer seal each one the instant before it sends it — never sooner. Sealing
//! at `send` time would fix the nonce order at enqueue time and interleaving
//! would corrupt the stream.
//!
//! ## Starvation
//!
//! Strict priority is broken on purpose in one place: when bulk traffic has
//! waited out `BULK_STARVATION_LIMIT` frames of the classes above it, one
//! whole bulk message goes next. Control and input are exempt and still go
//! first; see `Queues::pop_highest`.
//!
//! ## Latency vs. buffering
//!
//! Queues are unbounded but drained continuously, so steady traffic (input,
//! audio) never accumulates — latency stays minimal, which is the whole point.
//! Only a genuine bulk transfer builds depth, and only in its own low-priority
//! class where it can't delay anything above it.
//!
//! ## Where priority stops
//!
//! Ordering here is only worth as much as the queue *after* it is shallow.
//! Two things bound what an input frame can find in front of it once chosen:
//! the frame the drainer is part-way through writing, at most one Noise frame
//! (64 KiB — a frame, once started, has to finish or the stream is corrupt),
//! and whatever the kernel still holds unsent, which
//! [`crate::send_backlog`] keeps to a few tens of KiB. Without that second
//! bound the socket buffer autotuned to megabytes and the scheduling above
//! was decided long before the bytes actually left.
//!
//! Frames stay at the Noise maximum rather than being cut smaller for the
//! lower classes: at gigabit one is half a millisecond, and smaller parts
//! would push a 64 MiB message past the part count every receiver accepts.
//!
//! ## Measuring it
//!
//! Every frame carries the instant it was queued, and the drainer records how
//! long it took to reach the socket, per class, over a [`DELAY_WINDOW`]. The
//! input figure is logged at the end of each window (see [`log_window`]), so
//! "the mouse lags" can be answered with a number, and read back through
//! [`TransportTx::queue_delay`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cursio_protocol::{Message, Priority};
use tokio::sync::Notify;
use tracing::Instrument;

use crate::crypto::{chunk_plaintext, has_more_parts, seal_frame, SharedNoise};
use crate::{QueueDelay, TransportError, TransportTx};

/// Number of priority classes (see [`Priority`]). Kept as an array index space.
const CLASS_COUNT: usize = 6;

/// Frames of higher-priority traffic that may go ahead of a waiting bulk frame
/// before one is let through regardless.
///
/// Sized so the guarantee is cheap for everything above: at 32, a transfer gets
/// at least one message through per 32 frames of audio, video or metadata, and
/// those wait out one bulk message when it happens. Input and control never
/// do, since a forced frame does not overtake them. Lower would spend more of
/// a busy link on bulk than a latency-sensitive session can afford; much
/// higher stops rescuing the transfer at all.
const BULK_STARVATION_LIMIT: u32 = 32;

/// Map a [`Priority`] to a queue index, highest priority = lowest index so the
/// drainer can scan `0..` and stop at the first non-empty class.
/// [`class_index`] backwards.
const CLASS_ORDER: [Priority; CLASS_COUNT] = [
    Priority::Control,
    Priority::Input,
    Priority::Audio,
    Priority::Video,
    Priority::Metadata,
    Priority::Bulk,
];

fn class_index(p: Priority) -> usize {
    match p {
        Priority::Control => 0,
        Priority::Input => 1,
        Priority::Audio => 2,
        Priority::Video => 3,
        Priority::Metadata => 4,
        Priority::Bulk => 5,
    }
}

/// How long a queue-delay measurement runs before it is logged and published.
///
/// Long enough that one window of ordinary pointer motion holds thousands of
/// input frames, so a p99 is a p99; short enough that a lag the user just
/// noticed shows up in the log while they are still looking at it.
const DELAY_WINDOW: Duration = Duration::from_secs(10);

/// Input p99 at which a window is logged at `info` rather than `debug`.
///
/// Half a frame at 120–144 Hz: below it the pointer lands in the frame it was
/// moved in; above it the user starts to see it trail.
const INPUT_DELAY_NOTICE: Duration = Duration::from_millis(4);

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
struct Queued {
    frame: Vec<u8>,
    queued_at: Instant,
    /// From a message that only matters fresh ([`Message::is_lossy`]).
    lossy: bool,
}

/// A frame the drainer chose, on its way out.
pub(crate) struct OutFrame {
    pub tagged: Vec<u8>,
    pub priority: Priority,
    pub lossy: bool,
}

/// Where the drainer writes, one frame at a time, in the order it chose them.
///
/// Sealing a frame and writing it are one step here because Noise nonces must
/// advance in exactly the order ciphertext reaches the wire. A plain link seals
/// with its one connection's keys ([`Sealing`]); a resumable one numbers the
/// frame, keeps it for resending, and seals it for whichever path is carrying
/// the session at that moment ([`crate::resume`]).
#[async_trait]
pub(crate) trait FrameSink: Send {
    async fn send_frame(&mut self, frame: OutFrame) -> Result<(), TransportError>;
}

/// The sink of a link over one connection.
struct Sealing {
    inner: Box<dyn TransportTx>,
    noise: SharedNoise,
}

#[async_trait]
impl FrameSink for Sealing {
    async fn send_frame(&mut self, frame: OutFrame) -> Result<(), TransportError> {
        let sealed = seal_frame(&self.noise, &frame.tagged)?;
        self.inner.send(&sealed).await
    }
}

/// The per-class frame queues, shared between the [`PrioritizedTx`] handles and
/// the drainer task.
#[derive(Default)]
struct Queues {
    classes: [VecDeque<Queued>; CLASS_COUNT],
    /// Bytes waiting in each class, kept alongside the queues so a producer can
    /// read its own backlog without walking a `VecDeque` of frames.
    queued_bytes: [usize; CLASS_COUNT],
    /// Bytes of each class actually written to the socket, ever. Paired with
    /// `queued_bytes` in [`QueueDepth`]; see there for what it is for.
    sent_bytes: [u64; CLASS_COUNT],
    /// Frames sent from a class above `Bulk` since the last `Bulk` frame went
    /// out, counted only while `Bulk` has something waiting.
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
    delay: [DelayHistogram; CLASS_COUNT],
    /// When the running window began; `None` until the first frame is sent.
    window_start: Option<Instant>,
    /// The last complete window, per class — what `queue_delay` reports, so
    /// every reader sees the same finished figures rather than a window that
    /// is still filling.
    last_window: [QueueDelay; CLASS_COUNT],
}

impl Queues {
    /// Queue one frame in `class`, keeping the byte count with it.
    fn push(&mut self, class: usize, frame: Vec<u8>) {
        self.push_frame(class, frame, false);
    }

    fn push_frame(&mut self, class: usize, frame: Vec<u8>, lossy: bool) {
        self.queued_bytes[class] += frame.len();
        self.classes[class].push_back(Queued {
            frame,
            queued_at: Instant::now(),
            lossy,
        });
    }

    /// Record that a frame of `class` queued at `queued_at` reached the socket
    /// at `now`. Returns the window just completed, when this closes one.
    fn record_sent(
        &mut self,
        class: usize,
        queued_at: Instant,
        now: Instant,
    ) -> Option<[QueueDelay; CLASS_COUNT]> {
        self.delay[class].record(now.saturating_duration_since(queued_at));
        let start = *self.window_start.get_or_insert(now);
        if now.duration_since(start) < DELAY_WINDOW {
            return None;
        }
        let finished: [QueueDelay; CLASS_COUNT] = std::array::from_fn(|c| self.delay[c].summary());
        self.last_window = finished;
        self.delay = Default::default();
        self.window_start = Some(now);
        Some(finished)
    }

    /// Pop the next frame to send: the front of the highest-priority non-empty
    /// class. `None` when everything is empty.
    ///
    /// Returns the class alongside the frame so the drainer can credit the
    /// bytes to it once they are on the wire.
    fn pop_highest(&mut self) -> Option<(usize, Queued)> {
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
        // Control and Input stay above a forced frame and above a release in
        // progress. They cannot starve anything — neither stays non-empty — and
        // a logical message may be up to 64 MiB, so a release that held them
        // back would stall heartbeats and the cursor for the whole of it.
        let bulk = CLASS_COUNT - 1;
        let urgent = class_index(Priority::Input);
        let urgent_waiting = self.classes[..=urgent].iter().any(|q| !q.is_empty());
        let forced = !urgent_waiting
            && !self.classes[bulk].is_empty()
            && (self.releasing_bulk || self.starved_bulk >= BULK_STARVATION_LIMIT);
        let (idx, frame) = if forced {
            self.classes[bulk].pop_front().map(|f| (bulk, f))?
        } else {
            self.classes
                .iter_mut()
                .enumerate()
                .find_map(|(idx, q)| q.pop_front().map(|f| (idx, f)))?
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
        // Leaving the queue is what "no longer waiting" means; the bytes are
        // credited as sent only after the socket takes them.
        self.queued_bytes[idx] -= frame.frame.len();
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
struct Shared {
    queues: Mutex<Queues>,
    wake: Notify,
    drained: Notify,
}

/// A [`TransportTx`] that queues by priority instead of writing synchronously.
///
/// Cloneable, because each session module sends from its own thread: a clone
/// is another handle onto the same queues, and all real work lives in the
/// drainer task spawned by [`Self::spawn`]. The queue closes when the *last*
/// handle goes, so a module finishing early cannot cut off the session's `Bye`.
#[derive(Clone)]
pub struct PrioritizedTx {
    shared: Arc<Shared>,
    _open: Arc<OpenHandle>,
    /// Answers [`TransportTx::holding`] for a sink that can lose its
    /// connection and wait for another.
    holding: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// Closes the queues when dropped, which happens once, with the last clone.
struct OpenHandle(Arc<Shared>);

/// Queues control frames of the link's own without holding the queue open:
/// a resumable link's acknowledgements and keepalives, which must stop when
/// the session drops its sender rather than keep the drainer alive forever.
#[derive(Clone)]
pub(crate) struct QueueKick(std::sync::Weak<Shared>);

impl QueueKick {
    /// Queue `tagged` ahead of everything; `false` once the queue is gone.
    pub(crate) fn push_control(&self, tagged: Vec<u8>) -> bool {
        let Some(shared) = self.0.upgrade() else {
            return false;
        };
        {
            let mut q = shared.queues.lock().unwrap();
            if q.closed || q.terminal_error.is_some() {
                return false;
            }
            q.push(class_index(Priority::Control), tagged);
        }
        shared.wake.notify_one();
        true
    }
}

impl PrioritizedTx {
    /// Wrap a raw inner tx + Noise state in a prioritized sender, spawning the
    /// drainer task that owns the socket. Returns the sender the session sends
    /// through; the drainer runs until the sender is dropped and the queues
    /// empty (or the socket errors).
    pub fn spawn(inner: Box<dyn TransportTx>, noise: SharedNoise) -> Self {
        Self::spawn_sink(Box::new(Sealing { inner, noise }))
    }

    /// A handle for queueing the link's own control frames; see [`QueueKick`].
    pub(crate) fn kick_handle(&self) -> QueueKick {
        QueueKick(Arc::downgrade(&self.shared))
    }

    /// As [`Self::spawn`], writing through `sink` rather than one connection.
    pub(crate) fn spawn_sink(mut sink: Box<dyn FrameSink>) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            wake: Notify::new(),
            drained: Notify::new(),
        });
        let drain_shared = shared.clone();

        // Carried into the task so the delay report names the link it is
        // about; a spawned task otherwise starts with no span at all.
        let span = tracing::Span::current();
        let drainer = async move {
            loop {
                // Take the next frame under the lock, then release it before any
                // await (sealing is CPU-only; the socket write is the await).
                let (frame, closed) = {
                    let mut q = drain_shared.queues.lock().unwrap();
                    match q.pop_highest() {
                        Some(f) => (Some(f), false),
                        None => (None, q.closed),
                    }
                };

                match frame {
                    Some((class, queued)) => {
                        let Queued {
                            frame: tagged,
                            queued_at,
                            lossy,
                        } = queued;
                        let len = tagged.len();
                        let out = OutFrame {
                            tagged,
                            priority: CLASS_ORDER[class],
                            lossy,
                        };
                        if let Err(e) = sink.send_frame(out).await {
                            // The link is gone; nothing more can be sent. Stop.
                            tracing::debug!(error = %e, "prioritized drainer: link closed");
                            drain_shared.queues.lock().unwrap().fail(e.to_string());
                            drain_shared.drained.notify_waiters();
                            break;
                        }
                        // Credited only now: a producer pacing itself against
                        // this rate must see bytes the socket has taken, not
                        // bytes we merely intended to write. The wait is
                        // measured to the same point, since the socket
                        // refusing more is part of what a frame waits for.
                        let finished = {
                            let mut q = drain_shared.queues.lock().unwrap();
                            q.sent_bytes[class] += len as u64;
                            q.record_sent(class, queued_at, Instant::now())
                        };
                        if let Some(window) = finished {
                            log_window(&window);
                        }
                    }
                    None => {
                        // Queues are empty: wake anyone blocked in `flush`.
                        drain_shared.drained.notify_waiters();
                        if closed {
                            break; // sender dropped and nothing left to flush
                        }
                        // Nothing to do: park until woken by a send or close.
                        drain_shared.wake.notified().await;
                    }
                }
            }
            drain_shared.drained.notify_waiters();
        };
        tokio::spawn(drainer.instrument(span));

        PrioritizedTx {
            _open: Arc::new(OpenHandle(shared.clone())),
            shared,
            holding: None,
        }
    }

    /// Answer [`TransportTx::holding`] with `holding`.
    pub(crate) fn with_holding(mut self, holding: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.holding = Some(holding);
        self
    }
}

/// Report a finished delay window, if input moved during it.
///
/// A window with no input frames says nothing about the pointer and is not
/// logged: an idle link would otherwise write a line every ten seconds
/// forever. Video and bulk ride along because they are what input waits
/// behind, and seeing them deep while input stays shallow is the evidence
/// that priority is holding.
fn log_window(window: &[QueueDelay; CLASS_COUNT]) {
    let input = window[class_index(Priority::Input)];
    if input.frames == 0 {
        return;
    }
    let video = window[class_index(Priority::Video)];
    let bulk = window[class_index(Priority::Bulk)];
    if input.p99 >= INPUT_DELAY_NOTICE {
        tracing::info!(
            input_frames = input.frames,
            input_p50 = ?input.p50,
            input_p99 = ?input.p99,
            input_max = ?input.max,
            video_p99 = ?video.p99,
            bulk_p99 = ?bulk.p99,
            "input waited long enough to be felt before reaching the socket"
        );
    } else {
        tracing::debug!(
            input_frames = input.frames,
            input_p50 = ?input.p50,
            input_p99 = ?input.p99,
            input_max = ?input.max,
            video_p99 = ?video.p99,
            bulk_p99 = ?bulk.p99,
            "send queue delay"
        );
    }
}

#[async_trait]
impl TransportTx for PrioritizedTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        // Chunk to tagged-plaintext frames (may be one, or many for a big
        // payload), then drop them into this message's priority class. This is
        // the only work `send` does — no socket, no crypto — so the caller (the
        // session loop) returns at once and never blocks behind a bulk transfer.
        let idx = class_index(msg.priority());
        let lossy = msg.is_lossy();
        let frames = chunk_plaintext(msg)?;
        {
            let mut q = self.shared.queues.lock().unwrap();
            if let Some(error) = &q.terminal_error {
                return Err(TransportError::Other(error.clone()));
            }
            for f in frames {
                q.push_frame(idx, f, lossy);
            }
        }
        self.shared.wake.notify_one();
        Ok(())
    }

    fn queue_depth(&self, priority: cursio_protocol::Priority) -> crate::QueueDepth {
        let idx = class_index(priority);
        let q = self.shared.queues.lock().unwrap();
        crate::QueueDepth {
            queued_bytes: q.queued_bytes[idx],
            sent_bytes: q.sent_bytes[idx],
        }
    }

    fn queue_delay(&self, priority: Priority) -> QueueDelay {
        self.shared.queues.lock().unwrap().last_window[class_index(priority)]
    }

    fn clone_sender(&self) -> Option<Box<dyn TransportTx>> {
        Some(Box::new(self.clone()))
    }

    fn holding(&self) -> bool {
        self.holding.as_ref().is_some_and(|holding| holding())
    }

    async fn flush(&mut self) {
        loop {
            // Register interest *before* checking, so a drain that completes
            // between the check and the await can't slip past us.
            let drained = self.shared.drained.notified();
            let done = {
                let q = self.shared.queues.lock().unwrap();
                q.pop_is_empty() || q.terminal_error.is_some()
            };
            if done {
                return;
            }
            // Make sure the drainer is running, then wait for it to empty the
            // queues (or for the link to die, which also ends draining).
            self.shared.wake.notify_one();
            drained.await;
        }
    }
}

impl Drop for OpenHandle {
    fn drop(&mut self) {
        // Mark closed so the drainer exits once it has flushed what's queued,
        // then wake it in case it's parked.
        if let Ok(mut q) = self.0.queues.lock() {
            q.closed = true;
        }
        self.0.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_order_is_control_first_bulk_last() {
        // The drainer scans indices ascending; assert the mapping puts the most
        // urgent class first and the bulk class last.
        assert_eq!(class_index(Priority::Control), 0);
        assert!(class_index(Priority::Input) < class_index(Priority::Audio));
        assert!(class_index(Priority::Audio) < class_index(Priority::Video));
        assert_eq!(class_index(Priority::Bulk), CLASS_COUNT - 1);
    }

    #[test]
    fn pop_highest_prefers_lower_index() {
        let mut q = Queues::default();
        const BULK: u8 = 0xBB;
        const INPUT: u8 = 0x11;
        const AUDIO: u8 = 0xAA;
        q.push(class_index(Priority::Bulk), vec![BULK]);
        q.push(class_index(Priority::Input), vec![INPUT]);
        q.push(class_index(Priority::Audio), vec![AUDIO]);
        // Input must come out before audio, audio before bulk.
        assert_eq!(q.pop_highest().map(|(_, f)| f.frame), Some(vec![INPUT]));
        assert_eq!(q.pop_highest().map(|(_, f)| f.frame), Some(vec![AUDIO]));
        assert_eq!(q.pop_highest().map(|(_, f)| f.frame), Some(vec![BULK]));
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
        let mut q = Queues::default();
        let bulk = class_index(Priority::Bulk);
        let meta = class_index(Priority::Metadata);

        // One transfer waiting, and metadata arriving faster than it drains.
        for _ in 0..8 {
            q.push(bulk, vec![0u8; 4]);
        }
        let mut sent_bulk = 0;
        for _ in 0..(BULK_STARVATION_LIMIT * 4) {
            q.push(meta, vec![0u8; 1]);
            if let Some((idx, _)) = q.pop_highest() {
                if idx == bulk {
                    sent_bulk += 1;
                }
            }
        }

        assert!(
            sent_bulk > 0,
            "a transfer makes progress however busy the class above it is"
        );
    }

    /// A forced bulk frame releases the rest of its message with it.
    ///
    /// The quota counts frames, but a bulk message is many: an application's
    /// 1.2 MB write is chunked to the Noise plaintext limit and arrives here as
    /// thirteen. Charged one at a time, that write needed thirteen turns of the
    /// quota and four hundred metadata frames could pass in between — which is
    /// why guaranteeing one frame in thirty-two barely moved a transfer.
    #[test]
    fn a_forced_bulk_message_goes_out_whole() {
        let mut q = Queues::default();
        let bulk = class_index(Priority::Bulk);
        let meta = class_index(Priority::Metadata);

        // One file-sized message, and endless metadata alongside it.
        let parts = chunked_message();
        let part_count = parts.len();
        assert!(part_count > 1, "the test message must need several frames");
        for f in parts {
            q.push(bulk, f);
        }
        for _ in 0..64 {
            q.push(meta, vec![0u8; 1]);
        }

        // Drain until the first bulk frame is forced, then the run must follow
        // without interruption.
        let mut seen_bulk = 0;
        let mut run_broken = false;
        while let Some((idx, _)) = q.pop_highest() {
            if idx == bulk {
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

    /// A release ends with its message, not with the bulk queue.
    ///
    /// Every piece of a chunked message is tagged `PART`, the last included, so
    /// keying the release on the tag alone let one forced frame carry every
    /// transfer queued behind it — and nothing else went out meanwhile.
    #[test]
    fn a_release_stops_at_the_end_of_its_message() {
        let mut q = Queues::default();
        let bulk = class_index(Priority::Bulk);
        let meta = class_index(Priority::Metadata);

        let part_count = chunked_message().len();
        for _ in 0..3 {
            for f in chunked_message() {
                q.push(bulk, f);
            }
        }
        q.starved_bulk = BULK_STARVATION_LIMIT;
        q.push(meta, vec![0u8; 1]);

        for _ in 0..part_count {
            assert_eq!(q.pop_highest().map(|(idx, _)| idx), Some(bulk));
        }
        assert_eq!(
            q.pop_highest().map(|(idx, _)| idx),
            Some(meta),
            "the next message waits for its own turn of the quota"
        );
    }

    /// Input and control still go first while a bulk message is released.
    ///
    /// A logical message may run to 64 MiB; holding the cursor and heartbeats
    /// back for all of it would be felt as a frozen pointer or a dropped link.
    #[test]
    fn input_overtakes_a_bulk_release_in_progress() {
        let mut q = Queues::default();
        let bulk = class_index(Priority::Bulk);
        let input = class_index(Priority::Input);
        let control = class_index(Priority::Control);

        for f in chunked_message() {
            q.push(bulk, f);
        }
        q.starved_bulk = BULK_STARVATION_LIMIT;
        assert_eq!(q.pop_highest().map(|(idx, _)| idx), Some(bulk));

        q.push(input, vec![0u8; 1]);
        q.push(control, vec![0u8; 1]);
        assert_eq!(q.pop_highest().map(|(idx, _)| idx), Some(control));
        assert_eq!(q.pop_highest().map(|(idx, _)| idx), Some(input));
        assert_eq!(
            q.pop_highest().map(|(idx, _)| idx),
            Some(bulk),
            "the release resumes once the urgent frames are out"
        );
    }

    /// The tagged frames of one message the size of a file read reply.
    fn chunked_message() -> Vec<Vec<u8>> {
        chunk_plaintext(&Message::Opaque(vec![0u8; 256 * 1024])).unwrap()
    }

    /// Credit is not banked while nothing bulk is waiting.
    ///
    /// Otherwise a quiet period of metadata would buy a later transfer the
    /// right to jump the queue for frames that had never been delayed.
    #[test]
    fn metadata_alone_banks_no_credit_against_bulk() {
        let mut q = Queues::default();
        let meta = class_index(Priority::Metadata);
        for _ in 0..(BULK_STARVATION_LIMIT * 2) {
            q.push(meta, vec![0u8; 1]);
            q.pop_highest();
        }
        assert_eq!(
            q.starved_bulk, 0,
            "no bulk was waiting, so nothing was starved"
        );
    }

    /// The backlog a class reports must be its own, and must fall as the
    /// drainer takes frames.
    ///
    /// This is what the files module paces itself against: a count that never
    /// dropped would read as a permanently congested link and throttle a
    /// transfer to nothing, while one that counted every class would let a
    /// burst of audio look like a file backlog.
    #[test]
    fn a_class_reports_only_its_own_backlog() {
        let mut q = Queues::default();
        q.push(class_index(Priority::Bulk), vec![0u8; 100]);
        q.push(class_index(Priority::Bulk), vec![0u8; 50]);
        q.push(class_index(Priority::Input), vec![0u8; 7]);

        assert_eq!(q.queued_bytes[class_index(Priority::Bulk)], 150);
        assert_eq!(q.queued_bytes[class_index(Priority::Input)], 7);

        // Input drains first, leaving the bulk backlog untouched.
        let (class, frame) = q.pop_highest().expect("a frame");
        assert_eq!(class, class_index(Priority::Input));
        assert_eq!(frame.frame.len(), 7);
        assert_eq!(q.queued_bytes[class_index(Priority::Bulk)], 150);
        assert_eq!(q.queued_bytes[class_index(Priority::Input)], 0);

        q.pop_highest().expect("a bulk frame");
        assert_eq!(q.queued_bytes[class_index(Priority::Bulk)], 50);
    }

    #[test]
    fn terminal_failure_discards_every_backlog() {
        let mut q = Queues::default();
        q.push(class_index(Priority::Video), vec![0; 100]);
        q.push(class_index(Priority::Bulk), vec![0; 200]);
        q.fail("socket closed".into());

        assert!(q.pop_is_empty());
        assert_eq!(q.queued_bytes, [0; CLASS_COUNT]);
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
        let mut q = Queues::default();
        let input = class_index(Priority::Input);
        let start = Instant::now();

        let queued = start;
        assert!(q
            .record_sent(input, queued, start + Duration::from_millis(2))
            .is_none());
        assert_eq!(q.last_window[input], QueueDelay::default());

        let closing = start + DELAY_WINDOW + Duration::from_millis(3);
        let window = q
            .record_sent(input, queued, closing)
            .expect("the window has run its length");
        assert_eq!(window[input].frames, 2);
        assert_eq!(q.last_window[input], window[input]);
        assert_eq!(q.delay[input].frames, 0, "the next window starts empty");
    }

    // --- End-to-end scheduling + flush over the real encrypted transport. ---
    //
    // The in-memory transport has no backpressure — the drainer empties the
    // queues into it instantly — so it can show that scheduling keeps the
    // stream intact and that flush works, but not that input overtakes a
    // backlog. That needs a socket whose far end is slower than the near one,
    // which the loopback tests further down arrange.

    use crate::crypto::EncryptedTransport;
    use crate::pairing::Role;
    use crate::tcp::TcpTransport;
    use crate::test_util::MemTransport;
    use crate::{Transport, TransportRx};
    use cursio_protocol::{InputEvent, Message};

    /// Priority-scheduled sends must still decode correctly on the far side: a
    /// bulk payload and an input event both round-trip intact through the
    /// prioritized tx + chunking + reassembly path.
    #[tokio::test]
    async fn prioritized_sends_roundtrip_intact() {
        let (a, b) = MemTransport::pair();
        let init = tokio::spawn(async move {
            EncryptedTransport::establish(a, b"k", Role::Initiator)
                .await
                .unwrap()
        });
        let resp = EncryptedTransport::establish(b, b"k", Role::Responder)
            .await
            .unwrap();
        let init = init.await.unwrap();

        let (mut tx, _u) = Box::new(init).split_prioritized();
        let (_u2, mut rx) = Box::new(resp).split();

        let bulk = Message::Opaque(vec![0u8; 800_000]); // ~13 frames
        let input = Message::Input(InputEvent::PointerMotion { dx: 5, dy: -3 });

        tx.send(&bulk).await.unwrap();
        tx.send(&input).await.unwrap();

        // Both arrive intact. (Order isn't asserted: without socket backpressure
        // the mock can't reproduce the interleave — see the note above.)
        let mut got = [rx.recv().await.unwrap(), rx.recv().await.unwrap()];
        got.sort_by_key(|m| format!("{m:?}").len());
        assert!(got.contains(&input), "input must round-trip");
        assert!(got.contains(&bulk), "bulk must round-trip");
    }

    /// `flush` must not return until queued frames have actually been sent, so a
    /// final `Bye` isn't lost when the session drops the tx.
    #[tokio::test]
    async fn flush_waits_for_queued_frames() {
        let (a, b) = MemTransport::pair();
        let init = tokio::spawn(async move {
            EncryptedTransport::establish(a, b"k", Role::Initiator)
                .await
                .unwrap()
        });
        let resp = EncryptedTransport::establish(b, b"k", Role::Responder)
            .await
            .unwrap();
        let init = init.await.unwrap();

        let (mut tx, _u) = Box::new(init).split_prioritized();
        let (_u2, mut rx) = Box::new(resp).split();

        tx.send(&Message::Bye).await.unwrap();
        tx.flush().await; // must have hit the wire by now

        // Receiver already has it available.
        assert_eq!(rx.recv().await.unwrap(), Message::Bye);
    }

    #[tokio::test]
    async fn writer_failure_unblocks_flush_and_rejects_later_sends() {
        let (a, b) = MemTransport::pair();
        let init = tokio::spawn(async move {
            EncryptedTransport::establish(a, b"k", Role::Initiator)
                .await
                .unwrap()
        });
        let resp = EncryptedTransport::establish(b, b"k", Role::Responder)
            .await
            .unwrap();
        let init = init.await.unwrap();

        let (mut tx, _rx) = Box::new(init).split_prioritized();
        let (_peer_tx, peer_rx) = Box::new(resp).split();
        drop(peer_rx);

        let _ = tx.send(&Message::Bye).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), tx.flush())
            .await
            .expect("flush remained blocked after the writer failed");

        assert!(tx.send(&Message::Bye).await.is_err());
        assert_eq!(
            tx.queue_depth(Priority::Control).queued_bytes,
            0,
            "dead queues must not retain unsendable frames"
        );
    }

    /// A queued bulk payload must show up as a backlog, and be credited as sent
    /// once it has left — the two readings a producer paces itself with.
    #[tokio::test]
    async fn the_backlog_a_producer_reads_tracks_the_wire() {
        let (a, b) = MemTransport::pair();
        let init = tokio::spawn(async move {
            EncryptedTransport::establish(a, b"k", Role::Initiator)
                .await
                .unwrap()
        });
        let resp = EncryptedTransport::establish(b, b"k", Role::Responder)
            .await
            .unwrap();
        let init = init.await.unwrap();

        let (mut tx, _u) = Box::new(init).split_prioritized();
        let (_u2, mut rx) = Box::new(resp).split();

        assert_eq!(tx.queue_depth(Priority::Bulk), crate::QueueDepth::default());

        tx.send(&Message::Opaque(vec![0u8; 800_000])).await.unwrap();
        tx.flush().await;
        let _ = rx.recv().await.unwrap();

        let depth = tx.queue_depth(Priority::Bulk);
        assert_eq!(depth.queued_bytes, 0, "nothing is left after a flush");
        assert!(
            depth.sent_bytes >= 800_000,
            "the payload must be credited to its own class, got {depth:?}"
        );
        // Another class stays at zero: a bulk transfer is not audio's backlog.
        assert_eq!(
            tx.queue_depth(Priority::Audio),
            crate::QueueDepth::default()
        );
    }

    /// A prioritized sender and a plain receiver over real loopback TCP, plus
    /// the halves each side does not use, kept alive so neither direction
    /// shuts down under the test.
    struct Loopback {
        tx: Box<dyn TransportTx>,
        rx: Box<dyn TransportRx>,
        _unused: (Box<dyn TransportRx>, Box<dyn TransportTx>),
    }

    /// `receive_window` caps the receiver's socket buffer, standing in for the
    /// bandwidth-delay product of a real link: left to autotune on loopback it
    /// grows to tens of megabytes and would hold the backlog itself, which no
    /// receiver that reads as fast as the network delivers ever does.
    async fn loopback(bounded: bool, receive_window: Option<u32>) -> Loopback {
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
            TcpTransport::from_stream(dialled)
        } else {
            TcpTransport::from_stream_unbounded(dialled)
        }
        .unwrap();
        let receiver = TcpTransport::from_stream(accepted.await.unwrap()).unwrap();

        let init = tokio::spawn(async move {
            EncryptedTransport::establish(sender, b"k", Role::Initiator)
                .await
                .unwrap()
        });
        let resp = EncryptedTransport::establish(receiver, b"k", Role::Responder)
            .await
            .unwrap();
        let (tx, sender_rx) = Box::new(init.await.unwrap()).split_prioritized();
        let (receiver_tx, rx) = Box::new(resp).split();
        Loopback {
            tx,
            rx,
            _unused: (sender_rx, receiver_tx),
        }
    }

    /// One file read reply's worth: five Noise frames, like `FilesMsg::MAX_CHUNK`.
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

        let Loopback {
            mut tx,
            mut rx,
            _unused,
        } = loopback(bounded, Some(RECEIVE_WINDOW)).await;

        let input = Message::Input(InputEvent::PointerMotion { dx: 1, dy: 1 });
        let expected = input.clone();
        let input_sent = Arc::new(AtomicBool::new(false));
        let after_input = input_sent.clone();
        let reader = tokio::spawn(async move {
            let mut ahead = 0;
            loop {
                match rx.recv().await.unwrap() {
                    msg if msg == expected => return ahead,
                    Message::Opaque(bytes) if after_input.load(Ordering::SeqCst) => {
                        ahead += bytes.len();
                    }
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        for _ in 0..64 {
            tx.send(&Message::Opaque(vec![0u8; BULK_MESSAGE]))
                .await
                .unwrap();
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
        let Loopback {
            mut tx,
            mut rx,
            _unused,
        } = loopback(bounded, None).await;

        let reader = tokio::spawn(async move {
            for _ in 0..messages {
                rx.recv().await.unwrap();
            }
            Instant::now()
        });
        let started = Instant::now();
        for _ in 0..messages {
            tx.send(&Message::Opaque(vec![0u8; BULK_MESSAGE]))
                .await
                .unwrap();
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

    /// A module's clone keeps the link open after the session's own handle is
    /// gone, and what it sends still arrives; the queue closes with the last.
    #[tokio::test]
    async fn a_cloned_sender_shares_the_queue_until_the_last_handle_drops() {
        let (a, b) = MemTransport::pair();
        let init = tokio::spawn(async move {
            EncryptedTransport::establish(a, b"k", Role::Initiator)
                .await
                .unwrap()
        });
        let resp = EncryptedTransport::establish(b, b"k", Role::Responder)
            .await
            .unwrap();
        let init = init.await.unwrap();

        let (tx, _u) = Box::new(init).split_prioritized();
        let (_u2, mut rx) = Box::new(resp).split();

        let mut clone = tx.clone_sender().expect("a prioritized sender clones");
        drop(tx);
        clone.send(&Message::Heartbeat { nonce: 7 }).await.unwrap();
        clone.flush().await;
        assert_eq!(rx.recv().await.unwrap(), Message::Heartbeat { nonce: 7 });

        drop(clone);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("the link must close once the last handle is gone")
                .is_err()
        );
    }
}
