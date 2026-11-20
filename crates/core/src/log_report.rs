//! A node's day, counted from its own log: which peers' sessions and paths
//! ended and why, how long each peer went with no path at all, which
//! warnings repeat, and which modules kept their traffic waiting.
//!
//! This is the analysis that, done by hand with `grep` and `awk` over five
//! days of two machines' logs, found a USB scan waking idle controllers, a
//! module thread left behind at every phone session's end and a warning
//! written every five seconds. Counted by each node from its own file and
//! sent as a few kilobytes, `cursio report all` gives the same picture for
//! the whole mesh in seconds — including a machine nobody can log into.
//!
//! The counting works from the lines' text, which is a contract this module
//! keeps with the ones that write them: a session's path lines carry its
//! `session=` label, and the registry names the peer behind each label once
//! (`holding the session with this peer`). Lines from builds older than the
//! labels are counted under no peer rather than guessed at.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use cursio_protocol::diag::{Counted, NodeReport, PeerReport, SlowModule};

use crate::log_tail::{leading_timestamp, scan_since, stamp_secs, timestamp};

/// Most kinds of warning a report lists.
const TOP_WARNINGS: usize = 15;

/// Count `dir`'s log named `file_name` over the last `since` before `now`.
/// Memory, processes and current problems are filled in by the caller.
pub fn build(
    dir: &Path,
    file_name: &str,
    since: Duration,
    now: SystemTime,
) -> Result<NodeReport, String> {
    let mut warnings: HashMap<String, u32> = HashMap::new();
    let mut slow: HashMap<String, (u32, u64)> = HashMap::new();
    let mut starts = 0;
    // The few lines that need reading in order, kept newest first as they
    // come and turned round below.
    let mut timeline: Vec<String> = Vec::new();

    let coverage = scan_since(dir, file_name, &timestamp(now - since), |line| {
        let Some(level) = level_of(line) else {
            return;
        };
        if line.contains("listening for peers") {
            starts += 1;
        }
        if level == "WARN" || level == "ERROR" {
            *warnings.entry(template(line)).or_default() += 1;
        }
        if line.contains("module pump was slow") || line.contains("module was slow handling") {
            if let (Some(module), Some(ms)) = (field(line, "module"), field(line, "took_ms")) {
                let entry = slow.entry(module.to_string()).or_default();
                entry.0 += 1;
                // `took_ms=24 Module(files)`: the figure is the first word.
                let ms = ms.split_whitespace().next().unwrap_or_default();
                entry.1 = entry.1.max(ms.parse().unwrap_or(0));
            }
        }
        // The session's lines come from the channel's crate now and from the
        // transport's in a log written before it was extracted.
        if line.contains("cursio_channel::session")
            || line.contains("cursio_transport::resume")
            || line.contains("holding the session with this peer")
        {
            timeline.push(line.to_string());
        }
    })?;
    timeline.reverse();

    let mut report = NodeReport {
        since_secs: since.as_secs(),
        from: coverage.oldest,
        truncated: coverage.truncated,
        starts,
        peers: peers(&timeline),
        warnings: ranked(warnings, TOP_WARNINGS),
        ..NodeReport::default()
    };
    let mut slow: Vec<SlowModule> = slow
        .into_iter()
        .map(|(module, (count, max_ms))| SlowModule {
            module,
            count,
            max_ms,
        })
        .collect();
    slow.sort_by_key(|s| std::cmp::Reverse(s.count));
    report.slow_modules = slow;
    Ok(report)
}

