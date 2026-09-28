//! Naming the local program that will not let go of a device.
//!
//! Exporting a device takes it away from this machine's own driver, and the
//! operating system refuses when something here still has it open. The refusal
//! itself names nobody. On Windows the peer was told `no such device` — a
//! phone plugged in the whole time — because `usbipd` answers the import with a
//! bare non-zero status and writes the reason only to its event log:
//!
//! ```text
//! CR_REMOVE_VETOED: PNP_VetoOutstandingOpen
//! ```
//!
//! "An open handle vetoed the removal". Which handle is the question the user
//! actually has, and the answer is almost always one of a few programs that
//! keep phones open for their own reasons: `adb`'s server, Phone Link, an
//! `scrcpy` session. So this reads the veto out of the log to know the refusal
//! *is* a hold, then looks for those programs among the running processes.
//!
//! ## A likely holder, said as likely
//!
//! Windows does not tell an unprivileged process which process owns a handle
//! to a device: the handle table is walked with `NtQuerySystemInformation` and
//! then each handle duplicated into this process to be named, which needs
//! debug privilege Cursio deliberately does not hold. A running `adb.exe` next
//! to a vetoed removal is not proof, but it is the cause every time it has been
//! seen, and "most likely adb.exe (PID 7312) — run `adb kill-server`" is a
//! sentence the user can act on where `no such device` was not.
//!
//! ## Linux knows exactly
//!
//! There every open file is a symlink under `/proc/<pid>/fd`, so a device node
//! held open is found by reading links rather than guessed at — for the
//! processes this user owns, which are the ones the user can close.

use std::path::Path;

/// A process on this machine holding a device open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// The process's image or command name: `adb.exe`, `adb`.
    pub process: String,
    pub pid: u32,
    /// Whether the process was *seen* holding the device (Linux) or is only
    /// the likeliest candidate (Windows). Decides how the sentence is phrased.
    pub certain: bool,
}

impl Holder {
    /// What the user can do about it.
    pub fn hint(&self) -> String {
        match known_fix(&self.process) {
            Some(fix) => fix.to_string(),
            None => format!("close {} (PID {}) and try again", self.process, self.pid),
        }
    }
}

/// The sentence for a device a local program will not release.
///
/// `device` is whatever the reader knows it by — the product name where there
/// is one, the bus id otherwise.
pub fn held_sentence(device: &str, holder: Option<&Holder>) -> String {
    match holder {
        Some(h) if h.certain => format!("{device} is held open by {} (PID {})", h.process, h.pid),
        Some(h) => format!(
            "{device} is held open on this machine, most likely by {} (PID {})",
            h.process, h.pid
        ),
        None => format!("{device} is held open by a program on this machine"),
    }
}

/// What to do about a hold, when the holder may be unknown.
pub fn held_hint(holder: Option<&Holder>) -> String {
    match holder {
        Some(h) => h.hint(),
        None => "close whatever is using it (adb, Phone Link, a file manager showing the \
                 phone) and try again"
            .to_string(),
    }
}

/// The fix for a program known to keep devices open.
///
/// Matched on the image name without its extension and case, so `adb`,
/// `adb.exe` and `ADB.EXE` are one entry.
fn known_fix(process: &str) -> Option<&'static str> {
    let name = process.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    Some(match name {
        "adb" => "run `adb kill-server` (the adb server keeps every phone it sees open)",
        "phoneexperiencehost" | "yourphone" => {
            "close Phone Link, or turn off its connection to this phone"
        }
        "scrcpy" => "close scrcpy",
        "fastboot" => "let fastboot finish, or stop it",
        "mtp-probe" | "gvfsd-mtp" | "kio_mtp" => {
            "unmount the phone in the file manager (its MTP view keeps it open)"
        }
        _ => return None,
    })
}

