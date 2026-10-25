//! Log lines read into a [`Summary`]: each health line's fields by key, and
//! the durations `tracing` prints in every unit it has.

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
pub(super) const FILE_WARNINGS: &[&str] = &[
    "a request from the peer took long enough",
    "peer did not answer a file operation",
    "filesystem calls have entered and not returned",
    "answering Windows with a device error",
];

/// Messages that mean the remote screen or a window stream degraded.
pub(super) const WINDOW_WARNINGS: &[&str] = &[
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

pub(super) fn scan_line(s: &mut Summary, line: &str) {
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
pub(super) fn level(line: &str) -> Option<&str> {
    line.split_whitespace().nth(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logscan::fixture::WINDOW;
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
    fn reads_the_full_stall_total_from_proc_pressure() {
        let text = "some avg10=0.00 avg60=0.10 avg300=0.20 total=900\nfull avg10=0.00 avg60=0.05 avg300=0.10 total=12345\n";
        assert_eq!(pressure_full_total_us(text), Some(12345));
        assert_eq!(pressure_full_total_us("some total=1\n"), None);
    }
}
