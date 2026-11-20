//! What the engine says about itself every half minute, and when a quiet
//! stream counts as stalled.

use crate::audio_session::AudioEngine;

/// How often the periodic audio health line is emitted.
///
/// Deliberately time-based, not chunk-based. Logging every N chunks scales with
/// throughput, so at ~100 chunks/s the file filled with thousands of identical
/// counters and buried the events that explain a problem.
pub(super) const HEALTH_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long without a chunk from the peer counts as a stall worth warning about.
///
/// Well above any normal jitter (chunks arrive every ~10 ms), so this only fires
/// when audio has genuinely stopped rather than merely hiccuped. Also well above
/// [`IDLE_ANNOUNCE_AFTER`](super::send::IDLE_ANNOUNCE_AFTER), so a source that
/// merely went quiet is announced idle before it could be counted here.
pub(super) const AUDIO_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

/// The inbound streams that have gone quiet past [`AUDIO_STALL_AFTER`] without
/// the peer saying so, and were not reported yet, with how long each has been
/// quiet.
///
/// Per stream rather than "any chunk at all": the peer's other streams keep
/// arriving while one is cut off, and one stream's chunks used to hide that.
fn newly_stalled(
    now: std::time::Instant,
    last_arrival: &std::collections::HashMap<u32, (std::time::Instant, u64)>,
    peer_idle: &std::collections::HashSet<u32>,
    reported: &std::collections::HashSet<u32>,
    listening: impl Fn(u32) -> bool,
) -> Vec<(u32, std::time::Duration)> {
    let mut stalled: Vec<_> = last_arrival
        .iter()
        .filter(|(id, _)| !peer_idle.contains(id) && !reported.contains(id))
        .map(|(&id, &(at, _))| (id, now.saturating_duration_since(at)))
        .filter(|&(id, gap)| gap >= AUDIO_STALL_AFTER && listening(id))
        .collect();
    stalled.sort_unstable();
    stalled
}

impl AudioEngine {
    /// Emit a throughput summary at most once per [`HEALTH_LOG_INTERVAL`], and
    /// warn when audio from the peer has stopped arriving.
    ///
    /// Both live here so a quiet link is visible in the log: silence used to be
    /// indistinguishable from "nothing to report", which is exactly the case
    /// where you most want a line.
    pub(super) fn log_health(&mut self) {
        let now = std::time::Instant::now();

        // Stall detection: audio was flowing, then stopped without the peer
        // saying its source went quiet.
        //
        // Only while something here is actually listening. A stream nobody
        // plays is refused on purpose (see `unwanted_streams`), and the peer
        // then correctly sends nothing — so after that refusal silence is the
        // system working, not a fault. Warning about it would train the reader
        // to ignore the one line that means a real stall.
        let playbacks = &self.playbacks;
        let stalled = newly_stalled(
            now,
            &self.recv_timing,
            &self.peer_idle,
            &self.stalled,
            |id| playbacks.get(&id).is_some_and(|p| p.is_listening()),
        );
        if let Some(&(_, gap)) = stalled.iter().max_by_key(|&&(_, gap)| gap) {
            let ids: Vec<u32> = stalled.iter().map(|&(id, _)| id).collect();
            self.stalled.extend(ids.iter().copied());
            tracing::warn!(
                streams = ?ids,
                silent_for_ms = gap.as_millis() as u64,
                played = self.played,
                sent = self.sent,
                "audio from the peer stalled — no chunks arriving"
            );
        }

        let due = self
            .last_health_log
            .is_none_or(|t| now.duration_since(t) >= HEALTH_LOG_INTERVAL);
        if !due {
            return;
        }
        self.last_health_log = Some(now);

        tracing::info!(
            sent = self.sent,
            played = self.played,
            silent_dropped = self.silent_dropped,
            unwanted_dropped = self.unwanted_dropped,
            over_budget = self.over_budget,
            stale_dropped = self.stale_dropped,
            streams = self.playbacks.len(),
            peer = %self.peer_name,
            max_arrival_gap_ms = self.max_arrival_gap.as_secs_f64() * 1000.0,
            max_arrival_lateness_ms = self.max_arrival_lateness.as_secs_f64() * 1000.0,
            max_decode_ms = self.max_decode_time.as_secs_f64() * 1000.0,
            capture_seq_skipped = self.capture_seq_skipped,
            "audio health"
        );

        self.max_arrival_gap = std::time::Duration::ZERO;
        self.max_arrival_lateness = std::time::Duration::ZERO;
        self.max_decode_time = std::time::Duration::ZERO;
        self.capture_seq_skipped = 0;

        // The automatic threshold is invisible otherwise, and it is the first
        // thing to look at when a peer says this mic cuts out.
        let mode = self.mic_gate.get();
        for id in self.mic_captures.keys() {
            if let Some(gate) = self.gates.get(id) {
                tracing::info!(
                    id,
                    ?mode,
                    floor_dbfs = gate.floor_db(),
                    auto_threshold_dbfs = gate.auto_threshold(),
                    "mic gate"
                );
            }
        }
        if self.mic_gate_logged != Some(mode) {
            self.mic_gate_logged = Some(mode);
            tracing::info!(?mode, "microphones gated");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    fn arrivals(now: Instant, quiet_for: &[(u32, Duration)]) -> HashMap<u32, (Instant, u64)> {
        quiet_for
            .iter()
            .map(|&(id, quiet)| (id, (now - quiet, 0)))
            .collect()
    }

    /// The case the warning exists for: sound was arriving, then stopped with
    /// no word from the peer — the link or the sender broke.
    #[test]
    fn a_stream_that_stops_without_a_word_is_a_stall() {
        let now = Instant::now();
        let quiet = AUDIO_STALL_AFTER + Duration::from_millis(1);
        let stalled = newly_stalled(
            now,
            &arrivals(now, &[(7, quiet)]),
            &HashSet::new(),
            &HashSet::new(),
            |_| true,
        );
        assert_eq!(stalled, vec![(7, quiet)]);
    }

    /// A paused song on the peer: its sender said so, and it is not a fault.
    #[test]
    fn a_stream_the_peer_announced_idle_is_not_a_stall() {
        let now = Instant::now();
        let stalled = newly_stalled(
            now,
            &arrivals(now, &[(7, Duration::from_secs(60))]),
            &HashSet::from([7]),
            &HashSet::new(),
            |_| true,
        );
        assert!(stalled.is_empty());
    }

    /// Another stream still flowing must not hide one that was cut off: a
    /// single "last chunk from anyone" clock could not see that.
    #[test]
    fn one_flowing_stream_does_not_hide_another_that_stalled() {
        let now = Instant::now();
        let stalled = newly_stalled(
            now,
            &arrivals(
                now,
                &[(1, Duration::from_millis(5)), (2, Duration::from_secs(3))],
            ),
            &HashSet::new(),
            &HashSet::new(),
            |_| true,
        );
        assert_eq!(stalled.iter().map(|&(id, _)| id).collect::<Vec<_>>(), [2]);
    }

    /// Not reported while nothing here plays the stream, nor twice, nor
    /// before the gap is longer than ordinary jitter.
    #[test]
    fn quiet_is_not_reported_unheard_twice_or_early() {
        let now = Instant::now();
        let long = Duration::from_secs(5);
        let quiet = arrivals(now, &[(1, long), (2, long), (3, AUDIO_STALL_AFTER / 2)]);
        let stalled = newly_stalled(now, &quiet, &HashSet::new(), &HashSet::from([2]), |id| {
            id != 1
        });
        assert!(stalled.is_empty(), "{stalled:?}");
    }
}
