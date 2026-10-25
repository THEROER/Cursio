//! Reading a Cursio log back as numbers.
//!
//! The daemons already measure what matters — input latency percentiles every
//! 30 s, send-queue delay, audio and file-sharing health, and a warning for
//! every stall — but only as log lines. This module turns the lines written
//! during one `live-test` window into a [`Summary`] per machine, and
//! [`evaluate`] turns two summaries into a PASS/FAIL table.
//!
//! Matching is by message text, not by a structured format, because the text
//! *is* the interface: the daemons write `tracing`'s default formatter and
//! xtask deliberately depends on none of the app crates. Messages are matched
//! by a stable prefix so a reworded tail (the slow-turn warning has changed
//! wording more than once) does not silently turn a failure into "no data".

/// One `input latency` report: percentiles over the last 30 s of one path.
#[derive(Debug, Clone, PartialEq)]
pub struct Latency {
    /// `captured->sent` on the machine driving, `received->injected` on the
    /// one being driven.
    pub path: String,
    pub events: u64,
    pub p50_us: f64,
    pub p99_us: f64,
}

/// One stall warning: which loop or module, and for how long.
#[derive(Debug, Clone, PartialEq)]
pub struct Stall {
    pub what: String,
    pub took_ms: f64,
}

/// Everything a window of one machine's log says about health.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Summary {
    pub lines: usize,
    pub latency: Vec<Latency>,
    /// `input_p99` of each send-queue report, in µs.
    pub queue_input_p99_us: Vec<f64>,
    pub process_input_age_us: Vec<f64>,
    pub process_queue_failures: usize,
    /// `video_p99` of each send-queue report that carried video, in µs.
    pub queue_video_p99_us: Vec<f64>,
    /// Stalls of the shared session loop: every link's input waited.
    pub loop_stalls: Vec<Stall>,
    /// Stalls isolated to one module's thread: only its own traffic waited.
    pub module_stalls: Vec<Stall>,
    pub session_drops: Vec<String>,
    pub peer_slow: usize,
    pub audio_stalls: usize,
    /// `(played, stale_dropped)` of each `audio health` line; cumulative per
    /// session, so a drop in value means a new session began.
    pub audio_health: Vec<(f64, f64)>,
    /// Jitter-buffer overflows the backends log on their own.
    pub audio_jitter_drops: usize,
    pub files_failed: f64,
    pub files_served_slow: f64,
    pub files_rtt_max_ms: f64,
    pub files_reports: usize,
    pub file_warnings: Vec<String>,
    pub window_warnings: Vec<String>,
    pub errors: usize,
}

/// Messages that mean a file operation hung or was abandoned.
const FILE_WARNINGS: &[&str] = &[
    "a request from the peer took long enough",
    "peer did not answer a file operation",
    "filesystem calls have entered and not returned",
    "answering Windows with a device error",
];

/// Messages that mean the remote screen or a window stream degraded.
const WINDOW_WARNINGS: &[&str] = &[
    "video encode failed",
    "no video encoder for this size",
    "surrogate tick blocked",
    "could not open a remote-screen view",
];

/// Parse the lines of one machine's log written during the test window.
pub fn summarize<'a>(lines: impl IntoIterator<Item = &'a str>) -> Summary {
    let mut s = Summary::default();
    for raw in lines {
        let line = raw.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        s.lines += 1;
        if level(line) == Some("ERROR") {
            s.errors += 1;
        }
        scan_line(&mut s, line);
    }
    s
}

