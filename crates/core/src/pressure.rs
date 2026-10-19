//! Telling the user when the machine itself is the problem.
//!
//! A machine short of memory does not fail; it slows. Swap fills, reclaim
//! stalls every allocation, and from inside Cursio that looks exactly like a
//! network fault: the cursor stutters, audio drops out, a file copy crawls, a
//! module "blocks the session loop" for a second. Seen live with swap 100%
//! full — every one of those symptoms, and each was chased as its own bug.
//! So the machine's own pressure is read and raised as a problem while it
//! lasts, and cleared when it passes, next to whatever it is causing.
//!
//! ## What is read
//!
//! **Linux**: pressure stall information (`/proc/pressure/{memory,io}`), which
//! reports the share of recent time tasks spent *waiting* on memory or I/O.
//! That is the question — "is work being delayed" — rather than a proxy such
//! as free memory, which a healthy page cache keeps near zero. `avg10`, the
//! last ten seconds, because pressure worth telling the user about is pressure
//! they are feeling now. `/proc/meminfo` adds swap, since a full swap is the
//! state before the OOM killer and PSI can be quiet just before it.
//!
//! **Windows**: `GlobalMemoryStatusEx` — memory load and the commit charge
//! against its limit. Windows has no stall figure an unprivileged process can
//! read cheaply; a commit charge near its limit is its equivalent of full swap,
//! the point at which allocations start failing.
//!
//! ## Raise high, clear low
//!
//! Each figure has a raise level and a lower clear level. One threshold would
//! flap: pressure hovering at the line would raise and clear every poll, and a
//! panel that blinks is one the user learns to ignore.

use std::time::Duration;

use crate::diagnostics::{self, Problem, Severity};

/// How often the machine is read. Pressure that lasts less than this is not
/// something a user could act on anyway.
pub const POLL: Duration = Duration::from_secs(5);

/// One reading of the machine. `None` is "cannot say", never zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    /// Percent of the last 10 s some task waited on memory.
    pub memory_some: Option<f64>,
    /// Percent of the last 10 s *all* non-idle tasks waited on memory.
    pub memory_full: Option<f64>,
    /// Percent of the last 10 s all non-idle tasks waited on I/O.
    pub io_full: Option<f64>,
    pub swap_total: Option<u64>,
    pub swap_free: Option<u64>,
    /// Committed bytes and the commit limit (Windows).
    pub commit: Option<(u64, u64)>,
}

/// A figure over its line, as the monitor should report it.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub summary: String,
    pub hint: &'static str,
}

/// Raise at, and clear below, for one figure.
#[derive(Debug, Clone, Copy)]
struct Band {
    raise: f64,
    clear: f64,
}

impl Band {
    fn over(self, value: f64, active: bool) -> bool {
        match active {
            true => value >= self.clear,
            false => value >= self.raise,
        }
    }
}

/// Every task stalled on memory for a tenth of the time: the desktop is
/// visibly freezing in bursts.
const MEMORY_FULL: Band = Band {
    raise: 10.0,
    clear: 5.0,
};
/// Some task stalled on memory a fifth of the time: reclaim is constant.
const MEMORY_SOME: Band = Band {
    raise: 20.0,
    clear: 10.0,
};
/// Every task stalled on disk a fifth of the time.
const IO_FULL: Band = Band {
    raise: 20.0,
    clear: 10.0,
};
/// Swap or commit charge this full, in percent.
const NEARLY_FULL: Band = Band {
    raise: 90.0,
    clear: 80.0,
};

pub const MEMORY_ID: &str = "machine.memory";
pub const SWAP_ID: &str = "machine.swap";
pub const IO_ID: &str = "machine.io";
pub const COMMIT_ID: &str = "machine.commit";

const MEMORY_HINT: &str = "close something large (a browser with many tabs, a VM, a build); \
     until then input, audio and file transfers will stutter on this machine";
const SWAP_HINT: &str = "close something large, or add swap; when it fills the kernel starts \
     killing processes";
const IO_HINT: &str = "something is saturating the disk (an update, an indexer, a copy); \
     `iotop` names it";
const COMMIT_HINT: &str = "close something large, or let Windows grow the page file; when \
     the commit limit is reached programs start failing to allocate";

