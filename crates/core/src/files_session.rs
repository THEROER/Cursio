//! The files [`SessionModule`]: the peer's disks here, ours there.
//!
//! Two halves sharing one message type:
//!
//! - **Server half** — answers the peer's requests against our own volumes.
//!   The work is real disk I/O, so it never runs inline: see [`FilesEngine::serve`].
//! - **Client half** — carries requests from the FUSE mount ([`cursio_files::mount`])
//!   to the peer and routes replies back to whichever callback is waiting.
//!
//! ## Nothing blocks the session loop
//!
//! A `pump` that takes longer than ~50 ms is logged as having blocked the
//! session, and the shared cursor rides the same channel — so a file read must
//! never happen inline. Disk work, and the compression that goes with it, is
//! handed to low-priority worker pools ([`cursio_files::Workers`]) — one
//! serving the peer, one for our own compression, so neither side's traffic
//! queues behind the other's — and collected on a later pump. Each pass is bounded by a time slice rather than
//! a message count, so a pump stays short whatever the size of the transfer.
//!
//! ## Nothing else slows down either
//!
//! A transfer is the heaviest thing cursio does, and it happens while the user
//! is still working on both machines. Three separate mechanisms keep it from
//! being felt elsewhere, because they answer three different questions:
//!
//! - *Who gets the link?* — a file's bytes ride `Bulk`, the lowest class the
//!   transport schedules, so a mouse event queued mid-transfer is written
//!   before the transfer's next frame; questions about paths ride `Metadata`,
//!   one class up, so an `ls` does not wait out a copy (see
//!   [`FilesMsg::priority`]).
//! - *Who gets the CPU and the disk?* — the worker pool runs below normal
//!   thread priority and is bounded well under the core count, so foreground
//!   applications are scheduled first.
//! - *How much may be in flight at once?* — [`Pacer`] sizes the window of
//!   outstanding requests from the delay it measures on the link, so file
//!   traffic keeps the pipe full without building a queue in front of
//!   everything sharing it.

use std::collections::HashMap;
// Every lock of the file server recovers from poisoning rather than
// propagating it. A panic in one request — the one kind the worker pool now
// survives — would otherwise poison the server for every request after it,
// and the peer's mount would stop answering for the rest of the session. The
// server's state is a table of open handles; the entry a panicking request
// was touching is the worst it can leave behind.
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use cursio_files::bridge::{Op, PeerLink};
use cursio_files::{FileServer, Workers};
use cursio_protocol::{
    Chunk, FileError, FileHandle, FileReqId, FilesMsg, Message, ModuleId, Volume,
};
use cursio_transport::{QueueDepth, TransportError, TransportTx};

use crate::module::SessionModule;

/// How long one pump may spend moving file traffic before handing the session
/// loop back.
///
/// A budget in time rather than in messages, because messages are not the cost:
/// a pass of thirty `Ok` replies is free, while a pass of thirty 256 KiB chunks
/// is thirty memory copies. A fixed count has to be tuned for the expensive
/// case, which then throttles the cheap one — the count that kept large reads
/// smooth left the mount's threads parked between passes on a directory walk.
///
/// Sized against the loop's own 5 ms pace: file traffic may take a share of
/// each turn and no more, so input and audio keep their pumps whatever the
/// transfer is doing.
const PUMP_SLICE: Duration = Duration::from_millis(2);

/// How the pump's slice is divided between replies, our data and our requests.
///
/// Replies take two shares of four because a peer is blocked on them and they
/// are usually small; data and requests take one each. What matters is that
/// none of the three can take the whole slice — see the division in `pump`.
const SLICE_SHARES: u32 = 4;

/// Shares of [`SLICE_SHARES`] reserved for replies to the peer.
const REPLY_SHARES: u32 = 2;

/// Absolute cap on messages moved in one pump.
///
/// The time slice is the real budget; this only bounds the damage if the clock
/// misbehaves (a suspended laptop, a coarse timer) so a pass cannot run away
/// with the loop.
const MAX_MESSAGES_PER_PUMP: usize = 512;

/// How long file traffic may sit in the link's queue before the pacer eases
/// off.
///
/// Not zero, and not large. Some queue is what keeps the link busy: with none,
/// every round trip is a stall, and a transfer runs at one chunk per RTT. Too
/// much and the queue itself becomes the problem — bytes committed to the wire
/// minutes before they are wanted, a `Bye` that cannot get out, and a Wi-Fi
/// radio held busy on behalf of a background copy while the user's own traffic
/// waits behind it in the same buffers.
///
/// Set against the round trip rather than against a copy's throughput. A
/// transfer that keeps a link busy *must* hold roughly bandwidth × delay in
/// flight — that product is what a full pipe looks like, not what congestion
/// looks like — so a target below it makes the pacer fight the very state it
/// wants.
///
/// A tenth of a second was chosen against a 6 ms round trip, where it is 16×
/// the pipe and no transfer could reach it. On Wi-Fi the same figure is barely
/// three round trips: a 1 MiB queue on an 8 MB/s link is 125 ms, so an ordinary
/// copy tripped the threshold immediately and the window collapsed
/// 128 → 64 → 32 → 20 in a single millisecond. Less in flight means less on the
/// wire, which lowers the measured rate, which makes the same queue look longer
/// still — the controller talking itself down to a fraction of the link. Seen
/// live: 3.1 MB/s of file traffic on a link measured at 8 MB/s in the same log
/// line.
///
/// Half a second was the first choice and it was two orders of magnitude too
/// generous. It is the definition of what queue theory calls a *bad* queue: one
/// held at length as a steady state, adding delay without improving throughput
/// at all. CoDel, which exists for this exact failure, targets 5 ms and its
/// authors report that figure working "in just about any setting".
///
/// 5 ms is not reachable here — one 256 KiB chunk alone is about 15 ms on this
/// link, so the smallest possible queue already exceeds it. 50 ms is a tenth of
/// what this was, leaves room for a few chunks, and is the difference between a
/// queue that drains and one that stands: measured live, the old target held a
/// 581 ms round trip on a link whose own idle latency was 5 ms.
///
/// Every request here waits for its answer, so a standing queue is not merely
/// latency somewhere — it is added to every single operation, and it is what
/// turned an ordinary download into `rtt_max` spikes of six seconds and the
/// device errors that ended it.
const TARGET_QUEUE_DELAY: Duration = Duration::from_millis(50);

/// Never fewer than this many requests in flight.
///
/// The floor exists so that easing off cannot turn into a stall: the mount's
/// callers are blocked in a filesystem call with a deadline
/// ([`cursio_files::bridge::OP_TIMEOUT`]), so a window that shrank toward one
/// would turn congestion into `EIO`.
///
/// Eight was chosen to cover a directory walk without the window having to grow
/// first, and on a LAN it is itself a queue: BBR sizes a connection at about
/// twice the bandwidth-delay product, which on this link — 130 Mbit/s at a 5 ms
/// idle round trip — is *one* 256 KiB chunk. A floor of eight is 2 MB the pacer
/// could never give back however congested it judged the link, which is why
/// narrowing the window above it changed so little. Three keeps a walk moving
/// and a mount answering while leaving the controller somewhere to retreat to.
const MIN_IN_FLIGHT: usize = 3;

/// Never more than this many, however fast the link looks.
///
/// The window is the pipeline depth, and every request in it holds a buffer on
/// both machines — at 256 KiB a chunk this is the memory a transfer can commit
/// to. Well above the depth a LAN needs (a 1 Gbit/s link with a 6 ms round trip
/// is filled by roughly three 256 KiB chunks in flight), so the cap only ever
/// bites on a link fast enough not to need the help.
const MAX_IN_FLIGHT: usize = 128;

/// Shortest interval between two rate samples.
///
/// Below this the measurement is mostly quantisation: a pump every 5 ms sees a
/// handful of frames, and dividing by a near-zero interval turns one late frame
/// into a wildly wrong rate.
const RATE_SAMPLE_INTERVAL: Duration = Duration::from_millis(20);

/// Least a sample must carry before it is allowed to describe the link.
///
/// A rate measured while nothing much is being sent is a measure of idleness,
/// not of capacity, and this controller cannot tell the two apart on its own:
/// it divides queued bytes by the last rate and reads the answer as congestion.
///
/// Seen on a live link with a 4.8 ms round trip and no loss. Ordinary metadata
/// traffic — a `stat` here, a `readdir` there — moved a few dozen bytes per
/// 20 ms sample, which smoothed down to an estimate of 3–4 KB/s. The next 72 KB
/// queued was then judged to need 15.9 s to drain (it needed about 3.5 ms), so
/// the window halved, which sent even less, which lowered the estimate again.
/// The mount timed out at 3 s and Explorer showed every one of the peer's
/// drives as empty with an I/O error inside.
///
/// One full chunk is the threshold because that is the smallest transfer whose
/// duration says anything about throughput. Below it the previous estimate is
/// kept — the same reasoning as an idle link keeping its estimate rather than
/// re-learning from the floor.
const RATE_SAMPLE_MIN_BYTES: u64 = FilesMsg::MAX_CHUNK as u64;

/// Slowest link this controller will believe in.
///
/// A second guard on the same failure as [`RATE_SAMPLE_MIN_BYTES`], from the
/// other end: that one stops a too-small sample from being recorded, this one
/// stops an estimate that is already absurd from being acted on. Both are
/// needed, because an estimate is smoothed and so outlives the sample that
/// produced it.
///
/// Cursio is a LAN tool — the mesh is trusted devices on one network, and even
/// Wi-Fi measured ~17.5 MB/s here. A link genuinely doing less than this could
/// not carry the audio that shares it, so a rate below this figure is a
/// mismeasurement, not a slow network. Judging a queue by it produces delays in
/// the tens of seconds and a window that shrinks toward its floor for no
/// reason.
///
/// Set at 4 MB/s: comfortably under the slowest link Cursio is meant to run on,
/// so a genuinely congested one is still detected, and high enough that a
/// directory listing's worth of bytes — tens of kilobytes — reads as the few
/// milliseconds it really is rather than as [`TARGET_QUEUE_DELAY`] exceeded.
const RATE_FLOOR_BYTES_PER_SEC: f64 = 4.0 * 1024.0 * 1024.0;

/// Sizes the window of outstanding requests from what the link is doing.
///
/// The mount can always ask for more than the link can carry: a copy issues
/// requests as fast as its threads are answered, and every one of them is a
/// blind write into a queue that never pushes back. Left alone, the queue
/// grows to whatever the mount's concurrency allows and every byte in it is a
/// byte of delay — for this transfer, for anything else sharing the link, and
/// for anything else sharing the network.
///
/// So the window is *measured* rather than configured. Two numbers come back
/// from the transport — bytes waiting, bytes gone — and their ratio is the
/// delay that queue represents. Above [`TARGET_QUEUE_DELAY`] the window halves;
/// comfortably below it, the window grows by one. That is TCP's own
/// additive-increase/multiplicative-decrease rule, for the same reason it uses
/// it: growth has to be cautious and retreat immediate, or the controller
/// oscillates around the very congestion it is trying to avoid.
///
/// The effect is that the window finds the link. On a gigabit LAN it settles
/// near the ceiling; on a saturated Wi-Fi link it settles low, and the transfer
/// slows down instead of everything else doing so.
struct Pacer {
    /// Requests allowed outstanding at once.
    in_flight_limit: usize,
    /// Measured drain rate of our own traffic class, in bytes per second.
    /// `None` until enough has moved to measure anything.
    rate: Option<f64>,
    /// The previous reading: when it was taken, and the sent-byte count then.
    last_sample: Option<(Instant, u64)>,
    /// The same estimate taken from bytes the peer has confirmed storing.
    ///
    /// Kept beside `rate` rather than replacing it: the socket's count is still
    /// what tells a stalled link from an idle one promptly, while this is what
    /// the window is sized from — see [`Pacer::sample_acked`].
    acked_rate: Option<f64>,
    acked_sample_at: Option<Instant>,
    acked_since_sample: u64,
    /// When the window was last halved.
    ///
    /// A cut takes a round trip to show up in the queue: the requests already
    /// on the wire have to be answered before a shorter queue can be observed.
    /// Without this the pacer read the *same* backlog on consecutive pumps and
    /// cut again for each one — 128 → 64 → 32 → 20 inside a single millisecond
    /// in the log — arriving at the floor over a queue that one cut would have
    /// cleared.
    last_cut: Option<Instant>,
    /// How many times the window has been halved, and how many times widened.
    ///
    /// Reported rather than merely acted on, because the pacer's own decisions
    /// are invisible from outside: a transfer running at a fraction of the link
    /// looks identical to a slow network. The bug that made this worth counting
    /// had the window collapsing on a link the same log line measured at four
    /// times the speed being achieved — countable only by watching the
    /// controller, not the throughput.
    cuts: u64,
    grows: u64,
}

/// How long one of the peer's requests may take before it is named in the log.
///
/// Set below the mount's own deadline (`cursio_files::bridge::OP_TIMEOUT`, 3 s)
/// so a request that is *about* to be given up on is reported before the
/// application sees a device error rather than after. Well above an ordinary
/// disk operation, which is sub-millisecond.
const SLOW_SERVE_WARN: Duration = Duration::from_millis(1500);

/// How often the file module reports what the link is doing.
///
/// Matched to the audio module's own health line, so a log read after the fact
/// interleaves the two at the same cadence.
const HEALTH_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// What the link is measured to be doing, as opposed to what it is asked to do.
///
/// Every field here answers a question that cost real time to answer by hand:
///
/// - *Is the network slow, or are we?* — `rate_kb_s` against `rtt_ms`. A link
///   with a short round trip and a low rate is not a slow network.
/// - *Is the pacer helping?* — `window`, `cuts`, `grows`. A window sitting at
///   the floor while the rate is high is the controller fighting the link.
/// - *Where is the time going?* — `rtt_ms` against the size of what is being
///   moved. Small operations are bounded by round trips, large ones by rate,
///   and the fix is different for each.
#[derive(Default)]
struct Health {
    /// When the last report went out.
    last_log: Option<Instant>,
    /// Round trips measured since the last report: total and count, so the mean
    /// can be reported without holding every sample.
    rtt_total: Duration,
    rtt_count: u64,
    /// The slowest single round trip in this window, which is what a user feels
    /// as a stall even when the mean looks healthy.
    rtt_max: Duration,
    /// When each outstanding request was sent, so its answer can be timed.
    sent_at: HashMap<FileReqId, Instant>,
    /// Bytes carried in each direction since the last report.
    read_bytes: u64,
    written_bytes: u64,
    /// Requests answered, and requests that failed.
    answered: u64,
    failed: u64,
    /// Where a write's round trip is actually spent, summed since the last
    /// report, with the count so a mean can be taken.
    ///
    /// The total is easy to see and useless on its own: a round trip of 800 ms
    /// on a link whose queue holds 130 ms of traffic says the time went
    /// somewhere else, and every candidate — compression, the wire, the peer's
    /// disk — was individually measured too fast to explain it. Splitting the
    /// trip is the only way to stop guessing which one it is.
    ///
    /// `queued` is from accepting the mount's write to handing the compressed
    /// message to the transport; `wire` is from there to the peer's answer.
    stage_queued: Duration,
    stage_wire: Duration,
    stage_count: u64,
    /// When each write was handed to the transport, to time the wire half.
    sent_to_wire: HashMap<FileReqId, Instant>,
    /// Bytes served *to* the peer since the last report, in each direction.
    ///
    /// Kept apart from `read_bytes`/`written_bytes`, which count only what this
    /// machine asked for. That is the wrong half whenever the peer is the one
    /// driving: Steam writing 51 GB an hour onto our disk reported
    /// `write_kb_s=0`, because every byte of it arrived in a request rather
    /// than leaving in one, and the whole transfer was invisible to the numbers
    /// meant to describe it.
    served_read_bytes: u64,
    served_written_bytes: u64,
    /// Requests sent, counted by kind.
    ///
    /// The totals alone say how much was asked and not what was asked, and the
    /// two lead to different fixes: a workload dominated by `stat` wants a
    /// cache, one dominated by `open` wants fewer opens, one dominated by
    /// `read` wants a bigger read. Working that out by reading a log of
    /// refusals only — the successful calls being invisible — put the count of
    /// round trips per created file at "somewhere between 4 and 15".
    by_kind: HashMap<&'static str, u64>,
}

