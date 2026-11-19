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
/// when audio has genuinely stopped rather than merely hiccuped.
pub(super) const AUDIO_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

impl AudioEngine {
    /// Emit a throughput summary at most once per [`HEALTH_LOG_INTERVAL`], and
    /// warn when audio from the peer has stopped arriving.
    ///
    /// Both live here so a quiet link is visible in the log: silence used to be
    /// indistinguishable from "nothing to report", which is exactly the case
    /// where you most want a line.
    pub(super) fn log_health(&mut self) {
        let now = std::time::Instant::now();

        // Stall detection: audio was flowing, then stopped.
        //
        // Only while something here is actually listening. A stream nobody
        // plays is refused on purpose (see `unwanted_streams`), and the peer
        // then correctly sends nothing — so after that refusal silence is the
        // system working, not a fault. Warning about it would train the reader
        // to ignore the one line that means a real stall.
        let anyone_listening = self.playbacks.values().any(|p| p.is_listening());
        if let Some(last) = self.last_chunk_at.filter(|_| anyone_listening) {
            let gap = now.duration_since(last);
            if gap >= AUDIO_STALL_AFTER && !self.stall_reported {
                self.stall_reported = true;
                tracing::warn!(
                    silent_for_ms = gap.as_millis() as u64,
                    played = self.played,
                    sent = self.sent,
                    "audio from the peer stalled — no chunks arriving"
                );
            }
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