/// Each peer's sessions and paths, from the transport's lines in order.
fn peers(timeline: &[String]) -> Vec<PeerReport> {
    const UNLABELLED: &str = "(no session label)";
    let mut owner: HashMap<String, String> = HashMap::new();
    let mut kinds: HashMap<(String, String), String> = HashMap::new();
    let mut pathless_since: HashMap<String, f64> = HashMap::new();
    let mut out: HashMap<String, PeerReport> = HashMap::new();
    let mut ended: HashMap<String, HashMap<String, u32>> = HashMap::new();
    let mut paths: HashMap<String, HashMap<String, u32>> = HashMap::new();

    for line in timeline {
        let at = leading_timestamp(line).and_then(stamp_secs);
        if line.contains("holding the session with this peer") {
            if let (Some(peer), Some(session)) = (field(line, "peer"), field(line, "session")) {
                owner.insert(session.to_string(), peer.to_string());
                out.entry(peer.to_string()).or_default().sessions += 1;
            }
            continue;
        }
        let session = field(line, "session");
        let peer = session
            .and_then(|s| owner.get(s).cloned())
            .unwrap_or_else(|| UNLABELLED.to_string());
        let session = session.unwrap_or_default().to_string();
        if line.contains("a path joined the session") {
            if let (Some(path), Some(kind)) = (field(line, "path"), field(line, "kind")) {
                kinds.insert((session.clone(), path.to_string()), kind.to_string());
            }
        } else if line.contains("a path of the session ended") {
            let kind = field(line, "path")
                .and_then(|path| kinds.get(&(session.clone(), path.to_string())))
                .map_or("path", String::as_str);
            let why = field(line, "why").map(numbers_out).unwrap_or_default();
            *paths
                .entry(peer.clone())
                .or_default()
                .entry(format!("{kind}: {why}"))
                .or_default() += 1;
        } else if line.contains("the session's paths changed") {
            let none = field(line, "network") == Some("false")
                && field(line, "bluetooth") == Some("false");
            match (none, at) {
                (true, Some(at)) => {
                    pathless_since.entry(session.clone()).or_insert(at);
                }
                (false, Some(at)) => {
                    if let Some(start) = pathless_since.remove(&session) {
                        let gap = (at - start).max(0.0).round() as u64;
                        let report = out.entry(peer.clone()).or_default();
                        report.pathless_secs += gap;
                        report.longest_pathless_secs = report.longest_pathless_secs.max(gap);
                    }
                }
                _ => {}
            }
        } else if line.contains("the session can no longer be resumed") {
            if let (Some(start), Some(at)) = (pathless_since.remove(&session), at) {
                let gap = (at - start).max(0.0).round() as u64;
                let report = out.entry(peer.clone()).or_default();
                report.pathless_secs += gap;
                report.longest_pathless_secs = report.longest_pathless_secs.max(gap);
            }
            let why = field(line, "why").map(numbers_out).unwrap_or_default();
            *ended
                .entry(peer.clone())
                .or_default()
                .entry(why)
                .or_default() += 1;
        }
        out.entry(peer).or_default();
    }

    let mut peers: Vec<PeerReport> = out
        .into_iter()
        .map(|(name, mut report)| {
            report.sessions_ended = ranked(ended.remove(&name).unwrap_or_default(), usize::MAX);
            report.paths_ended = ranked(paths.remove(&name).unwrap_or_default(), usize::MAX);
            report.name = name;
            report
        })
        .filter(|p| p.sessions > 0 || !p.paths_ended.is_empty() || !p.sessions_ended.is_empty())
        .collect();
    peers.sort_by(|a, b| a.name.cmp(&b.name));
    peers
}

fn ranked(counts: HashMap<String, u32>, at_most: usize) -> Vec<Counted> {
    let mut out: Vec<Counted> = counts
        .into_iter()
        .map(|(what, count)| Counted { what, count })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.what.cmp(&b.what)));
    out.truncate(at_most);
    out
}

/// The level word of a log line: `INFO`, `WARN`, …
fn level_of(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    leading_timestamp(words.next()?)?;
    words.next()
}

