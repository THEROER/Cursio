//! `cursio logs` and `cursio path`: the mesh's machines read and steered from
//! this one, through the running node.
//!
//! Several machines' logs are merged by each line's timestamp, so the two ends
//! of one event read in order. That trusts the machines' clocks to agree to
//! well under the gaps that matter; each line keeps its machine's name, so a
//! skewed clock shows as a machine whose lines all sit early or late.

use std::time::Duration;

use cursio_core::control::query::{log_name, merge_by_time, Source};
use cursio_core::control::{Command, Reply};
use cursio_core::log_tail::leading_timestamp;
use cursio_protocol::diag::{Maintenance, NodeReport, PathChoice, PathReport};

use crate::cli::PathArg;
use crate::client::control_client;

/// How often `--follow` asks again.
const FOLLOW_EVERY: Duration = Duration::from_secs(1);
/// How far back a follower that has no mark yet asks, in seconds.
const FOLLOW_WINDOW: u64 = 3;

/// What `cursio logs` was asked for.
pub(crate) struct LogsArgs {
    pub(crate) machines: Vec<String>,
    pub(crate) lines: u32,
    pub(crate) since: Option<String>,
    pub(crate) grep: Option<String>,
    pub(crate) follow: bool,
    pub(crate) log: Option<String>,
}

pub(crate) async fn logs(args: LogsArgs) -> anyhow::Result<()> {
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!("no running node to read logs through ({e}); start `cursio mesh` first")
    })?;
    let within = args.since.as_deref().map(parse_since).transpose()?;
    let mut sources = sources(&client, &args.machines).await?;
    let tag = sources.len() > 1;
    let mut first = true;
    loop {
        let mut merged: Vec<(String, String)> = Vec::new();
        for source in &mut sources {
            let reply = client
                .request(Command::Logs {
                    machine: source.machine.clone(),
                    after: source.mark.clone(),
                    // A follower that has seen no line yet has no mark to
                    // ask after; a short window, rather than none, keeps a
                    // filter that matches nothing from rescanning the whole
                    // log every second.
                    within_secs: match (first, &source.mark) {
                        (true, _) => within,
                        (false, None) => Some(FOLLOW_WINDOW),
                        (false, Some(_)) => None,
                    },
                    contains: args.grep.clone(),
                    max_lines: args.lines,
                    log: args.log.as_deref().map(log_name),
                })
                .await?;
            match reply {
                Reply::LogLines { lines, truncated } => {
                    if truncated && first && !args.follow {
                        eprintln!("({}: older lines left out)", source.label);
                    }
                    if let Some(last) = lines.iter().rev().find_map(|l| leading_timestamp(l)) {
                        source.mark = Some(last.to_string());
                    }
                    merged.extend(lines.into_iter().map(|line| (source.label.clone(), line)));
                }
                Reply::Error { message } => eprintln!("{}: {message}", source.label),
                other => anyhow::bail!(
                    "the running node does not know how to read logs ({other:?}); restart it with this version of Cursio"
                ),
            }
        }
        if tag {
            merge_by_time(&mut merged);
        }
        for (label, line) in merged {
            if tag {
                println!("{label:<12} {line}");
            } else {
                println!("{line}");
            }
        }
        if !args.follow {
            return Ok(());
        }
        first = false;
        tokio::time::sleep(FOLLOW_EVERY).await;
    }
}

