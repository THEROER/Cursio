//! Whether another machine may inspect an application's DevTools, and where
//! that endpoint really is — the owning side of `docs/devtools-design.md`,
//! rung 5.
//!
//! DevTools run any code inside the application and read what it stores, so
//! this is the strictest consent the node asks for:
//!
//! - `[apps.<id>] devtools` is `deny` until this machine's person changes it;
//!   a refused request says so and offers, as a button, to start asking.
//! - Under `ask` every request from a machine not allowed right now raises a
//!   problem whose buttons allow it **for a while**: an hour, or until the
//!   application restarts (the grant names the processes running then). There
//!   is no "always". A grant is written as a config edit — the buttons are
//!   config edits everywhere the app shows a problem — but it carries its own
//!   end and is worthless after it.
//! - Per machine: allowing Nocturne allows nothing for dark.
//!
//! **Only the application's own port.** An application declares a port in its
//! hello; before anything is forwarded the node checks, in the system's socket
//! table, that one of the application's registered processes is what listens
//! on it, and on loopback. Without that any program of the user could declare
//! any local service — a database, another program's debugger — and have the
//! mesh carry it.

use std::time::{SystemTime, UNIX_EPOCH};

use cursio_app_protocol::{AppId, Devtools, ProcessName};

use crate::config::{ConfigEdit, DevtoolsConsent, DevtoolsGrant};
use crate::diagnostics::{self, Problem, ProblemAction, Severity};

/// How long "allow for an hour" allows.
pub const HOUR: u64 = 60 * 60;

/// The longest "until it restarts" allows: an application left running for
/// a week does not keep a machine allowed for that week.
pub const UNTIL_RESTART_AT_MOST: u64 = 24 * 60 * 60;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn request_problem(app: &AppId) -> String {
    format!("app.{app}.devtools")
}

/// What this machine's settings answer for `peer` inspecting `app` now.
pub enum Verdict {
    Allowed,
    /// Refused; the problem raised (if any) holds the buttons.
    Refused(String),
}

/// Decide, from `consent`, the grants and the running `pids`, whether `peer`
/// may inspect `app` at `now` — and, when it may not, what to ask the person.
pub fn decide(
    app: &AppId,
    peer: &str,
    consent: DevtoolsConsent,
    grants: &[DevtoolsGrant],
    pids: &[u32],
    now: u64,
) -> (Verdict, Option<Problem>) {
    if grants.iter().any(|g| g.peer == peer && g.holds(now, pids)) {
        return (Verdict::Allowed, None);
    }
    let edit = |label: String, edit: ConfigEdit| ProblemAction { label, edit };
    match consent {
        DevtoolsConsent::Deny => (
            Verdict::Refused(format!(
                "DevTools of {app} are off on that machine; its person can let machines ask \
                 in Cursio there"
            )),
            Some(Problem {
                id: request_problem(app),
                severity: Severity::Info,
                summary: format!("{peer} asked to inspect {app}, and DevTools of it are off here"),
                hint: Some(
                    "inspecting runs any code inside the application and reads what it \
                     stores; turn asking on only for an application you develop"
                        .into(),
                ),
                actions: vec![edit(
                    format!("Ask me when a machine wants to inspect {app}"),
                    ConfigEdit::AppDevtools {
                        app: app.to_string(),
                        consent: DevtoolsConsent::Ask,
                    },
                )],
                since: SystemTime::now(),
            }),
        ),
        DevtoolsConsent::Ask => (
            Verdict::Refused(
                "waiting for that machine's person to allow it: a problem with the answers \
                 as buttons is up there"
                    .to_owned(),
            ),
            Some(Problem {
                id: request_problem(app),
                severity: Severity::Warning,
                summary: format!(
                    "{peer} asks to inspect {app}. It could run any code inside it and read \
                     what it stores, including signed-in accounts."
                ),
                hint: Some("an allowance ends by itself; Stop ends it at once".into()),
                actions: vec![
                    edit(
                        format!("Allow {peer} for 1 hour"),
                        ConfigEdit::GrantDevtools {
                            app: app.to_string(),
                            grant: DevtoolsGrant {
                                peer: peer.to_owned(),
                                until: now + HOUR,
                                pids: Vec::new(),
                            },
                        },
                    ),
                    edit(
                        format!("Allow until {app} restarts"),
                        ConfigEdit::GrantDevtools {
                            app: app.to_string(),
                            grant: DevtoolsGrant {
                                peer: peer.to_owned(),
                                until: now + UNTIL_RESTART_AT_MOST,
                                pids: pids.to_vec(),
                            },
                        },
                    ),
                    edit(
                        "Deny".into(),
                        ConfigEdit::AppDevtools {
                            app: app.to_string(),
                            consent: DevtoolsConsent::Deny,
                        },
                    ),
                ],
                since: SystemTime::now(),
            }),
        ),
    }
}

