//! Rational sample positions retained across capture and playback packets,
//! and the source clock a capture stream stamps its chunks with.
use std::time::{Duration, Instant};

/// Wire rate the capture timestamps count in.
const WIRE_RATE: f64 = 48_000.0;

/// How long a source may go without delivering, beyond the audio it last
/// delivered, before the time is counted as the source not running.
///
/// Erring short is harmless: a timestamp that jumps while packets keep arriving
/// on time reads at the receiver as audio that is not late at all. Erring long
/// is what this exists to stop.
const PAUSE: Duration = Duration::from_millis(100);

/// A capture stream's timestamp: its own sample count while the source runs,
/// advanced by the wall-clock time it spent not running.
///
/// The receiver sizes its playout reserve from how late each chunk is against
/// this clock. A plain sample count stands still while an application is
/// paused — PipeWire stops calling a capture whose source is corked, and a
/// WASAPI process loopback stops delivering packets — so the first chunk after
/// a pause or a seek arrived "late" by the whole pause. The receiver took that
/// for a network stall, raised its reserve to the ceiling and held it there
/// for minutes, which is where the next ordinary burst of arrivals was cut.
#[derive(Debug, Default)]
pub(crate) struct SourceClock {
    next: u64,
    last: Option<(Instant, u64)>,
}

impl SourceClock {
    /// The timestamp of `frames` wire frames delivered at `now`.
    pub(crate) fn stamp(&mut self, now: Instant, frames: u64) -> u64 {
        if let Some((at, delivered)) = self.last {
            let idle = now
                .saturating_duration_since(at)
                .saturating_sub(Duration::from_secs_f64(delivered as f64 / WIRE_RATE));
            if idle > PAUSE {
                self.next = self
                    .next
                    .wrapping_add((idle.as_secs_f64() * WIRE_RATE) as u64);
            }
        }
        self.last = Some((now, frames));
        let timestamp = self.next;
        self.next = self.next.wrapping_add(frames);
        timestamp
    }
}

/// Return the output frame count and first source position, in target-rate units.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn layout(
    frames: usize,
    source_rate: u32,
    target_rate: u32,
    phase: &mut u64,
) -> (usize, u64) {
    let source_rate = source_rate.max(1) as u64;
    let end = frames as u64 * target_rate.max(1) as u64;
    let first = *phase;
    let count = end.saturating_sub(first).div_ceil(source_rate);
    *phase = first + count * source_rate - end;
    (count as usize, first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_source_is_stamped_by_its_own_sample_count() {
        let start = Instant::now();
        let mut clock = SourceClock::default();
        // Delivered in irregular bursts, as a capture loop that wakes late does.
        let stamps: Vec<u64> = [0u64, 0, 30, 30, 31, 90]
            .iter()
            .map(|ms| clock.stamp(start + Duration::from_millis(*ms), 480))
            .collect();
        assert_eq!(stamps, [0, 480, 960, 1440, 1920, 2400]);
    }

    #[test]
    fn a_paused_source_resumes_with_the_time_it_was_paused_for() {
        let start = Instant::now();
        let mut clock = SourceClock::default();
        clock.stamp(start, 480);
        let resumed = clock.stamp(start + Duration::from_secs(54), 480);
        let expected = 54 * 48_000;
        assert!(
            resumed.abs_diff(expected) <= 1,
            "a pause must not read as a chunk delivered 54 s late: {resumed}"
        );
        assert_eq!(
            clock.stamp(start + Duration::from_millis(54_010), 480),
            resumed + 480,
            "once running again the sample count is exact"
        );
    }

    #[test]
    fn repeated_five_millisecond_packets_do_not_lose_half_a_sample_each() {
        let mut phase = 0;
        let count: usize = (0..200)
            .map(|_| layout(240, 48000, 44100, &mut phase).0)
            .sum();
        assert_eq!(count, 44100);
        assert_eq!(phase, 0);
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_source_sample_positions() {
        for (source, target) in [(48000u32, 44100u32), (44100, 48000), (48000, 96000)] {
            let (mut phase, mut input, mut output) = (0, 0u64, 0u64);
            for size in [1, 127, 13, 240, 1024, 1, 256] {
                let (count, first) = layout(size, source, target, &mut phase);
                for i in 0..count as u64 {
                    let actual = input + (first + i * source as u64) / target as u64;
                    let expected = output * source as u64 / target as u64;
                    assert_eq!(actual, expected);
                    output += 1;
                }
                input += size as u64;
            }
            assert!(phase < source as u64);
        }
    }
}
