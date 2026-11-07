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
use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;

const INITIAL_RATE: f64 = 1024.0 * 1024.0;
const MIN_RATE: f64 = 64.0 * 1024.0;
const MAX_RATE: f64 = 128.0 * 1024.0 * 1024.0;
const UPDATE_INTERVAL: Duration = Duration::from_millis(200);
const QUEUE_TARGET: Duration = Duration::from_millis(15);
/// How far back the baseline round trip reaches. Long enough that a transfer
/// running throughout still had moments of an empty queue in it — each cut
/// drains what bulk built — so its own queue does not become the baseline;
/// short enough that a link whose quiet round trip rose is believed within
/// minutes.
const BASE_WINDOW: Duration = Duration::from_secs(180);
/// One bucket of [`BASE_WINDOW`]: its lowest round trip is all that is kept.
const BASE_BUCKET: Duration = Duration::from_secs(10);

pub(crate) struct BackgroundRate {
    rate: f64,
    /// Lowest round trip of each recent bucket, oldest first, with when the
    /// bucket began.
    minima: VecDeque<(Instant, Duration)>,
    updated: Option<Instant>,
}

impl Default for BackgroundRate {
    fn default() -> Self {
        Self {
            rate: INITIAL_RATE,
            minima: VecDeque::new(),
            updated: None,
        }
    }
}

impl BackgroundRate {
    pub(crate) fn bytes_per_second(&self) -> f64 {
        self.rate
    }

    pub(crate) fn observe(&mut self, rtt: Duration, now: Instant, active: bool) {
        let baseline = self.baseline(rtt, now);
        if !active {
            self.rate = self.rate.min(INITIAL_RATE);
            self.updated = None;
            return;
        }
        if self
            .updated
            .is_some_and(|at| now.duration_since(at) < UPDATE_INTERVAL)
        {
            return;
        }
        self.updated = Some(now);
        if rtt.saturating_sub(baseline) > QUEUE_TARGET {
            self.rate = (self.rate * 0.7).max(MIN_RATE);
        } else {
            self.rate = (self.rate * 1.1 + 16_384.0).min(MAX_RATE);
        }
    }
}

impl BackgroundRate {
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
    fn background_yields_to_queue_growth_and_recovers_gradually() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        controller.observe(Duration::from_millis(10), now, true);
        let clear = controller.bytes_per_second();
        now += UPDATE_INTERVAL;
        controller.observe(Duration::from_millis(60), now, true);
        let congested = controller.bytes_per_second();
        assert!(congested < clear);
        // Repeated readings within one interval must not compound the cut.
        controller.observe(Duration::from_millis(60), now, true);
        assert_eq!(controller.bytes_per_second(), congested);
        now += UPDATE_INTERVAL;
        controller.observe(Duration::from_millis(10), now, true);
        assert!(controller.bytes_per_second() > congested);
        assert!(controller.bytes_per_second() < clear);
    }

    #[test]
    fn a_round_trip_that_rose_for_good_stops_reading_as_congestion() {
        let mut controller = BackgroundRate::default();
        let mut now = Instant::now();
        controller.observe(Duration::from_millis(2), now, true);
        // The quiet round trip is 30 ms from here on: a roam, another
        // channel. Within the window it reads as queue and is cut ...
        now += UPDATE_INTERVAL;
        controller.observe(Duration::from_millis(30), now, true);
        let cut = controller.bytes_per_second();
        assert!(cut < INITIAL_RATE);
        // ... and once the 2 ms reading has aged out it is the baseline, so
        // the rate grows again instead of staying pinned.
        let mut last = cut;
        for _ in 0..((BASE_WINDOW + BASE_BUCKET * 2).as_millis() / UPDATE_INTERVAL.as_millis()) {
            now += UPDATE_INTERVAL;
            controller.observe(Duration::from_millis(30), now, true);
            last = controller.bytes_per_second();
        }
        assert!(
            last > INITIAL_RATE,
            "{last} after the old baseline aged out"
        );
    }
}