/// Whether `peer` may inspect `app` now, raising the question for this
/// machine's person when it is theirs to answer.
pub fn allowed(peer: &str, app: &AppId) -> Result<(), String> {
    let builds =
        super::builds::builds().ok_or("this machine keeps no settings for applications")?;
    let config = builds.app_config(app.as_str());
    let pids = super::registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pids(app);
    match decide(
        app,
        peer,
        config.devtools,
        &config.devtools_grants,
        &pids,
        now(),
    ) {
        (Verdict::Allowed, _) => {
            diagnostics::clear(&request_problem(app));
            Ok(())
        }
        (Verdict::Refused(why), problem) => {
            if let Some(problem) = problem {
                diagnostics::raise(problem);
            }
            Err(why)
        }
    }
}

/// Whether `peer` is still allowed, without asking anybody: what an open
/// stream checks while it lasts, so Stop and the end of a grant close it.
pub fn still_allowed(peer: &str, app: &AppId) -> bool {
    let Some(builds) = super::builds::builds() else {
        return false;
    };
    let config = builds.app_config(app.as_str());
    let pids = super::registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pids(app);
    config
        .devtools_grants
        .iter()
        .any(|g| g.peer == peer && g.holds(now(), &pids))
}

/// Part of the refusal when no running process declares DevTools, which the
/// inspecting side reads to start the application with them
/// (`cursio devtools --launch`). Older nodes say it in the same words.
pub const NOT_DECLARED: &str = "declares a DevTools endpoint";

/// Part of the refusal when the declared port is not listened on yet: what
/// an application just started answers until its web view is up.
pub const NOT_LISTENING: &str = "nothing listens on";

/// The loopback port of `app`'s DevTools (`process`'s when named), checked to
/// be listened on by that very process.
pub fn endpoint(app: &AppId, process: Option<&ProcessName>) -> Result<u16, String> {
    let (key, pid, devtools) = super::registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .devtools(app, process)
        .ok_or_else(|| {
            format!("no running process of {app} here {NOT_DECLARED} (App::devtools)")
        })?;
    let Devtools::Cdp { port } = devtools;
    match listener(port)? {
        Listener::Loopback(owner) if owner == pid => Ok(port),
        Listener::Loopback(owner) => Err(format!(
            "port {port} that {} declared is listened on by another process ({owner}); \
             nothing is forwarded",
            key.app
        )),
        Listener::Everywhere(_) => Err(format!(
            "{app}'s DevTools port {port} listens on every interface; open it on 127.0.0.1 only"
        )),
        Listener::Nobody => Err(format!(
            "{NOT_LISTENING} {app}'s declared DevTools port {port}; was it started with \
             DevTools on?"
        )),
    }
}

/// Who listens on a TCP port of this machine.
#[derive(Debug, PartialEq, Eq)]
pub enum Listener {
    Loopback(u32),
    /// On every interface, by this process.
    Everywhere(u32),
    Nobody,
}

/// Listening sockets in a `/proc/net/tcp` or `tcp6` table, as (port,
/// loopback, inode). Loopback is 127.0.0.1 or ::1; anything else bound is
/// every interface or a real address, which this treats as not loopback.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn listening(table: &str) -> Vec<(u16, bool, u64)> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (address, state, inode) = (fields.get(1)?, fields.get(3)?, fields.get(9)?);
            if *state != "0A" {
                return None;
            }
            let (ip, port) = address.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let loopback = ip == "0100007F" || ip == "00000000000000000000000001000000";
            Some((port, loopback, inode.parse().ok()?))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn listener(port: u16) -> Result<Listener, String> {
    let mut found = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(table) {
            found.extend(listening(&text).into_iter().filter(|(p, _, _)| *p == port));
        }
    }
    if found.is_empty() {
        return Ok(Listener::Nobody);
    }
    let owner = |inode: u64| -> Option<u32> {
        let needle = format!("socket:[{inode}]");
        std::fs::read_dir("/proc")
            .ok()?
            .flatten()
            .find_map(|entry| {
                let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
                let fds = std::fs::read_dir(entry.path().join("fd")).ok()?;
                fds.flatten()
                    .any(|fd| {
                        std::fs::read_link(fd.path())
                            .is_ok_and(|target| target.to_string_lossy() == needle)
                    })
                    .then_some(pid)
            })
    };
    for (_, loopback, inode) in &found {
        if let Some(pid) = owner(*inode) {
            return Ok(match loopback {
                true => Listener::Loopback(pid),
                false => Listener::Everywhere(pid),
            });
        }
    }
    Ok(Listener::Nobody)
}

