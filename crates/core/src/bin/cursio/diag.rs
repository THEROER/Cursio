//! `cursio logs` and `cursio path`: the mesh's machines read and steered from
//! this one, through the running node.
//!
//! Several machines' logs are merged by each line's timestamp, so the two ends
//! of one event read in order. That trusts the machines' clocks to agree to
//! well under the gaps that matter; each line keeps its machine's name, so a
//! skewed clock shows as a machine whose lines all sit early or late.

use std::time::Duration;

use cursio_core::control::{Command, Presence, Reply};
use cursio_core::log_tail::leading_timestamp;
use cursio_protocol::diag::{PathChoice, PathReport};

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

/// One machine to read: `None` is this one, otherwise a peer's name or id.
struct Source {
    machine: Option<String>,
    label: String,
    /// The last line shown, which a follower asks for lines after.
    mark: Option<String>,
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
                    // `mount-helper` is read as `cursio-mount-helper`, the
                    // name the helper's log file carries.
                    log: args.log.as_ref().map(|log| {
                        if log.starts_with("cursio") {
                            log.clone()
                        } else {
                            format!("cursio-{log}")
                        }
                    }),
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
            // Stable, so a machine's own continuation lines stay under the
            // line they belong to.
            merged.sort_by(|a, b| {
                let at = |line: &str| leading_timestamp(line).map(str::to_owned);
                match (at(&a.1), at(&b.1)) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    _ => std::cmp::Ordering::Equal,
                }
            });
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
    let this = |label: String| Source {
        machine: None,
        label,
        mark: None,
    };
    let status = match client.request(Command::Status).await? {
        Reply::Status(status) => status,
        other => anyhow::bail!("the running node answered status with {other:?}"),
    };
    let own = status.node.name.clone();
    if names.is_empty() {
        return Ok(vec![this(own)]);
    }
    let mut out = Vec::new();
    for name in names {
        if name == "all" {
            out.push(this(own.clone()));
            for peer in &status.peers {
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
        } else if name == "this" || name.eq_ignore_ascii_case(&own) {
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
fn parse_since(text: &str) -> anyhow::Result<u64> {
    let (number, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let n: u64 = number.parse().map_err(|_| {
        anyhow::anyhow!("--since takes a number and a unit, like 10m; got {text:?}")
    })?;
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => anyhow::bail!("--since takes s, m, h or d; got {text:?}"),
    };
    Ok(n * scale)
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
