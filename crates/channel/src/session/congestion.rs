//! Session-wide background pacing driven by interactive delivery latency.
//!
//! The interactive lane's round trip is compared with the lowest one seen
//! recently: what bulk adds above that is queue it built, and the rate backs
//! off when that queue passes [`QUEUE_TARGET`].
//!
//! "Recently", not "ever": the lowest round trip of the whole session was the
//! baseline once, and a Wi-Fi link whose quiet round trip drifted up after a
//! roam, a channel change or a neighbour's traffic then read as permanently
//! congested, holding bulk near its floor for the rest of the session. The
//! baseline is the lowest of the last [`BASE_WINDOW`], kept as per-bucket
//! minima the way LEDBAT keeps its base delay.
//!
//! Nor is one reading the queue. A Wi-Fi link's quiet round trip spikes by
//! tens of milliseconds on its own — measured to one Windows laptop: 7 to
//! 35 ms with nothing else crossing — and every spike cut bulk by 30%, so a
//! file crossed that link at a third of what TCP alone carried while the
//! pointer gained nothing. A queue bulk built persists and a spike does not,
//! so the queue is read from the lowest of the last [`CURRENT_SAMPLES`]
//! readings, as LEDBAT filters its current delay.
//!
//! ## Strict while a person is at the input, generous while not
//!
//! How much queue bulk may build is a trade between a file's speed and the
//! pointer's, and the right answer depends on whether anyone is moving it.
//! Measured to a Windows laptop on Wi-Fi: holding the queue to 15 ms carried
//! files at a third of the link; letting it grow carried them at 12–14 MB/s,
//! but the laptop's radio then lost the interactive lane's packets, each a
//! 300 ms retransmission timer, and the pointer's round trip reached 50–130 ms
//! at the 95th percentile. So the queue may grow to [`IDLE_QUEUE_TARGET`]
//! while no input has crossed the link for [`INPUT_HOLD`], and the moment
//! input crosses, either way, bulk halves at once and is held to
//! [`QUEUE_TARGET`] again.
use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;

const INITIAL_RATE: f64 = 1024.0 * 1024.0;
const MIN_RATE: f64 = 64.0 * 1024.0;
const MAX_RATE: f64 = 128.0 * 1024.0 * 1024.0;
const UPDATE_INTERVAL: Duration = Duration::from_millis(200);
const QUEUE_TARGET: Duration = Duration::from_millis(15);
/// The queue bulk may build while no person's input is crossing the link.
const IDLE_QUEUE_TARGET: Duration = Duration::from_millis(60);
/// How long after the last input event the link is held to [`QUEUE_TARGET`]:
/// a pause between strokes, or reading what was just typed, is still use.
const INPUT_HOLD: Duration = Duration::from_secs(2);
/// How far back the baseline round trip reaches. Long enough that a transfer
/// running throughout still had moments of an empty queue in it — each cut
/// drains what bulk built — so its own queue does not become the baseline;
/// short enough that a link whose quiet round trip rose is believed within
/// minutes.
const BASE_WINDOW: Duration = Duration::from_secs(180);
/// One bucket of [`BASE_WINDOW`]: its lowest round trip is all that is kept.
const BASE_BUCKET: Duration = Duration::from_secs(10);
/// Readings the queue is judged from: at a probe every 200 ms under load, a
/// queue standing for 600 ms is believed, a one-reading spike is not.
const CURRENT_SAMPLES: usize = 3;

pub(crate) struct BackgroundRate {
    rate: f64,
    /// Lowest round trip of each recent bucket, oldest first, with when the
    /// bucket began.
    minima: VecDeque<(Instant, Duration)>,
    /// The latest readings, newest last.
    current: VecDeque<Duration>,
    updated: Option<Instant>,
    /// No reading has stood above the baseline since bulk last started:
    /// the rate doubles rather than creeping up.
    ///
    /// Each transfer starts at [`INITIAL_RATE`], and at 10% per reading it
    /// took six seconds to reach the 18 MB/s a Wi-Fi link carried, so a
    /// ten-second test measured a third of it and a game's bursty reads,
    /// each starting over after a pause, ran at the bottom of the ramp. As in
    /// TCP's slow start, doubling until the first sign of a queue reaches
    /// that in under a second; from then on the cautious increase holds.
    ///
    /// The first sign is read early and raw, as HyStart does: one reading
    /// past half the target ends the doubling, without a cut. Waiting for the
    /// filtered queue to pass the whole target let three more doublings
    /// through — eight times the rate the link had just shown it could not
    /// take — and a Windows laptop's Wi-Fi answered that burst by losing the
    /// interactive lane's packets, each loss a 300 ms retransmission timer:
    /// the pointer's round trip reached 1.1 s.
    slow_start: bool,
    /// When a person's input last crossed the link.
    last_input: Option<Instant>,
}

