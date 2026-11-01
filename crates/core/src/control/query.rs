//! Asking the running node about the mesh, shared by the CLI (`cursio logs`,
//! `cursio report`) and the agent tools (`cursio mcp`).
//!
//! Both read the same machines through the same control commands; written
//! twice they had drifted in how `all` was resolved and how a log's name was
//! completed. Nothing here prints: the CLI prints, the MCP server answers.

use std::future::Future;

use super::{Command, Presence, Reply};
use crate::log_tail::leading_timestamp;

/// Something that puts control commands to a node: the socket client, or a
/// fake in tests.
pub trait Ask {
    fn ask(&self, command: Command) -> impl Future<Output = Result<Reply, String>> + Send;
}

impl Ask for super::transport::Client {
    async fn ask(&self, command: Command) -> Result<Reply, String> {
        self.request(command).await.map_err(|e| e.to_string())
    }
}

/// One machine to read: `machine: None` is this one, otherwise a peer's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub machine: Option<String>,
    pub label: String,
    /// The last line seen, which a follower asks for lines after.
    pub mark: Option<String>,
}

/// The machines `names` asks for: this one when empty, `this`, a name, or
/// `all` — this machine and every connected one that answers diagnostics.
pub async fn sources(ask: &impl Ask, names: &[String]) -> Result<Vec<Source>, String> {
    let this = |label: String| Source {
        machine: None,
        label,
        mark: None,
    };
    // Only `this`, `all` and no name at all need the node's status; for a
    // named machine it only tells whether that name is this one.
    let needs = names.is_empty() || names.iter().any(|n| n == "all" || n == "this");
    let status = match ask.ask(Command::Status).await {
        Ok(Reply::Status(status)) => Some(status),
        Ok(other) if needs => {
            return Err(format!("the running node answered status with {other:?}"))
        }
        Err(e) if needs => return Err(e),
        _ => None,
    };
    let own = status
        .as_ref()
        .map(|s| s.node.name.clone())
        .unwrap_or_default();
    if names.is_empty() {
        return Ok(vec![this(own)]);
    }
    let mut out = Vec::new();
    for name in names {
        if name == "all" {
            out.push(this(own.clone()));
            for peer in status.iter().flat_map(|s| &s.peers) {
                let answers = peer.presence == Presence::Online
                    && peer
                        .link
                        .as_ref()
                        .is_some_and(|l| l.modules.iter().any(|m| m == "diag"));
                if answers {
                    out.push(Source {
                        machine: Some(peer.name.clone()),
                        label: peer.name.clone(),
                        mark: None,
                    });
                }
            }
        } else if name == "this" || (!own.is_empty() && name.eq_ignore_ascii_case(&own)) {
            out.push(this(own.clone()));
        } else {
            out.push(Source {
                machine: Some(name.clone()),
                label: name.clone(),
                mark: None,
            });
        }
    }
    Ok(out)
}

/// `30s`, `10m`, `2h`, `1d` as seconds.
pub fn parse_since(text: &str) -> Result<u64, String> {
    let (number, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let n: u64 = number
        .parse()
        .map_err(|_| format!("a span is a number and a unit, like 10m; got {text:?}"))?;
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("a span takes s, m, h or d; got {text:?}")),
    };
    Ok(n * scale)
}

/// The name a node reads a log by: `mount-helper` is `cursio-mount-helper`,
/// the name its file carries; `cursio…` and an application's `app:<id>` pass
/// as they are.
pub fn log_name(log: &str) -> String {
    if log.starts_with("cursio") || log.starts_with("app:") {
        log.to_owned()
    } else {
        format!("cursio-{log}")
    }
}

/// Order several machines' `(label, line)` by each line's timestamp. Stable,
/// so a line without one (a continuation) stays under the line it belongs to.
pub fn merge_by_time(lines: &mut [(String, String)]) {
    lines.sort_by(|a, b| {
        let at = |line: &str| leading_timestamp(line).map(str::to_owned);
        match (at(&a.1), at(&b.1)) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => std::cmp::Ordering::Equal,
        }
    });
}

/// Lines of `log` (the node's own when `None`) from each of `machines`,
/// newer than `within_secs` and containing `contains`, at most `max_lines`
/// per machine, merged by time and labelled when there are several. A
/// machine that cannot answer is reported in place of its lines.
pub async fn read_logs(
    ask: &impl Ask,
    machines: &[String],
    within_secs: Option<u64>,
    contains: Option<String>,
    max_lines: u32,
    log: Option<&str>,
) -> Result<Vec<String>, String> {
    let sources = sources(ask, machines).await?;
    let tag = sources.len() > 1;
    let mut merged = Vec::new();
    let mut notes = Vec::new();
    for source in &sources {
        let reply = ask
            .ask(Command::Logs {
                machine: source.machine.clone(),
                after: None,
                within_secs,
                contains: contains.clone(),
                max_lines,
                log: log.map(log_name),
            })
            .await?;
        match reply {
            Reply::LogLines { lines, truncated } => {
                if truncated {
                    notes.push(format!("({}: older lines left out)", source.label));
                }
                merged.extend(lines.into_iter().map(|l| (source.label.clone(), l)));
            }
            Reply::Error { message } => notes.push(format!("{}: {message}", source.label)),
            other => return Err(format!("the running node answered logs with {other:?}")),
        }
    }
    if tag {
        merge_by_time(&mut merged);
    }
    Ok(notes
        .into_iter()
        .chain(merged.into_iter().map(|(label, line)| match tag {
            true => format!("{label:<12} {line}"),
            false => line,
        }))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_reads_as_seconds_in_any_of_its_units() {
        assert_eq!(parse_since("90").unwrap(), 90);
        assert_eq!(parse_since("10m").unwrap(), 600);
        assert_eq!(parse_since("2h").unwrap(), 7200);
        assert_eq!(parse_since("1d").unwrap(), 86_400);
        assert!(parse_since("ten").is_err());
        assert!(parse_since("5w").is_err());
    }

    #[test]
    fn a_short_log_name_is_completed_and_an_application_s_kept() {
        assert_eq!(log_name("mount-helper"), "cursio-mount-helper");
        assert_eq!(log_name("cursio-app"), "cursio-app");
        assert_eq!(log_name("app:leavepulse"), "app:leavepulse");
    }

    #[test]
    fn lines_of_several_machines_read_in_time_order() {
        let mut lines = vec![
            ("b".to_owned(), "2026-11-01T10:00:02Z later".to_owned()),
            ("a".to_owned(), "2026-11-01T10:00:01Z earlier".to_owned()),
        ];
        merge_by_time(&mut lines);
        assert_eq!(lines[0].0, "a");
    }
}