/// The likeliest holder among running processes, when only candidates are
/// known.
///
/// The first known holder in table order wins, not in process order: `adb` is
/// by far the most common cause, and a machine running both it and Phone Link
/// should be told about the one that is usually to blame.
pub fn likeliest(processes: &[(u32, String)]) -> Option<Holder> {
    const ORDER: [&str; 5] = [
        "adb",
        "scrcpy",
        "fastboot",
        "phoneexperiencehost",
        "yourphone",
    ];
    ORDER.iter().find_map(|wanted| {
        processes.iter().find_map(|(pid, name)| {
            let lower = name.to_ascii_lowercase();
            let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
            (stem == *wanted).then(|| Holder {
                process: name.clone(),
                pid: *pid,
                certain: false,
            })
        })
    })
}

/// The veto `usbipd` recorded, when its log text carries one.
///
/// `CR_REMOVE_VETOED` is the configuration manager refusing to take the device
/// from its driver; what follows it names the kind of veto. Only the
/// outstanding-open kind is a program holding the device, so only that one is
/// reported as a hold — a veto from a driver or a service is another problem
/// with another fix.
pub fn open_handle_veto(log: &str) -> bool {
    log.lines()
        .any(|line| line.contains("CR_REMOVE_VETOED") && line.contains("VetoOutstandingOpen"))
}

/// Read `tasklist /FO CSV /NH` output into (pid, image name) pairs.
///
/// Only the first two columns are read. The rest — session name, memory with a
/// localised thousands separator — change with the system language, and the
/// image name and PID do not.
pub fn parse_tasklist(csv: &str) -> Vec<(u32, String)> {
    csv.lines()
        .filter_map(|line| {
            let mut fields = line.split("\",\"");
            let image = fields.next()?.trim().trim_start_matches('"');
            let pid = fields.next()?.trim_matches('"').parse().ok()?;
            (!image.is_empty()).then(|| (pid, image.to_string()))
        })
        .collect()
}

/// Every process whose open files include `node`, found by reading
/// `<proc_root>/<pid>/fd/*` links.
///
/// Processes that cannot be read — another user's, or one that exited
/// mid-scan — are skipped: the ones this user can close are the ones worth
/// naming.
pub fn holders_of(proc_root: &Path, node: &Path) -> Vec<Holder> {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let holds = fds
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == node));
        if !holds {
            continue;
        }
        let process = std::fs::read_to_string(entry.path().join("comm"))
            .map(|c| c.trim().to_string())
            .unwrap_or_else(|_| format!("pid {pid}"));
        out.push(Holder {
            process,
            pid,
            certain: true,
        });
    }
    out.sort_by_key(|h| h.pid);
    out
}

