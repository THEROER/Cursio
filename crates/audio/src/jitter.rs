//! Adaptive playout reserve and conservative, correlation-gated catch-up.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(crate) struct Delay {
    rate: usize,
    base: usize,
    ceiling: usize,
    target: f64,
    previous: Option<(Instant, usize, Option<u64>)>,
    disturbed: Option<Instant>,
}

impl Delay {
    pub fn new(rate: usize, base: usize, ceiling: usize) -> Self {
        Self {
            rate,
            base,
            ceiling: ceiling.max(base),
            target: base as f64,
            previous: None,
            disturbed: None,
        }
    }

    pub fn arrival(&mut self, at: Instant, frames: usize, timestamp: Option<u64>) {
        if let Some((previous, duration, previous_timestamp)) = self.previous {
            let gap = at.saturating_duration_since(previous);
            let expected = match (timestamp, previous_timestamp) {
                (Some(current), Some(previous)) if current > previous => {
                    (current - previous) as f64 * self.rate as f64 / 48000.0
                }
                _ => duration as f64,
            };
            let late = (gap.as_secs_f64() * self.rate as f64 - expected).max(0.0);
            let reset = matches!((timestamp, previous_timestamp), (Some(current), Some(previous)) if current < previous);
            let quiet =
                gap > Duration::from_secs(1) && (timestamp.is_none() || late <= self.base as f64);
            if reset || quiet {
                self.target = self.base as f64;
                self.disturbed = None;
            } else {
                if late > self.base as f64 {
                    self.disturbed = Some(at);
                }
                let needed = (late + self.rate as f64 * 0.010)
                    .max(self.base as f64)
                    .min(self.ceiling as f64);
                if needed > self.target + (self.rate / 200) as f64 {
                    self.target = needed;
                    self.disturbed = Some(at);
                } else if self.stable(at) {
                    // Two milliseconds per second; never collapse a reserve after a burst.
                    self.target = (self.target - gap.as_secs_f64() * self.rate as f64 * 0.002)
                        .max(self.base as f64);
                }
            }
        }
        self.previous = Some((at, frames, timestamp));
    }

    pub fn frames(&self) -> usize {
        self.target.ceil() as usize
    }

    pub fn stable(&self, now: Instant) -> bool {
        self.disturbed
            .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(30))
    }
}

#[derive(Default)]
pub(crate) struct Recovery {
    credit: f64,
    searched: Option<Instant>,
}

impl Recovery {
    /// Remove correlated pitch periods with an overlap, at most 0.5% of playback.
    /// Aperiodic material is left alone rather than forcing a poor match.
    pub fn process(
        &mut self,
        queue: &mut VecDeque<u8>,
        channels: usize,
        delay: &Delay,
        reserve: usize,
        played: usize,
        now: Instant,
    ) -> usize {
        if channels == 0 || !delay.stable(now) {
            self.credit = 0.0;
            return 0;
        }
        let frame = channels * 4;
        let spare =
            (queue.len() / frame).saturating_sub(reserve + delay.frames() + delay.rate / 200);
        if spare == 0 {
            self.credit = 0.0;
            return 0;
        }
        self.credit = (self.credit + played as f64 * 0.005).min((delay.rate / 100) as f64);
        let minimum = (delay.rate / 1000).max(1);
        let maximum = (self.credit as usize).min(spare);
        if maximum < minimum
            || self
                .searched
                .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_millis(100))
        {
            return 0;
        }
        self.searched = Some(now);
        let window = (delay.rate / 200).max(1);
        if queue.len() / frame < maximum + window {
            return 0;
        }
        let sample = |index: usize| -> f64 {
            let at = index * 4;
            f32::from_le_bytes([queue[at], queue[at + 1], queue[at + 2], queue[at + 3]]) as f64
        };
        let score = |lag: usize| {
            let (mut cross, mut a, mut b) = (0.0, 0.0, 0.0);
            for i in (0..window).step_by(4) {
                for channel in 0..channels {
                    let x = sample(i * channels + channel);
                    let y = sample((i + lag) * channels + channel);
                    cross += x * y;
                    a += x * x;
                    b += y * y;
                }
            }
            if a + b < 1e-12 {
                1.0
            } else {
                cross / (a * b).sqrt().max(1e-30)
            }
        };
        let step = (delay.rate / 8000).max(1);
        let mut best = (0, 0.98);
        for lag in (minimum..=maximum).step_by(step) {
            let correlation = score(lag);
            if correlation > best.1 {
                best = (lag, correlation);
            }
        }
        if best.0 == 0 {
            return 0;
        }
        for lag in best.0.saturating_sub(step).max(minimum)..=(best.0 + step).min(maximum) {
            let correlation = score(lag);
            if correlation > best.1 {
                best = (lag, correlation);
            }
        }
        let bytes = best.0 * frame;
        crate::declick::skip_crossfaded(queue, bytes, channels, window);
        self.credit -= best.0 as f64;
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_cannot_immediately_destroy_the_learned_reserve() {
        let start = Instant::now();
        let mut delay = Delay::new(48000, 1920, 9600);
        delay.arrival(start, 240, None);
        delay.arrival(start + Duration::from_millis(250), 240, None);
        assert_eq!(delay.frames(), 9600);
        for i in 0..1000 {
            delay.arrival(start + Duration::from_millis(250 + i * 5), 240, None);
        }
        assert_eq!(delay.frames(), 9600);
        assert!(!delay.stable(start + Duration::from_secs(20)));
        delay.arrival(start + Duration::from_millis(5360), 240, None);
        assert!(
            !delay.stable(start + Duration::from_secs(32)),
            "a second spike renews the recovery hold even at the ceiling"
        );
    }

