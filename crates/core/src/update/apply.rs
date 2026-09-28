//! `cursio update apply`: stop the node, switch versions, start it again, and
//! go back if the new one does not come up.
//!
//! Derived from the LeavePulse launcher's `launcher-updater-helper` (same
//! author; that repository is Apache-2.0): its wait-for-exit on both
//! platforms, its "verify everything before touching anything" order, and its
//! rule that the helper runs from a copy of itself outside the install, so no
//! file it replaces is the image it is running from.
//!
//! What differs is what "replace" means. The launcher swaps files in place and
//! restores a backup on failure. Here the new build already sits in its own
//! `versions/<v>/`, so switching is rewriting one pointer, and going back is
//! rewriting it again — nothing is moved, and the old build is intact until a
//! later prune. The launcher also has a UI to report to; a node has none, so
//! the success test is the one thing every client of a node needs: its control
//! socket answering. A version that installs but never answers is rolled back
//! and the previous one restarted, rather than leaving the machine off the mesh.
//!
//! The relaunch inherits this process's session and token. Started by an
//! elevated node, the helper is elevated, and so is the node it starts — no
//! second UAC prompt for an update the user did not sit down for. Started
//! without a console, it starts the node without one.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use super::layout::InstallLayout;
use super::UpdateError;

/// Set on the temporary copy of this executable that does the work.
const HELPER_ENV: &str = "CURSIO_UPDATE_HELPER";

/// How long a stopped process gets to exit before the update is abandoned.
pub const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Plan {
    pub version: String,
    /// Processes that must be gone first: the node, the app.
    pub wait_pids: Vec<u32>,
    /// Ask those processes to stop rather than only waiting for them.
    pub stop: bool,
    /// What the node is started with. Empty means `mesh`: the settings live in
    /// `config.toml`, not in flags.
    pub node_args: Vec<OsString>,
    /// Start the node after switching. Off for a first install with no node.
    pub relaunch: bool,
    /// How long the new node has to answer on its control socket. Generous:
    /// an unelevated start raises a UAC prompt, and a person has to click it.
    pub health_timeout: Duration,
    /// Versions kept after a successful switch.
    pub keep: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Switched, and the node answered (or was not asked to start).
    Switched { from: Option<String>, to: String },
}

/// If this executable lives inside the install, run the rest from a copy of it
/// in a temporary directory and return `true`: the caller exits at once.
///
/// The copy inherits stdio, so a terminal still sees its output; this process
/// does not wait for it, because waiting would keep its own file open.
pub fn relocate_if_installed(layout: &InstallLayout) -> Result<bool, UpdateError> {
    if std::env::var_os(HELPER_ENV).is_some() {
        return Ok(false);
    }
    let exe = std::env::current_exe().map_err(|e| UpdateError::io(Path::new("current_exe"), e))?;
    let inside = match (exe.canonicalize(), layout.root().canonicalize()) {
        (Ok(exe), Ok(root)) => exe.starts_with(root),
        _ => false,
    };
    if !inside {
        return Ok(false);
    }
    let dir = std::env::temp_dir().join(format!("cursio-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| UpdateError::io(&dir, e))?;
    let copy = dir.join(exe.file_name().unwrap_or_default());
    std::fs::copy(&exe, &copy).map_err(|e| UpdateError::io(&copy, e))?;
    std::process::Command::new(&copy)
        .args(std::env::args_os().skip(1))
        .env(HELPER_ENV, "1")
        .spawn()
        .map_err(|e| UpdateError::io(&copy, e))?;
    Ok(true)
}

pub async fn run(layout: &InstallLayout, plan: &Plan) -> Result<Outcome, UpdateError> {
    // Everything that can be checked is checked before anything is stopped.
    let node = layout.executable(&plan.version, "cursio")?;
    layout
        .manifest(&plan.version)?
        .verify(&layout.version_dir(&plan.version))?;

    for &pid in &plan.wait_pids {
        if plan.stop {
            stop_process(pid);
        }
        let exited = tokio::task::spawn_blocking(move || wait_for_exit(pid, EXIT_TIMEOUT))
            .await
            .unwrap_or(false);
        if !exited {
            return Err(UpdateError::Refused(format!(
                "process {pid} did not exit within {}s; nothing was switched",
                EXIT_TIMEOUT.as_secs()
            )));
        }
    }

    let from = layout.current();
    layout.activate(&plan.version)?;
    for (path, e) in layout.refresh_stable_copies() {
        tracing::warn!(path = %path.display(), error = %e, "stable copy not refreshed; it still hands over to the current version");
    }

    if plan.relaunch {
        launch(&node, &plan.node_args)?;
        if !answers_within(plan.health_timeout).await {
            let back = rollback_after_failure(layout, plan).await;
            return Err(UpdateError::Refused(format!(
                "{} did not answer on the control socket within {}s; {back}",
                plan.version,
                plan.health_timeout.as_secs()
            )));
        }
    }

    for gone in layout.prune(plan.keep, std::slice::from_ref(&plan.version)) {
        tracing::info!(version = %gone, "pruned an old version");
    }
    Ok(Outcome::Switched {
        from,
        to: plan.version.clone(),
    })
}

async fn rollback_after_failure(layout: &InstallLayout, plan: &Plan) -> String {
    let Ok(previous) = layout.roll_back() else {
        return "there is no previous version to go back to".into();
    };
    let _ = layout.refresh_stable_copies();
    let started = layout
        .executable(&previous, "cursio")
        .and_then(|exe| launch(&exe, &plan.node_args));
    match started {
        Err(e) => format!("rolled back to {previous}, but could not start it: {e}"),
        Ok(()) if answers_within(plan.health_timeout).await => {
            format!("rolled back to {previous}, which is running")
        }
        Ok(()) => format!("rolled back to {previous}, which did not answer either"),
    }
}

/// Start the node detached from this helper, which is about to exit.
pub fn launch(exe: &Path, args: &[OsString]) -> Result<(), UpdateError> {
    let mut command = std::process::Command::new(exe);
    match args.is_empty() {
        true => command.arg("mesh"),
        false => command.args(args),
    };
    detach(&mut command);
    command
        .spawn()
        .map(drop)
        .map_err(|e| UpdateError::io(exe, e))
}

/// Detach `command` from this process: its own process group, no stdio, and
/// on Windows no console — a node started from a helper that has none would
/// otherwise get a window of its own.
pub fn detach(command: &mut std::process::Command) {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

/// Whether a node answers `Ping` on this user's control socket in time.
///
/// After the old processes are gone, whatever answers is the node just
/// started: the socket belongs to one node per user.
pub async fn answers_within(timeout: Duration) -> bool {
    use crate::control::{transport, Command, Reply};
    let Ok(endpoint) = transport::endpoint() else {
        return false;
    };
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok((client, _pushes)) = transport::connect(&endpoint).await {
            let ping = tokio::time::timeout(Duration::from_secs(2), client.request(Command::Ping));
            if let Ok(Ok(Reply::Pong { .. })) = ping.await {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Ask `pid` to stop.
///
/// Unix: SIGINT, the Ctrl-C the node already shuts down cleanly on — mounts
/// released, peers told. Windows has no signal to send a process without a
/// console, so it is terminated; phase 2 replaces this with a request over the
/// control socket, which lets the node pick its quiet moment.
fn stop_process(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: kill with a valid signal number has no memory effects.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        // SAFETY: the handle is checked before use and closed after.
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(handle, 1);
                let _ = CloseHandle(handle);
            }
        }
    }
}

/// Block until `pid` exits or `timeout` elapses; `true` if it is gone.
#[cfg(unix)]
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // kill(pid, 0) succeeds while the process exists (or is a zombie of
        // ours, which a node started elsewhere never is).
        // SAFETY: signal 0 only checks for existence.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        if !alive {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(windows)]
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    // SAFETY: the handle is checked before use and closed after.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            // Already gone, or not ours to observe: nothing to wait for.
            return true;
        };
        let millis = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        let done = WaitForSingleObject(handle, millis) == WAIT_OBJECT_0;
        let _ = CloseHandle(handle);
        done
    }
}