/// Why `usbipd` refused an import, when the reason is a local program holding
/// the device. `None` means the refusal was something else.
///
/// Both queries run only after a refusal, never on the listing path: reading
/// the event log costs a PowerShell start, which is seconds on a cold machine.
#[cfg(target_os = "windows")]
pub fn windows_hold() -> Option<Option<Holder>> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // Recent entries only: an old veto for a device since released must not
    // explain today's refusal. The provider is `usbipd-win`, not the name of
    // its executable: filtering on `usbipd` matched nothing, so every hold was
    // reported to the peer as `no such device`.
    let script = "Get-WinEvent -FilterHashtable @{LogName='Application';ProviderName='usbipd-win';\
                  StartTime=(Get-Date).AddMinutes(-1)} -MaxEvents 20 \
                  -ErrorAction SilentlyContinue | ForEach-Object { $_.Message }";
    let log = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !open_handle_veto(&String::from_utf8_lossy(&log.stdout)) {
        return None;
    }

    let tasks = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let processes = match tasks {
        Ok(out) => parse_tasklist(&String::from_utf8_lossy(&out.stdout)),
        Err(error) => {
            tracing::warn!(%error, "cannot list processes to name the device's holder");
            Vec::new()
        }
    };
    Some(likeliest(&processes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log line seen live when `adb` held a POCO X7 Pro on the Windows peer.
    #[test]
    fn an_outstanding_open_veto_is_recognised_as_a_hold() {
        let log = "Failed to attach device 2-1.\n\
                   CR_REMOVE_VETOED: PNP_VetoOutstandingOpen USB\\VID_2717&PID_FF08\\49FMUCWG7HM7Z5HI";
        assert!(open_handle_veto(log));
    }

    /// A veto of another kind has another fix; calling it a hold would send the
    /// user looking for a program that is not there.
    #[test]
    fn a_veto_of_another_kind_is_not_a_hold() {
        assert!(!open_handle_veto("CR_REMOVE_VETOED: PNP_VetoDriver"));
        assert!(!open_handle_veto("device bound"));
        assert!(!open_handle_veto(""));
    }

    #[test]
    fn tasklist_csv_is_read_as_pid_and_image() {
        let csv = "\"System Idle Process\",\"0\",\"Services\",\"0\",\"8 K\"\r\n\
                   \"adb.exe\",\"7312\",\"Console\",\"1\",\"12 345 K\"\r\n\
                   \"PhoneExperienceHost.exe\",\"9100\",\"Console\",\"1\",\"80 000 K\"\r\n";
        let got = parse_tasklist(csv);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1], (7312, "adb.exe".to_string()));
    }

    /// Output that is not the table — an error message, an empty run — yields
    /// no processes rather than invented ones.
    #[test]
    fn garbage_from_tasklist_yields_no_processes() {
        assert!(parse_tasklist("INFO: No tasks are running.").is_empty());
        assert!(parse_tasklist("").is_empty());
    }

    #[test]
    fn adb_is_named_ahead_of_phone_link_whatever_the_process_order() {
        let running = vec![
            (9100, "PhoneExperienceHost.exe".to_string()),
            (4, "System".to_string()),
            (7312, "adb.exe".to_string()),
        ];
        let holder = likeliest(&running).expect("a holder");
        assert_eq!(holder.process, "adb.exe");
        assert_eq!(holder.pid, 7312);
        assert!(!holder.certain, "a candidate must not be reported as seen");
        assert!(holder.hint().contains("adb kill-server"));
    }

    #[test]
    fn no_known_holder_running_names_nobody() {
        let running = vec![
            (4, "System".to_string()),
            (1200, "explorer.exe".to_string()),
        ];
        assert_eq!(likeliest(&running), None);
    }

    /// An unknown program still gets a hint naming it, so the user knows what
    /// to close.
    #[test]
    fn an_unknown_holder_is_told_to_close_by_name_and_pid() {
        let holder = Holder {
            process: "myflasher".into(),
            pid: 77,
            certain: true,
        };
        assert_eq!(holder.hint(), "close myflasher (PID 77) and try again");
    }

    /// `/proc` shaped as the kernel lays it out: a process whose fd links to
    /// the device node is found; one holding something else is not.
    #[cfg(unix)]
    #[test]
    fn a_process_with_the_device_node_open_is_found_under_proc() {
        let dir = tempfile::tempdir().expect("tempdir");
        let proc_root = dir.path().join("proc");
        let node = dir.path().join("dev/bus/usb/001/005");
        std::fs::create_dir_all(node.parent().expect("parent")).expect("dev");
        std::fs::write(&node, "").expect("node");

        for (pid, comm, target) in [
            ("4242", "adb", node.clone()),
            ("100", "bash", dir.path().join("dev/null")),
        ] {
            let fd = proc_root.join(pid).join("fd");
            std::fs::create_dir_all(&fd).expect("fd dir");
            std::fs::write(proc_root.join(pid).join("comm"), format!("{comm}\n")).expect("comm");
            std::os::unix::fs::symlink(&target, fd.join("3")).expect("link");
        }
        std::fs::create_dir_all(proc_root.join("self")).expect("non-numeric entry");

        let holders = holders_of(&proc_root, &node);
        assert_eq!(
            holders,
            vec![Holder {
                process: "adb".into(),
                pid: 4242,
                certain: true
            }]
        );
    }
}
