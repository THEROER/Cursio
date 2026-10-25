//! What the module says about itself: serving and round-trip times every
//! half minute, and a disk walk named by the process doing it.

use crate::files_session::FilesEngine;
use cursio_protocol::FileReqId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long one of the peer's requests may take before it is named in the log.
///
/// Set below the mount's own deadline (`cursio_files::bridge::OP_TIMEOUT`, 3 s)
/// so a request that is *about* to be given up on is reported before the
/// application sees a device error rather than after. Well above an ordinary
/// disk operation, which is sub-millisecond.
pub(super) const SLOW_SERVE_WARN: Duration = Duration::from_millis(1500);

/// How often the file module reports what the link is doing.
///
/// Matched to the audio module's own health line, so a log read after the fact
/// interleaves the two at the same cadence.
pub(super) const HEALTH_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Whether a refusal from the owner is a fault rather than an answer about
/// the filesystem as it stands. See [`Health::refused`].
pub(super) fn is_fault(error: &cursio_protocol::FileError) -> bool {
    use cursio_protocol::FileError;
    matches!(error, FileError::Io(_) | FileError::BadHandle)
}

/// What the link is measured to be doing, as opposed to what it is asked to do.
///
/// Every field here answers a question that cost real time to answer by hand:
///
/// - *Is the network slow, or are we?* — `rate_kb_s` against `rtt_ms`. A link
///   with a short round trip and a low rate is not a slow network.
/// - *Is the pacer helping?* — `window`, `cuts`, `grows`. A window sitting at
///   the floor while the rate is high is the controller fighting the link.
/// - *Where is the time going?* — `rtt_ms` against the size of what is being
///   moved. Small operations are bounded by round trips, large ones by rate,
///   and the fix is different for each.
#[derive(Default)]
pub(super) struct Health {
    /// When the last report went out.
    last_log: Option<Instant>,
    /// Round trips measured since the last report: total and count, so the mean
    /// can be reported without holding every sample.
    rtt_total: Duration,
    rtt_count: u64,
    /// The slowest single round trip in this window, which is what a user feels
    /// as a stall even when the mean looks healthy.
    pub(super) rtt_max: Duration,
    /// When each outstanding request was sent, so its answer can be timed.
    pub(super) sent_at: HashMap<FileReqId, Instant>,
    /// Bytes carried in each direction since the last report.
    pub(super) read_bytes: u64,
    pub(super) written_bytes: u64,
    /// Requests answered; those that failed for a fault — the owner's I/O, a
    /// handle it does not know — and those it turned down as the filesystem
    /// stands: no such file, permission, an existing name. A program probing
    /// for files that are not there is ordinary work, and counting it with
    /// the faults made every health report after a file manager opened a
    /// mount read as trouble.
    pub(super) answered: u64,
    pub(super) failed: u64,
    pub(super) refused: u64,
    /// Where a write's round trip is actually spent, summed since the last
    /// report, with the count so a mean can be taken.
    ///
    /// The total is easy to see and useless on its own: a round trip of 800 ms
    /// on a link whose queue holds 130 ms of traffic says the time went
    /// somewhere else, and every candidate — compression, the wire, the peer's
    /// disk — was individually measured too fast to explain it. Splitting the
    /// trip is the only way to stop guessing which one it is.
    ///
    /// `queued` is from accepting the mount's write to handing the compressed
    /// message to the transport; `wire` is from there to the peer's answer.
    pub(super) stage_queued: Duration,
    pub(super) stage_wire: Duration,
    stage_count: u64,
    /// When each write was handed to the transport, to time the wire half.
    sent_to_wire: HashMap<FileReqId, Instant>,
    /// Bytes served *to* the peer since the last report, in each direction.
    ///
    /// Kept apart from `read_bytes`/`written_bytes`, which count only what this
    /// machine asked for. That is the wrong half whenever the peer is the one
    /// driving: Steam writing 51 GB an hour onto our disk reported
    /// `write_kb_s=0`, because every byte of it arrived in a request rather
    /// than leaving in one, and the whole transfer was invisible to the numbers
    /// meant to describe it.
    pub(super) served_read_bytes: u64,
    pub(super) served_written_bytes: u64,
    /// Requests sent, counted by kind.
    ///
    /// The totals alone say how much was asked and not what was asked, and the
    /// two lead to different fixes: a workload dominated by `stat` wants a
    /// cache, one dominated by `open` wants fewer opens, one dominated by
    /// `read` wants a bigger read. Working that out by reading a log of
    /// refusals only — the successful calls being invisible — put the count of
    /// round trips per created file at "somewhere between 4 and 15".
    by_kind: HashMap<&'static str, u64>,
}

impl Health {
    /// Note that a request has gone out.
    pub(super) fn sent(&mut self, req: FileReqId, kind: &'static str, now: Instant) {
        self.sent_at.insert(req, now);
        *self.by_kind.entry(kind).or_insert(0) += 1;
    }