#[cfg(windows)]
fn listener(port: u16) -> Result<Listener, String> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_LISTENER,
    };
    const AF_INET: u32 = 2;
    let mut size = 0u32;
    // SAFETY: the first call only reports the size; the second fills a
    // buffer of that size, read as the table it is documented to hold.
    unsafe {
        let _ = GetExtendedTcpTable(
            None,
            &mut size,
            false,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        let mut buffer = vec![0u8; size as usize + 1024];
        let status = GetExtendedTcpTable(
            Some(buffer.as_mut_ptr().cast()),
            &mut size,
            false,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if status != 0 {
            return Err(format!("cannot read the TCP table (error {status})"));
        }
        let table = &*(buffer.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
        let rows = std::slice::from_raw_parts(
            table.table.as_ptr() as *const MIB_TCPROW_OWNER_PID,
            table.dwNumEntries as usize,
        );
        let mut everywhere = None;
        for row in rows {
            let row_port = u16::from_be((row.dwLocalPort & 0xFFFF) as u16);
            if row_port != port {
                continue;
            }
            // dwLocalAddr is in network order: 127.0.0.1 reads 0x0100007F.
            if row.dwLocalAddr == 0x0100_007F {
                return Ok(Listener::Loopback(row.dwOwningPid));
            }
            everywhere = Some(row.dwOwningPid);
        }
        Ok(everywhere.map_or(Listener::Nobody, Listener::Everywhere))
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn listener(_port: u16) -> Result<Listener, String> {
    Err("forwarding DevTools is not built for this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2406 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 100 0 0 10 0
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1 0000000000000000 100 0 0 10 0
   2: 0100007F:2406 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 999 1 0000000000000000 20 4 30 10 -1";

    #[test]
    fn only_listening_sockets_count_and_loopback_is_told_apart() {
        assert_eq!(
            listening(TABLE),
            [(9222, true, 4242), (8080, false, 777)],
            "the established connection on 9222 is not a listener"
        );
    }

    fn grant(peer: &str, until: u64, pids: &[u32]) -> DevtoolsGrant {
        DevtoolsGrant {
            peer: peer.into(),
            until,
            pids: pids.to_vec(),
        }
    }

    #[test]
    fn a_grant_holds_for_its_machine_until_its_end_or_its_processes_end() {
        let app = AppId::parse("leavepulse").unwrap();
        let grants = [grant("Nocturne", 100, &[]), grant("dark", 1000, &[7])];
        let ask = DevtoolsConsent::Ask;
        assert!(matches!(
            decide(&app, "Nocturne", ask, &grants, &[7], 99).0,
            Verdict::Allowed
        ));
        assert!(
            matches!(
                decide(&app, "Nocturne", ask, &grants, &[7], 100).0,
                Verdict::Refused(_)
            ),
            "an hour's grant ends at its time"
        );
        assert!(matches!(
            decide(&app, "dark", ask, &grants, &[7], 500).0,
            Verdict::Allowed
        ));
        assert!(
            matches!(
                decide(&app, "dark", ask, &grants, &[8], 500).0,
                Verdict::Refused(_)
            ),
            "until it restarts: a new process is not the one it was given for"
        );
        assert!(
            matches!(
                decide(&app, "Lightmoon", ask, &grants, &[7], 1).0,
                Verdict::Refused(_)
            ),
            "per machine"
        );
    }

    #[test]
    fn asking_offers_only_limited_grants_and_denial_offers_to_start_asking() {
        let app = AppId::parse("leavepulse").unwrap();
        let (_, problem) = decide(&app, "Nocturne", DevtoolsConsent::Ask, &[], &[42], 1000);
        let problem = problem.unwrap();
        assert!(problem.summary.contains("run any code"));
        let grants: Vec<&DevtoolsGrant> = problem
            .actions
            .iter()
            .filter_map(|a| match &a.edit {
                ConfigEdit::GrantDevtools { grant, .. } => Some(grant),
                _ => None,
            })
            .collect();
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].until, 1000 + HOUR);
        assert_eq!(grants[1].pids, [42]);
        assert!(grants
            .iter()
            .all(|g| g.until <= 1000 + UNTIL_RESTART_AT_MOST));

        let (verdict, problem) = decide(&app, "Nocturne", DevtoolsConsent::Deny, &[], &[], 0);
        assert!(matches!(verdict, Verdict::Refused(_)));
        assert!(matches!(
            problem.unwrap().actions[0].edit,
            ConfigEdit::AppDevtools {
                consent: DevtoolsConsent::Ask,
                ..
            }
        ));
    }
}
