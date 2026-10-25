//! Shared budget for every video capture on a peer link.
use std::time::{Duration, Instant};

const INITIAL: u32 = 12_000_000;
const MIN: u32 = 256_000;
const MAX: u32 = 200_000_000;
const SAMPLE: Duration = Duration::from_millis(500);
pub(crate) const TARGET_QUEUE: Duration = Duration::from_millis(75);

pub(crate) struct VideoBudget {
    bits_per_second: u32,
    sample: Option<(Instant, u64)>,
    congested_until: Option<Instant>,
}

impl Default for VideoBudget {
    fn default() -> Self {
        Self {
            bits_per_second: INITIAL,
            sample: None,
            congested_until: None,
        }
    }
}

impl VideoBudget {
    pub(crate) fn congested(&self, now: Instant) -> bool {
        self.congested_until.is_some_and(|until| now < until)
    }

    pub(crate) fn observe(&mut self, sent: u64, age: Duration, active: bool, now: Instant) -> u32 {
        if age > TARGET_QUEUE {
            self.congested_until = Some(now + Duration::from_secs(4));
        }
        let Some((at, previous)) = self.sample else {
            self.sample = Some((now, sent));
            return self.bits_per_second;
        };
        let elapsed = now.saturating_duration_since(at);
        if elapsed < SAMPLE {
            return self.bits_per_second;
        }
        self.sample = Some((now, sent));
        let moved = sent.saturating_sub(previous);
        if age > TARGET_QUEUE {
            let delivered = (moved as f64 * 8.0 / elapsed.as_secs_f64()) as u32;
            self.bits_per_second = (self.bits_per_second * 7 / 10)
                .min(delivered.saturating_mul(9) / 10)
                .max(MIN);
        } else if active && moved > 0 {
            self.bits_per_second = (self.bits_per_second.saturating_mul(11) / 10).min(MAX);
        }
        self.bits_per_second
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn congestion_cuts_the_shared_budget_and_idle_does_not_raise_it() {
        let mut budget = VideoBudget::default();
        let now = Instant::now();
        budget.observe(0, Duration::ZERO, true, now);
        let reduced = budget.observe(100_000, TARGET_QUEUE * 2, true, now + SAMPLE);
        assert!(reduced < INITIAL);
        assert_eq!(
            budget.observe(100_000, Duration::ZERO, false, now + SAMPLE * 2),
            reduced
        );
        assert!(budget.observe(200_000, Duration::ZERO, true, now + SAMPLE * 3) > reduced);
    }
}