impl Health {
    /// Note that a request has gone out.
    fn sent(&mut self, req: FileReqId, kind: &'static str, now: Instant) {
        self.sent_at.insert(req, now);
        *self.by_kind.entry(kind).or_insert(0) += 1;
    }

    /// Note that a request has been answered, timing its round trip.
    fn answered(&mut self, req: FileReqId, now: Instant) {
        self.answered += 1;
        if let Some(at) = self.sent_at.remove(&req) {
            let rtt = now.saturating_duration_since(at);
            self.rtt_total += rtt;
            self.rtt_count += 1;
            self.rtt_max = self.rtt_max.max(rtt);
        }
        if let Some(at) = self.sent_to_wire.remove(&req) {
            self.stage_wire += now.saturating_duration_since(at);
            self.stage_count += 1;
        }
    }

    /// Note that a write has reached the transport, splitting its round trip
    /// into the part spent here and the part spent on the link.
    fn reached_wire(&mut self, req: FileReqId, now: Instant) {
        if let Some(accepted) = self.sent_at.get(&req) {
            self.stage_queued += now.saturating_duration_since(*accepted);
        }
        self.sent_to_wire.insert(req, now);
    }

    /// Mean of one stage total over the writes that completed, in ms.
    fn mean_stage_ms(&self, total: Duration) -> u64 {
        match self.stage_count {
            0 => 0,
            n => (total.as_millis() as u64) / n,
        }
    }

    /// Mean round trip over this reporting window, in milliseconds.
    fn mean_rtt_ms(&self) -> u64 {
        match self.rtt_count {
            0 => 0,
            n => (self.rtt_total.as_millis() as u64) / n,
        }
    }

    /// Clear the per-window counters, keeping the outstanding-request map: a
    /// request sent before a report is still owed an answer after it.
    fn roll(&mut self, now: Instant) {
        self.last_log = Some(now);
        self.rtt_total = Duration::ZERO;
        self.rtt_count = 0;
        self.rtt_max = Duration::ZERO;
        self.read_bytes = 0;
        self.written_bytes = 0;
        self.served_read_bytes = 0;
        self.served_written_bytes = 0;
        self.stage_queued = Duration::ZERO;
        self.stage_wire = Duration::ZERO;
        self.stage_count = 0;
        let deadline = cursio_files::bridge::BULK_OP_TIMEOUT;
        self.sent_to_wire
            .retain(|_, at| now.saturating_duration_since(*at) < deadline);
        self.answered = 0;
        self.failed = 0;
        self.by_kind.clear();
        // An answer that never came would otherwise hold its entry for the life
        // of the session. The mount gives up after `BULK_OP_TIMEOUT` at the
        // latest, so anything older than that is never going to be timed.
        self.sent_at
            .retain(|_, at| now.saturating_duration_since(*at) < deadline);
    }
}

/// How long a halving is given to take effect before another is allowed.
///
/// One cut per round trip is the most that can be justified, since that is how
/// long it takes for the queue to reflect it. Generous compared with the 5 ms
/// pump so a Wi-Fi round trip (measured 20–38 ms here) is comfortably covered.
const CUT_COOLOFF: Duration = Duration::from_millis(100);

impl Pacer {
    fn new() -> Self {
        Pacer {
            // Start at the floor and grow. The opposite — start wide, retreat
            // on congestion — fills the link's queue before the first
            // measurement exists to say it should not have.
            in_flight_limit: MIN_IN_FLIGHT,
            rate: None,
            acked_rate: None,
            acked_sample_at: None,
            acked_since_sample: 0,
            last_sample: None,
            last_cut: None,
            cuts: 0,
            grows: 0,
        }
    }

    /// Fold in a fresh reading of the link and re-size the window.
    fn observe(&mut self, depth: QueueDepth, now: Instant) {
        self.sample_rate(depth.sent_bytes, now);
        match self.queue_delay(depth.queued_bytes) {
            // Backed up: retreat, but no faster than one halving per round
            // trip — the queue cannot show the effect of a cut sooner than
            // that, so cutting again on the same backlog compounds a single
            // reading into a collapse. See [`Pacer::last_cut`].
            Some(delay)
                if delay > TARGET_QUEUE_DELAY
                    && self
                        .last_cut
                        .is_none_or(|at| now.duration_since(at) >= CUT_COOLOFF) =>
            {
                let was = self.in_flight_limit;
                self.in_flight_limit = (self.in_flight_limit / 2).max(MIN_IN_FLIGHT);
                self.last_cut = Some(now);
                self.cuts += 1;
                if self.in_flight_limit != was {
                    // Worth saying out loud: this is the module deciding the
                    // link is congested and slowing itself down, which from
                    // outside looks like "the copy got slower for no reason".
                    tracing::debug!(
                        was,
                        now = self.in_flight_limit,
                        queued_kb = depth.queued_bytes / 1024,
                        delay_ms = delay.as_millis() as u64,
                        rate_kb_s = self.rate.unwrap_or(0.0) as u64 / 1024,
                        "link is backing up; easing off file traffic"
                    );
                }
            }
            // Comfortably clear of the target: room for one more. Deliberately
            // not "anything below target" — at the target itself, growing again
            // would just push it back over.
            Some(delay) if delay * 2 <= TARGET_QUEUE_DELAY => {
                if self.in_flight_limit < MAX_IN_FLIGHT {
                    self.grows += 1;
                }
                self.in_flight_limit = (self.in_flight_limit + 1).min(MAX_IN_FLIGHT);
            }
            // Either between the two, or nothing has drained yet so the queue's
            // age is unknown. Hold: acting on a guess is how a controller ends
            // up amplifying what it cannot see.
            _ => {}
        }
    }

    /// Fold in bytes the peer has confirmed it stored.
    ///
    /// The socket's own count is what this controller used to judge the link
    /// by, and it measures the wrong thing once a queue exists: bytes handed to
    /// TCP leave immediately into its send buffer whether or not the radio has
    /// carried them, so a link that is backing up reports a *rising* rate — the
    /// speed the buffer is filling, not the speed it is draining. Measured
    /// here, the estimate read 215 Mbit/s while the wire carried 104 and the
    /// disk took 53-81, so every window sized from it was twice too generous
    /// and the round trip settled at 581 ms with the pacer none the wiser.
    ///
    /// An acknowledgement cannot outrun the link: the peer sends it after the
    /// bytes have arrived and been written. That makes this the slower and
    /// truer of the two, and it is the one the window is sized from.
    fn sample_acked(&mut self, bytes: u64, now: Instant) {
        self.acked_since_sample += bytes;
        let Some(at) = self.acked_sample_at else {
            self.acked_sample_at = Some(now);
            return;
        };
        let elapsed = now.saturating_duration_since(at);
        if elapsed < RATE_SAMPLE_INTERVAL || self.acked_since_sample < RATE_SAMPLE_MIN_BYTES {
            return;
        }
        let sample = self.acked_since_sample as f64 / elapsed.as_secs_f64();
        self.acked_sample_at = Some(now);
        self.acked_since_sample = 0;
        self.acked_rate = Some(match self.acked_rate {
            Some(previous) => previous * 0.75 + sample * 0.25,
            None => sample,
        });
    }

    /// Bytes this pump may add to the link, from what it is measured to carry.
    ///
    /// A window bounds what is *outstanding*; it says nothing about how fast it
    /// is handed over. A pump that empties its whole allowance at once puts a
    /// burst on the wire, and everything behind that burst waits out all of it
    /// — the instantaneous rate is far above the link even when the average is
    /// below it, which is how a correctly sized window still produces a queue.
    ///
    /// Spreading the same bytes across the pumps of one round trip is what
    /// keeps the sending rate near the link's own. `None` before anything has
    /// been measured, when there is nothing to pace against.
    fn pump_budget(&self) -> Option<usize> {
        let rate = self.delivered_rate()?;
        // One pump's share of a second, with a chunk as the floor so a single
        // message is never split across pumps — it cannot be, and a budget
        // below one chunk would stall the transfer instead of pacing it.
        let per_pump = rate * crate::session::MODULE_PUMP_INTERVAL.as_secs_f64();
        Some((per_pump as usize).max(FilesMsg::MAX_CHUNK))
    }

    /// The rate to size a window from: what the peer has confirmed, falling
    /// back to what the socket took until an acknowledgement has been timed.
    fn delivered_rate(&self) -> Option<f64> {
        self.acked_rate.or(self.rate)
    }

    /// Update the drain-rate estimate, if enough time has passed to mean
    /// anything.
    fn sample_rate(&mut self, sent_bytes: u64, now: Instant) {
        let Some((at, previous)) = self.last_sample else {
            self.last_sample = Some((now, sent_bytes));
            return;
        };
        let elapsed = now.saturating_duration_since(at);
        if elapsed < RATE_SAMPLE_INTERVAL {
            return;
        }
        let moved = sent_bytes.saturating_sub(previous);
        if moved < RATE_SAMPLE_MIN_BYTES {
            // Too little went out for this interval to describe the link — an
            // idle one, or one carrying nothing but metadata, says nothing
            // about how fast it is. Keeping the last estimate is what lets a
            // transfer resume at speed rather than re-learning the link from
            // the floor, and what stops a trickle of `stat` calls from being
            // read as congestion. See [`RATE_SAMPLE_MIN_BYTES`].
            //
            // The sample point is deliberately NOT advanced: a slow link
            // sending one chunk over several intervals must still be measured
            // across the whole time it took, not have each partial interval
            // discarded.
            return;
        }
        self.last_sample = Some((now, sent_bytes));
        let sample = moved as f64 / elapsed.as_secs_f64();
        self.rate = Some(match self.rate {
            // Smoothed heavily: the samples are noisy (a 5 ms pump against a
            // 20 ms window catches whole frames or none), and the window reacts
            // to this figure.
            Some(previous) => previous * 0.75 + sample * 0.25,
            None => sample,
        });
    }

    /// How long the queued bytes will take to leave, or `None` when there is no
    /// rate to judge them by.
    fn queue_delay(&self, queued_bytes: usize) -> Option<Duration> {
        if queued_bytes == 0 {
            return Some(Duration::ZERO);
        }
        // Floored: an estimate below this is a mismeasurement rather than a slow
        // link, and dividing by it turns a queue that drains in milliseconds
        // into an apparent stall of tens of seconds. See
        // [`RATE_FLOOR_BYTES_PER_SEC`].
        self.rate.map(|rate| {
            Duration::from_secs_f64(queued_bytes as f64 / rate.max(RATE_FLOOR_BYTES_PER_SEC))
        })
    }

    /// How many further requests may be issued with `in_flight` outstanding.
    ///
    /// Bounded by what the link carries in [`TARGET_QUEUE_DELAY`] as well as by
    /// the count, because the two disagree by more than an order of magnitude
    /// once the requests are large. The count was chosen against a mount's
    /// small ones and a `MAX_IN_FLIGHT` of 128 is 32 MB of 256 KiB writes —
    /// four times what a 110 Mbit/s link drains in half a second. Starting
    /// there the window cannot help but overshoot: the queue hit 9 MB, the
    /// pacer halved four times in 200 ms, and it spent the transfer
    /// oscillating between 8 and 13 — 96 cuts and 391 growths in one
    /// half-minute window — instead of settling anywhere near the link.
    ///
    /// The rate is the same estimate the cuts are judged by, so the ceiling
    /// moves with the link rather than being guessed at once.
    fn allowance(&self, in_flight: usize) -> usize {
        let by_count = self.in_flight_limit.saturating_sub(in_flight);
        let Some(rate) = self.delivered_rate() else {
            // Nothing measured yet: the count is all there is to go on.
            return by_count;
        };
        let carried = rate.max(RATE_FLOOR_BYTES_PER_SEC) * TARGET_QUEUE_DELAY.as_secs_f64();
        // Sized in whole chunks, and never below the floor: a window that
        // shrank toward zero would turn a slow link into a filesystem error.
        let by_bytes = ((carried / FilesMsg::MAX_CHUNK as f64) as usize).max(MIN_IN_FLIGHT);
        by_count.min(by_bytes.saturating_sub(in_flight.min(by_bytes)))
    }
}

/// Threads for compressing our writes and unpacking our reads.
///
/// Two, because one zstd thread at [`ZSTD_LEVEL`] already outruns the link by
/// an order of magnitude; the second keeps a large chunk from holding up the
/// small ones queued behind it.
const CODEC_THREADS: usize = 2;

/// Compression level for file chunks.
///
/// Level 1 measured ~284 MB/s on compressible data here — an order of magnitude
/// above the link — while still reaching ~38% on binaries and ~30% on text. A
/// higher level buys a few percent for a large slice of that headroom, which is
/// the wrong trade when the network is the bottleneck.
const ZSTD_LEVEL: i32 = 1;

/// How often the local volume set is re-read, so a medium plugged in after the
/// session started is offered without waiting for a reconnect.
///
/// Polled rather than driven by udev or udisks: the volume set is a handful of
/// lines from `/proc/self/mounts` plus one sysfs read per block device, which
/// is cheaper than holding a D-Bus subscription that would have to be
/// duplicated for Windows anyway. A second is far below what a person notices
/// between plugging a stick in and looking for it, and udisks itself takes a
/// noticeable fraction of that to mount the thing.
const VOLUME_RESCAN: Duration = Duration::from_secs(1);

/// How long to wait before trying a failed mount again.
///
/// Long enough that a mount point which cannot be made is not hammered — each
/// attempt is filesystem work — and short enough that a disk missing because of
/// something the user is in the middle of clearing appears without them having
/// to restart the session.
#[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
const MOUNT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Where a peer's volumes are mounted on Linux.
///
/// Under the user's runtime directory rather than `/mnt`: no privileges needed,
/// and it disappears on logout instead of leaving a stale mount point behind.
#[cfg(target_os = "linux")]
fn mount_root(peer_name: &str) -> std::path::PathBuf {
    // Under the home directory, not `$XDG_RUNTIME_DIR`.
    //
    // The runtime directory is the tidier choice — it needs no privileges and
    // is cleared at logout — but file managers deliberately hide it, so the
    // peer's disks were mounted, readable, and invisible: not in Nautilus's
    // sidebar, not reachable by browsing, only by typing the path. A share
    // nobody can find is not shared. `~/Cursio/<peer>` shows up like any other
    // folder, and the stale-mount sweep at startup covers the cleanup the
    // runtime directory used to give for free.
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Cursio").join(peer_name)
}

/// Where one of a peer's volumes is mounted on Linux.
///
/// `~/Cursio/<peer>/<volume>`, so a peer's disks stay grouped under its name
/// while each is a mount of its own. Kept under the peer directory rather than
/// flattened to `~/Cursio/<peer>-<volume>` because the grouping is what makes
/// several peers legible, and gvfs lists each mount in the sidebar by its own
/// name regardless of where it sits.
#[cfg(target_os = "linux")]
fn volume_mount_point(peer_name: &str, volume: &Volume) -> std::path::PathBuf {
    mount_root(peer_name).join(cursio_files::volume_display_name(&volume.id, &volume.label))
}

