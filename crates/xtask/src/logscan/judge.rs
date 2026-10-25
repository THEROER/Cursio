//! Two machines' summaries judged against the limits: one row per check, a
//! reason for every SKIP, and the table `live-test` prints.

use crate::logscan::growth;
use crate::logscan::Latency;
use crate::logscan::Stall;
use crate::logscan::Summary;

/// Received-to-injected p99 above which the pointer is felt to lag. The
/// daemons warn at 20 ms; a healthy link measures hundreds of microseconds,
/// so a regression shows here long before the warning fires.
pub const INJECT_P99_MAX_US: f64 = 5_000.0;
/// Captured-to-sent p99 on the driving machine.
pub const CAPTURE_P99_MAX_US: f64 = 5_000.0;
/// Time input spends queued behind other classes before the socket.
pub const QUEUE_INPUT_P99_MAX_US: f64 = 5_000.0;
/// Time a video frame spends queued; beyond this the remote screen stutters.
pub const QUEUE_VIDEO_P99_MAX_US: f64 = 200_000.0;
/// The module budget from CLAUDE.md: anything longer on the shared loop held
/// every link's cursor.
pub const LOOP_STALL_MAX_MS: f64 = 50.0;
/// A module's own thread may block longer before its traffic is felt late.
pub const MODULE_STALL_MAX_MS: f64 = 500.0;
/// Slowest file answer tolerated within one health interval.
pub const FILES_RTT_MAX_MS: f64 = 500.0;
/// Share of the window this machine may spend with every task stalled on
/// memory, or on I/O, per `/proc/pressure`.
pub const PSI_MEMORY_FULL_MAX_PCT: f64 = 5.0;
pub const PSI_IO_FULL_MAX_PCT: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// Nothing in the window exercised this, so there is no number to judge.
    Skip,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        }
    }
}

/// One line of the result table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub check: String,
    pub machine: String,
    pub measured: String,
    pub limit: String,
    pub status: Status,
}

/// One machine's log, named for the table.
pub struct Machine<'a> {
    pub name: &'a str,
    pub summary: &'a Summary,
    /// The latency path synthetic input must show on this machine, so a
    /// missing report is a failure rather than an idle link.
    pub expects_input: Option<&'static str>,
}

/// What the harness measured itself, outside the logs.
#[derive(Debug, Default)]
pub struct Probes {
    /// `Ok(detail)` or `Err(reason)`; `None` when the roundtrip was not run.
    pub file_roundtrip: Option<Result<String, String>>,
    /// `(memory full %, io full %)` over the window on the local machine.
    pub pressure: Option<(f64, f64)>,
}

pub(super) fn row(
    check: &str,
    machine: &str,
    measured: String,
    limit: String,
    status: Status,
) -> Row {
    Row {
        check: check.to_string(),
        machine: machine.to_string(),
        measured,
        limit,
        status,
    }
}

pub(super) fn judge(value: f64, max: f64) -> Status {
    if value <= max {
        Status::Pass
    } else {
        Status::Fail
    }
}

pub(super) fn max_of(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    values
        .into_iter()
        .fold(None, |acc, v| Some(acc.map_or(v, |a: f64| a.max(v))))
}

/// Turn each machine's summary and the harness's own probes into the table.
pub fn evaluate(machines: &[Machine], probes: &Probes) -> Vec<Row> {
    let mut rows = Vec::new();
    for m in machines {
        latency_rows(&mut rows, m);
        stall_rows(&mut rows, m);
        link_rows(&mut rows, m);
    }
    probe_rows(
        &mut rows,
        machines.first().map_or("local", |m| m.name),
        probes,
    );
    rows
}