/// A field's value from a log line: `key=value`, `key="a value"`, or a
/// displayed value with spaces in it (`why=silent for 3097 ms`), which runs
/// to the next field or the end of the line.
pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    if let Some(quoted) = rest.strip_prefix('"') {
        return quoted.find('"').map(|end| &quoted[..end]);
    }
    let end = rest
        .match_indices(' ')
        .map(|(i, _)| i)
        .find(|&i| {
            let next = &rest[i + 1..];
            next.find('=').is_some_and(|eq| {
                eq > 0
                    && next[..eq]
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '_')
            })
        })
        .unwrap_or(rest.len());
    Some(rest[..end].trim_end())
}

/// A warning as its kind: the target and message, without the timestamp and
/// fields, so every occurrence of one warning counts as one.
fn template(line: &str) -> String {
    let mut words = line.splitn(3, char::is_whitespace);
    words.next();
    // A span's fields reach the file with their colour codes even though the
    // file writer asks for none.
    let rest = without_ansi(&words.collect::<Vec<_>>().join(" "));
    let rest = without_link_span(rest.trim_start());
    let mut out = String::new();
    let mut chars = rest.trim_start().chars().peekable();
    let mut word = String::new();
    while let Some(c) = chars.next() {
        if c == ' ' {
            push_word(&mut out, &mut word);
            continue;
        }
        word.push(c);
        // A field: skip its value, a quoted one whole.
        if c == '='
            && word
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c == '=')
        {
            if chars.peek() == Some(&'"') {
                chars.next();
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                }
            } else {
                while chars.peek().is_some_and(|c| *c != ' ') {
                    chars.next();
                }
            }
            word.clear();
        }
    }
    push_word(&mut out, &mut word);
    // A reading in the message ("waited 24% of the last 10 s") would make each
    // value its own kind.
    let mut out: String = numbers_out(&out).chars().take(160).collect();
    out.truncate(out.trim_end().len());
    out
}

/// `WARN peer{name="Lightmoon"}: target: …` as `WARN target: …`: one
/// warning is one kind whichever peer's link wrote it (`crate::link::link_span`).
fn without_link_span(text: &str) -> String {
    let (level, rest) = text.split_once(' ').unwrap_or((text, ""));
    let rest = rest.trim_start();
    let rest = if let Some(after) = rest.strip_prefix("peer{") {
        after.split_once("}: ").map_or(rest, |(_, tail)| tail)
    } else {
        rest.strip_prefix("peer: ").unwrap_or(rest)
    };
    format!("{level} {rest}")
}

/// `text` without terminal colour sequences (`ESC [ … m`).
fn without_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn push_word(out: &mut String, word: &mut String) {
    if !word.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        word.clear();
    }
}