#[cfg(not(any(unix, windows)))]
pub fn wait_for_exit(_pid: u32, _timeout: Duration) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::manifest::Manifest;
    use std::fs;

    fn layout_with(versions: &[&str], name: &str) -> (std::path::PathBuf, InstallLayout) {
        let base = std::env::temp_dir().join(format!("cursio-apply-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let layout = InstallLayout::at(base.join("root"));
        for v in versions {
            let dir = base.join(format!("stage-{v}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("cursio"), v).unwrap();
            Manifest::describe(&dir, v, &["cursio"])
                .unwrap()
                .write(&dir)
                .unwrap();
            layout.install(&dir).unwrap();
        }
        (base, layout)
    }

    fn plan(version: &str) -> Plan {
        Plan {
            version: version.into(),
            wait_pids: Vec::new(),
            stop: false,
            node_args: Vec::new(),
            relaunch: false,
            health_timeout: Duration::from_secs(1),
            keep: 3,
        }
    }

    #[tokio::test]
    async fn applying_switches_current_and_remembers_the_previous_version() {
        let (base, layout) = layout_with(&["1", "2"], "switch");
        layout.activate("1").unwrap();
        let outcome = run(&layout, &plan("2")).await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Switched {
                from: Some("1".into()),
                to: "2".into()
            }
        );
        assert_eq!(layout.current().as_deref(), Some("2"));
        assert_eq!(layout.previous().as_deref(), Some("1"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn a_version_that_is_not_installed_changes_nothing() {
        let (base, layout) = layout_with(&["1"], "missing");
        layout.activate("1").unwrap();
        assert!(run(&layout, &plan("9")).await.is_err());
        assert_eq!(layout.current().as_deref(), Some("1"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn a_build_damaged_after_install_is_refused_before_switching() {
        let (base, layout) = layout_with(&["1", "2"], "damaged");
        layout.activate("1").unwrap();
        fs::write(layout.version_dir("2").join("cursio"), "changed").unwrap();
        assert!(matches!(
            run(&layout, &plan("2")).await,
            Err(UpdateError::HashMismatch { .. })
        ));
        assert_eq!(layout.current().as_deref(), Some("1"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn waiting_returns_once_the_process_has_exited() {
        let mut child = std::process::Command::new("sleep")
            .arg("0.2")
            .spawn()
            .unwrap();
        let pid = child.id();
        // Reap it from another thread, as the process that owns a node would.
        let reaper = std::thread::spawn(move || child.wait());
        assert!(wait_for_exit(pid, Duration::from_secs(5)));
        reaper.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn waiting_gives_up_on_a_process_that_does_not_exit() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        assert!(!wait_for_exit(child.id(), Duration::from_millis(300)));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
