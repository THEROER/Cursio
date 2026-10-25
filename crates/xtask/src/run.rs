//! `cargo xtask run`: start a build in the foreground the way the user runs it,
//! after stopping the one already running.

use crate::build;
use crate::exe_name;
use crate::out_dir;
use crate::select_for_host;
use crate::BinArg;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use std::process::Command;

/// The arguments `cursio` gets when the user names none.
///
/// The daily set (see memory: cursio-default-runset), not a minimal one: a
/// `run` that came up without input sharing would look broken in exactly the
/// way the tool exists to avoid.
pub(crate) const DEFAULT_CURSIO_ARGS: &[&str] = &["mesh", "--send-input", "--clipboard"];

/// Build one binary and run it, restarting it if it dies.
///
/// The restart loop is what makes this usable for a daemon under development:
/// `cursio` is killed by every rebuild and by its own crashes, and re-running
/// the whole command by hand each time is the friction this removes. A clean
/// exit (status 0) is taken as "it meant to stop" and ends the loop, as does
/// Ctrl-C, which the shell delivers to the child and to us alike.
pub(crate) fn run_task(release: bool, bin: BinArg, once: bool, args: &[String]) -> Result<()> {
    let name = bin.as_str();
    // Kill it before building, not after: cargo cannot overwrite a running
    // executable ("Text file busy" on Linux, sharing violation on Windows), so
    // a leftover from the last run fails the build rather than the launch.
    stop_running(name);

    let selected = select_for_host(Some(bin))?;
    let Some(target) = selected.first() else {
        bail!("{name} is not built on this platform");
    };
    let package = target.package;
    build(release, Some(bin), None)?;

    let exe = out_dir(release, None).join(exe_name(name, None));
    if !exe.exists() {
        bail!("{} not found after building {package}", exe.display());
    }

    // `trailing_var_arg` hands us the `--` separator itself, wherever the user
    // put it, and passing it on makes the binary read what follows as
    // positionals: `cursio mesh -- --send-input` fails with "unexpected
    // argument '--send-input'". Both spellings people reach for —
    // `run -- mesh --flags` and `run mesh -- --flags` — mean the same thing
    // here, so every separator is dropped rather than only a leading one.
    let passed: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--")
        .collect();
    let effective: Vec<&str> = if !passed.is_empty() {
        passed
    } else if name == "cursio" {
        DEFAULT_CURSIO_ARGS.to_vec()
    } else {
        // Only `cursio` has a subcommand worth defaulting to; the GUIs take
        // none and would reject one.
        Vec::new()
    };

    loop {
        println!("→ running {} {}", exe.display(), effective.join(" "));
        let mut cmd = Command::new(&exe);
        cmd.args(&effective);
        let status = cmd
            .status()
            .with_context(|| format!("spawning {}", exe.display()))?;

        if status.success() {
            return Ok(());
        }
        // A signal (Ctrl-C, or a kill) is the user stopping it, not a crash to
        // recover from — restarting there would make the process impossible to
        // stop from the terminal that started it.
        if once || terminated_by_signal(&status) {
            bail!("{name} exited with {status}");
        }
        eprintln!("--- {name} exited with {status}; restarting (--once to disable) ---");
    }
}

/// Whether a process was ended by a signal rather than by returning a status.
#[cfg(unix)]
pub(crate) fn terminated_by_signal(status: &std::process::ExitStatus) -> bool {
    std::os::unix::process::ExitStatusExt::signal(status).is_some()
}

/// Windows has no signals; every exit carries a code.
#[cfg(not(unix))]
pub(crate) fn terminated_by_signal(_status: &std::process::ExitStatus) -> bool {
    false
}

/// Ask any running copy of `bin` to quit, so the build can replace it.
///
/// Best-effort by design: no `pkill` on the box, nothing running, or a process
/// belonging to another user are all ordinary, and none of them should stop a
/// build that will fail loudly on its own if the executable really is locked.
pub(crate) fn stop_running(bin: &str) {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("pkill");
        // `-x` because a substring match would also hit `cursio-app` when asked
        // for `cursio`; `-U` so this never reaches another account's processes.
        c.args(["-x", "-U", &whoami(), bin]);
        c
    };
    #[cfg(not(unix))]
    let mut cmd = {
        let mut c = Command::new("taskkill");
        c.args(["/IM", &format!("{bin}.exe"), "/F"]);
        c
    };

    let quiet = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(quiet, Ok(s) if s.success()) {
        println!("→ stopped a running {bin}");
    }
}

/// This user's name, for `pkill -U`.
///
/// Read from the environment rather than pulled in as a `libc` dependency for
/// one call; `USER` is set by every login shell, and the fallback simply names
/// the account `pkill` would refuse to match, which fails safe.
#[cfg(unix)]
pub(crate) fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "\u{0}".to_string())
}