    #[test]
    fn source_clock_gaps_from_a_silence_gate_do_not_raise_the_network_reserve() {
        let start = Instant::now();
        let mut delay = Delay::new(48000, 1920, 9600);
        delay.arrival(start, 240, Some(0));
        delay.arrival(start + Duration::from_millis(150), 240, Some(7200));
        assert_eq!(delay.frames(), 1920);
        delay.arrival(start + Duration::from_millis(330), 240, Some(7440));
        assert!((175 * 48..=200 * 48).contains(&delay.frames()));
    }

    #[test]
    fn long_network_stalls_are_distinguished_from_intentional_silence_and_clock_resets() {
        let start = Instant::now();
        let mut delay = Delay::new(48000, 1920, 9600);
        delay.arrival(start, 240, Some(10000));
        delay.arrival(start + Duration::from_secs(2), 240, Some(10240));
        assert_eq!(delay.frames(), 9600);
        delay.arrival(start + Duration::from_secs(4), 240, Some(106240));
        assert_eq!(delay.frames(), 1920);
        delay.arrival(start + Duration::from_millis(4100), 240, Some(0));
        assert_eq!(delay.frames(), 1920);
    }

    #[test]
    fn quiet_streams_reset_and_stable_streams_lower_the_target_slowly() {
        let start = Instant::now();
        let mut delay = Delay::new(1000, 40, 200);
        delay.arrival(start, 5, None);
        delay.arrival(start + Duration::from_millis(150), 5, None);
        let raised = delay.frames();
        for i in 1..10000 {
            delay.arrival(start + Duration::from_millis(150 + i * 5), 5, None);
        }
        assert!(delay.frames() < raised && delay.frames() > 100);
        delay.arrival(start + Duration::from_secs(60), 5, None);
        assert_eq!(delay.frames(), 40);
    }

    #[test]
    fn correlated_catch_up_preserves_channels_and_the_waveform_with_a_rate_limit() {
        let now = Instant::now();
        let delay = Delay::new(48000, 1920, 9600);
        let input: Vec<f32> = (0..9600)
            .flat_map(|n| {
                let x = (n as f32 * std::f32::consts::TAU / 120.0).sin();
                [x, -x]
            })
            .collect();
        let mut queue: VecDeque<u8> = input.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut recovery = Recovery::default();
        let mut removed = 0;
        for n in 0..200 {
            removed += recovery.process(
                &mut queue,
                2,
                &delay,
                240,
                240,
                now + Duration::from_millis(n * 5),
            );
        }
        assert!(removed > 0 && removed <= 48000 * 8 / 200);
        let samples: Vec<f32> = queue
            .make_contiguous()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        for pair in samples.as_chunks::<2>().0 {
            assert!((pair[0] + pair[1]).abs() < 1e-6);
        }
        for pair in samples.as_chunks::<2>().0.windows(2) {
            assert!((pair[0][0] - pair[1][0]).abs() < 0.08);
        }
    }

    #[test]
    fn noisy_or_insufficient_audio_is_never_forced_through_a_bad_overlap() {
        let now = Instant::now();
        let delay = Delay::new(48000, 1920, 9600);
        let mut seed = 17u32;
        let mut queue: VecDeque<u8> = (0..9600)
            .flat_map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 - 0.5).to_le_bytes()
            })
            .collect();
        let original = queue.clone();
        let mut recovery = Recovery::default();
        for n in 0..200 {
            recovery.process(
                &mut queue,
                1,
                &delay,
                240,
                240,
                now + Duration::from_millis(n * 5),
            );
        }
        assert_eq!(queue, original);
    }
}
