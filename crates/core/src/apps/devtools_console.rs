//! The web view's console as the application's `console` log, kept by the
//! node on the machine the application runs on while a DevTools session of
//! it is allowed (`docs/devtools-design.md`, "The console as a log").
//!
//! `web_console` sees only what is logged while it waits; an error that
//! happened between two looks, or before anybody looked, was gone. So while
//! some machine holds a grant to inspect an application, its node opens a
//! DevTools connection of its own to the page — straight to the loopback
//! port, no tunnel — and writes every console call, uncaught exception and
//! browser log entry through [`super::app_logs`] as process `console`. The
//! lines carry the page's own time, so `cursio logs <m> --app <id>` merges
//! them with the application's other processes and with other machines.
//!
//! **Only while a grant holds.** The watcher starts with the first stream a
//! granted machine opens and checks every few seconds that one of the
//! machines it watches for is still allowed; Stop and the end of an hour end
//! it as they end the streams. It reads nothing the inspecting machine could
//! not read itself; it only keeps what would otherwise scroll away.
//!
//! **Replays are not written twice.** `Runtime.enable` hands over the
//! messages the page logged before, so a watcher started again — the app
//! reloaded, a second session — would repeat them. The newest page time
//! written per application is kept for the node's life, and nothing at or
//! before it is written again; a restarted node may repeat what was
//! replayed to it once.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cursio_app_protocol::{AppId, ProcessName};

use super::Key;
use crate::mcp::cdp::{console_entry, Cdp, ConsoleEntry};

/// The process name the console's lines are kept under.
pub const PROCESS: &str = "console";

/// How often the watcher checks the grant, and how long it waits before
/// connecting again after the page went away.
const RECHECK: Duration = Duration::from_secs(3);

struct Watch {
    /// Machines whose grant keeps the watcher going.
    peers: Vec<String>,
    /// The newest page time written, in milliseconds.
    written_ms: Option<f64>,
    running: bool,
}

fn watches() -> MutexGuard<'static, HashMap<AppId, Watch>> {
    static WATCHES: OnceLock<Mutex<HashMap<AppId, Watch>>> = OnceLock::new();
    WATCHES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// `peer` opened a DevTools stream into `app` (`process`'s page when named):
/// keep its console while that machine is allowed.
pub fn watch(peer: &str, app: &AppId, process: Option<ProcessName>) {
    {
        let mut watches = watches();
        let watch = watches.entry(app.clone()).or_insert(Watch {
            peers: Vec::new(),
            written_ms: None,
            running: false,
        });
        if !watch.peers.iter().any(|p| p == peer) {
            watch.peers.push(peer.to_owned());
        }
        if watch.running {
            return;
        }
        watch.running = true;
    }
    let app = app.clone();
    super::tunnel::runtime().spawn(async move {
        loop {
            run(&app, process.as_ref()).await;
            let mut watches = watches();
            let Some(watch) = watches.get_mut(&app) else {
                break;
            };
            // A machine allowed between the last check and here would
            // otherwise find a watcher marked running that has just ended.
            if watch.peers.is_empty() {
                watch.running = false;
                break;
            }
        }
        tracing::info!(%app, "stopped keeping the web view's console: no machine is allowed to inspect it any more");
    });
}

/// Whether any machine the watcher of `app` serves is still allowed.
fn still_wanted(app: &AppId) -> bool {
    let mut watches = watches();
    let Some(watch) = watches.get_mut(app) else {
        return false;
    };
    watch
        .peers
        .retain(|peer| super::devtools::still_allowed(peer, app));
    !watch.peers.is_empty()
}

