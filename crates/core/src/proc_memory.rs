//! How much memory this node holds, and which processes on the machine hold
//! the most.
//!
//! [`crate::pressure`] says the machine is short; this says who it is short
//! because of. Both questions came up together: a laptop sat at 95% of its
//! commit limit for a day, Cursio's windows took seconds to answer, and
//! nothing in the logs could say whether Cursio was the one holding 53 GiB —
//! nor, with the laptop on a hotspot and no shell to reach it, could anyone
//! look. Earlier leaks of Cursio's own (an audio queue that grew to 37 GiB,
//! FUSE buffers left resident) were found the same slow way.
//!
//! So the node logs its own figures every [`LOG_EVERY`], and a peer can ask
//! for them and the machine's largest processes through
//! [`crate::diag_session`].
//!
//! What counts as "holds" follows each system's own limit. On Linux that is
//! resident memory: the OOM killer and PSI answer to it. On Windows it is
//! private bytes, the process's share of the commit charge — the figure that
//! runs out first there, and the one Task Manager's "Commit size" shows.

use std::time::Duration;

use cursio_protocol::diag::{NodeMemory, ProcessUse};

/// How often the node writes its own memory to the log.
pub const LOG_EVERY: Duration = Duration::from_secs(10 * 60);

/// This process's own memory, as far as the system says.
pub fn own() -> NodeMemory {
    imp::own()
}

/// The `n` processes holding the most memory, largest first.
pub fn top(n: usize) -> Vec<ProcessUse> {
    let mut all = imp::all();
    all.sort_by_key(|p| std::cmp::Reverse(p.bytes));
    all.truncate(n);
    all
}

/// Whether a process whose name starts with `prefix` is running. `false`
/// where the system cannot be asked, which callers take as "not running".
pub fn running(prefix: &str) -> bool {
    imp::all().iter().any(|p| p.name.starts_with(prefix))
}

/// Write this node's memory to the log.
pub fn log_own() {
    let own = own();
    tracing::info!(
        held_mib = own.held.map(|b| b / (1 << 20)),
        peak_mib = own.peak.map(|b| b / (1 << 20)),
        threads = own.threads,
        "this node's memory"
    );
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use super::*;

    fn field(status: &str, key: &str) -> Option<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
    }

    pub(super) fn own() -> NodeMemory {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return NodeMemory::default();
        };
        NodeMemory {
            held: field(&status, "VmRSS:").map(|kib| kib * 1024),
            peak: field(&status, "VmHWM:").map(|kib| kib * 1024),
            threads: field(&status, "Threads:").map(|n| n as u32),
        }
    }

    pub(super) fn all() -> Vec<ProcessUse> {
        // `statm`'s second field is resident pages: one short read per
        // process rather than parsing all of `status`.
        let page = 4096;
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
                let statm = std::fs::read_to_string(entry.path().join("statm")).ok()?;
                let resident: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
                let name = std::fs::read_to_string(entry.path().join("comm")).ok()?;
                Some(ProcessUse {
                    name: name.trim().to_string(),
                    pid,
                    bytes: resident * page,
                })
            })
            .filter(|p| p.bytes > 0)
            .collect()
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Private bytes and peak working set of the process behind `handle`.
    fn counters(handle: windows::Win32::Foundation::HANDLE) -> Option<PROCESS_MEMORY_COUNTERS_EX> {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // SAFETY: the EX struct begins with PROCESS_MEMORY_COUNTERS and `cb`
        // says how large it is, which is how the API is meant to be called
        // for the extended figures.
        unsafe {
            GetProcessMemoryInfo(
                handle,
                &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
                counters.cb,
            )
        }
        .ok()?;
        Some(counters)
    }

    /// Every process in a toolhelp snapshot: (pid, threads, name).
    fn processes() -> Vec<(u32, u32, String)> {
        // SAFETY: a snapshot of processes needs no process id; the handle is
        // closed below whatever happens.
        let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: `entry` is a live PROCESSENTRY32W with its size set.
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            out.push((
                entry.th32ProcessID,
                entry.cntThreads,
                String::from_utf16_lossy(&entry.szExeFile[..len]),
            ));
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
        }
        // SAFETY: the snapshot handle is ours and closed once.
        let _ = unsafe { CloseHandle(snapshot) };
        out
    }

    pub(super) fn own() -> NodeMemory {
        // SAFETY: the pseudo-handle of the current process needs no closing.
        let counters = counters(unsafe { GetCurrentProcess() });
        // SAFETY: no arguments, no failure.
        let me = unsafe { GetCurrentProcessId() };
        NodeMemory {
            held: counters.map(|c| c.PrivateUsage as u64),
            peak: counters.map(|c| c.PeakWorkingSetSize as u64),
            threads: processes()
                .into_iter()
                .find(|(pid, _, _)| *pid == me)
                .map(|(_, threads, _)| threads),
        }
    }

    pub(super) fn all() -> Vec<ProcessUse> {
        processes()
            .into_iter()
            .filter_map(|(pid, _, name)| {
                // A process this one may not open (a protected service, one
                // run by another user while this node is not elevated) is
                // left out rather than shown as zero.
                // SAFETY: the handle is closed right after the read.
                let handle =
                    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
                let counters = counters(handle);
                // SAFETY: opened just above.
                let _ = unsafe { CloseHandle(handle) };
                Some(ProcessUse {
                    name,
                    pid,
                    bytes: counters?.PrivateUsage as u64,
                })
            })
            .collect()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "windows")))]
mod imp {
    use super::*;

    pub(super) fn own() -> NodeMemory {
        NodeMemory::default()
    }

    pub(super) fn all() -> Vec<ProcessUse> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn this_process_reads_its_own_memory_and_finds_itself_among_the_rest() {
        let own = own();
        assert!(own.held.is_some_and(|b| b > 0));
        assert!(own.threads.is_some_and(|n| n >= 1));
        let me = std::process::id();
        assert!(imp::all().iter().any(|p| p.pid == me));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_largest_come_first_and_only_as_many_as_asked() {
        let top = top(3);
        assert!(top.len() <= 3);
        assert!(top.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    }
}