impl Default for BackgroundRate {
    fn default() -> Self {
        Self {
            rate: INITIAL_RATE,
            minima: VecDeque::new(),
            current: VecDeque::new(),
            updated: None,
            slow_start: true,
            last_input: None,
        }
    }
}

impl BackgroundRate {
    pub(crate) fn bytes_per_second(&self) -> f64 {
        self.rate
    }

    /// Take one round-trip reading. Answers the reading that made it back
    /// off, and the baseline it stood above, when it did.
    pub(crate) fn observe(
        &mut self,
        rtt: Duration,
        now: Instant,
        active: bool,
    ) -> Option<(Duration, Duration)> {
        let baseline = self.baseline(rtt, now);
        if self.current.len() == CURRENT_SAMPLES {
            self.current.pop_front();
        }
        self.current.push_back(rtt);
        if !active {
            self.rate = self.rate.min(INITIAL_RATE);
            self.updated = None;
            self.slow_start = true;
            return None;
        }
        if self
            .updated
            .is_some_and(|at| now.duration_since(at) < UPDATE_INTERVAL)
        {
            return None;
        }
        self.updated = Some(now);
        let standing = self.current.iter().min().copied().unwrap_or(rtt);
        let target = self.target(now);
        if self.slow_start && rtt.saturating_sub(baseline) > target / 2 {
            self.slow_start = false;
        }
        if standing.saturating_sub(baseline) > target {
            self.rate = (self.rate * 0.7).max(MIN_RATE);
            self.slow_start = false;
            Some((standing, baseline))
        } else {
            self.rate = match self.slow_start {
                true => self.rate * 2.0,
                false => self.rate * 1.1 + 16_384.0,
            }
            .min(MAX_RATE);
            None
        }
    }
}

impl BackgroundRate {
    /// A person's input crossed the link at `now`. The first after a quiet
    /// spell halves bulk at once: the queue it built under the generous
    /// target has to drain before the pointer stops feeling it, and waiting
    /// for readings to show it would take most of a second.
    pub(crate) fn note_input(&mut self, now: Instant) {
        let was_quiet = self
            .last_input
            .is_none_or(|at| now.duration_since(at) > INPUT_HOLD);
        self.last_input = Some(now);
        if was_quiet {
            self.rate = (self.rate * 0.5).max(MIN_RATE);
            self.slow_start = false;
        }
    }

    /// The queue bulk may build now.
    fn target(&self, now: Instant) -> Duration {
        match self
            .last_input
            .is_some_and(|at| now.duration_since(at) <= INPUT_HOLD)
        {
            true => QUEUE_TARGET,
            false => IDLE_QUEUE_TARGET,
        }
    }

    /// Fold `rtt` into the recent minima and answer the lowest of them.
    fn baseline(&mut self, rtt: Duration, now: Instant) -> Duration {
        match self.minima.back_mut() {
            Some((began, low)) if now.duration_since(*began) < BASE_BUCKET => {
                *low = (*low).min(rtt);
            }
            _ => self.minima.push_back((now, rtt)),
        }
        while self
            .minima
            .front()
            .is_some_and(|(began, _)| now.duration_since(*began) > BASE_WINDOW)
        {
            self.minima.pop_front();
        }
        self.minima.iter().map(|(_, low)| *low).min().unwrap_or(rtt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_yields_to_a_standing_queue_and_recovers_gradually() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        // Someone is at the input: the strict target holds.
        controller.note_input(now);
        controller.observe(Duration::from_millis(10), now, true);
        // A queue is believed once it has stood for every reading the filter
        // holds, and not before.
        let mut cut = None;
        for _ in 0..=CURRENT_SAMPLES {
            now += UPDATE_INTERVAL;
            let before = controller.bytes_per_second();
            if controller
                .observe(Duration::from_millis(60), now, true)
                .is_some()
            {
                cut = Some((before, controller.bytes_per_second()));
                break;
            }
        }
        let (before, congested) = cut.expect("a standing queue cuts the rate");
        assert!(congested < before);
        // Repeated readings within one interval must not compound the cut.
        controller.observe(Duration::from_millis(60), now, true);
        assert_eq!(controller.bytes_per_second(), congested);
        now += UPDATE_INTERVAL;
        for _ in 0..CURRENT_SAMPLES {
            controller.observe(Duration::from_millis(10), now, true);
            now += UPDATE_INTERVAL;
        }
        assert!(controller.bytes_per_second() > congested);
    }

    #[test]
    fn a_lone_spike_of_a_wifi_link_is_not_taken_for_queue() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        controller.observe(Duration::from_millis(7), now, true);
        let before = controller.bytes_per_second();
        now += UPDATE_INTERVAL;
        controller.observe(Duration::from_millis(35), now, true);
        assert!(controller.bytes_per_second() > before);
    }