async fn run(app: &AppId, process: Option<&ProcessName>) {
    let key = Key {
        app: app.clone(),
        process: Some(ProcessName::parse(PROCESS).expect("a valid process name")),
    };
    let spec = super::app_logs::app_logs().spec(&key);
    if !super::registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .kept_without_process(&key, spec)
    {
        tracing::warn!(%app, "not keeping the web view's console: a process of it named {PROCESS} keeps a log file of its own");
        if let Some(watch) = watches().get_mut(app) {
            watch.peers.clear();
        }
        return;
    }
    while still_wanted(app) {
        let page = match super::devtools::endpoint(app, process) {
            Ok(super::devtools::Endpoint::Port(port)) => Cdp::local(port).await,
            Ok(super::devtools::Endpoint::Abstract { name, .. }) => Err(format!(
                "the console is read only from a DevTools port, and {app} declares the socket @{name}"
            )),
            Err(why) => Err(why),
        };
        let mut page = match page {
            Ok(page) => page,
            Err(why) => {
                tracing::debug!(%app, %why, "cannot read the web view's console now");
                tokio::time::sleep(RECHECK).await;
                continue;
            }
        };
        if let Err(why) = enable(&mut page).await {
            tracing::warn!(%app, %why, "the web view refused to report its console");
            tokio::time::sleep(RECHECK).await;
            continue;
        }
        tracing::info!(%app, "keeping the web view's console as {app}/{PROCESS} while it is inspected");
        loop {
            let open = page.listen(RECHECK).await;
            write(&key, app, std::mem::take(&mut page.events));
            if !open || !still_wanted(app) {
                break;
            }
        }
    }
}

async fn enable(page: &mut Cdp) -> Result<(), String> {
    page.call("Runtime.enable", serde_json::json!({})).await?;
    page.call("Log.enable", serde_json::json!({})).await?;
    Ok(())
}

fn write(key: &Key, app: &AppId, events: Vec<serde_json::Value>) {
    let now = SystemTime::now();
    let lines: Vec<String> = {
        let mut watches = watches();
        let Some(watch) = watches.get_mut(app) else {
            return;
        };
        let mut lines = Vec::new();
        for entry in events.iter().filter_map(console_entry) {
            let at = entry.at_ms.unwrap_or_else(|| millis(now));
            if watch.written_ms.is_some_and(|written| at <= written) {
                continue;
            }
            watch.written_ms = Some(at);
            lines.push(log_line(&entry, now));
        }
        lines
    };
    if lines.is_empty() {
        return;
    }
    if let Err(error) = super::app_logs::app_logs().append(key, &lines, 0, now) {
        tracing::warn!(%app, %error, "cannot keep the web view's console lines");
    }
}

fn millis(at: SystemTime) -> f64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// `entry` as a line of Cursio's log format: the page's time in UTC to the
/// microsecond (`now` when the page gave none), the level, the text.
pub fn log_line(entry: &ConsoleEntry, now: SystemTime) -> String {
    let at = match entry.at_ms {
        Some(ms) if ms.is_finite() && ms >= 0.0 => {
            UNIX_EPOCH + Duration::from_micros((ms * 1000.0) as u64)
        }
        _ => now,
    };
    let micros = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_micros());
    format!(
        "{}.{micros:06}Z {:>5} {}",
        crate::log_tail::timestamp(at),
        entry.level,
        entry.text
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_console_line_carries_the_pages_time_and_level() {
        let entry = ConsoleEntry {
            level: "WARN",
            text: "console.warning: slow".into(),
            at_ms: Some(1_761_998_400_123.5),
        };
        let line = log_line(&entry, UNIX_EPOCH);
        assert_eq!(
            line,
            "2025-11-01T12:00:00.123500Z  WARN console.warning: slow"
        );
        assert_eq!(
            crate::log_tail::leading_timestamp(&line),
            Some("2025-11-01T12:00:00.123500Z")
        );
    }

    #[test]
    fn a_console_line_without_a_time_takes_now() {
        let entry = ConsoleEntry {
            level: "ERROR",
            text: "exception: boom".into(),
            at_ms: None,
        };
        let now = UNIX_EPOCH + Duration::from_secs(1_761_998_400);
        assert_eq!(
            log_line(&entry, now),
            "2025-11-01T12:00:00.000000Z ERROR exception: boom"
        );
    }

    #[test]
    fn the_console_process_name_is_valid() {
        assert!(ProcessName::parse(PROCESS).is_ok());
    }
}