fn scan_line(s: &mut Summary, line: &str) {
    let has = |text: &str| line.contains(text);
    let took = || num(line, "took_ms").unwrap_or(0.0);
    let module = || field(line, "module").unwrap_or("?").to_string();

    if has("process queue overloaded") {
        s.process_queue_failures += 1;
    }
    if has("process queue health") {
        if matches!(
            field(line, "queue"),
            Some("raw capture" | "captured input" | "session inbound" | "session outbound")
        ) {
            s.process_input_age_us.push(
                num(line, "oldest_us")
                    .unwrap_or(0.0)
                    .max(num(line, "max_age_us").unwrap_or(0.0)),
            );
        }
    } else if has(" input latency") {
        if let (Some(path), Some(p99)) = (field(line, "path"), num(line, "p99_us")) {
            s.latency.push(Latency {
                path: path.to_string(),
                events: num(line, "events").unwrap_or(0.0) as u64,
                p50_us: num(line, "p50_us").unwrap_or(0.0),
                p99_us: p99,
            });
        }
    } else if has("send queue delay") || has("input waited long enough") {
        if let Some(p99) = field(line, "input_p99").and_then(duration_us) {
            s.queue_input_p99_us.push(p99);
        }
        if let Some(v) = field(line, "video_p99").and_then(duration_us) {
            if v > 0.0 {
                s.queue_video_p99_us.push(v);
            }
        }
    } else if has("session loop turn was slow") {
        s.loop_stalls.push(Stall {
            what: "session loop".into(),
            took_ms: took(),
        });
    } else if has("blocked the session loop") {
        s.loop_stalls.push(Stall {
            what: module(),
            took_ms: took(),
        });
    } else if has("module pump was slow") || has("module was slow handling a message") {
        s.module_stalls.push(Stall {
            what: module(),
            took_ms: took(),
        });
    } else if has("peer session ended") {
        // `error=` is a Display value, unquoted and full of spaces; it is the
        // last field, so it runs to the end of the line.
        let why = line
            .split_once(" error=")
            .map_or("clean close", |(_, e)| e.trim());
        s.session_drops.push(why.to_string());
    } else if has("peer is slow to respond") {
        s.peer_slow += 1;
    } else if has("audio from the peer stalled") {
        s.audio_stalls += 1;
    } else if has(" audio health ") {
        s.audio_health.push((
            num(line, "played").unwrap_or(0.0),
            num(line, "stale_dropped").unwrap_or(0.0),
        ));
    } else if has("jitter buffer over its limit") {
        s.audio_jitter_drops += 1;
    } else if has("file sharing health") {
        s.files_reports += 1;
        s.files_failed += num(line, "failed").unwrap_or(0.0);
        s.files_served_slow += num(line, "served_slow").unwrap_or(0.0);
        s.files_rtt_max_ms = s
            .files_rtt_max_ms
            .max(num(line, "rtt_max_ms").unwrap_or(0.0));
    } else if let Some(w) = FILE_WARNINGS.iter().find(|w| has(w)) {
        s.file_warnings.push((*w).to_string());
    } else if let Some(w) = WINDOW_WARNINGS.iter().find(|w| has(w)) {
        s.window_warnings.push((*w).to_string());
    }
}

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

fn row(check: &str, machine: &str, measured: String, limit: String, status: Status) -> Row {
    Row {
        check: check.to_string(),
        machine: machine.to_string(),
        measured,
        limit,
        status,
    }
}

fn judge(value: f64, max: f64) -> Status {
    if value <= max {
        Status::Pass
    } else {
        Status::Fail
    }
}