/// `silent for 3097 ms` → `silent for N ms`, so a reason counts as one.
fn numbers_out(text: &str) -> String {
    let mut out = String::new();
    let mut in_number = false;
    for c in text.chars() {
        if c.is_ascii_digit() || (in_number && c == '.') {
            if !in_number {
                out.push('N');
                in_number = true;
            }
        } else {
            in_number = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: &str = "\
2026-10-31T16:00:00.000000Z  INFO cursio: listening for peers bound=0.0.0.0:46073
2026-10-31T16:00:01.000000Z  INFO cursio_core::registry: holding the session with this peer peer=\"POCO X7 Pro\" session=aa11bb22
2026-10-31T16:00:01.100000Z  INFO cursio_transport::resume::inner: a path joined the session session=aa11bb22 path=1 kind=Bluetooth
2026-10-31T16:00:05.000000Z  INFO cursio_transport::resume::inner: a path of the session ended session=aa11bb22 path=1 why=silent for 3097 ms
2026-10-31T16:00:05.000100Z  INFO cursio_transport::resume::inner: the session's paths changed session=aa11bb22 network=false bluetooth=false
2026-10-31T16:00:15.000000Z  INFO cursio_transport::resume::inner: the session's paths changed session=aa11bb22 network=false bluetooth=true
2026-10-31T16:00:20.000000Z  WARN cursio_core::module_host: module pump was slow; only this module's traffic waited module=\"usb\" took_ms=79
2026-10-31T16:00:23.000000Z  WARN cursio_core::module_host: module pump was slow; only this module's traffic waited module=\"usb\" took_ms=112
2026-10-31T16:00:30.000000Z  INFO cursio_transport::resume::inner: the session's paths changed session=aa11bb22 network=false bluetooth=false
2026-10-31T16:01:30.000000Z  INFO cursio_transport::resume::inner: the session can no longer be resumed; ending it session=aa11bb22 why=no connection came back within 60 s
";

    fn day() -> NodeReport {
        day_of(DAY)
    }

    fn day_of(log: &str) -> NodeReport {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("cursio-log-report-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cursio.log.2026-10-31"), log).unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_466_000);
        let report = build(&dir, "cursio.log", Duration::from_secs(3600), now).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        report
    }

    #[test]
    fn the_sessions_lines_read_alike_from_the_channel_and_from_older_logs() {
        let channel = DAY.replace(
            "cursio_transport::resume::inner",
            "cursio_channel::session::inner",
        );
        assert_eq!(day_of(&channel).peers, day().peers);
        assert!(!day().peers.is_empty());
    }

    #[test]
    fn a_peers_paths_and_sessions_are_counted_under_its_name() {
        let report = day();
        assert_eq!(report.starts, 1);
        let poco = &report.peers[0];
        assert_eq!(poco.name, "POCO X7 Pro");
        assert_eq!(poco.sessions, 1);
        assert_eq!(poco.paths_ended[0].what, "Bluetooth: silent for N ms");
        assert_eq!(
            poco.sessions_ended[0].what,
            "no connection came back within N s"
        );
        assert_eq!(poco.pathless_secs, 70);
        assert_eq!(poco.longest_pathless_secs, 60);
    }

    #[test]
    fn a_repeated_warning_counts_as_one_kind_and_a_slow_module_keeps_its_worst() {
        let report = day();
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].count, 2);
        assert_eq!(
            report.warnings[0].what,
            "WARN cursio_core::module_host: module pump was slow; only this module's traffic waited"
        );
        assert_eq!(
            report.slow_modules,
            vec![SlowModule {
                module: "usb".into(),
                count: 2,
                max_ms: 112
            }]
        );
    }

    #[test]
    fn readings_and_colour_codes_do_not_split_one_warning_into_many() {
        let a = template("2026-10-31T16:00:00Z  WARN cursio_core::diagnostics: this machine is stalling on disk i/o: everything waited 24% of the last 10 s problem=machine.io");
        let b = template("2026-10-31T16:00:05Z  WARN cursio_core::diagnostics: this machine is stalling on disk i/o: everything waited 60% of the last 10 s problem=machine.io");
        assert_eq!(a, b);
        let coloured = template("2026-10-31T16:00:00Z  WARN {}{\u{1b}[3mtask_name\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"x\"}: zbus::proxy: failed");
        assert!(!coloured.contains('\u{1b}'));
        let lightmoon = template("2026-10-31T16:00:00Z  WARN peer{name=\"Lightmoon\"}: cursio_core::module_host: module pump was slow; only this module's traffic waited module=\"usb\" took_ms=79");
        let poco = template("2026-10-31T16:00:00Z  WARN peer{name=\"POCO X7 Pro\"}: cursio_core::module_host: module pump was slow; only this module's traffic waited module=\"usb\" took_ms=79");
        assert_eq!(lightmoon, poco);
        assert!(lightmoon.starts_with("WARN cursio_core::module_host:"));
    }

    #[test]
    fn a_field_is_read_whether_quoted_or_not() {
        let line =
            "2026-10-31T16:00:01Z  INFO x: y peer=\"POCO X7 Pro\" session=aa11 why=silent for 3 ms";
        assert_eq!(field(line, "peer"), Some("POCO X7 Pro"));
        assert_eq!(field(line, "session"), Some("aa11"));
        assert_eq!(field(line, "why"), Some("silent for 3 ms"));
        assert_eq!(field(line, "kind"), None);
    }
}
