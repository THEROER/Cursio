//! Switching versions under a systemd user unit.
//!
//! Started at login, the node is `cursio.service`, and the unit — not the
//! node — decides what runs. The helper used to be an ordinary detached
//! child: its own process group, but still inside the unit's cgroup. Stopping
//! the node stopped the unit, systemd killed everything in the cgroup, helper
//! included, and `Restart=` brought back whatever `ExecStart` named. Seen on a
//! laptop: "update" pressed, the node went away for ten seconds and came back
//! as the build it had been, with not one line from the helper in the log.
//!
//! So under a unit the helper runs as a transient unit of its own
//! (`systemd-run --user`), stops and starts the node with `systemctl`, and
//! refuses up front when `ExecStart` does not start the install's current
//! version — starting that unit would bring back the old build and pass the
//! health check doing it.

use std::path::Path;

use super::UpdateError;

/// The systemd user unit this process runs under, if it is a service.
///
/// Read from our own cgroup: a node started from a terminal sits in a scope
/// (`vte-spawn-….scope`, `app-gnome-….scope`), which is not something to stop
/// and start; only a `.service` is.
pub fn own_unit() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        unit_from_cgroup(&cgroup)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(any(target_os = "linux", test))]
fn unit_from_cgroup(cgroup: &str) -> Option<String> {
    // cgroup v2 is the single `0::` line; the unit is its last component.
    let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
    let last = path.trim().rsplit('/').next()?;
    (last.ends_with(".service") && !last.starts_with("user@")).then(|| last.to_owned())
}

pub fn systemctl(args: &[&str]) -> Result<(), UpdateError> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .map_err(|e| UpdateError::io(Path::new("systemctl"), e))?;
    match status.success() {
        true => Ok(()),
        false => Err(UpdateError::Refused(format!(
            "systemctl --user {} failed ({status})",
            args.join(" ")
        ))),
    }
}

/// Refuse when starting `unit` would not start `stable`, the install's copy
/// that hands over to the current version.
pub fn check_starts(unit: &str, stable: &Path) -> Result<(), UpdateError> {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", "--property=ExecStart", "--value", unit])
        .output()
        .map_err(|e| UpdateError::io(Path::new("systemctl"), e))?;
    let exec = String::from_utf8_lossy(&out.stdout);
    match exec.contains(&*stable.to_string_lossy()) {
        true => Ok(()),
        false => Err(UpdateError::Refused(format!(
            "{unit} starts something other than {} (its ExecStart: {}); switch \"Start the \
             node\" off and on in the app so it names the install, then update again",
            stable.display(),
            exec.trim()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_started_at_login_names_its_service() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/cursio.service\n";
        assert_eq!(unit_from_cgroup(cgroup).as_deref(), Some("cursio.service"));
    }

    /// Started from a terminal the node sits in a scope, and there is no unit
    /// to stop or start — the helper then works with signals as before.
    #[test]
    fn a_node_started_from_a_terminal_has_no_unit() {
        let cgroup =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/vte-spawn-1b2c.scope\n";
        assert_eq!(unit_from_cgroup(cgroup), None);
        let manager = "0::/user.slice/user-1000.slice/user@1000.service\n";
        assert_eq!(unit_from_cgroup(manager), None);
    }
}
