//! Logging setup shared by every Cursio binary.
//!
//! One call to [`init`] wires up two sinks at once:
//! - the **console** (stderr), coloured, at `info` by default — what you watch
//!   live while running `cursio`;
//! - a **file** under the platform data dir, plain (no ANSI), at `debug` by
//!   default and rotated daily — the record you read *after* something broke.
//!
//! Both honour `RUST_LOG` when set, so a one-off `RUST_LOG=trace` still works.
//! The file gets its own knob, `CURSIO_LOG_FILE`, so you can crank the file to
//! `trace` without flooding the terminal. `mdns_sd` is silenced by default on
//! both (see the note in [`init`]).
//!
//! [`init`] returns a [`LogGuard`]. **Keep it alive for the whole run** — the
//! file writer is non-blocking (logging happens on a background thread), and
//! dropping the guard flushes and stops that thread. Bind it in `main`:
//!
//! ```no_run
//! let _log = cursio_log::init("cursio");
//! // ... rest of main; `_log` drops last, flushing the file.
//! ```

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Keeps the file-logging background thread alive. Returned by [`init`]; bind it
/// in `main` (e.g. `let _log = ...`) so it drops last and flushes the log file.
///
/// Also carries the resolved [`log_dir`](LogGuard::log_dir) so callers can tell
/// the user where logs landed.
#[must_use = "dropping the guard stops file logging; bind it in main"]
pub struct LogGuard {
    _worker: WorkerGuard,
    log_dir: PathBuf,
}

impl LogGuard {
    /// Directory the rotating log files are written to.
    pub fn log_dir(&self) -> &std::path::Path {
        &self.log_dir
    }
}

/// Directory Cursio writes rotating log files into (`<data_dir>/logs`).
///
/// Uses the same `ProjectDirs` qualifier/org/app as identity and the trust
/// store, so config and logs sit under one Cursio tree per platform:
/// - Linux:   `~/.local/share/cursio/logs`
/// - Windows: `%APPDATA%\cursio\cursio\data\logs`
/// - macOS:   `~/Library/Application Support/dev.cursio.cursio/logs`
///
/// Falls back to `./cursio-logs` if the platform dir can't be resolved, so
/// logging never silently vanishes.
pub fn log_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "cursio", "cursio")
        .map(|dirs| dirs.data_dir().join("logs"))
        .unwrap_or_else(|| PathBuf::from("cursio-logs"))
}

/// Initialise console + file logging for a binary named `app_name`.
///
/// `app_name` becomes the log file prefix, so `cursio` and `clipd` write to
/// separate rotating files (`cursio.log.2026-07-20`, `clipd.log.2026-07-20`, …)
/// and don't interleave. Call this once, first thing in `main`, and hold the
/// returned [`LogGuard`].
///
/// Filters: console reads `RUST_LOG` (default `info`); the file reads
/// `CURSIO_LOG_FILE`, else `RUST_LOG`, else `debug`. `mdns_sd` is pinned `off`
/// in every default because on hosts with VPN interfaces (WireGuard etc.) it
/// logs a one-off multicast error at startup for interfaces it can't use —
/// discovery still works over the pinned LAN interface. Set the env vars
/// explicitly to override (e.g. `RUST_LOG=debug,mdns_sd=info`).
pub fn init(app_name: &str) -> LogGuard {
    // Before anything writes: make sure this console understands the colour
    // codes we're about to emit.
    enable_ansi_on_windows();

    let dir = log_dir();
    // Best-effort: if we can't create the dir, fall back to console-only rather
    // than panicking a networking tool over a logging detail.
    let file_layer = match std::fs::create_dir_all(&dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(&dir, format!("{app_name}.log"));
            let (writer, worker) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_writer(writer)
                .with_filter(file_env_filter());
            Some((layer, worker))
        }
        Err(e) => {
            eprintln!("cursio: could not create log dir {}: {e}", dir.display());
            None
        }
    };

    let console_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(console_env_filter());

    // Split the Option so the registry always gets a (possibly no-op) file layer
    // slot; `Option<Layer>` implements Layer as a pass-through when `None`.
    let (file_layer, worker) = match file_layer {
        Some((layer, worker)) => (Some(layer), Some(worker)),
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    // If the file layer never came up we still return a guard, just with a
    // detached worker — the caller's `let _log` contract stays the same.
    let worker = worker.unwrap_or_else(|| {
        let (_w, guard) = tracing_appender::non_blocking(std::io::sink());
        guard
    });

    tracing::info!(log_dir = %dir.display(), "logging to file");

    LogGuard {
        _worker: worker,
        log_dir: dir,
    }
}

/// Console filter: `RUST_LOG` if set, else `info` with `mdns_sd` silenced.
fn console_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,mdns_sd=off"))
}

/// File filter: `CURSIO_LOG_FILE` if set, else `RUST_LOG` if set, else `debug`
/// (with `mdns_sd` silenced). Lets the file stay verbose while the console
/// stays quiet.
fn file_env_filter() -> EnvFilter {
    if let Ok(f) = std::env::var("CURSIO_LOG_FILE") {
        return EnvFilter::new(f);
    }
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug,mdns_sd=off"))
}

/// Turn on ANSI/VT escape handling for this process's console (Windows only).
///
/// Windows Terminal enables it for its own sessions, so a normally-launched
/// Cursio looks right and this is a no-op. But a **freshly created** console
/// starts with `ENABLE_VIRTUAL_TERMINAL_PROCESSING` off — which is exactly what
/// a UAC re-launch gets (see `cursio_core::elevate`). There the colour codes we
/// emit are printed literally and the log turns into `[2m[32m INFO[0m` noise.
///
/// Enabling the flag fixes the cause rather than the symptom: dropping colour
/// instead would make the normal, non-elevated console worse to read too.
/// Best-effort — if the handle isn't a console (output redirected to a file or a
/// pipe) the call fails and we simply leave things as they are.
#[cfg(target_os = "windows")]
fn enable_ansi_on_windows() {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_ERROR_HANDLE,
    };

    // stderr, because that's where the console layer writes.
    // SAFETY: plain console API calls; the handle is owned by the process and
    // must not be closed, and `mode` is a local the OS writes into.
    unsafe {
        let Ok(handle) = GetStdHandle(STD_ERROR_HANDLE) else {
            return;
        };
        let mut mode = Default::default();
        if GetConsoleMode(handle, &mut mode).is_err() {
            return; // not a console (redirected) — nothing to enable
        }
        let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
    }
}

#[cfg(not(target_os = "windows"))]
fn enable_ansi_on_windows() {}