    /// Note that a request has been answered, timing its round trip.
    pub(super) fn answered(&mut self, req: FileReqId, now: Instant) {
        self.answered += 1;
        if let Some(at) = self.sent_at.remove(&req) {
            let rtt = now.saturating_duration_since(at);
            self.rtt_total += rtt;
            self.rtt_count += 1;
            self.rtt_max = self.rtt_max.max(rtt);
        }
        if let Some(at) = self.sent_to_wire.remove(&req) {
            self.stage_wire += now.saturating_duration_since(at);
            self.stage_count += 1;
        }
    }

    /// Note that a write has reached the transport, splitting its round trip
    /// into the part spent here and the part spent on the link.
    pub(super) fn reached_wire(&mut self, req: FileReqId, now: Instant) {
        if let Some(accepted) = self.sent_at.get(&req) {
            self.stage_queued += now.saturating_duration_since(*accepted);
        }
        self.sent_to_wire.insert(req, now);
    }

    /// Mean of one stage total over the writes that completed, in ms.
    pub(super) fn mean_stage_ms(&self, total: Duration) -> u64 {
        match self.stage_count {
            0 => 0,
            n => (total.as_millis() as u64) / n,
        }
    }

    /// Mean round trip over this reporting window, in milliseconds.
    pub(super) fn mean_rtt_ms(&self) -> u64 {
        match self.rtt_count {
            0 => 0,
            n => (self.rtt_total.as_millis() as u64) / n,
        }
    }

    /// Clear the per-window counters, keeping the outstanding-request map: a
    /// request sent before a report is still owed an answer after it.
    pub(super) fn roll(&mut self, now: Instant) {
        self.last_log = Some(now);
        self.rtt_total = Duration::ZERO;
        self.rtt_count = 0;
        self.rtt_max = Duration::ZERO;
        self.read_bytes = 0;
        self.written_bytes = 0;
        self.served_read_bytes = 0;
        self.served_written_bytes = 0;
        self.stage_queued = Duration::ZERO;
        self.stage_wire = Duration::ZERO;
        self.stage_count = 0;
        let deadline = cursio_files::bridge::BULK_OP_TIMEOUT;
        self.sent_to_wire
            .retain(|_, at| now.saturating_duration_since(*at) < deadline);
        self.answered = 0;
        self.failed = 0;
        self.refused = 0;
        self.by_kind.clear();
        // An answer that never came would otherwise hold its entry for the life
        // of the session. The mount gives up after `BULK_OP_TIMEOUT` at the
        // latest, so anything older than that is never going to be timed.
        self.sent_at
            .retain(|_, at| now.saturating_duration_since(*at) < deadline);
    }
}

/// Turn a walk a mount noticed into this machine's problem, or end one.
///
/// Installed once for the process, since a walk is a fact about this machine
/// whichever peer's disk it happens on.
pub(super) fn report_walk(event: &cursio_files::walk::WalkEvent) {
    use cursio_files::walk::{WalkEvent, Walker};
    let id = |w: &Walker| {
        format!(
            "files.walk.{}.{}.{}",
            w.pid,
            w.peer,
            cursio_files::volume_dir_name(&w.volume)
        )
    };
    match event {
        WalkEvent::Started(walker) => crate::diagnostics::raise(crate::diagnostics::Problem {
            id: id(walker),
            severity: crate::diagnostics::Severity::Warning,
            summary: walk_summary(walker),
            hint: Some(walk_hint(walker.pid)),
            actions: Vec::new(),
            since: std::time::SystemTime::now(),
        }),
        WalkEvent::Ended(walker) => crate::diagnostics::clear(&id(walker)),
    }
}

pub(super) fn walk_summary(walker: &cursio_files::walk::Walker) -> String {
    let who = match &walker.command {
        Some(command) => format!("PID {} (`{command}`)", walker.pid),
        None => format!("PID {}", walker.pid),
    };
    format!(
        "{who} is walking {}'s whole {} disk over the network, one round trip per directory",
        walker.peer,
        cursio_files::volume_dir_name(&walker.volume)
    )
}

pub(super) fn walk_hint(pid: u32) -> String {
    let stop = match cfg!(target_os = "windows") {
        true => format!("taskkill /PID {pid}"),
        false => format!("kill {pid}"),
    };
    format!(
        "if it is an indexer, a backup tool or a search, exclude the peer's drives from it; \
         otherwise stop it with `{stop}`"
    )
}