fn max_of(values: impl IntoIterator<Item = f64>) -> Option<f64> {
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

fn latency_rows(rows: &mut Vec<Row>, m: &Machine) {
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

fn describe_stalls(stalls: &[Stall]) -> String {
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

fn stall_rows(rows: &mut Vec<Row>, m: &Machine) {
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

fn count_row(check: &str, machine: &str, items: &[String]) -> Row {
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

fn link_rows(rows: &mut Vec<Row>, m: &Machine) {
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

fn probe_rows(rows: &mut Vec<Row>, local: &str, probes: &Probes) {
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

/// `total=` of the `full` line of a `/proc/pressure/*` file: microseconds
/// during which every non-idle task was stalled on that resource.
pub fn pressure_full_total_us(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("full "))?;
    field(&format!(" {line}"), "total")?.parse().ok()
}

/// How much a cumulative per-session counter grew across `values`.
///
/// A value lower than the one before means the session restarted and the
/// counter began again from zero, so it counts in full rather than negative.
pub fn growth(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut total = 0.0;
    let mut prev: Option<f64> = None;
    for v in values {
        total += match prev {
            Some(p) if v >= p => v - p,
            Some(_) => v,
            None => 0.0,
        };
        prev = Some(v);
    }
    total
}

/// The value of `key=` in a `tracing` line, without surrounding quotes.
///
/// Searched as ` key=` so `p99_us` never matches inside `input_p99_us`.
pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find('"')?;
        return Some(&quoted[..end]);
    }
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// A numeric field; `None` when it is missing or not a number.
pub fn num(line: &str, key: &str) -> Option<f64> {
    field(line, key)?.parse().ok()
}

/// A `Debug`-formatted `Duration` (`48µs`, `102.1ms`, `0ns`, `1.5s`) in
/// microseconds.
pub fn duration_us(text: &str) -> Option<f64> {
    let units: [(&str, f64); 5] = [
        ("ns", 0.001),
        ("µs", 1.0),
        ("us", 1.0),
        ("ms", 1_000.0),
        ("s", 1_000_000.0),
    ];
    for (suffix, scale) in units {
        if let Some(n) = text.strip_suffix(suffix) {
            return n.parse::<f64>().ok().map(|v| v * scale);
        }
    }
    None
}

/// The level column of a line (`INFO`, `WARN`, ...), if it has one.
fn level(line: &str) -> Option<&str> {
    line.split_whitespace().nth(1)
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

    const WINDOW: &str = "\
2026-09-27T09:52:24.083471Z  INFO cursio_core::session: input latency path=\"received->injected\" events=2154 p50_us=33 p99_us=271 max_us=347
2026-09-27T09:47:21.071981Z DEBUG cursio_transport::priority: send queue delay input_frames=1084 input_p50=20µs input_p99=48µs input_max=221.006µs video_p99=102.107486ms bulk_p99=0ns
2026-09-27T09:57:14.070526Z  WARN cursio_core::session: session loop turn was slow; input and cursor handoffs waited took_ms=4986
2026-09-27T09:18:02.450427Z  WARN cursio_core::session: module pump blocked the session loop module=\"audio\" took_ms=118
2026-09-27T09:56:08.282860Z  WARN cursio_core::module_host: module pump was slow; only this module's traffic waited module=\"usb\" took_ms=82
2026-09-27T09:43:09.637638Z  WARN cursio: peer session ended with error error=connection closed by peer
2026-09-27T09:56:20.376756Z  INFO cursio_core::audio_session: audio health sent=59 played=4 silent_dropped=1 unwanted_dropped=1 over_budget=0 stale_dropped=2 streams=11
2026-09-27T09:56:50.404795Z  INFO cursio_core::audio_session: audio health sent=59 played=10 silent_dropped=1 unwanted_dropped=1 over_budget=0 stale_dropped=5 streams=11
2026-09-27T09:52:49.435713Z  INFO cursio_core::files_session: file sharing health peer=Lightmoon read_kb_s=0 rtt_ms=3 rtt_max_ms=41 ops=3 failed=1 in_flight=0 by_kind= served=stat=41 served_slow=2
2026-09-27T08:33:30.535091Z  WARN cursio_core::files_session: a request from the peer took long enough that its caller may have given up kind=\"read\" took_ms=2022
2026-09-27T09:00:00.000000Z ERROR cursio_audio::pipewire_backend: failed to link app to capture after retries app_id=7
";

    #[test]
    fn reads_quoted_and_bare_fields_without_matching_inside_longer_keys() {
        let line = "x INFO t: m path=\"a->b\" input_p99_us=9 p99_us=271";
        assert_eq!(field(line, "path"), Some("a->b"));
        assert_eq!(num(line, "p99_us"), Some(271.0));
        assert_eq!(field(line, "missing"), None);
    }

    #[test]
    fn converts_every_debug_duration_unit_to_microseconds() {
        assert_eq!(duration_us("48µs"), Some(48.0));
        assert_eq!(duration_us("102.5ms"), Some(102_500.0));
        assert_eq!(duration_us("0ns"), Some(0.0));
        assert_eq!(duration_us("1.5s"), Some(1_500_000.0));
        assert_eq!(duration_us("fast"), None);
    }

    #[test]
    fn summarizes_every_kind_of_health_line() {
        let s = summarize(WINDOW.lines());
        assert_eq!(s.lines, 11);
        assert_eq!(s.latency.len(), 1);
        assert_eq!(s.latency[0].p99_us, 271.0);
        assert_eq!(s.queue_input_p99_us, vec![48.0]);
        assert!((s.queue_video_p99_us[0] - 102_107.486).abs() < 1e-6);
        assert_eq!(s.loop_stalls.len(), 2);
        assert_eq!(s.loop_stalls[1].what, "audio");
        assert_eq!(s.module_stalls[0].what, "usb");
        assert_eq!(
            s.session_drops,
            vec!["connection closed by peer".to_string()]
        );
        assert_eq!(s.audio_health, vec![(4.0, 2.0), (10.0, 5.0)]);
        assert_eq!(s.files_failed, 1.0);
        assert_eq!(s.files_served_slow, 2.0);
        assert_eq!(s.files_rtt_max_ms, 41.0);
        assert_eq!(s.file_warnings.len(), 1);
        assert_eq!(s.errors, 1);
    }

    #[test]
    fn cumulative_counters_count_a_session_restart_in_full() {
        assert_eq!(growth([4.0, 10.0, 3.0, 5.0]), 6.0 + 3.0 + 2.0);
        assert_eq!(growth([7.0]), 0.0);
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
    fn reads_the_full_stall_total_from_proc_pressure() {
        let text = "some avg10=0.00 avg60=0.10 avg300=0.20 total=900\nfull avg10=0.00 avg60=0.05 avg300=0.10 total=12345\n";
        assert_eq!(pressure_full_total_us(text), Some(12345));
        assert_eq!(pressure_full_total_us("some total=1\n"), None);
    }
}
