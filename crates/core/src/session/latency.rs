//! How long input waits, measured where it waits: windows of samples
//! reported as a median and a tail, and round trips over the session.

use cursio_protocol::PeerId;
use std::time::Duration;

/// How often the input latency figures are logged, when there were any.
pub(super) const INPUT_LATENCY_WINDOW: Duration = Duration::from_secs(30);

/// A p99 at or above this is logged at `warn`: a pointer that lags by this
/// much in one frame out of a hundred is visibly trailing the hand.
pub(super) const INPUT_LATENCY_WARN: Duration = Duration::from_millis(20);

/// Samples kept per window. Beyond this the oldest are overwritten, which
/// keeps a gaming mouse's thousand events a second from growing memory while
/// still leaving thousands of samples behind each percentile.
pub(super) const INPUT_LATENCY_SAMPLES: usize = 8192;

/// Input latency over one window, reported as percentiles.
///
/// Two of these per session, one per direction: from the reader taking an
/// `Input` off the wire to injecting it, and from capture handing an event
/// over to the transport accepting it. Both measure exactly the stretch the
/// session loop controls, which is where every lag this loop ever caused
/// lived.
pub(crate) struct LatencyWindow {
    what: &'static str,
    samples: Vec<Duration>,
    next: usize,
    total: u64,
    started: tokio::time::Instant,
}

impl LatencyWindow {
    pub(crate) fn new(what: &'static str) -> Self {
        Self {
            what,
            samples: Vec::new(),
            next: 0,
            total: 0,
            started: tokio::time::Instant::now(),
        }
    }

    pub(crate) fn record(&mut self, latency: Duration) {
        if self.samples.len() < INPUT_LATENCY_SAMPLES {
            self.samples.push(latency);
        } else {
            self.samples[self.next] = latency;
            self.next = (self.next + 1) % INPUT_LATENCY_SAMPLES;
        }
        self.total += 1;
    }

    /// `(p50, p99, max)` of the samples held, or `None` with none.
    pub(crate) fn percentiles(&self) -> Option<(Duration, Duration, Duration)> {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let last = *sorted.last()?;
        let at = |fraction: f64| {
            let idx = ((sorted.len() as f64 * fraction).ceil() as usize).clamp(1, sorted.len());
            sorted[idx - 1]
        };
        Some((at(0.50), at(0.99), last))
    }

    /// Log and restart the window once it has run its length. A window with
    /// no input says nothing and logs nothing, so an idle link stays quiet.
    pub(super) fn report_if_due(&mut self) {
        if self.started.elapsed() < INPUT_LATENCY_WINDOW {
            return;
        }
        if let Some((p50, p99, max)) = self.percentiles() {
            let (p50_us, p99_us, max_us) = (
                p50.as_micros() as u64,
                p99.as_micros() as u64,
                max.as_micros() as u64,
            );
            if p99 >= INPUT_LATENCY_WARN {
                tracing::warn!(
                    path = self.what,
                    events = self.total,
                    p50_us,
                    p99_us,
                    max_us,
                    "input latency is high enough to feel"
                );
            } else {
                tracing::info!(
                    path = self.what,
                    events = self.total,
                    p50_us,
                    p99_us,
                    max_us,
                    "input latency"
                );
            }
        }
        *self = Self::new(self.what);
    }
}

/// Round trips over the actual interactive session, including both endpoint loops.
/// This is not a one-way input-to-injection measurement.
pub(super) struct InteractiveProbe {
    peer: Option<PeerId>,
    pending: std::collections::VecDeque<(u64, std::time::Instant)>,
    latency: LatencyWindow,
}

impl InteractiveProbe {
    pub(super) fn new(peer: Option<PeerId>) -> Self {
        Self {
            peer,
            pending: std::collections::VecDeque::new(),
            latency: LatencyWindow::new("interactive RTT"),
        }
    }

    pub(super) fn sent(&mut self, nonce: u64, at: std::time::Instant) {
        if self.pending.len() == 8 {
            self.pending.pop_front();
        }
        self.pending.push_back((nonce, at));
    }

    pub(super) fn echo(&mut self, nonce: u64, now: std::time::Instant) {
        let nonce = nonce & !cursio_protocol::HEARTBEAT_ECHO;
        if let Some(index) = self.pending.iter().position(|(id, _)| *id == nonce) {
            let (_, sent) = self.pending.remove(index).unwrap();
            self.latency.record(now.saturating_duration_since(sent));
        }
    }

    pub(super) fn report(&mut self) {
        if self.latency.started.elapsed() < INPUT_LATENCY_WINDOW {
            return;
        }
        if let Some((p50, p99, maximum)) = self.latency.percentiles() {
            tracing::info!(
                peer = ?self.peer,
                samples = self.latency.total,
                p50_ms = p50.as_secs_f64() * 1000.0,
                p99_ms = p99.as_secs_f64() * 1000.0,
                max_ms = maximum.as_secs_f64() * 1000.0,
                outstanding = self.pending.len(),
                "interactive round trip"
            );
        }
        self.latency = LatencyWindow::new("interactive RTT");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_probes_match_echoes_once_and_bound_unanswered_requests() {
        let start = std::time::Instant::now();
        let mut probe = InteractiveProbe::new(None);
        for nonce in 0..100 {
            probe.sent(nonce, start);
        }
        assert_eq!(probe.pending.len(), 8);
        probe.echo(
            cursio_protocol::HEARTBEAT_ECHO | 1,
            start + Duration::from_millis(99),
        );
        assert_eq!(probe.latency.total, 0);
        probe.echo(
            cursio_protocol::HEARTBEAT_ECHO | 99,
            start + Duration::from_millis(12),
        );
        probe.echo(
            cursio_protocol::HEARTBEAT_ECHO | 99,
            start + Duration::from_millis(30),
        );
        assert_eq!(probe.latency.total, 1);
        assert_eq!(
            probe.latency.percentiles().unwrap().2,
            Duration::from_millis(12)
        );
    }

    /// The figures a lag report quotes are the real percentiles of what was
    /// recorded: a single slow event shows as the max, not as the typical case.
    #[test]
    fn input_latency_reports_the_median_and_the_tail_separately() {
        let mut window = LatencyWindow::new("test");
        assert_eq!(window.percentiles(), None, "no input, nothing to say");
        for _ in 0..99 {
            window.record(Duration::from_micros(200));
        }
        window.record(Duration::from_millis(40));
        let (p50, p99, max) = window.percentiles().unwrap();
        assert_eq!(p50, Duration::from_micros(200));
        assert_eq!(p99, Duration::from_micros(200));
        assert_eq!(max, Duration::from_millis(40));
    }

    /// A flood of events keeps a bounded window of the most recent ones, so an
    /// hour of gaming does not grow memory and the report reflects now.
    #[test]
    fn input_latency_keeps_only_the_most_recent_samples() {
        let mut window = LatencyWindow::new("test");
        for _ in 0..INPUT_LATENCY_SAMPLES {
            window.record(Duration::from_millis(30));
        }
        for _ in 0..INPUT_LATENCY_SAMPLES {
            window.record(Duration::from_micros(100));
        }
        assert_eq!(window.samples.len(), INPUT_LATENCY_SAMPLES);
        let (_, p99, max) = window.percentiles().unwrap();
        assert_eq!(p99, Duration::from_micros(100));
        assert_eq!(max, Duration::from_micros(100));
    }
}