pub(super) fn latency_rows(rows: &mut Vec<Row>, m: &Machine) {
    let s = m.summary;
    for (path, limit, name) in [
        (
            "received->injected",
            INJECT_P99_MAX_US,
            "input received->injected p99",
        ),
        (
            "captured->sent",
            CAPTURE_P99_MAX_US,
            "input captured->sent/queued p99",
        ),
    ] {
        let reports: Vec<&Latency> = s
            .latency
            .iter()
            .filter(|l| {
                l.path == path || (path == "captured->sent" && l.path == "captured->queued")
            })
            .collect();
        let worst = max_of(reports.iter().map(|l| l.p99_us));
        let events: u64 = reports.iter().map(|l| l.events).sum();
        let limit_text = format!("< {:.1} ms", limit / 1000.0);
        rows.push(match worst {
            Some(p99) => row(
                name,
                m.name,
                format!("{:.2} ms ({events} events)", p99 / 1000.0),
                limit_text,
                judge(p99, limit),
            ),
            None if m.expects_input == Some(path) => row(
                name,
                m.name,
                "no report: synthetic input never arrived".into(),
                limit_text,
                Status::Fail,
            ),
            None => row(
                name,
                m.name,
                "no input in window".into(),
                limit_text,
                Status::Skip,
            ),
        });
    }
    rows.push(row(
        "process queue overloads",
        m.name,
        s.process_queue_failures.to_string(),
        "0".into(),
        if s.process_queue_failures == 0 {
            Status::Pass
        } else {
            Status::Fail
        },
    ));
    for (values, limit, name) in [
        (
            &s.process_input_age_us,
            50_000.0,
            "process input queue max age",
        ),
        (
            &s.queue_input_p99_us,
            QUEUE_INPUT_P99_MAX_US,
            "send queue input p99",
        ),
        (
            &s.queue_video_p99_us,
            QUEUE_VIDEO_P99_MAX_US,
            "send queue video p99 (screen)",
        ),
    ] {
        let limit_text = format!("< {:.0} ms", limit / 1000.0);
        rows.push(match max_of(values.iter().copied()) {
            Some(v) => row(
                name,
                m.name,
                format!("{:.2} ms", v / 1000.0),
                limit_text,
                judge(v, limit),
            ),
            None => row(name, m.name, "no traffic".into(), limit_text, Status::Skip),
        });
    }
}

pub(super) fn describe_stalls(stalls: &[Stall]) -> String {
    match stalls.iter().max_by(|a, b| a.took_ms.total_cmp(&b.took_ms)) {
        None => "none".into(),
        Some(worst) => format!(
            "{} (worst {:.0} ms, {})",
            stalls.len(),
            worst.took_ms,
            worst.what
        ),
    }
}

pub(super) fn stall_rows(rows: &mut Vec<Row>, m: &Machine) {
    let s = m.summary;
    let loop_worst = max_of(s.loop_stalls.iter().map(|x| x.took_ms)).unwrap_or(0.0);
    rows.push(row(
        "session loop stalls",
        m.name,
        describe_stalls(&s.loop_stalls),
        format!("none > {LOOP_STALL_MAX_MS:.0} ms"),
        judge(loop_worst, LOOP_STALL_MAX_MS),
    ));
    let module_worst = max_of(s.module_stalls.iter().map(|x| x.took_ms)).unwrap_or(0.0);
    rows.push(row(
        "module thread stalls",
        m.name,
        describe_stalls(&s.module_stalls),
        format!("none > {MODULE_STALL_MAX_MS:.0} ms"),
        judge(module_worst, MODULE_STALL_MAX_MS),
    ));
}

pub(super) fn count_row(check: &str, machine: &str, items: &[String]) -> Row {
    let measured = match items.first() {
        None => "0".to_string(),
        Some(first) => format!("{} ({first})", items.len()),
    };
    let status = if items.is_empty() {
        Status::Pass
    } else {
        Status::Fail
    };
    row(check, machine, measured, "0".into(), status)
}