/// What is over its line, given which problems are already raised.
pub fn judge(reading: &Reading, active: &[&str]) -> Vec<Finding> {
    let is_active = |id| active.contains(&id);
    let mut out = Vec::new();

    let full = reading.memory_full.unwrap_or(0.0);
    let some = reading.memory_some.unwrap_or(0.0);
    let memory_active = is_active(MEMORY_ID);
    if MEMORY_FULL.over(full, memory_active) {
        out.push(Finding {
            id: MEMORY_ID,
            severity: Severity::Error,
            summary: format!(
                "this machine is stalling on memory: everything waited {full:.0}% of the last 10 s"
            ),
            hint: MEMORY_HINT,
        });
    } else if MEMORY_SOME.over(some, memory_active) {
        out.push(Finding {
            id: MEMORY_ID,
            severity: Severity::Warning,
            summary: format!(
                "this machine is short of memory: tasks waited on it {some:.0}% of the last 10 s"
            ),
            hint: MEMORY_HINT,
        });
    }

    if let (Some(total), Some(free)) = (reading.swap_total, reading.swap_free) {
        if total > 0 {
            let used = total.saturating_sub(free);
            let percent = used as f64 * 100.0 / total as f64;
            if NEARLY_FULL.over(percent, is_active(SWAP_ID)) {
                out.push(Finding {
                    id: SWAP_ID,
                    severity: Severity::Warning,
                    summary: format!(
                        "swap is {percent:.0}% full ({} of {})",
                        gib(used),
                        gib(total)
                    ),
                    hint: SWAP_HINT,
                });
            }
        }
    }

    if let Some(io) = reading.io_full {
        if IO_FULL.over(io, is_active(IO_ID)) {
            out.push(Finding {
                id: IO_ID,
                severity: Severity::Warning,
                summary: format!(
                    "this machine is stalling on disk i/o: everything waited {io:.0}% of the last 10 s"
                ),
                hint: IO_HINT,
            });
        }
    }

    if let Some((committed, limit)) = reading.commit {
        if limit > 0 {
            let percent = committed as f64 * 100.0 / limit as f64;
            if NEARLY_FULL.over(percent, is_active(COMMIT_ID)) {
                out.push(Finding {
                    id: COMMIT_ID,
                    severity: Severity::Warning,
                    summary: format!(
                        "committed memory is at {percent:.0}% of its limit ({} of {})",
                        gib(committed),
                        gib(limit)
                    ),
                    hint: COMMIT_HINT,
                });
            }
        }
    }
    out
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// `(some avg10, full avg10)` from a `/proc/pressure/*` file.
pub fn parse_psi(text: &str) -> (Option<f64>, Option<f64>) {
    let avg10 = |kind: &str| {
        text.lines()
            .find(|line| line.starts_with(kind))?
            .split_whitespace()
            .find_map(|field| field.strip_prefix("avg10="))?
            .parse()
            .ok()
    };
    (avg10("some "), avg10("full "))
}

/// `(SwapTotal, SwapFree)` in bytes from `/proc/meminfo`.
pub fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        let kib: u64 = line[name.len()..]
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()?;
        Some(kib * 1024)
    };
    (field("SwapTotal:"), field("SwapFree:"))
}

/// Read this machine now.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn read() -> Reading {
    let psi = |name: &str| {
        std::fs::read_to_string(format!("/proc/pressure/{name}"))
            .map(|t| parse_psi(&t))
            .unwrap_or((None, None))
    };
    let (memory_some, memory_full) = psi("memory");
    let (_, io_full) = psi("io");
    let (swap_total, swap_free) = std::fs::read_to_string("/proc/meminfo")
        .map(|t| parse_meminfo(&t))
        .unwrap_or((None, None));
    Reading {
        memory_some,
        memory_full,
        io_full,
        swap_total,
        swap_free,
        commit: None,
    }
}

/// Read this machine now.
#[cfg(target_os = "windows")]
pub fn read() -> Reading {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a live MEMORYSTATUSEX with its length set, which is
    // all the call requires.
    if let Err(error) = unsafe { GlobalMemoryStatusEx(&mut status) } {
        tracing::debug!(%error, "cannot read memory status");
        return Reading::default();
    }
    // "Page file" in this API is the commit limit and what is left of it, not
    // the page file on disk.
    let limit = status.ullTotalPageFile;
    let committed = limit.saturating_sub(status.ullAvailPageFile);
    Reading {
        commit: Some((committed, limit)),
        ..Reading::default()
    }
}

/// No source for any of this here; every figure stays "cannot say".
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "windows")))]
pub fn read() -> Reading {
    Reading::default()
}

/// Start watching this machine, once per process.
///
/// A plain thread: a few file reads every few seconds, which must keep going
/// however busy the runtime is — a machine short of memory is exactly when an
/// async task would be scheduled late.
pub fn watch() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("cursio-pressure".into())
            .spawn(|| {
                let mut active: Vec<&'static str> = Vec::new();
                loop {
                    active = apply(&judge(&read(), &active), &active);
                    std::thread::sleep(POLL);
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start watching memory and i/o pressure");
        }
    });
}