    #[test]
    fn a_round_trip_that_rose_for_good_stops_reading_as_congestion() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        // Someone is at the input: the strict target holds.
        controller.note_input(now);
        controller.observe(Duration::from_millis(2), now, true);
        // The quiet round trip is 30 ms from here on: a roam, another
        // channel. Within the window it reads as queue and is cut ...
        let mut cut = None;
        for _ in 0..=CURRENT_SAMPLES {
            now += UPDATE_INTERVAL;
            if controller
                .observe(Duration::from_millis(30), now, true)
                .is_some()
            {
                cut = Some(controller.bytes_per_second());
            }
        }
        let cut = cut.expect("read as queue while the old baseline holds");
        // ... and once the 2 ms reading has aged out it is the baseline, so
        // the rate grows again instead of staying pinned.
        let mut last = cut;
        for _ in 0..((BASE_WINDOW + BASE_BUCKET * 2).as_millis() / UPDATE_INTERVAL.as_millis()) {
            now += UPDATE_INTERVAL;
            controller.observe(Duration::from_millis(30), now, true);
            last = controller.bytes_per_second();
        }
        assert!(last > cut * 2.0, "{last} after the old baseline aged out");
    }

    #[test]
    fn a_clear_link_is_filled_within_a_second_of_bulk_starting() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        for _ in 0..5 {
            controller.observe(Duration::from_millis(5), now, true);
            now += UPDATE_INTERVAL;
        }
        assert!(controller.bytes_per_second() >= 16.0 * 1024.0 * 1024.0);
    }

    #[test]
    fn after_the_first_queue_the_rate_creeps_rather_than_doubles() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        controller.observe(Duration::from_millis(5), now, true);
        for _ in 0..CURRENT_SAMPLES {
            now += UPDATE_INTERVAL;
            controller.observe(Duration::from_millis(60), now, true);
        }
        now += UPDATE_INTERVAL;
        controller.observe(Duration::from_millis(5), now, true);
        let before = controller.bytes_per_second();
        now += UPDATE_INTERVAL;
        controller.observe(Duration::from_millis(5), now, true);
        assert!(controller.bytes_per_second() < before * 1.2);
    }

    #[test]
    fn the_first_reading_past_half_the_target_ends_the_doubling_without_a_cut() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        // Someone is at the input: the strict target holds.
        controller.note_input(now);
        controller.observe(Duration::from_millis(5), now, true);
        now += UPDATE_INTERVAL;
        let before = controller.bytes_per_second();
        assert!(controller
            .observe(Duration::from_millis(14), now, true)
            .is_none());
        let after = controller.bytes_per_second();
        assert!(
            after > before && after < before * 1.2,
            "{before} -> {after}"
        );
    }

    #[test]
    fn with_nobody_at_the_input_bulk_may_queue_more() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        controller.observe(Duration::from_millis(5), now, true);
        // 30 ms of standing queue: past the strict target, within the idle one.
        for _ in 0..=CURRENT_SAMPLES {
            now += UPDATE_INTERVAL;
            assert!(controller
                .observe(Duration::from_millis(35), now, true)
                .is_none());
        }
        // The same queue once input crosses is cut.
        controller.note_input(now);
        let mut cut = false;
        for _ in 0..=CURRENT_SAMPLES {
            now += UPDATE_INTERVAL;
            cut |= controller
                .observe(Duration::from_millis(35), now, true)
                .is_some();
        }
        assert!(cut);
    }

    #[test]
    fn input_after_a_quiet_spell_halves_bulk_at_once_and_only_once() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        for _ in 0..4 {
            controller.observe(Duration::from_millis(5), now, true);
            now += UPDATE_INTERVAL;
        }
        let before = controller.bytes_per_second();
        controller.note_input(now);
        assert_eq!(controller.bytes_per_second(), before * 0.5);
        now += Duration::from_millis(8);
        controller.note_input(now);
        assert_eq!(controller.bytes_per_second(), before * 0.5);
    }
}