pub(super) fn link_rows(rows: &mut Vec<Row>, m: &Machine) {
    let s = m.summary;
    let mut drops = s.session_drops.clone();
    drops.extend(std::iter::repeat_n(
        "peer slow to respond".to_string(),
        s.peer_slow,
    ));
    rows.push(count_row("peer session drops", m.name, &drops));

    let played = growth(s.audio_health.iter().map(|h| h.0));
    let stale = growth(s.audio_health.iter().map(|h| h.1)) + s.audio_jitter_drops as f64;
    let stalls = s.audio_stalls;
    // Nothing played and nothing stalled is an idle bus, not a healthy one.
    rows.push(if played == 0.0 && stale == 0.0 && stalls == 0 {
        row(
            "audio continuity",
            m.name,
            "no audio played".into(),
            "0 stalls, 0 drops".into(),
            Status::Skip,
        )
    } else {
        row(
            "audio continuity",
            m.name,
            format!("{stalls} stalls, {stale:.0} stale drops, {played:.0} chunks played"),
            "0 stalls, 0 drops".into(),
            if stalls == 0 && stale == 0.0 {
                Status::Pass
            } else {
                Status::Fail
            },
        )
    });

    rows.push(if s.files_reports == 0 && s.file_warnings.is_empty() {
        row(
            "file sharing health",
            m.name,
            "no health lines".into(),
            "0 failed/slow".into(),
            Status::Skip,
        )
    } else {
        let bad = s.files_failed + s.files_served_slow + s.file_warnings.len() as f64;
        let rtt_ok = s.files_rtt_max_ms <= FILES_RTT_MAX_MS;
        row(
            "file sharing health",
            m.name,
            format!(
                "failed {:.0}, served_slow {:.0}, hung {}, rtt_max {:.0} ms",
                s.files_failed,
                s.files_served_slow,
                s.file_warnings.len(),
                s.files_rtt_max_ms
            ),
            format!("0 failed/slow, rtt_max < {FILES_RTT_MAX_MS:.0} ms"),
            if bad == 0.0 && rtt_ok {
                Status::Pass
            } else {
                Status::Fail
            },
        )
    });

    rows.push(count_row(
        "screen/window errors",
        m.name,
        &s.window_warnings,
    ));
}

pub(super) fn probe_rows(rows: &mut Vec<Row>, local: &str, probes: &Probes) {
    rows.push(match &probes.file_roundtrip {
        None => row(
            "file roundtrip on peer mount",
            local,
            "not run".into(),
            "ok".into(),
            Status::Skip,
        ),
        Some(Ok(detail)) => row(
            "file roundtrip on peer mount",
            local,
            detail.clone(),
            "ok".into(),
            Status::Pass,
        ),
        Some(Err(why)) => row(
            "file roundtrip on peer mount",
            local,
            why.clone(),
            "ok".into(),
            Status::Fail,
        ),
    });
    rows.push(match probes.pressure {
        None => row(
            "memory/io pressure",
            local,
            "unreadable".into(),
            String::new(),
            Status::Skip,
        ),
        Some((mem, io)) => row(
            "memory/io pressure (full)",
            local,
            format!("memory {mem:.1}%, io {io:.1}%"),
            format!("memory < {PSI_MEMORY_FULL_MAX_PCT:.0}%, io < {PSI_IO_FULL_MAX_PCT:.0}%"),
            if mem <= PSI_MEMORY_FULL_MAX_PCT && io <= PSI_IO_FULL_MAX_PCT {
                Status::Pass
            } else {
                Status::Fail
            },
        ),
    });
}

/// The table as text, one row per line, plus the overall verdict.
pub fn render(rows: &[Row]) -> String {
    let widths = rows.iter().fold([5, 7, 8, 5], |w, r| {
        [
            w[0].max(r.check.chars().count()),
            w[1].max(r.machine.chars().count()),
            w[2].max(r.measured.chars().count()),
            w[3].max(r.limit.chars().count()),
        ]
    });
    let line = |a: &str, b: &str, c: &str, d: &str, e: &str| {
        format!(
            "{a:<w0$}  {b:<w1$}  {c:<w2$}  {d:<w3$}  {e}\n",
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3]
        )
    };
    let mut out = line("check", "machine", "measured", "limit", "result");
    for r in rows {
        out += &line(
            &r.check,
            &r.machine,
            &r.measured,
            &r.limit,
            r.status.label(),
        );
    }
    out
}