/// Raise what is over its line and clear what no longer is. Returns the ids
/// now raised.
fn apply(findings: &[Finding], was: &[&'static str]) -> Vec<&'static str> {
    for finding in findings {
        diagnostics::raise(Problem {
            id: finding.id.to_string(),
            severity: finding.severity,
            summary: finding.summary.clone(),
            hint: Some(finding.hint.to_string()),
            actions: Vec::new(),
            since: std::time::SystemTime::now(),
        });
    }
    let now: Vec<&'static str> = findings.iter().map(|f| f.id).collect();
    for id in was.iter().filter(|id| !now.contains(id)) {
        diagnostics::clear(id);
    }
    now
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSI: &str = "some avg10=23.51 avg60=10.00 avg300=2.00 total=123456\n\
                       full avg10=12.25 avg60=4.00 avg300=1.00 total=65432\n";

    #[test]
    fn psi_avg10_is_read_for_some_and_full() {
        assert_eq!(parse_psi(PSI), (Some(23.51), Some(12.25)));
        // Older kernels report no `full` line for cpu; its absence is "cannot
        // say", not zero.
        assert_eq!(
            parse_psi("some avg10=1.00 avg60=0 avg300=0 total=1\n"),
            (Some(1.0), None)
        );
    }

    #[test]
    fn meminfo_swap_is_read_in_bytes() {
        let text = "MemTotal:       61440000 kB\nSwapTotal:       8388604 kB\nSwapFree:          10240 kB\n";
        assert_eq!(
            parse_meminfo(text),
            (Some(8_388_604 * 1024), Some(10_240 * 1024))
        );
    }

    fn ids(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.id).collect()
    }

    /// Seen live: swap at 100% while every module logged itself as slow. That
    /// must be a problem the user is shown.
    #[test]
    fn nearly_full_swap_is_raised() {
        let reading = Reading {
            swap_total: Some(8 << 30),
            swap_free: Some(100 << 20),
            ..Reading::default()
        };
        let found = judge(&reading, &[]);
        assert_eq!(ids(&found), vec![SWAP_ID]);
        assert!(
            found[0].summary.starts_with("swap is 99% full"),
            "{}",
            found[0].summary
        );
    }

    #[test]
    fn a_machine_without_swap_has_no_swap_problem() {
        let reading = Reading {
            swap_total: Some(0),
            swap_free: Some(0),
            ..Reading::default()
        };
        assert!(judge(&reading, &[]).is_empty());
    }

    #[test]
    fn full_memory_stall_outranks_partial_and_is_an_error() {
        let reading = Reading {
            memory_some: Some(40.0),
            memory_full: Some(15.0),
            ..Reading::default()
        };
        let found = judge(&reading, &[]);
        assert_eq!(found.len(), 1, "one memory problem, not two");
        assert_eq!(found[0].severity, Severity::Error);

        let partial = Reading {
            memory_some: Some(25.0),
            memory_full: Some(1.0),
            ..Reading::default()
        };
        assert_eq!(judge(&partial, &[])[0].severity, Severity::Warning);
    }

    /// Pressure hovering at the line must not raise and clear on every poll.
    #[test]
    fn a_raised_figure_stays_raised_until_it_falls_well_below_the_line() {
        let at = |io| Reading {
            io_full: Some(io),
            ..Reading::default()
        };
        assert!(judge(&at(15.0), &[]).is_empty(), "below the raise level");
        assert_eq!(ids(&judge(&at(21.0), &[])), vec![IO_ID]);
        assert_eq!(
            ids(&judge(&at(15.0), &[IO_ID])),
            vec![IO_ID],
            "still above the clear level"
        );
        assert!(judge(&at(9.0), &[IO_ID]).is_empty(), "cleared");
    }

    #[test]
    fn commit_charge_near_its_limit_is_raised() {
        let reading = Reading {
            commit: Some((95, 100)),
            ..Reading::default()
        };
        assert_eq!(ids(&judge(&reading, &[])), vec![COMMIT_ID]);
        let fine = Reading {
            commit: Some((50, 100)),
            ..Reading::default()
        };
        assert!(judge(&fine, &[]).is_empty());
    }

    /// Nothing known is nothing wrong: a platform that cannot say must not
    /// raise anything.
    #[test]
    fn an_empty_reading_raises_nothing() {
        assert!(judge(&Reading::default(), &[]).is_empty());
    }
}
