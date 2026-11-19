//! How much of the link a transfer may use: the window of requests in
//! flight, sized by what the link is seen to carry and how long it queues.

use cursio_protocol::FilesMsg;
use cursio_transport::QueueDepth;
use std::time::{Duration, Instant};

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
pub(super) const TARGET_QUEUE_DELAY: Duration = Duration::from_millis(50);

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
pub(super) const MIN_IN_FLIGHT: usize = 3;

/// Never more than this many, however fast the link looks.
///
/// The window is the pipeline depth, and every request in it holds a buffer on
/// both machines — at 256 KiB a chunk this is the memory a transfer can commit
/// to. Well above the depth a LAN needs (a 1 Gbit/s link with a 6 ms round trip
/// is filled by roughly three 256 KiB chunks in flight), so the cap only ever
/// bites on a link fast enough not to need the help.
pub(super) const MAX_IN_FLIGHT: usize = 128;

/// Shortest interval between two rate samples.
///
/// Below this the measurement is mostly quantisation: a pump every 5 ms sees a
/// handful of frames, and dividing by a near-zero interval turns one late frame
/// into a wildly wrong rate.
pub(super) const RATE_SAMPLE_INTERVAL: Duration = Duration::from_millis(20);

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
pub(super) const RATE_SAMPLE_MIN_BYTES: u64 = FilesMsg::MAX_CHUNK as u64;

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
pub(super) const RATE_FLOOR_BYTES_PER_SEC: f64 = 4.0 * 1024.0 * 1024.0;

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
pub(super) struct Pacer {
    /// Requests allowed outstanding at once.
    pub(super) in_flight_limit: usize,
    /// Measured drain rate of our own traffic class, in bytes per second.
    /// `None` until enough has moved to measure anything.
    pub(super) rate: Option<f64>,
    /// The previous reading: when it was taken, and the sent-byte count then.
    last_sample: Option<(Instant, u64)>,
    /// The same estimate taken from bytes the peer has confirmed storing.
    ///
    /// Kept beside `rate` rather than replacing it: the socket's count is still
    /// what tells a stalled link from an idle one promptly, while this is what
    /// the window is sized from — see [`Pacer::sample_acked`].
    pub(super) acked_rate: Option<f64>,
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
    pub(super) cuts: u64,
    pub(super) grows: u64,
}

/// How long a halving is given to take effect before another is allowed.
///
/// One cut per round trip is the most that can be justified, since that is how
/// long it takes for the queue to reflect it. Generous compared with the 5 ms
/// pump so a Wi-Fi round trip (measured 20–38 ms here) is comfortably covered.
pub(super) const CUT_COOLOFF: Duration = Duration::from_millis(100);

impl Pacer {
    pub(super) fn new() -> Self {
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
    pub(super) fn observe(&mut self, depth: QueueDepth, now: Instant) {
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
    pub(super) fn sample_acked(&mut self, bytes: u64, now: Instant) {
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
    pub(super) fn pump_budget(&self) -> Option<usize> {
        let rate = self.delivered_rate()?;
        // One pump's share of a second, with a chunk as the floor so a single
        // message is never split across pumps — it cannot be, and a budget
        // below one chunk would stall the transfer instead of pacing it.
        let per_pump = rate * crate::module_host::MODULE_PUMP_INTERVAL.as_secs_f64();
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
    pub(super) fn allowance(&self, in_flight: usize) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files_session::health::Health;

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
}