/// Paths this machine must not serve to the peer.
///
/// Where the peer's own volumes are mounted here. Without this the two machines
/// serve each other their own disks: Linux offers `/`, the peer's drives hang
/// off it under `~/Cursio/<peer>/`, and the peer mounts them again as drives of
/// its own — a machine with two disks announcing six, four of which are the
/// asking machine's own files taking a round trip across the network to come
/// home under a different letter.
///
/// Windows needs none: its volumes are drive letters, and a peer's mount gets a
/// letter of its own rather than appearing inside one this machine offers.
#[cfg(target_os = "linux")]
fn excluded_paths(peer_name: &str) -> Vec<std::path::PathBuf> {
    // The whole `~/Cursio` tree, not just this peer's directory: every peer's
    // mounts live under it, and a third machine's disks are no more ours to
    // serve than this one's.
    let root = mount_root(peer_name);
    match root.parent() {
        Some(cursio) => vec![cursio.to_path_buf()],
        None => vec![root],
    }
}

#[cfg(not(target_os = "linux"))]
fn excluded_paths(_peer_name: &str) -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// A live mount of the peer's volumes, whichever OS is presenting them.
#[cfg(target_os = "linux")]
type Mount = cursio_files::mount::MountHandle;
#[cfg(all(target_os = "windows", target_env = "msvc"))]
type Mount = cursio_files::winmount::MountHandle;

/// Uninhabited where no mount implementation exists, so `Option<Mount>` is
/// always `None` and every path that reads it stays compiled and honest.
#[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
enum Mount {}

