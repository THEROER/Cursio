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

/// How often an app in the tray checks whether the install has moved on.
const STALE_CHECK: Duration = Duration::from_secs(60);

/// Come back as the current version by itself while nothing of this app is
/// on screen.
///
/// The alert asks only once the window is opened, and a tray app may go a
/// week without that: one machine ran a six-day-old app across six node
/// switches, its user seeing none of the interface changes made meanwhile.
/// Restarting a window nobody is looking at disturbs nobody, so it is done
/// unasked; one that is open keeps the alert, since its user may be in the
/// middle of something.
pub fn follow_install(app: tauri::AppHandle) {
    // Not started from an install — a development build — has nothing to
    // follow.
    let Some(mine) = own_version() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("cursio-app-stale".into())
        .spawn(move || loop {
            std::thread::sleep(STALE_CHECK);
            let current = cursio_core::update::layout::InstallLayout::for_user()
                .ok()
                .and_then(|layout| layout.current());
            let Some(current) = current.filter(|current| *current != mine) else {
                continue;
            };
            if anything_on_screen(&app) {
                continue;
            }
            tracing::info!(
                window = %mine,
                %current,
                "this app is older than the install and none of it is on screen; restarting as the current version"
            );
            if let Err(e) = start_current(&app, true) {
                tracing::warn!(error = %e, "could not restart as the current version; the window offers it when opened");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot watch for a newer install; the window offers it when opened");
    }
}

/// Whether any of this app's windows is showing. One whose state cannot be
/// read counts as showing: restarting under a person is the worse mistake.
fn anything_on_screen(app: &tauri::AppHandle) -> bool {
    tauri::Manager::webview_windows(app)
        .values()
        .any(|window| window.is_visible().unwrap_or(true))
}

/// Start the current version's window, waiting for this one, and leave.
pub fn relaunch(app: &tauri::AppHandle) -> Result<(), String> {
    start_current(app, false)
}

/// Start the current version, in the tray alone when `hidden`, and leave.
fn start_current(app: &tauri::AppHandle, hidden: bool) -> Result<(), String> {
    let exe = crate::node::app_executable().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(&exe);
    command.args([AFTER, &std::process::id().to_string()]);
    if hidden {
        command.arg("--hidden");
    }
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
