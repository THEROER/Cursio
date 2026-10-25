//! Session-wide background pacing driven by interactive delivery latency.
use std::time::Duration;
use tokio::time::Instant;

const INITIAL_RATE: f64 = 1024.0 * 1024.0;
const MIN_RATE: f64 = 64.0 * 1024.0;
const MAX_RATE: f64 = 128.0 * 1024.0 * 1024.0;
const UPDATE_INTERVAL: Duration = Duration::from_millis(200);
const QUEUE_TARGET: Duration = Duration::from_millis(15);

pub(crate) struct BackgroundRate {
    rate: f64,
    baseline: Option<Duration>,
    updated: Option<Instant>,
}

impl Default for BackgroundRate {
    fn default() -> Self {
        Self {
            rate: INITIAL_RATE,
            baseline: None,
            updated: None,
        }
    }
}

impl BackgroundRate {
    pub(crate) fn bytes_per_second(&self) -> f64 {
        self.rate
    }

    pub(crate) fn observe(&mut self, rtt: Duration, now: Instant, active: bool) {
        let baseline = self.baseline.map_or(rtt, |old| old.min(rtt));
        self.baseline = Some(baseline);
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
}