/// Mount one of the peer's volumes with whatever this platform provides.
///
/// The two implementations differ in everything the OS cares about — a
/// directory versus a drive letter, FUSE versus WinFsp — and in nothing this
/// module does: both take the same [`PeerLink`] and answer the same protocol.
///
/// One volume at a time on both, and driven by the peer's `Volumes`
/// announcement rather than by startup — see [`FilesEngine::sync_peer_mounts`].
/// A drive letter forced the shape on Windows (the letter *is* the volume, and
/// none can be added to a mount already made); Linux followed because the
/// alternative it allowed — every volume a folder under one mount — could
/// report only one capacity for all of them and put a single entry in a file
/// manager's sidebar. A peer's disk should look here the way it looks on the
/// machine that owns it.
#[cfg(target_os = "linux")]
fn mount_one_volume(peer: PeerLink, peer_name: &str, volume: &Volume) -> anyhow::Result<Mount> {
    Ok(cursio_files::mount::mount(
        peer,
        volume_mount_point(peer_name, volume),
        peer_name,
        &volume.id,
    )?)
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
fn mount_one_volume(peer: PeerLink, peer_name: &str, volume: &Volume) -> anyhow::Result<Mount> {
    cursio_files::winmount::mount(peer, peer_name, &volume.id, &volume.label)
}

/// One request we sent that has not been answered yet.
enum Pending {
    Volumes(std::sync::mpsc::Sender<Result<Vec<Volume>, FileError>>),
    Dir {
        reply: std::sync::mpsc::Sender<Result<Vec<cursio_protocol::FileMeta>, FileError>>,
        /// Entries gathered so far: a large listing arrives in several messages.
        acc: Vec<cursio_protocol::FileMeta>,
    },
    Stat(std::sync::mpsc::Sender<Result<cursio_protocol::FileMeta, FileError>>),
    Link(std::sync::mpsc::Sender<Result<String, FileError>>),
    Open(std::sync::mpsc::Sender<Result<(cursio_protocol::FileHandle, u64), FileError>>),
    Read {
        reply: std::sync::mpsc::Sender<Result<Vec<u8>, FileError>>,
        acc: Vec<u8>,
    },
    Wrote(std::sync::mpsc::Sender<Result<u64, FileError>>),
    Unit(std::sync::mpsc::Sender<Result<(), FileError>>),
}

impl Pending {
    /// Hand this request's failure to whoever is waiting on it.
    fn fail(self, e: FileError) {
        match self {
            Pending::Volumes(tx) => drop(tx.send(Err(e))),
            Pending::Dir { reply, .. } => drop(reply.send(Err(e))),
            Pending::Stat(tx) => drop(tx.send(Err(e))),
            Pending::Link(tx) => drop(tx.send(Err(e))),
            Pending::Open(tx) => drop(tx.send(Err(e))),
            Pending::Read { reply, .. } => drop(reply.send(Err(e))),
            Pending::Wrote(tx) => drop(tx.send(Err(e))),
            Pending::Unit(tx) => drop(tx.send(Err(e))),
        }
    }
}

/// How long a request of ours may go unanswered before it is given up on here.
///
/// The mount's own caller has stopped waiting by then — no operation waits
/// longer than `BULK_OP_TIMEOUT` — so the entry serves nobody. It still counts
/// against the window, though, and that is what made an unanswered request
/// expensive: the window is sized from the number of entries, so a handful the
/// peer never answered (a job that died on its side, a reply lost to a
/// reconnect of the transport under us) closed it for good, and every later
/// operation timed out without ever being sent. Seen as a mount that stopped
/// answering until the link was torn down and rebuilt.
const PENDING_EXPIRY: Duration = cursio_files::bridge::BULK_OP_TIMEOUT;

/// Requests we have sent and not yet had answered, with when each went out.
#[derive(Default)]
struct PendingMap(HashMap<FileReqId, (Pending, Instant)>);

impl PendingMap {
    fn insert(&mut self, req: FileReqId, pending: Pending) {
        self.0.insert(req, (pending, Instant::now()));
    }

    fn remove(&mut self, req: &FileReqId) -> Option<Pending> {
        self.0.remove(req).map(|(pending, _)| pending)
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every request still waiting for the peer's volume list.
    fn waiting_for_volumes(&self) -> Vec<FileReqId> {
        self.0
            .iter()
            .filter(|(_, (p, _))| matches!(p, Pending::Volumes(_)))
            .map(|(&req, _)| req)
            .collect()
    }

    fn drain(&mut self) -> impl Iterator<Item = Pending> + '_ {
        self.0.drain().map(|(_, (pending, _))| pending)
    }

    /// Fail every request older than `limit`, returning how many there were.
    ///
    /// Failed rather than dropped, so a caller still waiting — there should be
    /// none, but a clock can jump — hears why instead of waiting out its own
    /// deadline.
    fn expire(&mut self, now: Instant, limit: Duration) -> usize {
        let stale: Vec<FileReqId> = self
            .0
            .iter()
            .filter(|(_, (_, since))| now.saturating_duration_since(*since) >= limit)
            .map(|(&req, _)| req)
            .collect();
        for req in &stale {
            if let Some(pending) = self.remove(req) {
                pending.fail(FileError::Io("the peer never answered".into()));
            }
        }
        stale.len()
    }
}

/// Serves our volumes to the peer and mounts theirs here.
pub struct FilesEngine {
    peer_name: String,
    /// Our disks, as the peer sees them. Behind a mutex because the blocking
    /// tasks that touch the disk need it across await points.
    server: std::sync::Arc<std::sync::Mutex<FileServer>>,
    /// Requests from the local FUSE mount, waiting to go out.
    ops_rx: tokio::sync::mpsc::UnboundedReceiver<Op>,
    /// Replies from disk work finished on a worker thread.
    done_tx: tokio::sync::mpsc::UnboundedSender<FilesMsg>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<FilesMsg>,
    /// Our own requests, once the workers have prepared them.
    ///
    /// A write has to be compressed before it can be sent, and compression of a
    /// 256 KiB chunk is milliseconds of CPU — done inline it was the session
    /// loop's own stall, one per chunk, for the whole length of a copy.
    outbox_tx: tokio::sync::mpsc::UnboundedSender<FilesMsg>,
    outbox_rx: tokio::sync::mpsc::UnboundedReceiver<FilesMsg>,
    /// Writes handed to a worker to compress and not yet queued on `outbox`,
    /// counted per handle.
    ///
    /// A close must not overtake them. Compression runs on the pool, so between
    /// accepting a `Write` and its message reaching `outbox` there is a window
    /// in which the handle has bytes outstanding that nothing on the link knows
    /// about yet. Closing inside that window let the peer drop the handle first
    /// and answer the writes behind it with `BadHandle`: the bytes were lost,
    /// and the application learned of it only at the end of a file it had
    /// finished writing. The count is what a deferred close waits on.
    ///
    /// Shared with the workers, which decrement their own entry immediately
    /// after queueing the compressed write — so the count falls in the same
    /// order the messages enter `outbox`.
    compressing: std::sync::Arc<std::sync::Mutex<HashMap<FileHandle, usize>>>,
    /// Closes held back until this handle's writes have been queued.
    ///
    /// Retried each pump by [`FilesEngine::flush_deferred_closes`].
    deferred_closes: Vec<FileHandle>,
    /// Threads that do the disk work the peer asks of us, kept below the
    /// priority of everything the user is actually looking at.
    workers: Workers,
    /// Threads that compress our own writes and unpack our own reads.
    ///
    /// Apart from `workers` because the two answer to different people. That
    /// pool serves the peer, and a peer walking a directory tree keeps it busy
    /// with hundreds of `readdir` and `stat` a second — each of which, on a
    /// Windows disk, can take milliseconds apiece. Sharing one queue put every
    /// chunk of a local download behind that walk: seen live as Steam writing
    /// to the peer's disk at 0 B/s while the peer's crawl was being served.
    /// Compression is pure CPU and needs no disk, so a small pool of its own
    /// keeps our transfer moving whatever the peer is asking.
    codec: Workers,
    /// Sizes the in-flight window from what the link is measured to be doing.
    pace: Pacer,
    /// Requests we have sent and not yet had answered.
    pending: PendingMap,
    next_req: FileReqId,
    /// Requests the peer asked of us, counted by kind.
    ///
    /// The other counters watch what this machine asks for. When the peer is
    /// the one doing the work — an installer downloading onto our disk — every
    /// request is inbound, and without this the log for a failed transfer holds
    /// nothing but the refusals.
    served: HashMap<&'static str, u64>,
    /// How many of those took long enough that their caller may have given up.
    ///
    /// Incremented from the worker that ran the request, hence the atomic.
    slow_served: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Requests from the peer currently on a worker thread.
    ///
    /// The gap this closes: between accepting a request and its answer reaching
    /// `done_rx`, the module holds nothing at all — `pending` is what *we*
    /// asked for, and a peer's request is neither sent by us nor yet answered.
    /// So a mount serving a download looked idle for exactly as long as it was
    /// busy, and the session loop slept 50 ms in the middle of every one.
    in_progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// What the link is actually doing, reported periodically.
    ///
    /// Kept because none of it can be inferred from outside: a transfer running
    /// at a fraction of the link looks exactly like a slow network, and the
    /// difference is only visible by watching the round trip and the pacer's
    /// own decisions side by side. Diagnosing that by hand meant measuring the
    /// link with `ssh` and guessing — which produced the wrong answer twice
    /// before the log line above gave it away.
    health: Health,
    /// One live mount per peer volume, keyed by volume id; unmounted when
    /// dropped.
    ///
    /// Per volume rather than per peer because a volume cannot be added to a
    /// mount already made — on Windows the drive letter *is* the volume, and on
    /// Linux a single mount could report only one capacity for all of them. So
    /// a peer plugging in a stick means a new mount, and unplugging it means
    /// dropping that one and no other.
    #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
    volume_mounts: HashMap<String, Mount>,
    /// Kept so a volume that appears mid-session can be mounted then, rather
    /// than only at startup.
    mount_link: PeerLink,
    /// When a volume that failed to mount may be tried again.
    ///
    /// A mount can fail for a reason that clears on its own: the commonest is a
    /// previous run's mount still occupying the path, which the user (or the
    /// next sweep) removes a moment later. Without a retry the volume stayed
    /// missing for the whole session, because the only thing that drove
    /// mounting was the peer's announcement — and a peer whose volume set has
    /// not changed never sends another. Seen live: every one of a peer's six
    /// drives failed against a stale mount point and none came back.
    #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
    mount_retry_at: Option<Instant>,
    /// Deferred until the first pump, because `new` is not async.
    announce_volumes: bool,
    /// The peer's volumes as last announced, answered from here instead of over
    /// the link. See where it is filled for why this matters.
    peer_volumes: Vec<Volume>,
    /// When the local volume set was last re-read, so a stick plugged in after
    /// the session started is noticed. See [`VOLUME_RESCAN`].
    last_rescan: Instant,
    /// Set while a rescan is running on a worker, so the once-a-second timer
    /// cannot stack a second one behind a scan that is still stuck.
    rescanning: bool,
    /// Finished scans, collected by a later pump.
    rescan_tx: tokio::sync::mpsc::UnboundedSender<FileServer>,
    rescan_rx: tokio::sync::mpsc::UnboundedReceiver<FileServer>,
}

impl FilesEngine {
    pub fn new(peer_name: String) -> anyhow::Result<Self> {
        let (ops_tx, ops_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outbox_tx, outbox_rx) = tokio::sync::mpsc::unbounded_channel();
        let (rescan_tx, rescan_rx) = tokio::sync::mpsc::unbounded_channel();

        // Nothing is mounted here: a mount is per volume, and the peer has not
        // said what it has yet. `sync_peer_mounts` makes them when its `Volumes`
        // announcement arrives, and follows the set from then on.
        let link = PeerLink::new(ops_tx);

        // Clear what a previous run left mounted here before mounting anything.
        //
        // Each volume's directory is named after the disk, so a peer relabelling
        // a drive — or a Cursio that named them differently — leaves a mount
        // whose name nothing now looks up. Those stay in a file manager's
        // sidebar as entries that answer nothing, and multiply with every
        // restart.
        #[cfg(target_os = "linux")]
        cursio_files::mount::release_stale_peer_mounts(&mount_root(&peer_name));

        let mut server = FileServer::detect();
        server.exclude(excluded_paths(&peer_name));

        Ok(Self {
            peer_name,
            server: std::sync::Arc::new(std::sync::Mutex::new(server)),
            ops_rx,
            done_tx,
            done_rx,
            outbox_tx,
            outbox_rx,
            compressing: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            deferred_closes: Vec::new(),
            workers: Workers::new("cursio-files"),
            codec: Workers::with_threads("cursio-files-codec", CODEC_THREADS),
            pace: Pacer::new(),
            pending: PendingMap::default(),
            next_req: 1,
            health: Health::default(),
            served: HashMap::new(),
            slow_served: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            in_progress: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
            volume_mounts: HashMap::new(),
            mount_link: link,
            #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
            mount_retry_at: None,
            announce_volumes: true,
            peer_volumes: Vec::new(),
            last_rescan: Instant::now(),
            rescanning: false,
            rescan_tx,
            rescan_rx,
        })
    }

    /// Re-read the local volumes, and schedule an announcement if the *set*
    /// changed.
    ///
    /// Compared as a whole rather than watched for arrivals: a medium going away
    /// matters as much as one appearing, and the peer is sent the entire set in
    /// either case, so there is nothing a delta would buy.
    ///
    /// Compared on identity only, deliberately excluding `free_bytes` and
    /// `total_bytes`. Free space on a live disk changes constantly — a browser
    /// cache write is enough — so comparing whole `Volume`s made every rescan
    /// report a change, once a second, forever: an announcement to the peer and
    /// a "local volumes changed was=5 now=5" line each time. The set was
    /// identical every time; only the statistics had moved. Capacity still
    /// reaches the peer, it just no longer *triggers* the announcement.
    fn rescan_volumes(&mut self) {
        // Take delivery of a scan that finished since the last pump.
        while let Ok(after) = self.rescan_rx.try_recv() {
            self.rescanning = false;
            let mut server = self.server.lock().unwrap_or_else(PoisonError::into_inner);
            let before = server.volumes();
            if volume_identity(&after.volumes()) == volume_identity(&before) {
                continue;
            }
            tracing::info!(
                peer = %self.peer_name,
                was = before.len(),
                now = after.volumes().len(),
                "local volumes changed"
            );
            // Only the volume set, never the open handles: see
            // [`FileServer::adopt_volumes`] for what replacing the whole server
            // did to a peer that had files open when a disk was unplugged.
            server.adopt_volumes(after);
            drop(server);
            self.announce_volumes = true;
        }

        if self.rescanning || self.last_rescan.elapsed() < VOLUME_RESCAN {
            return;
        }
        self.last_rescan = Instant::now();
        self.rescanning = true;

        // Scanned on a worker, never inline.
        //
        // Reading the volume set is not the cheap directory read it looks like.
        // Windows probes every drive letter, and a letter whose medium is gone
        // — an empty card reader, a disconnected USB disk — does not fail fast:
        // the request goes to the device and waits out its timeout. Measured on
        // a real machine the moment a disk dropped out: 3048 ms per scan, once
        // a second, which stopped input, audio and the file mount alike and
        // left the peer timing out file operations in a loop. Linux is cheap by
        // comparison but not free, and the cost is the same kind: a stale NFS
        // or autofs mount blocks in `statfs` exactly the same way.
        let tx = self.rescan_tx.clone();
        self.workers.spawn(move || {
            // The receiver only closes when the session is ending, in which case
            // the scan is moot.
            let _ = tx.send(FileServer::detect());
        });
    }

    /// Bring the set of mounted drives in line with what the peer now offers.
    ///
    /// Driven by the announcement rather than by startup: the peer's volume set
    /// is not known when the session begins, and it changes while the session
    /// runs — a stick goes in, a disk is unplugged. Each volume owns a mount of
    /// its own, so following the set means mounting what arrived and dropping
    /// what left; anything already mounted is untouched, so a peer
    /// re-announcing an unchanged set costs nothing.
    #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
    fn sync_peer_mounts(&mut self) {
        // Gone: drop the handle, which unmounts and frees the letter or the
        // mount point.
        let live: std::collections::HashSet<&str> =
            self.peer_volumes.iter().map(|v| v.id.as_str()).collect();
        self.volume_mounts.retain(|id, _| {
            let keep = live.contains(id.as_str());
            if !keep {
                tracing::info!(peer = %self.peer_name, volume = %id, "peer volume went away; unmounting");
            }
            keep
        });

        // Arrived: one mount each. Anything still missing is retried, since a
        // failure here is often temporary — see [`FilesEngine::mount_retry_at`].
        if self.mount_retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        let mut failed = false;
        for volume in &self.peer_volumes {
            if self.volume_mounts.contains_key(&volume.id) {
                continue;
            }
            match mount_one_volume(self.mount_link.clone(), &self.peer_name, volume) {
                Ok(m) => {
                    self.volume_mounts.insert(volume.id.clone(), m);
                }
                // One volume failing must not cost the others: a machine out of
                // drive letters, or one whose mount point cannot be made,
                // should still show the disks it had room for.
                Err(e) => {
                    failed = true;
                    tracing::warn!(
                        peer = %self.peer_name,
                        volume = %volume.id,
                        error = %e,
                        "cannot mount this peer volume; will retry"
                    );
                }
            }
        }
        // Spaced out rather than attempted every pump: a mount that cannot be
        // made usually cannot be made a millisecond later either, and each
        // attempt touches the filesystem.
        self.mount_retry_at = failed.then(|| Instant::now() + MOUNT_RETRY_INTERVAL);
    }

    /// No-op where this platform has no mount backend at all.
    ///
    /// Serving is portable; mounting is not. macOS has no mount backend here
    /// (FUSE needs a kernel extension the user must install and approve), and a
    /// MinGW build has none either — `winfsp-sys` refuses that target. Only
    /// this half is missing, so a Mac still serves its volumes to a peer that
    /// can mount them; `start` says plainly why the reverse direction is.
    #[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
    fn sync_peer_mounts(&mut self) {}

    /// Queue a close, or hold it back while this handle still has writes being
    /// compressed.
    ///
    /// See [`FilesEngine::compressing`] for what closing too early cost.
    fn close_handle(&mut self, handle: FileHandle) {
        if self
            .compressing
            .lock()
            .expect("files compressing")
            .contains_key(&handle)
        {
            self.deferred_closes.push(handle);
            return;
        }
        let _ = self.outbox_tx.send(FilesMsg::Close { handle });
    }

    /// Send the closes whose writes have since reached the queue.
    ///
    /// Ordering within `outbox` does the rest: the writes are already in it, so
    /// a close appended now is behind them.
    fn flush_deferred_closes(&mut self) {
        if self.deferred_closes.is_empty() {
            return;
        }
        let guard = self.compressing.lock().expect("files compressing");
        let (ready, waiting): (Vec<_>, Vec<_>) = self
            .deferred_closes
            .drain(..)
            .partition(|h| !guard.contains_key(h));
        drop(guard);
        self.deferred_closes = waiting;
        for handle in ready {
            let _ = self.outbox_tx.send(FilesMsg::Close { handle });
        }
    }

    /// Report what the link is doing, once every [`HEALTH_LOG_INTERVAL`].
    ///
    /// Silent while nothing is happening: a mount that is simply not in use
    /// would otherwise print a line of zeroes twice a minute for the life of
    /// the session, which is how a log stops being read.
    fn log_health(&mut self, now: Instant) {
        let due = self
            .health
            .last_log
            .is_none_or(|t| now.duration_since(t) >= HEALTH_LOG_INTERVAL);
        if !due {
            return;
        }
        // Before the idle check, deliberately. A mount whose calls have stopped
        // returning reports *no* traffic — that is exactly what being stuck
        // looks like from here — so anything said only on a busy link would go
        // unsaid in the one situation it exists for.
        #[cfg(all(target_os = "windows", target_env = "msvc"))]
        cursio_files::inflight::report_stuck();

        // Serving the peer counts as activity too. Judging by our own requests
        // alone made the module silent through exactly the case worth watching:
        // a download onto our disk, where every request is inbound.
        let idle = self.health.answered == 0 && self.health.failed == 0 && self.served.is_empty();
        if idle {
            // Nothing to report, but the clock still rolls — otherwise the next
            // burst of traffic would report against a window of hours.
            self.health.roll(now);
            return;
        }

        let secs = self
            .health
            .last_log
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or_else(|| HEALTH_LOG_INTERVAL.as_secs_f64())
            .max(0.001);

        tracing::info!(
            peer = %self.peer_name,
            // What the link achieved, both ways.
            read_kb_s = (self.health.read_bytes as f64 / secs / 1024.0) as u64,
            write_kb_s = (self.health.written_bytes as f64 / secs / 1024.0) as u64,
            // Where a write's round trip went. `queued_ms` is ours — accepted
            // to on the wire; `wire_ms` is everything past that, including the
            // peer's disk. Reported because the total alone never said which.
            queued_ms = self.health.mean_stage_ms(self.health.stage_queued),
            wire_ms = self.health.mean_stage_ms(self.health.stage_wire),
            // The same two directions for work the peer asked of us, which is
            // the whole of the traffic whenever it is the peer that is copying.
            served_read_kb_s = (self.health.served_read_bytes as f64 / secs / 1024.0) as u64,
            served_write_kb_s = (self.health.served_written_bytes as f64 / secs / 1024.0) as u64,
            // What one operation costs. Small operations live and die by this.
            rtt_ms = self.health.mean_rtt_ms(),
            rtt_max_ms = self.health.rtt_max.as_millis() as u64,
            // What the pacer believes and what it did about it. A window at the
            // floor beside a healthy rate is the controller fighting the link.
            window = self.pace.in_flight_limit,
            link_kb_s = self.pace.rate.unwrap_or(0.0) as u64 / 1024,
            // What the peer has confirmed, beside what the socket took. The two
            // diverging is the signature of a queue: measured live at 215
            // against 104 Mbit/s, which is what made every window twice too
            // wide.
            acked_kb_s = self.pace.acked_rate.unwrap_or(0.0) as u64 / 1024,
            cuts = self.pace.cuts,
            grows = self.pace.grows,
            ops = self.health.answered,
            failed = self.health.failed,
            in_flight = self.pending.len(),
            // What was asked, not just how much: the mix is what says which
            // round trip to attack next.
            by_kind = %{
                let mut kinds: Vec<_> = self.health.by_kind.iter().collect();
                kinds.sort_by(|a, b| b.1.cmp(a.1));
                kinds
                    .iter()
                    .map(|(k, n)| format!("{k}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            // What the peer asked of us. The counters above are what we asked
            // of it, and a transfer onto our disk is entirely this column.
            served = %{
                let mut kinds: Vec<_> = self.served.iter().collect();
                kinds.sort_by(|a, b| b.1.cmp(a.1));
                kinds
                    .iter()
                    .map(|(k, n)| format!("{k}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            served_slow = self
                .slow_served
                .swap(0, std::sync::atomic::Ordering::Relaxed),
            "file sharing health"
        );
        self.served.clear();

        self.pace.cuts = 0;
        self.pace.grows = 0;
        self.health.roll(now);
    }

    fn next_req(&mut self) -> FileReqId {
        let id = self.next_req;
        self.next_req += 1;
        id
    }
}

/// What makes a volume *this* volume, for deciding whether the offered set has
/// changed.
///
/// Everything a peer addresses or displays, and nothing that moves on its own:
/// a disk filling up is not a new disk. See [`FilesEngine::rescan_volumes`] for
/// what including the byte counts here cost.
fn volume_identity(volumes: &[Volume]) -> Vec<(&str, &str, bool, bool)> {
    volumes
        .iter()
        .map(|v| (v.id.as_str(), v.label.as_str(), v.writable, v.removable))
        .collect()
}

/// Compress a chunk, keeping the result only when it genuinely shrank.
fn pack(bytes: Vec<u8>) -> Chunk {
    let plain_len = bytes.len();
    match zstd::encode_all(bytes.as_slice(), ZSTD_LEVEL) {
        Ok(data) if Chunk::worth_compressing(plain_len, data.len()) => Chunk::Zstd {
            data,
            plain_len: plain_len as u32,
        },
        // Either it did not compress usefully or zstd failed; the raw bytes are
        // always correct, so a compression failure is not an error.
        _ => Chunk::Raw(bytes),
    }
}

/// Undo [`pack`].
fn unpack(chunk: Chunk) -> Result<Vec<u8>, FileError> {
    match chunk {
        Chunk::Raw(b) => Ok(b),
        Chunk::Zstd { data, plain_len } => {
            let out = zstd::decode_all(data.as_slice())
                .map_err(|e| FileError::Io(format!("decompress: {e}")))?;
            if out.len() != plain_len as usize {
                // A payload that decompresses to the wrong length would be
                // written into a file as if it were correct.
                return Err(FileError::Io(format!(
                    "chunk decompressed to {} bytes, expected {plain_len}",
                    out.len()
                )));
            }
            Ok(out)
        }
    }
}

async fn send(tx: &mut dyn TransportTx, msg: FilesMsg) -> Result<(), TransportError> {
    // Per message, not per module: a `stat` someone is blocked on must not
    // queue behind a copy's chunks. See [`FilesMsg::priority`].
    let framed = Message::module(ModuleId::FILES, msg.priority(), &msg)?;
    tx.send(&framed).await
}

/// Move messages the workers have prepared onto the link, until the queue runs
/// dry or this pump's slice is spent.
///
/// Nothing here is subject to the pacer's window: these are answers the peer is
/// blocked on, and requests already counted against the window when they were
/// accepted. Holding them back would not reduce what is in flight, only make
/// the peer wait out its timeout for something we had ready.
async fn drain(
    queue: &mut tokio::sync::mpsc::UnboundedReceiver<FilesMsg>,
    tx: &mut dyn TransportTx,
    deadline: Instant,
    // Written to as each message leaves, so a write's time here can be told
    // apart from its time on the link. `None` for the reply queue, whose
    // messages answer the peer rather than waiting on it.
    mut timing: Option<&mut Health>,
    // Bytes this pump may hand to the link, or `None` for a queue that is not
    // paced. See [`Pacer::pump_budget`] for why a window alone is not enough.
    budget: Option<usize>,
) -> Result<(), TransportError> {
    let mut spent = 0usize;
    for _ in 0..MAX_MESSAGES_PER_PUMP {
        if Instant::now() >= deadline {
            break;
        }
        if budget.is_some_and(|b| spent >= b) {
            break;
        }
        let Ok(msg) = queue.try_recv() else { break };
        spent += msg.wire_cost();
        if let (Some(health), FilesMsg::Write { req, .. }) = (timing.as_deref_mut(), &msg) {
            health.reached_wire(*req, Instant::now());
        }
        send(tx, msg).await?;
    }
    Ok(())
}

impl FilesEngine {
    /// Run one request from the peer against our disks, off the session thread.
    ///
    /// Every arm here touches a real filesystem, which can block for as long as
    /// the disk feels like — so the whole thing goes to the worker pool and its
    /// reply is picked up by a later pump. Doing this inline is what would show
    /// up in the log as the audio module's "pump blocked the session loop", only
    /// for reads that can be far slower.
    ///
    /// The pool rather than `spawn_blocking`: this is the peer's work, done on
    /// our machine while its owner is using it for something else, so it runs
    /// behind everything local — see [`cursio_files::background`].
    fn serve(&mut self, msg: FilesMsg) {
        let server = std::sync::Arc::clone(&self.server);
        let done = self.done_tx.clone();
        // Counted here, where the peer's requests arrive. The other counters
        // watch what *this* machine asks for, which is the wrong half when the
        // peer is the one writing: Steam downloading onto our disk produced a
        // "Device Error" that left nothing at all in our log, because every
        // request involved was inbound and nothing was watching this side.
        let kind = request_kind(&msg);
        *self.served.entry(kind).or_insert(0) += 1;
        // Sized from the request, before it goes to a worker: a write carries
        // its bytes with it, and a read states how many it wants. A read that
        // returns short overstates by the difference, which is the cheaper
        // error than reporting nothing at all — the alternative is threading a
        // count back from the worker for a number only a log line reads.
        match &msg {
            FilesMsg::Write { bytes, .. } => {
                self.health.served_written_bytes += bytes.wire_len() as u64;
            }
            FilesMsg::Read { len, .. } => {
                self.health.served_read_bytes += *len as u64;
            }
            _ => {}
        }
        let slow = self.slow_served.clone();
        // Counted before the worker starts and cleared when it finishes, so the
        // module reads as busy for exactly as long as it is. The guard clears
        // it on unwind too: `run_request` takes a lock it expects to hold, and
        // one panicking job that left the count raised would keep the session
        // loop at its fast pace for the rest of the link's life.
        let busy = InFlight::raise(&self.in_progress);
        self.workers.spawn(move || {
            let started = Instant::now();
            let req = request_req(&msg);
            // A request that panicked is still owed an answer. Without one the
            // peer's entry for it never clears, and those entries are what its
            // window is counted from: enough of them and it sends nothing more
            // until the link is torn down.
            let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_request(&server, msg)
            })) {
                Ok(reply) => reply,
                Err(panic) => {
                    let why = cursio_files::background::panic_message(&*panic).to_string();
                    tracing::warn!(kind, why, "a request from the peer panicked");
                    req.map(|req| FilesMsg::Failed {
                        req,
                        error: FileError::Io(format!("the request failed on the peer: {why}")),
                    })
                }
            };
            drop(busy);
            let took = started.elapsed();
            // A request the peer is blocked on, taking long enough that its
            // caller may already have given up: the mount's own deadline is
            // `OP_TIMEOUT`, and an application sees a device error rather than
            // a slow disk. Worth naming individually, not just counting.
            if took >= SLOW_SERVE_WARN {
                slow.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    kind,
                    took_ms = took.as_millis() as u64,
                    "a request from the peer took long enough that its caller may have given up"
                );
            }
            if let Some(reply) = reply {
                // The receiver only closes when the session is ending, in which
                // case the answer is moot.
                let _ = done.send(reply);
            }
        });
    }
}

/// A request counted as in flight for as long as this lives.
///
/// The count decides whether the module reports pending work, so it has to fall
/// on every exit a worker has, unwinding included — a job that panicked while
/// holding the count raised would leave the session loop believing there was
/// work outstanding forever.
struct InFlight(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl InFlight {
    fn raise(count: &std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        InFlight(std::sync::Arc::clone(count))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Execute one peer request. Returns the reply to send, if any.
fn run_request(server: &std::sync::Mutex<FileServer>, msg: FilesMsg) -> Option<FilesMsg> {
    /// Turn a `Result` into either its success message or a `Failed`.
    ///
    /// `what` names the operation and its target, so a refusal says which path
    /// was refused rather than only that something was. A bare error kind is
    /// nearly useless here: `NotFound` repeating twice a second says an
    /// application is hunting for something, and only the path says what.
    fn answer<T>(
        req: FileReqId,
        what: &str,
        r: Result<T, FileError>,
        ok: impl FnOnce(T) -> FilesMsg,
    ) -> Option<FilesMsg> {
        Some(match r {
            Ok(v) => ok(v),
            // Said out loud before it goes on the wire. A refusal is invisible
            // on this side otherwise — the peer reports "the drive failed" and
            // the machine that decided so has nothing in its log to say why.
            Err(error) => {
                tracing::debug!(?error, what, "refusing a file request from the peer");
                FilesMsg::Failed { req, error }
            }
        })
    }

    match msg {
        FilesMsg::ReadDir { req, volume, path } => {
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .read_dir(&volume, &path);
            answer(req, &format!("readdir {volume}/{path}"), r, |entries| {
                FilesMsg::DirEntries {
                    req,
                    entries,
                    // Sent whole: a listing is metadata, and splitting it would only
                    // matter for a directory with tens of thousands of entries.
                    last: true,
                }
            })
        }
        FilesMsg::Stat { req, volume, path } => {
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .stat(&volume, &path);
            answer(req, &format!("stat {volume}/{path}"), r, |meta| {
                FilesMsg::StatResult { req, meta }
            })
        }
        FilesMsg::ReadLink { req, volume, path } => {
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .read_link(&volume, &path);
            answer(req, &format!("readlink {volume}/{path}"), r, |target| {
                FilesMsg::LinkTarget { req, target }
            })
        }
        FilesMsg::Open {
            req,
            volume,
            path,
            mode,
        } => {
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .open(&volume, &path, mode);
            answer(
                req,
                &format!("open {volume}/{path}"),
                r,
                |(handle, size)| FilesMsg::Opened { req, handle, size },
            )
        }
        FilesMsg::Read {
            req,
            handle,
            offset,
            len,
        } => {
            // Clamped to the wire chunk size: a caller asking for more than one
            // frame can hold gets what fits, and asks again.
            let len = len.min(FilesMsg::MAX_CHUNK as u32);
            // The handle is taken under the lock and the disk is touched after
            // it is released. A temporary guard lives to the end of its
            // expression, so `server.lock().read(..)` held the whole server for
            // the length of every read — every other request, for every other
            // file, queued behind it. Measured against a peer downloading a
            // game: 11 writes a second with 0.2 operations in flight, against a
            // window of 128.
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .detach(handle)
                .and_then(|file| file.read(offset, len));
            answer(
                req,
                &format!("read handle {handle} at {offset}"),
                r,
                |bytes| FilesMsg::Data {
                    req,
                    offset,
                    bytes: pack(bytes),
                    last: true,
                },
            )
        }
        FilesMsg::Write {
            req,
            handle,
            offset,
            bytes,
            ..
        } => {
            let r = unpack(bytes).and_then(|bytes| {
                server
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .detach_writable(handle)
                    .and_then(|file| file.write(offset, &bytes))
            });
            answer(
                req,
                &format!("write handle {handle} at {offset}"),
                r,
                |written| FilesMsg::Wrote { req, written },
            )
        }
        FilesMsg::Truncate {
            req,
            handle,
            size,
            reserve,
        } => {
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .detach_writable(handle)
                .and_then(|file| file.set_size(size, reserve));
            answer(
                req,
                &format!("set_size handle {handle} to {size} reserve={reserve}"),
                r,
                |()| FilesMsg::Ok { req },
            )
        }
        FilesMsg::MkDir { req, volume, path } => {
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .mkdir(&volume, &path);
            answer(req, &format!("mkdir {volume}/{path}"), r, |()| {
                FilesMsg::Ok { req }
            })
        }
        FilesMsg::Remove { req, volume, path } => {
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&volume, &path);
            answer(req, &format!("remove {volume}/{path}"), r, |()| {
                FilesMsg::Ok { req }
            })
        }
        FilesMsg::Rename {
            req,
            volume,
            from,
            to,
        } => {
            let r = server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .rename(&volume, &from, &to);
            answer(req, &format!("rename {volume}/{from} -> {to}"), r, |()| {
                FilesMsg::Ok { req }
            })
        }
        FilesMsg::ListVolumes { .. } => {
            // Answered with the announcement itself, which carries no request
            // id — the list is idempotent, so the asker takes the next one.
            Some(FilesMsg::Volumes(
                server
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .volumes(),
            ))
        }
        FilesMsg::Close { handle } => {
            server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .close(handle);
            None // deliberately unanswered; see the protocol docs
        }
        // Replies and announcements are not requests: they are handled inline
        // by `handle_message`, which never routes them here.
        _ => None,
    }
}

impl FilesEngine {
    /// Turn queued mount operations into requests on the wire.
    ///
    /// Bounded three ways, each for its own reason: by the pacer's window, so
    /// the link is kept full but not backed up; by `deadline`, so the session
    /// loop gets its turn back; and by a message cap that only matters if the
    /// clock lies. Whatever is left waits for the next pump — a request is
    /// worth nothing early and everything eventually, unlike an audio frame.
    async fn pump_requests(
        &mut self,
        tx: &mut dyn TransportTx,
        deadline: Instant,
    ) -> Result<(), TransportError> {
        // The window counts requests *outstanding*, which is what `pending`
        // holds: everything sent and not yet answered, plus the writes the
        // workers are still compressing.
        let mut allowance = self.pace.allowance(self.pending.len());
        let mut seen = 0;
        while allowance > 0 && seen < MAX_MESSAGES_PER_PUMP && Instant::now() < deadline {
            seen += 1;
            let Ok(op) = self.ops_rx.try_recv() else {
                break;
            };
            let req = self.next_req();
            let (pending, msg) = match op {
                Op::Volumes { reply } => {
                    // Answer from what the peer already told us. The list is
                    // pushed on connect and on any change, so asking again
                    // would only add a round trip — and, before the mount has
                    // heard the first announcement, a timeout.
                    if !self.peer_volumes.is_empty() {
                        let _ = reply.send(Ok(self.peer_volumes.clone()));
                        continue;
                    }
                    (Pending::Volumes(reply), FilesMsg::ListVolumes { req })
                }
                Op::ReadDir {
                    volume,
                    path,
                    reply,
                } => (
                    Pending::Dir {
                        reply,
                        acc: Vec::new(),
                    },
                    FilesMsg::ReadDir { req, volume, path },
                ),
                Op::Stat {
                    volume,
                    path,
                    reply,
                } => (Pending::Stat(reply), FilesMsg::Stat { req, volume, path }),
                Op::ReadLink {
                    volume,
                    path,
                    reply,
                } => (
                    Pending::Link(reply),
                    FilesMsg::ReadLink { req, volume, path },
                ),
                Op::Open {
                    volume,
                    path,
                    mode,
                    reply,
                } => (
                    Pending::Open(reply),
                    FilesMsg::Open {
                        req,
                        volume,
                        path,
                        mode,
                    },
                ),
                Op::Read {
                    handle,
                    offset,
                    len,
                    reply,
                } => (
                    Pending::Read {
                        reply,
                        acc: Vec::new(),
                    },
                    FilesMsg::Read {
                        req,
                        handle,
                        offset,
                        len,
                    },
                ),
                // Compressed on a worker, not here.
                //
                // `pack` on a 256 KiB chunk is milliseconds of CPU, and a copy
                // produces one per chunk without pause — inline, that is the
                // session loop spending most of a transfer inside zstd while
                // input and audio wait their turn behind it. The prepared
                // message comes back through `outbox` and goes out on a later
                // pump; the request counts against the window from now, because
                // it is work the peer will owe us an answer for.
                Op::Write {
                    handle,
                    offset,
                    bytes,
                    reply,
                } => {
                    self.pending.insert(req, Pending::Wrote(reply));
                    // Timed from here rather than from the send: the round trip
                    // the caller waits out includes the compression, so timing
                    // only the wire would understate what a write costs.
                    self.health.sent(req, "write", Instant::now());
                    self.health.written_bytes += bytes.len() as u64;
                    let outbox = self.outbox_tx.clone();
                    // Counted before the worker starts, cleared only once the
                    // message is on the queue, so a close arriving in between
                    // can see that this handle still owes bytes to the link.
                    let compressing = std::sync::Arc::clone(&self.compressing);
                    *compressing
                        .lock()
                        .expect("files compressing")
                        .entry(handle)
                        .or_insert(0) += 1;
                    self.codec.spawn(move || {
                        let packed = pack(bytes);
                        let _ = outbox.send(FilesMsg::Write {
                            req,
                            handle,
                            offset,
                            bytes: packed,
                            last: true,
                        });
                        let mut guard = compressing.lock().expect("files compressing");
                        if let Some(n) = guard.get_mut(&handle) {
                            *n -= 1;
                            if *n == 0 {
                                guard.remove(&handle);
                            }
                        }
                    });
                    allowance -= 1;
                    continue;
                }
                Op::Truncate {
                    handle,
                    size,
                    reserve,
                    reply,
                } => (
                    Pending::Unit(reply),
                    FilesMsg::Truncate {
                        req,
                        handle,
                        size,
                        reserve,
                    },
                ),
                Op::MkDir {
                    volume,
                    path,
                    reply,
                } => (Pending::Unit(reply), FilesMsg::MkDir { req, volume, path }),
                Op::Remove {
                    volume,
                    path,
                    reply,
                } => (Pending::Unit(reply), FilesMsg::Remove { req, volume, path }),
                Op::Rename {
                    volume,
                    from,
                    to,
                    reply,
                } => (
                    Pending::Unit(reply),
                    FilesMsg::Rename {
                        req,
                        volume,
                        from,
                        to,
                    },
                ),
                // Nothing to wait for, so nothing to track — but it must not
                // overtake this handle's writes.
                //
                // A `Write` is compressed on a worker and reaches the link
                // through `outbox` on a later pump; sending the close straight
                // down the socket here jumped that queue, so the peer closed
                // the handle and answered the writes that arrived after it with
                // `BadHandle`. The bytes were never written and the application
                // saw the failure at the end of a file it had finished writing
                // — which is what Steam reports as "Disk Write Error".
                //
                // Queueing it behind them restores the ordering the caller
                // expects: writes still compressing are waited for, everything
                // already queued goes out first, and the close lands last.
                Op::Close { handle } => {
                    self.close_handle(handle);
                    continue;
                }
            };

            // The peer answers `Volumes` with its own list; the request carries
            // no payload of ours, so an empty list is the ask.
            self.pending.insert(req, pending);
            self.health.sent(req, request_kind(&msg), Instant::now());
            allowance -= 1;
            send(tx, msg).await?;
        }
        Ok(())
    }

    /// Route one reply back to the callback waiting for it.
    fn on_reply(&mut self, msg: FilesMsg) {
        // Timed here, before the match, so every reply is measured in one place
        // rather than each arm having to remember to. A `DirEntries` that is not
        // the last batch is timed at its first message, which is the round trip
        // the caller actually waited for.
        let now = Instant::now();
        if let Some(req) = reply_req(&msg) {
            self.health.answered(req, now);
        }
        if let FilesMsg::Data { bytes, .. } = &msg {
            // Deliberately not fed to the pacer. These bytes crossed the link
            // in the other direction, and the window being sized is on ours:
            // counting them made a mount that was reading report 303 Mbit/s of
            // outbound capacity against the 144 the socket was managing, so the
            // window grew on evidence from a direction it does not control.
            self.health.read_bytes += bytes.plain_len() as u64;
        }
        if let FilesMsg::Failed { error, .. } = &msg {
            self.health.failed += 1;
            // Logged, not just counted. A failure rate says something is wrong;
            // only the reason says what, and an operation the peer refuses
            // leaves no trace on this side otherwise — the count alone had me
            // guessing at which operation was failing.
            tracing::debug!(peer = %self.peer_name, ?error, "peer refused a file operation");
        }

        match msg {
            FilesMsg::Volumes(volumes) => {
                // Remember them: the peer announces its volumes unprompted a few
                // seconds after the session starts, which is *after* the mount
                // has already asked and timed out. Without keeping the answer,
                // that first listing fails and every later one pays a round trip
                // to learn what we were told for free.
                //
                // An empty list never replaces a known one. A machine with no
                // shareable disks is not a case worth modelling, whereas an
                // empty announcement arriving for any other reason would blank
                // a working mount — which is exactly what happened when the
                // request and the announcement were the same message.
                if !volumes.is_empty() {
                    self.peer_volumes = volumes.clone();
                    // Follow the set: on Windows each volume is its own drive
                    // letter, so this is where a peer's disks appear and
                    // disappear.
                    self.sync_peer_mounts();
                }
                // The peer announces its volumes unprompted at start, and also
                // answers our request with the same message — so satisfy every
                // waiting request rather than assuming there is exactly one.
                for req in self.pending.waiting_for_volumes() {
                    if let Some(Pending::Volumes(tx)) = self.pending.remove(&req) {
                        let _ = tx.send(Ok(volumes.clone()));
                    }
                }
                tracing::info!(
                    peer = %self.peer_name,
                    count = volumes.len(),
                    "peer offered its volumes"
                );
            }
            FilesMsg::DirEntries { req, entries, last } => {
                let Some(Pending::Dir { reply, mut acc }) = self.pending.remove(&req) else {
                    return;
                };
                acc.extend(entries);
                if last {
                    let _ = reply.send(Ok(acc));
                } else {
                    // More batches to come: keep the request open.
                    self.pending.insert(req, Pending::Dir { reply, acc });
                }
            }
            FilesMsg::StatResult { req, meta } => {
                if let Some(Pending::Stat(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok(meta));
                }
            }
            FilesMsg::LinkTarget { req, target } => {
                if let Some(Pending::Link(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok(target));
                }
            }
            FilesMsg::Opened { req, handle, size } => {
                if let Some(Pending::Open(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok((handle, size)));
                }
            }
            FilesMsg::Data {
                req, bytes, last, ..
            } => {
                let Some(Pending::Read { reply, mut acc }) = self.pending.remove(&req) else {
                    return;
                };
                // The whole read in one message — every read the server sends
                // today — is decompressed on a worker, which then answers the
                // waiting mount thread directly. Inline it is a quarter of a
                // millisecond per chunk on the session loop, several hundred
                // times a second while a file is being read: not a stall, but a
                // steady tax on input latency for the length of every copy.
                if last && acc.is_empty() {
                    self.codec.spawn(move || {
                        let _ = reply.send(unpack(bytes));
                    });
                    return;
                }
                // Split across messages: the pieces have to be joined in order,
                // so this path stays here. Rare enough not to matter.
                match unpack(bytes) {
                    Ok(b) => acc.extend(b),
                    Err(e) => return drop(reply.send(Err(e))),
                }
                if last {
                    let _ = reply.send(Ok(acc));
                } else {
                    self.pending.insert(req, Pending::Read { reply, acc });
                }
            }
            FilesMsg::Wrote { req, written } => {
                // The peer has stored these bytes, which makes this the one
                // measurement of the link that cannot outrun it.
                self.pace.sample_acked(written, Instant::now());
                if let Some(Pending::Wrote(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok(written));
                }
            }
            FilesMsg::Ok { req } => {
                if let Some(Pending::Unit(tx)) = self.pending.remove(&req) {
                    let _ = tx.send(Ok(()));
                }
            }
            FilesMsg::Failed { req, error } => {
                if let Some(pending) = self.pending.remove(&req) {
                    pending.fail(error);
                }
            }
            // A request from the peer, not a reply — served elsewhere.
            _ => {}
        }
    }
}

#[async_trait::async_trait]
impl SessionModule for FilesEngine {
    fn name(&self) -> &'static str {
        "files"
    }

    /// Busy whenever anything is owed an answer, in either direction.
    ///
    /// A mount serving a download barely sends: it receives requests and
    /// answers them, so the loop's send-based idle test judged it quiet and
    /// slept 50 ms between answers. Both directions count here — requests this
    /// machine is waiting on (`pending`), replies the workers have prepared
    /// (`done_rx`), and writes still being compressed (`outbox_rx`).
    fn has_pending_work(&self) -> bool {
        !self.pending.is_empty()
            || !self.done_rx.is_empty()
            || !self.outbox_rx.is_empty()
            || self.in_progress.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        let volumes = self
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .volumes();
        tracing::info!(
            peer = %self.peer_name,
            volumes = volumes.len(),
            // The peer's own volumes are mounted when it announces them, so
            // there is nothing to report here yet — see `sync_peer_mounts`.
            "file sharing up"
        );
        #[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
        tracing::warn!(
            peer = %self.peer_name,
            "no mount backend on this platform, so the peer's volumes will not appear here; \
             sharing ours still works"
        );
        send(tx, FilesMsg::Volumes(volumes)).await?;
        self.announce_volumes = false;
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        self.rescan_volumes();

        if self.announce_volumes {
            let volumes = self
                .server
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .volumes();
            send(tx, FilesMsg::Volumes(volumes)).await?;
            self.announce_volumes = false;
        }

        // Read the link before deciding how much to put on it. One reading per
        // pump — every 5 ms — is frequent enough to catch a link going from
        // idle to congested within a couple of chunks, and cheap: two integers
        // from behind the transport's lock.
        let now = Instant::now();
        // The data class, which is where a transfer's bytes queue. Metadata
        // rides a class above and is small enough that its depth says nothing
        // about whether the link is keeping up.
        self.pace.observe(tx.queue_depth(FilesMsg::PRIORITY), now);
        self.log_health(now);
        let deadline = now + PUMP_SLICE;

        // Retry any volume that could not be mounted. Driven from here rather
        // than only from the peer's announcement, because a peer whose volume
        // set has not changed never sends another one — so a mount that failed
        // once would otherwise stay failed for the life of the session.
        // `mount_retry_at` keeps this to one attempt every few seconds.
        self.sync_peer_mounts();

        // Before the window is sized from the entries: see [`PENDING_EXPIRY`].
        let expired = self.pending.expire(now, PENDING_EXPIRY);
        if expired > 0 {
            self.health.failed += expired as u64;
            tracing::warn!(
                peer = %self.peer_name,
                expired,
                "requests the peer never answered were given up on"
            );
        }

        // Before draining, so a close whose writes finished compressing since
        // the last pump goes out behind them in this same pass.
        self.flush_deferred_closes();

        // Replies first: the peer is blocked on them, and a request of ours can
        // wait a pass. First is not *all*, though — each of the three gets its
        // own share of the slice rather than the whole of what the one before
        // it left. Handing all three the same deadline reads as a priority
        // order and behaves as starvation: serving a peer's download fills
        // `done_rx` with hundreds of answers per chunk we send, and they spent
        // the 2 ms every time. Measured against Steam writing to our disk, our
        // own data reached the link in 6% of pumps — 12.6 writes a second where
        // the loop's 5 ms pace allows 200, which is below even the idle pace's
        // ceiling.
        //
        // Replies still get the largest share, and an empty queue costs its
        // holder nothing: `drain` returns on the first `try_recv` miss, so a
        // quiet direction hands its share straight to the next.
        let share = PUMP_SLICE / SLICE_SHARES;
        // Replies are not paced: the peer is blocked on them, they are small,
        // and holding one back to smooth our own sending would add latency to
        // the very machine waiting on us.
        drain(
            &mut self.done_rx,
            tx,
            now + share * REPLY_SHARES,
            None,
            None,
        )
        .await?;
        let budget = self.pace.pump_budget();
        drain(
            &mut self.outbox_rx,
            tx,
            deadline.min(now + share * REPLY_SHARES + share),
            Some(&mut self.health),
            budget,
        )
        .await?;
        self.pump_requests(tx, deadline).await?;
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        match msg.decode_module::<FilesMsg>(ModuleId::FILES) {
            Ok(Some(msg)) => {
                if is_request(&msg) {
                    // Disk work: off to a blocking thread, answered later.
                    self.serve(msg);
                } else {
                    self.on_reply(msg);
                }
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, "undecodable files message from peer");
                true
            }
        }
    }

    async fn shutdown(&mut self, _tx: &mut dyn TransportTx) {
        // Fail everything still in flight before the mount goes away, or its
        // FUSE threads sit out the full timeout for answers that will never
        // come — and the unmount blocks behind them.
        for pending in self.pending.drain() {
            pending.fail(FileError::Io("session ended".into()));
        }
        // Release the mount's own threads before taking its mounts away.
        //
        // Failing the pending map covers requests this engine has already sent;
        // it does nothing for a dispatcher thread still waiting on a reply
        // channel, and dropping the session's receiver would not either — the
        // sender lives here, in `mount_link`, so those threads wait out their
        // full deadline rather than failing at once. Unmount waits for exactly
        // those threads, so the process hung in `unmount` with this function's
        // own log line as its last.
        self.mount_link.shut_down();
        // Dropping the handles unmounts.
        #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
        if !self.volume_mounts.is_empty() {
            tracing::info!(
                peer = %self.peer_name,
                volumes = self.volume_mounts.len(),
                "unmounting the peer's volumes"
            );
            self.volume_mounts.clear();
        }
    }
}

/// Builds [`FilesEngine`] when file sharing is enabled.
///
/// Linux-only for now: sharing our own volumes works anywhere, but a peer that
/// could not mount ours would advertise a capability it cannot honour, and the
/// mount half needs WinFsp on Windows.
pub struct FilesFactory {
    pub enabled: bool,
}

impl crate::module::ModuleFactory for FilesFactory {
    fn id(&self) -> ModuleId {
        ModuleId::FILES
    }

    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        self.enabled
            .then(|| cursio_protocol::ModuleAdvert::symmetric(ModuleId::FILES, VERSION))
    }

    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        Ok(Box::new(FilesEngine::new(ctx.peer_name().to_string())?))
    }
}

/// Version of this module's wire vocabulary ([`FilesMsg`]). Bump when those
/// types change shape; peers run the module only on an exact match.
pub const VERSION: u16 = 4;

/// Whether a message is something to act on, rather than an answer to something
/// we asked.
fn is_request(msg: &FilesMsg) -> bool {
    matches!(
        msg,
        FilesMsg::ReadDir { .. }
            | FilesMsg::Stat { .. }
            | FilesMsg::ReadLink { .. }
            | FilesMsg::Open { .. }
            | FilesMsg::Read { .. }
            | FilesMsg::Write { .. }
            | FilesMsg::Truncate { .. }
            | FilesMsg::MkDir { .. }
            | FilesMsg::Remove { .. }
            | FilesMsg::Rename { .. }
            | FilesMsg::Close { .. }
            | FilesMsg::ListVolumes { .. }
    )
}

/// A one-word name for what a request asks, for counting by kind.
fn request_kind(msg: &FilesMsg) -> &'static str {
    match msg {
        FilesMsg::ReadDir { .. } => "readdir",
        FilesMsg::Stat { .. } => "stat",
        FilesMsg::ReadLink { .. } => "readlink",
        FilesMsg::Open { .. } => "open",
        FilesMsg::Read { .. } => "read",
        FilesMsg::Write { .. } => "write",
        FilesMsg::Truncate { .. } => "set_size",
        FilesMsg::MkDir { .. } => "mkdir",
        FilesMsg::Remove { .. } => "remove",
        FilesMsg::Rename { .. } => "rename",
        FilesMsg::Close { .. } => "close",
        FilesMsg::ListVolumes { .. } => "volumes",
        _ => "other",
    }
}

