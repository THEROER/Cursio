//! Restarting this window into the version the install now names.
//!
//! The node switches versions on its own (the update agent, at a quiet
//! moment), but a window keeps running the build it started as until someone
//! closes it — tray apps are rarely closed. An old window reading a newer
//! node's answers is how the settings page sat empty: it could not read the
//! config the node sent. So the window says when it is older than the node,
//! and offers to come back as the current version.
//!
//! The new window is the install's stable copy, which hands over to whatever
//! `current` names. It is told to wait for this one to exit first: the
//! single-instance plugin would otherwise see this one still running, hand
//! it the arguments and quit, leaving the old window where it was.

use std::time::Duration;

const AFTER: &str = "--after";

/// How long a new window waits for the one it replaces; that one exits as
/// soon as the new one is started, so this only bounds a stuck exit.
const PREDECESSOR_WAIT: Duration = Duration::from_secs(10);

/// The version of the install this window runs from, when it runs from one.
pub fn own_version() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    cursio_core::update::layout::InstallLayout::for_user()
        .ok()?
        .version_of(&exe)
}

/// Started to replace another window: wait for it to be gone.
pub fn wait_for_predecessor(args: &[String]) {
    let Some(pid) = args
        .iter()
        .position(|a| a == AFTER)
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u32>().ok())
    else {
        return;
    };
    if !cursio_core::update::apply::wait_for_exit(pid, PREDECESSOR_WAIT) {
        tracing::warn!(
            pid,
            "the window being replaced is still running; starting anyway"
        );
    }
}

/// Start the current version's window, waiting for this one, and leave.
pub fn relaunch(app: &tauri::AppHandle) -> Result<(), String> {
    let exe = crate::node::app_executable().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(&exe);
    command
        .args([AFTER, &std::process::id().to_string()])
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
    command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    tracing::info!(exe = %exe.display(), "restarting this window as the current version");
    app.exit(0);
    Ok(())
}

pub mod commands {
    #[tauri::command]
    pub fn app_relaunch(app: tauri::AppHandle) -> Result<(), String> {
        super::relaunch(&app)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_started_without_a_predecessor_does_not_wait() {
        let start = std::time::Instant::now();
        wait_for_predecessor(&["cursio-app".into(), "--hidden".into()]);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