/// Whether any row failed.
pub fn failed(rows: &[Row]) -> bool {
    rows.iter().any(|r| r.status == Status::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logscan::fixture::WINDOW;
    use crate::logscan::summarize;
    use crate::logscan::Summary;
    fn status_of(rows: &[Row], check: &str, machine: &str) -> Status {
        rows.iter()
            .find(|r| r.check.starts_with(check) && r.machine == machine)
            .unwrap_or_else(|| panic!("no row {check} for {machine}"))
            .status
    }

    #[test]
    fn a_healthy_window_passes_and_a_stalled_one_fails() {
        let bad = summarize(WINDOW.lines());
        let good = summarize(WINDOW.lines().take(2));
        let rows = evaluate(
            &[
                Machine {
                    name: "good",
                    summary: &good,
                    expects_input: None,
                },
                Machine {
                    name: "bad",
                    summary: &bad,
                    expects_input: None,
                },
            ],
            &Probes::default(),
        );
        assert_eq!(
            status_of(&rows, "input received->injected", "good"),
            Status::Pass
        );
        assert_eq!(
            status_of(&rows, "session loop stalls", "good"),
            Status::Pass
        );
        assert_eq!(status_of(&rows, "session loop stalls", "bad"), Status::Fail);
        assert_eq!(
            status_of(&rows, "module thread stalls", "bad"),
            Status::Pass
        );
        assert_eq!(status_of(&rows, "peer session drops", "bad"), Status::Fail);
        assert_eq!(status_of(&rows, "audio continuity", "bad"), Status::Fail);
        assert_eq!(status_of(&rows, "audio continuity", "good"), Status::Skip);
        assert_eq!(status_of(&rows, "file sharing health", "bad"), Status::Fail);
        assert!(failed(&rows));
    }

    #[test]
    fn missing_latency_fails_only_where_synthetic_input_was_expected() {
        let empty = Summary::default();
        let rows = evaluate(
            &[
                Machine {
                    name: "driven",
                    summary: &empty,
                    expects_input: Some("received->injected"),
                },
                Machine {
                    name: "idle",
                    summary: &empty,
                    expects_input: None,
                },
            ],
            &Probes::default(),
        );
        assert_eq!(
            status_of(&rows, "input received->injected", "driven"),
            Status::Fail
        );
        assert_eq!(
            status_of(&rows, "input received->injected", "idle"),
            Status::Skip
        );
        assert_eq!(
            status_of(&rows, "input captured->sent", "driven"),
            Status::Skip
        );
    }

    #[test]
    fn latency_just_over_the_limit_fails() {
        let line = format!(
            "t  INFO s: input latency path=\"received->injected\" events=5 p50_us=1 p99_us={} max_us=1",
            INJECT_P99_MAX_US + 1.0
        );
        let s = summarize([line.as_str()]);
        let rows = evaluate(
            &[Machine {
                name: "m",
                summary: &s,
                expects_input: None,
            }],
            &Probes::default(),
        );
        assert_eq!(
            status_of(&rows, "input received->injected", "m"),
            Status::Fail
        );
    }

    #[test]
    fn probes_judge_the_roundtrip_and_pressure() {
        let s = Summary::default();
        let probes = Probes {
            file_roundtrip: Some(Err("timed out".into())),
            pressure: Some((0.5, PSI_IO_FULL_MAX_PCT + 1.0)),
        };
        let rows = evaluate(
            &[Machine {
                name: "m",
                summary: &s,
                expects_input: None,
            }],
            &probes,
        );
        assert_eq!(status_of(&rows, "file roundtrip", "m"), Status::Fail);
        assert_eq!(status_of(&rows, "memory/io pressure", "m"), Status::Fail);
        assert!(render(&rows).contains("FAIL"));
    }

    #[test]
    fn process_queue_age_and_overload_are_not_hidden_by_fast_admission() {
        let summary = summarize([
            "t DEBUG q: process queue health queue=\"session outbound\" oldest_us=10 max_age_us=70000",
            "t DEBUG q: process queue health queue=\"module inbox\" oldest_us=900000 max_age_us=900000",
            "t ERROR q: process queue overloaded; terminating its stream queue=\"raw capture\"",
            "t INFO s: input latency path=\"captured->queued\" events=128 p50_us=1 p99_us=2",
        ]);
        assert_eq!(summary.process_input_age_us, vec![70_000.0]);
        assert_eq!(summary.process_queue_failures, 1);
        let rows = evaluate(
            &[Machine {
                name: "m",
                summary: &summary,
                expects_input: Some("captured->sent"),
            }],
            &Probes::default(),
        );
        assert_eq!(
            status_of(&rows, "process input queue max age", "m"),
            Status::Fail
        );
        assert_eq!(
            status_of(&rows, "process queue overloads", "m"),
            Status::Fail
        );
        assert_eq!(status_of(&rows, "input captured->sent", "m"), Status::Pass);
    }
}