impl FilesEngine {
    /// Report what the link is doing, once every [`HEALTH_LOG_INTERVAL`].
    ///
    /// Silent while nothing is happening: a mount that is simply not in use
    /// would otherwise print a line of zeroes twice a minute for the life of
    /// the session, which is how a log stops being read.
    pub(super) fn log_health(&mut self, now: Instant) {
        let due = self
            .health
            .last_log
            .is_none_or(|t| now.duration_since(t) >= HEALTH_LOG_INTERVAL);
        if !due {
            return;
        }
        // Before the idle check, deliberately. A mount whose calls have stopped
        // returning reports *no* traffic — that is exactly what being stuck
        // looks like from here — so anything said only on a busy link would go
        // unsaid in the one situation it exists for.
        #[cfg(all(target_os = "windows", target_env = "msvc"))]
        cursio_files::inflight::report_stuck();

        // Serving the peer counts as activity too. Judging by our own requests
        // alone made the module silent through exactly the case worth watching:
        // a download onto our disk, where every request is inbound.
        let idle = self.health.answered == 0
            && self.health.failed == 0
            && self.health.refused == 0
            && self.served.is_empty();
        if idle {
            // Nothing to report, but the clock still rolls — otherwise the next
            // burst of traffic would report against a window of hours.
            self.health.roll(now);
            return;
        }

        let secs = self
            .health
            .last_log
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or_else(|| HEALTH_LOG_INTERVAL.as_secs_f64())
            .max(0.001);

        tracing::info!(
            peer = %self.peer_name,
            // What the link achieved, both ways.
            read_kb_s = (self.health.read_bytes as f64 / secs / 1024.0) as u64,
            write_kb_s = (self.health.written_bytes as f64 / secs / 1024.0) as u64,
            // Where a write's round trip went. `queued_ms` is ours — accepted
            // to on the wire; `wire_ms` is everything past that, including the
            // peer's disk. Reported because the total alone never said which.
            queued_ms = self.health.mean_stage_ms(self.health.stage_queued),
            wire_ms = self.health.mean_stage_ms(self.health.stage_wire),
            // The same two directions for work the peer asked of us, which is
            // the whole of the traffic whenever it is the peer that is copying.
            served_read_kb_s = (self.health.served_read_bytes as f64 / secs / 1024.0) as u64,
            served_write_kb_s = (self.health.served_written_bytes as f64 / secs / 1024.0) as u64,
            // What one operation costs. Small operations live and die by this.
            rtt_ms = self.health.mean_rtt_ms(),
            rtt_max_ms = self.health.rtt_max.as_millis() as u64,
            // What the pacer believes and what it did about it. A window at the
            // floor beside a healthy rate is the controller fighting the link.
            window = self.pace.in_flight_limit,
            link_kb_s = self.pace.rate.unwrap_or(0.0) as u64 / 1024,
            // What the peer has confirmed, beside what the socket took. The two
            // diverging is the signature of a queue: measured live at 215
            // against 104 Mbit/s, which is what made every window twice too
            // wide.
            acked_kb_s = self.pace.acked_rate.unwrap_or(0.0) as u64 / 1024,
            cuts = self.pace.cuts,
            grows = self.pace.grows,
            ops = self.health.answered,
            failed = self.health.failed,
            refused = self.health.refused,
            in_flight = self.pending.len(),
            // What was asked, not just how much: the mix is what says which
            // round trip to attack next.
            by_kind = %{
                let mut kinds: Vec<_> = self.health.by_kind.iter().collect();
                kinds.sort_by(|a, b| b.1.cmp(a.1));
                kinds
                    .iter()
                    .map(|(k, n)| format!("{k}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            // What the peer asked of us. The counters above are what we asked
            // of it, and a transfer onto our disk is entirely this column.
            served = %{
                let mut kinds: Vec<_> = self.served.iter().collect();
                kinds.sort_by(|a, b| b.1.cmp(a.1));
                kinds
                    .iter()
                    .map(|(k, n)| format!("{k}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            served_slow = self
                .slow_served
                .swap(0, std::sync::atomic::Ordering::Relaxed),
            "file sharing health"
        );
        self.served.clear();

        self.pace.cuts = 0;
        self.pace.grows = 0;
        self.health.roll(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The user must be able to recognise the walker from the sentence alone:
    /// its PID, what it is, and whose disk it is costing.
    #[test]
    fn a_disk_walk_is_raised_naming_the_process_and_the_peer_and_cleared_when_it_ends() {
        use cursio_files::walk::{WalkEvent, Walker};
        let walker = Walker {
            pid: 4_000_001,
            command: Some("find / -name *.rs".into()),
            peer: "Lightmoon".into(),
            volume: "C:".into(),
        };
        report_walk(&WalkEvent::Started(walker.clone()));
        let raised = crate::diagnostics::snapshot()
            .into_iter()
            .find(|p| p.id.starts_with("files.walk.4000001."))
            .expect("raised");
        assert_eq!(
            raised.summary,
            "PID 4000001 (`find / -name *.rs`) is walking Lightmoon's whole C disk over the \
             network, one round trip per directory"
        );
        assert!(raised.hint.expect("a hint").contains("4000001"));

        report_walk(&WalkEvent::Ended(walker));
        assert!(!crate::diagnostics::snapshot()
            .iter()
            .any(|p| p.id.starts_with("files.walk.4000001.")));
    }

    #[test]
    fn a_missing_file_is_an_answer_and_an_io_error_a_fault() {
        use cursio_protocol::FileError;
        assert!(!is_fault(&FileError::NotFound));
        assert!(!is_fault(&FileError::Denied));
        assert!(!is_fault(&FileError::Exists));
        assert!(is_fault(&FileError::Io("disk gone".into())));
        assert!(is_fault(&FileError::BadHandle));
    }
}