/// The machines `names` asks for, resolving `this` and `all`.
async fn sources(
    client: &cursio_core::control::transport::Client,
    names: &[String],
) -> anyhow::Result<Vec<Source>> {
    cursio_core::control::query::sources(client, names)
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

/// `30s`, `10m`, `2h`, `1d` as seconds.
fn parse_since(text: &str) -> anyhow::Result<u64> {
    cursio_core::control::query::parse_since(text).map_err(|e| anyhow::anyhow!("--since: {e}"))
}

pub(crate) async fn report(machines: &[String], since: &str) -> anyhow::Result<()> {
    let client = control_client()
        .await
        .map_err(|e| anyhow::anyhow!("no running node to ask ({e}); start `cursio mesh` first"))?;
    let since_secs = parse_since(since)?;
    for source in sources(&client, machines).await? {
        let reply = client
            .request(Command::Report {
                machine: source.machine.clone(),
                since_secs,
            })
            .await?;
        match reply {
            Reply::Report(report) => print_report(&source.label, since, &report),
            Reply::Error { message } => println!("== {}: {message}\n", source.label),
            other => anyhow::bail!(
                "the running node does not know how to report ({other:?}); restart it with this version of Cursio"
            ),
        }
    }
    Ok(())
}

fn print_report(label: &str, since: &str, report: &NodeReport) {
    let from = report
        .from
        .as_deref()
        .map(|f| f.get(..16).unwrap_or(f))
        .unwrap_or("-");
    let cut = if report.truncated {
        ", older lines unread"
    } else {
        ""
    };
    println!(
        "== {label} — last {since}, counted from {from}{cut}; {} start(s)",
        report.starts
    );
    let m = &report.memory;
    println!(
        "memory: node holds {}, peak {}, {} threads",
        size(m.held),
        size(m.peak),
        m.threads.map_or("?".to_string(), |t| t.to_string())
    );
    if !report.top.is_empty() {
        let top: Vec<String> = report
            .top
            .iter()
            .map(|p| format!("{} {}", p.name, size(Some(p.bytes))))
            .collect();
        println!("largest: {}", top.join(", "));
    }
    for problem in &report.problems {
        println!("problem: {problem}");
    }
    for peer in &report.peers {
        println!(
            "peer {}: {} session(s), {} with no path (longest {})",
            peer.name,
            peer.sessions,
            span(peer.pathless_secs),
            span(peer.longest_pathless_secs)
        );
        for c in &peer.sessions_ended {
            println!("    session ended {:>5}× {}", c.count, c.what);
        }
        for c in peer.paths_ended.iter().take(6) {
            println!("    path ended    {:>5}× {}", c.count, c.what);
        }
    }
    for s in &report.slow_modules {
        println!(
            "slow module {}: {}× (worst {} ms)",
            s.module, s.count, s.max_ms
        );
    }
    for w in &report.warnings {
        println!("{:>6}× {}", w.count, w.what);
    }
    println!();
}

fn size(bytes: Option<u64>) -> String {
    match bytes {
        None => "?".into(),
        Some(b) if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        Some(b) => format!("{} MiB", b / (1 << 20)),
    }
}

fn span(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

pub(crate) async fn restart(machine: Option<&str>) -> anyhow::Result<()> {
    let client = control_client()
        .await
        .map_err(|e| anyhow::anyhow!("no running node to ask ({e}); start `cursio mesh` first"))?;
    let command = match machine {
        None => Command::Restart,
        Some(machine) => Command::Maintain {
            machine: machine.to_string(),
            what: Maintenance::Restart,
        },
    };
    match client.request(command).await? {
        Reply::Done => {
            println!("{} is restarting", machine.unwrap_or("this node"));
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the running node answered {other:?}"),
    }
}

pub(crate) async fn apply_update(machine: &str) -> anyhow::Result<()> {
    let client = control_client()
        .await
        .map_err(|e| anyhow::anyhow!("no running node to ask ({e}); start `cursio mesh` first"))?;
    match client
        .request(Command::Maintain {
            machine: machine.to_string(),
            what: Maintenance::ApplyUpdate,
        })
        .await?
    {
        Reply::Done => {
            println!("{machine} is switching to its waiting build");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the running node answered {other:?}"),
    }
}

pub(crate) async fn path(machine: &str, choice: Option<PathArg>) -> anyhow::Result<()> {
    let client = control_client()
        .await
        .map_err(|e| anyhow::anyhow!("no running node to ask ({e}); start `cursio mesh` first"))?;
    let reply = client
        .request(Command::Paths {
            peer: machine.to_string(),
            choice: choice.map(PathChoice::from),
        })
        .await?;
    match reply {
        Reply::Paths {
            here,
            there,
            there_error,
        } => {
            println!("this end:  {}", describe(&here));
            match (there, there_error) {
                (Some(there), _) => println!("{machine}: {}", describe(&there)),
                (None, Some(why)) => println!("{machine}: not asked: {why}"),
                (None, None) => println!("{machine}: no answer"),
            }
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to report paths ({other:?}); restart it with this version of Cursio"
        ),
    }
}

fn describe(report: &PathReport) -> String {
    let have = match (report.network, report.bluetooth) {
        (true, true) => "network + bluetooth",
        (true, false) => "network",
        (false, true) => "bluetooth",
        (false, false) => "no path",
    };
    let carrying = report
        .carrying
        .as_deref()
        .unwrap_or("nothing (waiting for a path)");
    let choice = match report.choice {
        PathChoice::Auto => "auto",
        PathChoice::Network => "network (by hand)",
        PathChoice::Bluetooth => "bluetooth (by hand)",
    };
    format!("has {have}, input over {carrying}, choice {choice}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_read_in_the_units_people_type() {
        assert_eq!(parse_since("90").unwrap(), 90);
        assert_eq!(parse_since("10m").unwrap(), 600);
        assert_eq!(parse_since("2h").unwrap(), 7200);
        assert!(parse_since("ten").is_err());
        assert!(parse_since("5w").is_err());
    }
}