/// The id a request from the peer expects its answer under.
///
/// `None` for the two that are never answered by id: a `Close` is deliberately
/// unanswered, and `ListVolumes` is answered with the announcement itself.
fn request_req(msg: &FilesMsg) -> Option<FileReqId> {
    match msg {
        FilesMsg::ReadDir { req, .. }
        | FilesMsg::Stat { req, .. }
        | FilesMsg::ReadLink { req, .. }
        | FilesMsg::Open { req, .. }
        | FilesMsg::Read { req, .. }
        | FilesMsg::Write { req, .. }
        | FilesMsg::Truncate { req, .. }
        | FilesMsg::MkDir { req, .. }
        | FilesMsg::Remove { req, .. }
        | FilesMsg::Rename { req, .. } => Some(*req),
        _ => None,
    }
}

/// The request a reply answers, for timing its round trip.
///
/// `None` for the two messages that answer nothing in particular: `Volumes` is
/// announced unprompted as well as sent in reply, so it has no single request
/// to time, and anything that is not a reply at all is not this function's
/// business.
fn reply_req(msg: &FilesMsg) -> Option<FileReqId> {
    match msg {
        FilesMsg::DirEntries { req, .. }
        | FilesMsg::StatResult { req, .. }
        | FilesMsg::LinkTarget { req, .. }
        | FilesMsg::Opened { req, .. }
        | FilesMsg::Data { req, .. }
        | FilesMsg::Wrote { req, .. }
        | FilesMsg::Ok { req }
        | FilesMsg::Failed { req, .. } => Some(*req),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects what the module sends, standing in for the transport, and
    /// reports whatever backlog the test wants the link to have.
    #[derive(Default)]
    struct RecordingTx {
        sent: Vec<FilesMsg>,
        depth: QueueDepth,
    }

    #[async_trait::async_trait]
    impl TransportTx for RecordingTx {
        async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
            if let Ok(Some(msg)) = msg.decode_module::<FilesMsg>(ModuleId::FILES) {
                self.sent.push(msg);
            }
            Ok(())
        }

        fn queue_depth(&self, _priority: cursio_protocol::Priority) -> QueueDepth {
            self.depth
        }
    }

    /// An engine with no mount attached: the client half is driven directly
    /// through its operation channel, which is all these tests need and avoids
    /// mounting a filesystem on the machine running them.
    fn engine() -> (FilesEngine, tokio::sync::mpsc::UnboundedSender<Op>) {
        let (ops_tx, ops_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outbox_tx, outbox_rx) = tokio::sync::mpsc::unbounded_channel();
        let (rescan_tx, rescan_rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = FilesEngine {
            peer_name: "peer".into(),
            server: std::sync::Arc::new(std::sync::Mutex::new(FileServer::detect())),
            ops_rx,
            done_tx,
            done_rx,
            outbox_tx,
            outbox_rx,
            compressing: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            deferred_closes: Vec::new(),
            workers: Workers::with_threads("test-files", 2),
            codec: Workers::with_threads("test-files-codec", 1),
            pace: Pacer::new(),
            pending: PendingMap::default(),
            next_req: 1,
            health: Health::default(),
            served: HashMap::new(),
            slow_served: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            in_progress: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
            volume_mounts: HashMap::new(),
            mount_link: PeerLink::new(tokio::sync::mpsc::unbounded_channel().0),
            #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
            mount_retry_at: None,
            announce_volumes: false,
            peer_volumes: Vec::new(),
            last_rescan: Instant::now(),
            rescanning: false,
            rescan_tx,
            rescan_rx,
        };
        (engine, ops_tx)
    }

    /// A `Stat` the test can queue without caring about the answer.
    fn a_request() -> Op {
        let (reply, _rx) = std::sync::mpsc::channel();
        Op::Stat {
            volume: "C:".into(),
            path: "a.txt".into(),
            reply,
        }
    }

    /// The mount can queue far more work than the link can carry; only the
    /// window's worth may go out.
    ///
    /// Without this the queue depth is whatever the mount's thread count
    /// happens to be — dozens of chunks committed to a link that has not
    /// finished the first, which is delay for everything else riding it and
    /// memory held on both machines for work nobody is waiting on yet.
    #[tokio::test]
    async fn only_the_window_goes_out_however_much_is_waiting() {
        let (mut engine, ops) = engine();
        for _ in 0..100 {
            ops.send(a_request()).expect("queued");
        }

        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");

        assert_eq!(
            tx.sent.len(),
            MIN_IN_FLIGHT,
            "a fresh pacer opens at its floor and grows from there"
        );
        assert_eq!(engine.pending.len(), MIN_IN_FLIGHT);

        // Nothing further goes out until answers come back: the window counts
        // what is outstanding, not what was sent this pass.
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");
        assert_eq!(tx.sent.len(), MIN_IN_FLIGHT, "the window is still full");
    }

    /// Our own writes must not queue behind the work the peer asks of us.
    ///
    /// Sharing one pool put every chunk of a local download behind a peer's
    /// directory walk, and Steam's download sat at 0 B/s while it was served.
    #[tokio::test]
    async fn our_writes_are_prepared_while_the_peers_work_occupies_its_pool() {
        let (mut engine, ops) = engine();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        engine.workers = Workers::with_threads("test-files-busy", 1);
        engine.workers.spawn(move || {
            let _ = gate.recv();
        });

        let (reply, _rx) = std::sync::mpsc::channel();
        ops.send(Op::Write {
            handle: 7,
            offset: 0,
            bytes: vec![1u8; 4096],
            reply,
        })
        .expect("queued");
        let mut tx = RecordingTx::default();
        engine
            .pump_requests(&mut tx, Instant::now() + Duration::from_secs(30))
            .await
            .expect("pumps");

        let prepared = tokio::time::timeout(Duration::from_secs(10), engine.outbox_rx.recv())
            .await
            .expect("the write is compressed while the serving pool is stuck");
        assert!(matches!(prepared, Some(FilesMsg::Write { handle: 7, .. })));
        drop(release);
    }

    /// Requests the peer never answers must not keep the window shut.
    ///
    /// The window is sized from the outstanding entries, so entries that are
    /// never cleared close it for good: every later operation times out in
    /// the mount without having been sent.
    #[tokio::test]
    async fn requests_the_peer_never_answers_stop_holding_the_window() {
        let (mut engine, ops) = engine();
        for _ in 0..10 {
            ops.send(a_request()).expect("queued");
        }
        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");
        assert_eq!(tx.sent.len(), MIN_IN_FLIGHT, "the window is full");

        let later = Instant::now() + PENDING_EXPIRY + Duration::from_millis(1);
        assert_eq!(engine.pending.expire(later, PENDING_EXPIRY), MIN_IN_FLIGHT);
        assert!(engine.pending.is_empty());

        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");
        assert_eq!(
            tx.sent.len(),
            MIN_IN_FLIGHT * 2,
            "with the dead entries gone the next requests go out"
        );
    }

    /// An expired request is failed back to whoever asked, not dropped.
    #[test]
    fn an_expired_request_is_failed_back_to_its_caller() {
        let mut pending = PendingMap::default();
        let (reply, answer) = std::sync::mpsc::channel();
        pending.insert(1, Pending::Stat(reply));

        assert_eq!(pending.expire(Instant::now(), PENDING_EXPIRY), 0, "not yet");
        let later = Instant::now() + PENDING_EXPIRY;
        assert_eq!(pending.expire(later, PENDING_EXPIRY), 1);
        assert!(matches!(answer.try_recv(), Ok(Err(FileError::Io(_)))));
    }

    /// The pump must stop when its slice is up, whatever is left waiting.
    ///
    /// Input, audio and the shared cursor are pumped from the same loop, so a
    /// pass that ran until the queue emptied would hand a large copy the whole
    /// session — the stall this module logs as "pump blocked the session loop".
    #[tokio::test]
    async fn a_pump_stops_when_its_slice_is_spent() {
        let (mut engine, ops) = engine();
        for _ in 0..10 {
            ops.send(a_request()).expect("queued");
        }

        let mut tx = RecordingTx::default();
        // A deadline already past: the pass must decline to start rather than
        // run "just one more".
        engine
            .pump_requests(&mut tx, Instant::now())
            .await
            .expect("pumps");
        assert!(tx.sent.is_empty(), "sent {:?}", tx.sent);
    }

    /// Replies the peer is waiting on go out even while our own requests are
    /// being held back, and a write is not compressed on the session loop.
    #[tokio::test]
    async fn answers_are_never_held_back_and_writes_are_prepared_elsewhere() {
        let (mut engine, ops) = engine();
        let (reply, _rx) = std::sync::mpsc::channel();
        ops.send(Op::Write {
            handle: 7,
            offset: 0,
            bytes: b"the same words over and over ".repeat(64),
            reply,
        })
        .expect("queued");

        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");

        // The write left the loop without being sent: it is on a worker being
        // compressed, and is already counted as outstanding.
        assert!(tx.sent.is_empty(), "compression must not happen inline");
        assert_eq!(engine.pending.len(), 1);

        // It arrives on the outbox and goes out on a later pump, compressed.
        let prepared = tokio::time::timeout(Duration::from_secs(10), engine.outbox_rx.recv())
            .await
            .expect("the worker answers")
            .expect("a prepared message");
        let FilesMsg::Write { bytes, .. } = &prepared else {
            panic!("expected a write, got {prepared:?}");
        };
        assert!(
            matches!(bytes, Chunk::Zstd { .. }),
            "repetitive data must have been compressed by the worker"
        );
    }

    /// A close must not reach the peer before the writes it follows.
    ///
    /// This is the bug behind "Disk Write Error": a write goes to a worker to
    /// be compressed, and the close that came after it used to be written
    /// straight to the socket from the pump. The peer dropped the handle first
    /// and answered the write that arrived behind it with `BadHandle`, so the
    /// last bytes of a file were silently lost and the application failed at
    /// the very end of a copy that had otherwise worked.
    #[tokio::test]
    async fn a_close_waits_for_the_writes_it_follows() {
        let (mut engine, ops) = engine();
        // The pool is held busy until the close has been seen, so the write
        // cannot finish compressing first and make the deferral moot — which
        // is a timing the test would otherwise lose now and then.
        let (release, gate) = std::sync::mpsc::channel::<()>();
        engine.codec = Workers::with_threads("test-files-gated", 1);
        engine.codec.spawn(move || {
            let _ = gate.recv();
        });
        let (reply, _rx) = std::sync::mpsc::channel();
        ops.send(Op::Write {
            handle: 7,
            offset: 0,
            bytes: b"payload that takes a moment to compress ".repeat(64),
            reply,
        })
        .expect("queued");
        ops.send(Op::Close { handle: 7 }).expect("queued");

        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");

        // Neither has gone out: the write is still compressing, and the close
        // is being held back precisely because of it.
        assert!(
            tx.sent.is_empty(),
            "nothing may go straight to the socket here"
        );
        assert_eq!(
            engine.deferred_closes,
            vec![7],
            "the close must be waiting on the write"
        );

        // Once the worker has queued the write, the close follows it — in that
        // order, which is the whole point.
        release.send(()).expect("the gate job is waiting");
        let write = tokio::time::timeout(Duration::from_secs(10), engine.outbox_rx.recv())
            .await
            .expect("the worker answers")
            .expect("a prepared message");
        assert!(matches!(write, FilesMsg::Write { handle: 7, .. }));

        // The worker clears its count just after queueing the write, so the
        // close may need another pump's look — exactly as it would live.
        let close = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                engine.flush_deferred_closes();
                if let Ok(msg) = engine.outbox_rx.try_recv() {
                    return msg;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the close follows");
        assert!(
            matches!(close, FilesMsg::Close { handle: 7 }),
            "expected the close, got {close:?}"
        );
        assert!(engine.deferred_closes.is_empty());
    }

    /// A close on a handle with nothing in flight goes out without waiting.
    ///
    /// The deferral exists for writes still being compressed; making every
    /// close wait a pump would slow down closing the files — most of them —
    /// that were only ever read.
    #[tokio::test]
    async fn a_close_with_no_writes_behind_it_is_not_delayed() {
        let (mut engine, ops) = engine();
        ops.send(Op::Close { handle: 4 }).expect("queued");

        let mut tx = RecordingTx::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        engine
            .pump_requests(&mut tx, deadline)
            .await
            .expect("pumps");

        assert!(engine.deferred_closes.is_empty(), "nothing to wait for");
        let close = engine.outbox_rx.recv().await.expect("queued at once");
        assert!(matches!(close, FilesMsg::Close { handle: 4 }));
    }

    /// A scan that has not come back yet does not get a second one queued
    /// behind it.
    ///
    /// The timer fires once a second, but a scan takes as long as the slowest
    /// drive answers — 3 s on a Windows machine with a medium that has gone
    /// away. Without this guard each tick would start another scan of the same
    /// stuck device, and the pool would fill with scans that all block on it.
    #[test]
    fn a_rescan_does_not_start_while_one_is_still_running() {
        let (mut engine, _ops) = engine();

        // First pass: nothing in flight, so a scan starts.
        engine.last_rescan = Instant::now() - VOLUME_RESCAN * 2;
        engine.rescan_volumes();
        assert!(engine.rescanning, "a scan should have started");

        // Second pass, well past the interval: the first has not answered, so
        // no new scan may begin.
        engine.last_rescan = Instant::now() - VOLUME_RESCAN * 2;
        let before = engine.last_rescan;
        engine.rescan_volumes();
        assert!(
            engine.rescanning,
            "the in-flight scan should still be the only one"
        );
        assert_eq!(
            engine.last_rescan, before,
            "a skipped tick must not count as a scan"
        );
    }

    #[test]
    fn the_window_opens_up_while_the_link_keeps_up() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        // A link draining 10 MB/s with nothing queued.
        pace.observe(QueueDepth::default(), start);
        for i in 1..500u64 {
            let at = start + RATE_SAMPLE_INTERVAL * i as u32;
            pace.observe(
                QueueDepth {
                    queued_bytes: 0,
                    sent_bytes: 200_000 * i,
                },
                at,
            );
        }
        assert_eq!(pace.in_flight_limit, MAX_IN_FLIGHT);
    }

    /// Metadata traffic must not be mistaken for a slow link.
    ///
    /// Observed on a live link with a 4.8 ms round trip: ordinary `stat` and
    /// `readdir` calls moved a few dozen bytes per sample, the estimate settled
    /// at 3–4 KB/s, and the next 72 KB queued was judged to need 15.9 s to
    /// drain. The window collapsed to its floor, operations timed out at 3 s,
    /// and every one of the peer's drives showed as an empty folder that
    /// answered `EIO` when opened.
    #[test]
    fn a_trickle_of_metadata_is_not_read_as_a_slow_link() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);

        // First, a real transfer teaches the pacer what the link can do:
        // 10 MB/s.
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 1_000_000,
            },
            start + Duration::from_millis(100),
        );
        let learned = pace.rate.expect("a rate was measured");
        pace.in_flight_limit = 64;

        // Then the copy ends and only metadata crosses: a few dozen bytes per
        // sample, for seconds.
        let mut sent = 1_000_000u64;
        for i in 1..200u32 {
            sent += 40;
            pace.observe(
                QueueDepth {
                    queued_bytes: 0,
                    sent_bytes: sent,
                },
                start + Duration::from_millis(100) + RATE_SAMPLE_INTERVAL * i,
            );
        }
        assert_eq!(
            pace.rate.expect("still measured"),
            learned,
            "samples too small to describe the link must not rewrite the estimate"
        );

        // A directory listing now queues 72 KB. On this link that is under a
        // millisecond, so the window must not treat it as congestion.
        pace.observe(
            QueueDepth {
                queued_bytes: 72 * 1024,
                sent_bytes: sent,
            },
            start + Duration::from_secs(5),
        );
        assert!(
            pace.in_flight_limit >= 64,
            "72 KB on a 10 MB/s link is not congestion; window fell to {}",
            pace.in_flight_limit
        );
    }

    /// An absurdly low estimate must not be acted on even if one is reached.
    ///
    /// The guard above stops such a sample being recorded; this one stops a
    /// stale estimate — smoothing makes one outlive its sample — from turning a
    /// queue that drains in milliseconds into an apparent stall.
    #[test]
    fn an_impossibly_slow_estimate_is_not_believed() {
        let mut pace = Pacer::new();
        pace.rate = Some(4.0 * 1024.0); // 4 KB/s, as measured in the wild
        pace.in_flight_limit = 64;

        // 72 KB at a believed 4 KB/s is 18 s; floored, it is well under the
        // target and the window holds.
        pace.observe(
            QueueDepth {
                queued_bytes: 72 * 1024,
                sent_bytes: 0,
            },
            Instant::now(),
        );
        assert!(
            pace.in_flight_limit >= 64,
            "a 4 KB/s reading on a LAN is a mismeasurement, not congestion"
        );
    }

    /// A round trip is measured from the request going out to its answer.
    ///
    /// This is the number that separates "the network is slow" from "we are
    /// slow", and its absence is why diagnosing the pacer collapse took an
    /// afternoon of measuring the link by hand — twice reaching the wrong
    /// conclusion, because a measurement taken outside Cursio measures
    /// something else.
    #[test]
    fn a_round_trip_is_timed_from_request_to_answer() {
        let mut health = Health::default();
        let t0 = Instant::now();

        health.sent(1, "stat", t0);
        health.sent(2, "stat", t0);
        health.answered(1, t0 + Duration::from_millis(20));
        health.answered(2, t0 + Duration::from_millis(60));

        assert_eq!(health.mean_rtt_ms(), 40, "mean of 20 ms and 60 ms");
        assert_eq!(
            health.rtt_max,
            Duration::from_millis(60),
            "the worst round trip is what a user feels as a stall"
        );
        assert_eq!(health.answered, 2);

        // Rolling clears the window but keeps a request still owed an answer.
        health.sent(3, "stat", t0 + Duration::from_millis(61));
        health.roll(t0 + Duration::from_millis(62));
        assert_eq!(health.mean_rtt_ms(), 0, "the window starts empty");
        assert!(
            health.sent_at.contains_key(&3),
            "a request still in flight must survive the report that interrupted it"
        );
    }

    /// An answer that never comes must not hold its entry forever.
    ///
    /// The mount gives up after `BULK_OP_TIMEOUT` at the latest, so a request
    /// older than that will never be timed — and keeping it would leak an entry
    /// per lost request for the life of the session.
    #[test]
    fn a_request_that_is_never_answered_is_eventually_forgotten() {
        let mut health = Health::default();
        let t0 = Instant::now();
        health.sent(1, "stat", t0);

        health.roll(t0 + cursio_files::bridge::BULK_OP_TIMEOUT + Duration::from_millis(1));
        assert!(
            health.sent_at.is_empty(),
            "a request past the mount's own deadline is never going to be timed"
        );
    }

    /// A module waiting on an answer must not be counted as idle.
    ///
    /// The session loop follows *sent* bytes, and a mount serving a download
    /// sends almost nothing: it receives requests and answers them. Judged by
    /// that alone the loop dropped to its 50 ms idle tick between a request and
    /// its reply — 391 times during one transfer, which is where 111 ms per
    /// write on a 10 ms link came from.
    #[tokio::test]
    async fn a_module_awaiting_an_answer_is_not_idle() {
        let (mut engine, ops) = engine();
        assert!(
            !engine.has_pending_work(),
            "a module with nothing outstanding is idle"
        );

        ops.send(a_request()).expect("queued");
        let mut tx = RecordingTx::default();
        engine
            .pump_requests(&mut tx, Instant::now() + Duration::from_secs(5))
            .await
            .expect("pumps");

        assert!(
            engine.has_pending_work(),
            "a request sent and unanswered is work outstanding"
        );
    }

    /// A request from the peer counts while a worker is still running it.
    ///
    /// This is the half the send-based idle test could never see, and the half
    /// a download actually consists of: between accepting the peer's request
    /// and its answer reaching `done_rx`, the module holds nothing — `pending`
    /// tracks what *we* asked for. So a mount serving a download looked idle
    /// for exactly as long as it was busy.
    #[test]
    fn a_request_being_served_for_the_peer_is_work_outstanding() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let raised = InFlight::raise(&count);
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a request handed to a worker is in flight"
        );
        drop(raised);
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a finished request is not"
        );
    }

    /// A worker that panics must not leave the count raised.
    ///
    /// `run_request` takes a lock it expects to hold, so a panic is reachable,
    /// and the pool's worker thread dies with it. A count stranded above zero
    /// would keep the session loop at its fast pace for the rest of the link.
    #[test]
    fn a_panicking_worker_still_clears_its_count() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let for_job = std::sync::Arc::clone(&count);
        let panicked = std::panic::catch_unwind(move || {
            let _busy = InFlight::raise(&for_job);
            panic!("the disk went away");
        });
        assert!(panicked.is_err(), "the job panicked");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "unwinding clears the count"
        );
    }

    /// Replies to the peer must not spend the whole pump on their own.
    ///
    /// The three drains once shared a single deadline, which reads as a
    /// priority order and behaves as starvation: serving a download fills
    /// `done_rx` with hundreds of answers per chunk of ours, and they took the
    /// entire 2 ms every pass. Live against Steam that left our own data
    /// reaching the link in 6% of pumps — 12.6 writes a second where the loop's
    /// pace allows 200.
    #[test]
    fn replies_cannot_spend_the_whole_pump_slice() {
        let share = PUMP_SLICE / SLICE_SHARES;
        let replies = share * REPLY_SHARES;
        assert!(
            replies < PUMP_SLICE,
            "replies take a share of the slice, not all of it"
        );
        assert!(
            PUMP_SLICE - replies >= share,
            "at least one share is left for our own data and requests"
        );
    }

    /// The window follows the link rather than a fixed count.
    ///
    /// `MAX_IN_FLIGHT` was chosen against a mount's small requests; at 256 KiB
    /// a chunk it is 32 MB, four times what a 110 Mbit/s link drains inside
    /// `TARGET_QUEUE_DELAY`. Measured live, starting there meant the queue
    /// reached 9 MB, the pacer halved four times in 200 ms, and the window
    /// oscillated between 8 and 13 for the whole transfer.
    #[test]
    fn the_window_is_bounded_by_what_the_link_carries() {
        let mut pace = Pacer::new();
        pace.in_flight_limit = MAX_IN_FLIGHT;

        // A link doing 14 MB/s — what this Wi-Fi measured.
        pace.rate = Some(14.0 * 1024.0 * 1024.0);
        let allowed = pace.allowance(0);
        let queued = allowed * FilesMsg::MAX_CHUNK;
        let drains_in = queued as f64 / (14.0 * 1024.0 * 1024.0);
        assert!(
            drains_in <= TARGET_QUEUE_DELAY.as_secs_f64() + 0.01,
            "a full window drains within the target, not four times past it: \
             {allowed} chunks take {drains_in:.2}s"
        );
        assert!(
            allowed >= MIN_IN_FLIGHT,
            "and never falls below the floor that keeps a mount answering"
        );
    }

    /// A faster link is allowed a deeper pipeline.
    #[test]
    fn a_faster_link_earns_a_deeper_window() {
        let mut slow = Pacer::new();
        slow.in_flight_limit = MAX_IN_FLIGHT;
        slow.rate = Some(8.0 * 1024.0 * 1024.0);

        let mut fast = Pacer::new();
        fast.in_flight_limit = MAX_IN_FLIGHT;
        fast.rate = Some(64.0 * 1024.0 * 1024.0);

        assert!(
            fast.allowance(0) > slow.allowance(0),
            "the ceiling moves with the link it is measuring"
        );
    }

    /// A write's round trip is reported in two halves that add up.
    ///
    /// The total alone kept pointing at time nobody could account for: 800 ms
    /// round trips on a link whose whole queue was 130 ms, with compression
    /// measured at 716 MB/s and the serving side showing no slow operations.
    /// Splitting the trip at the point the message reaches the transport is
    /// what tells "we are sitting on it" apart from "the link and the peer
    /// are".
    #[test]
    fn a_write_round_trip_is_split_into_ours_and_the_links() {
        let mut health = Health::default();
        let start = Instant::now();

        health.sent(1, "write", start);
        // 40 ms spent here — compression, the pacer's window, our own pumps.
        health.reached_wire(1, start + Duration::from_millis(40));
        // 300 ms more before the peer answers.
        health.answered(1, start + Duration::from_millis(340));

        assert_eq!(health.mean_stage_ms(health.stage_queued), 40, "our half");
        assert_eq!(health.mean_stage_ms(health.stage_wire), 300, "the link's");
        assert_eq!(
            health.mean_rtt_ms(),
            340,
            "and the halves account for the whole trip"
        );
    }

    /// The window is sized from what the peer confirms, not what TCP accepted.
    ///
    /// Bytes handed to a socket leave into its send buffer at memory speed, so
    /// a link that is backing up reports a rising rate — the speed the buffer
    /// fills, not the speed it drains. Live, that read 215 Mbit/s while the
    /// wire carried 104, so every window sized from it was twice too generous
    /// and the round trip sat at 581 ms.
    #[test]
    fn the_window_follows_acknowledgements_not_the_send_buffer() {
        let mut pace = Pacer::new();
        pace.in_flight_limit = MAX_IN_FLIGHT;
        let start = Instant::now();

        // The socket swallowed bytes at twice the rate the peer confirms them.
        pace.rate = Some(26.0 * 1024.0 * 1024.0);
        let acked = 13 * 1024 * 1024;
        pace.sample_acked(0, start);
        pace.sample_acked(acked, start + Duration::from_secs(1));

        let allowed = pace.allowance(0);
        let by_socket = {
            let mut optimistic = Pacer::new();
            optimistic.in_flight_limit = MAX_IN_FLIGHT;
            optimistic.rate = Some(26.0 * 1024.0 * 1024.0);
            optimistic.allowance(0)
        };
        assert!(
            allowed < by_socket,
            "the confirmed rate is the smaller and is the one used: \
             {allowed} chunks against {by_socket}"
        );
    }

    /// One pump hands over a slice of the link, not the whole window.
    ///
    /// A window bounds what is outstanding and says nothing about how fast it
    /// is handed over: emptying it at once puts a burst on the wire whose
    /// instantaneous rate is far above the link, and everything behind that
    /// burst waits out all of it. That is how a correctly sized window still
    /// produces a standing queue.
    #[test]
    fn one_pump_hands_over_a_slice_of_the_link_not_all_of_it() {
        let mut pace = Pacer::new();
        pace.rate = Some(16.0 * 1024.0 * 1024.0);
        let budget = pace.pump_budget().expect("a measured link has a budget");

        let window_bytes = pace.allowance(0) * FilesMsg::MAX_CHUNK;
        assert!(
            budget < window_bytes,
            "a pump sends less than the whole window: {budget} against {window_bytes}"
        );
        assert!(
            budget >= FilesMsg::MAX_CHUNK,
            "but never less than one message, which cannot be split"
        );
    }

    /// An unmeasured link is not paced — there is nothing to pace against.
    #[test]
    fn an_unmeasured_link_is_not_paced() {
        let pace = Pacer::new();
        assert!(
            pace.pump_budget().is_none(),
            "pacing waits for a measurement rather than guessing one"
        );
    }

    /// The floor must leave the controller somewhere to retreat to.
    ///
    /// Eight chunks is 2 MB, and BBR sizes a connection at about twice the
    /// bandwidth-delay product — on this link, 130 Mbit/s at a 5 ms idle round
    /// trip, that is one chunk. A floor above what the link needs is a queue
    /// the pacer can never give back, which is why narrowing the window above
    /// it changed so little.
    #[test]
    fn the_floor_is_below_what_a_lan_link_needs() {
        let bdp = 130.0e6 / 8.0 * 0.005;
        let two_bdp_chunks = (2.0 * bdp / FilesMsg::MAX_CHUNK as f64).ceil() as usize;
        assert!(
            MIN_IN_FLIGHT <= two_bdp_chunks * 3,
            "the floor is within reach of what the link actually needs: \
             {MIN_IN_FLIGHT} against {two_bdp_chunks}"
        );
    }

    /// Only our own direction may size our own window.
    ///
    /// Replies to reads arrive on the peer's half of the link, and counting
    /// them made a mount that was reading report 303 Mbit/s of outbound
    /// capacity while the socket managed 144 — the window then grew on evidence
    /// from a direction it has no control over.
    #[test]
    fn inbound_replies_do_not_widen_the_outbound_window() {
        let mut pace = Pacer::new();
        pace.in_flight_limit = MAX_IN_FLIGHT;
        let start = Instant::now();
        pace.rate = Some(8.0 * 1024.0 * 1024.0);

        // Writes confirmed at 8 MB/s, matching the socket.
        pace.sample_acked(0, start);
        pace.sample_acked(8 * 1024 * 1024, start + Duration::from_secs(1));
        let honest = pace.allowance(0);

        // The same link, had inbound data been folded in as well.
        let mut inflated = Pacer::new();
        inflated.in_flight_limit = MAX_IN_FLIGHT;
        inflated.rate = Some(8.0 * 1024.0 * 1024.0);
        inflated.sample_acked(0, start);
        inflated.sample_acked(24 * 1024 * 1024, start + Duration::from_secs(1));

        assert!(
            honest < inflated.allowance(0),
            "a window sized from one direction is the smaller of the two"
        );
    }

    /// Until something has been confirmed there is nothing else to go on.
    ///
    /// The point is that the socket's own reading is *used*, not that it is
    /// generous: a fast link with no acknowledgements yet must be sized from
    /// what it is measured to carry rather than from the floor, which is what a
    /// missing estimate would leave it at.
    #[test]
    fn the_socket_rate_still_serves_before_the_first_acknowledgement() {
        let fast = {
            let mut pace = Pacer::new();
            pace.in_flight_limit = MAX_IN_FLIGHT;
            pace.rate = Some(64.0 * 1024.0 * 1024.0);
            pace.allowance(0)
        };
        let slow = {
            let mut pace = Pacer::new();
            pace.in_flight_limit = MAX_IN_FLIGHT;
            pace.rate = Some(8.0 * 1024.0 * 1024.0);
            pace.allowance(0)
        };
        assert!(
            fast > slow,
            "the socket's reading is what sizes the window until an \
             acknowledgement has been timed: {fast} against {slow}"
        );
    }

    /// A full pipe is not congestion.
    ///
    /// A transfer keeping a link busy holds roughly bandwidth × delay in
    /// flight, and that is what the queue depth measures. Judged against a
    /// target below that product, an ordinary copy trips the threshold just by
    /// working: seen live at 3.1 MB/s of file traffic on a link the same log
    /// line measured at 8 MB/s.
    #[test]
    fn an_ordinary_copy_does_not_look_like_congestion() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);
        // Teach it the link: 8 MB/s, which is what Wi-Fi measured here.
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 800_000,
            },
            start + Duration::from_millis(100),
        );
        pace.in_flight_limit = 128;

        // What keeping this link busy actually looks like. A megabyte was the
        // figure this test first used, on the belief that four chunks in flight
        // is a full pipe; at 8 MB/s that is 125 ms of queue, and the pipe
        // itself — twice the bandwidth-delay product at a 5 ms round trip — is
        // 82 KB. The old figure was twelve times the pipe, which is a backlog
        // by any measure, and calling it "full" is what let the queue stand.
        pace.observe(
            QueueDepth {
                queued_bytes: 128 * 1024,
                sent_bytes: 800_000,
            },
            start + Duration::from_millis(200),
        );
        assert_eq!(
            pace.in_flight_limit, 128,
            "a full pipe must not be read as congestion"
        );
    }

    /// One halving per round trip, however many pumps see the same backlog.
    ///
    /// A cut cannot show up in the queue until the requests already on the wire
    /// are answered, so reacting to every pump compounds one reading into a
    /// collapse — logged live as 128 → 64 → 32 → 20 inside a single
    /// millisecond.
    #[test]
    fn a_backlog_is_not_cut_twice_before_it_can_respond() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 1_000_000,
            },
            start + Duration::from_millis(100),
        );
        pace.in_flight_limit = 128;

        // Well past the target however it is measured: 64 MiB queued.
        let backed_up = QueueDepth {
            queued_bytes: 64 * 1024 * 1024,
            sent_bytes: 1_000_000,
        };
        pace.observe(backed_up, start + Duration::from_millis(200));
        assert_eq!(pace.in_flight_limit, 64, "the first reading halves it");

        // Three more pumps in the next millisecond see the same backlog.
        for ms in 201..204 {
            pace.observe(backed_up, start + Duration::from_millis(ms));
        }
        assert_eq!(
            pace.in_flight_limit, 64,
            "the same backlog must not be cut again before a round trip"
        );

        // Once a round trip has passed and it is still backed up, cut again.
        pace.observe(backed_up, start + Duration::from_millis(400));
        assert_eq!(pace.in_flight_limit, 32, "a persisting backlog does cut");
    }

    /// A link falling behind must be eased off at once — and never below the
    /// floor, or the mount's callers time out waiting for a slot.
    #[test]
    fn a_backed_up_link_closes_the_window_but_not_past_the_floor() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);
        // 10 MB/s measured, then 8 MB sitting in the queue: most of a second of
        // delay, against a target of a tenth.
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 1_000_000,
            },
            start + Duration::from_millis(100),
        );
        pace.in_flight_limit = 64;

        pace.observe(
            QueueDepth {
                queued_bytes: 8 * 1024 * 1024,
                sent_bytes: 1_000_000,
            },
            start + Duration::from_millis(200),
        );
        assert_eq!(pace.in_flight_limit, 32, "congestion halves the window");

        for i in 3..12u32 {
            pace.observe(
                QueueDepth {
                    queued_bytes: 8 * 1024 * 1024,
                    sent_bytes: 1_000_000,
                },
                start + Duration::from_millis(100) * i,
            );
        }
        assert_eq!(
            pace.in_flight_limit, MIN_IN_FLIGHT,
            "easing off must not become a stall"
        );
    }

    /// Until something has actually drained there is no rate, and a queue of
    /// unknown age is no reason to act in either direction.
    #[test]
    fn an_unmeasured_link_is_left_alone() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(
            QueueDepth {
                queued_bytes: 4 * 1024 * 1024,
                sent_bytes: 0,
            },
            start,
        );
        let opening = pace.in_flight_limit;
        pace.observe(
            QueueDepth {
                queued_bytes: 4 * 1024 * 1024,
                sent_bytes: 0,
            },
            start + Duration::from_millis(500),
        );
        assert_eq!(
            pace.in_flight_limit, opening,
            "a guess about an unmeasured link is worse than holding still"
        );
        assert!(pace.queue_delay(4 * 1024 * 1024).is_none());
    }

    /// The rate estimate must follow what actually left the machine, since the
    /// whole window is derived from it.
    #[test]
    fn the_rate_estimate_follows_what_left() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 1_000_000,
            },
            start + Duration::from_millis(100),
        );
        let rate = pace.rate.expect("a measured rate");
        assert!(
            (9_000_000.0..11_000_000.0).contains(&rate),
            "1 MB in 100 ms is ~10 MB/s, got {rate}"
        );
        // And a queue is judged against it: 1 MB at 10 MB/s is 100 ms.
        let delay = pace.queue_delay(1_000_000).expect("a delay");
        assert!(
            (Duration::from_millis(90)..Duration::from_millis(110)).contains(&delay),
            "got {delay:?}"
        );
    }

    /// Samples taken too close together are noise, not measurement.
    #[test]
    fn a_rate_is_not_read_from_too_short_an_interval() {
        let mut pace = Pacer::new();
        let start = Instant::now();
        pace.observe(QueueDepth::default(), start);
        pace.observe(
            QueueDepth {
                queued_bytes: 0,
                sent_bytes: 300_000,
            },
            start + Duration::from_millis(1),
        );
        assert!(
            pace.rate.is_none(),
            "300 KB in a millisecond is a scheduling artefact, not 300 MB/s"
        );
    }

    /// Requests are served; replies are routed. Confusing the two would either
    /// hang the mount or run a reply against our own disk.
    #[test]
    fn requests_and_replies_are_told_apart() {
        assert!(is_request(&FilesMsg::Stat {
            req: 1,
            volume: "C:".into(),
            path: "a".into()
        }));
        assert!(is_request(&FilesMsg::Close { handle: 1 }));
        assert!(!is_request(&FilesMsg::Ok { req: 1 }));
        assert!(!is_request(&FilesMsg::Volumes(Vec::new())));
        assert!(!is_request(&FilesMsg::Failed {
            req: 1,
            error: FileError::NotFound
        }));
    }

    /// Asking for the volume list must not look like announcing an empty one.
    ///
    /// These were the same message once: a request encoded as
    /// `Volumes(vec![])`. The peer's request then arrived here as "I have no
    /// volumes", blanked the list we had just been given, and the mount was torn
    /// down a moment later — with `count=1` and `count=0` logged back to back.
    #[test]
    fn asking_for_volumes_is_not_announcing_none() {
        assert!(
            is_request(&FilesMsg::ListVolumes { req: 1 }),
            "a request must be served, not routed as a reply"
        );
        assert!(
            !is_request(&FilesMsg::Volumes(Vec::new())),
            "an announcement is a reply, whatever it contains"
        );
    }

    /// Compressible data must survive the round trip, and so must data that
    /// does not compress at all.
    #[test]
    fn chunks_round_trip_whether_or_not_they_compress() {
        let text = b"the same words over and over ".repeat(64);
        let packed = pack(text.clone());
        assert!(
            matches!(packed, Chunk::Zstd { .. }),
            "repetitive text must compress"
        );
        assert!(
            packed.wire_len() < text.len(),
            "and must actually be smaller"
        );
        assert_eq!(unpack(packed).expect("unpacks"), text);

        // Incompressible: kept raw rather than declared compressed for nothing.
        let noise: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        let packed = pack(noise.clone());
        assert_eq!(unpack(packed).expect("unpacks"), noise);
    }

    /// A payload whose length does not match what was promised must be refused,
    /// not written into a file as if it were correct.
    #[test]
    fn a_mismatched_chunk_is_rejected() {
        let bad = Chunk::Zstd {
            data: zstd::encode_all(&b"four"[..], 1).expect("compresses"),
            plain_len: 999,
        };
        assert!(matches!(unpack(bad), Err(FileError::Io(_))));
    }

    /// Every in-flight request must be failed when the session ends, or the
    /// mount's threads wait out the full timeout for answers that cannot come.
    #[test]
    fn shutdown_releases_everyone_waiting() {
        let (tx, rx) = std::sync::mpsc::channel();
        let pending = Pending::Stat(tx);
        pending.fail(FileError::Io("session ended".into()));
        assert!(
            matches!(rx.recv(), Ok(Err(FileError::Io(_)))),
            "a waiting caller must be told, not left hanging"
        );
    }
}
