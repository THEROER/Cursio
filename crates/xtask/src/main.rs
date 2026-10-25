//! `cargo xtask` — Cursio's build / install / cross-compile automation.
//!
//! Wraps the handful of commands that otherwise need env vars and target
//! triples remembered by hand:
//! - `build`   — release build; `build <host|windows|macos|all>` picks what for.
//! - `run`     — build, then run it, restarting the binary when it crashes.
//! - `install` — build release, then copy binaries into `~/.local/bin` and
//!   install `.desktop` launchers (Linux).
//! - `windows` — cross-compile the `.exe`s from Linux (mingw), no Windows box.
//! - `logs`    — print the log directory (and `tail -f` it with `--follow`).
//! - `deploy`  — build both halves, ship the `.exe` to the Windows peer under
//!   a versioned name and restart it in the user's session.
//! - `live-test` — measure both real machines over a window; PASS/FAIL table.
//!   Both are described in `docs/live-test.md`.
//!
//! - `android` — build the phone app (the Tauri app in `app/`, carrying the
//!   node) as an arm64 APK. Needs the SDK, the NDK and bun.
//!
//! Run `cargo xtask <cmd> --help` for flags.

mod cross;
mod install;
mod run;

mod container;
mod deploy;
mod dotenv;
mod livetest;
mod logscan;
mod remote;
mod stage;
#[cfg(target_os = "linux")]
mod uinput;

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cross::android;
use crate::cross::macos;
use crate::cross::windows;
use crate::install::install;
use crate::run::run_task;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

/// The workspace binaries xtask knows how to build and install, paired with the
/// cargo package that produces each. `cursio-app` is the Tauri UI; it builds as
/// a plain cargo binary because its frontend is static (`app/dist`).
const BINARIES: &[Binary] = &[
    Binary {
        bin: "cursio",
        package: "cursio-core",
        gui: false,
    },
    Binary {
        bin: "clipd",
        package: "clipd",
        gui: true,
    },
    Binary {
        bin: "cursio-app",
        package: "cursio-app",
        gui: true,
    },
];

struct Binary {
    /// The produced executable name (target/<profile>/<bin>).
    bin: &'static str,
    /// The cargo package that builds it (`-p <package>`).
    package: &'static str,
    /// Whether it's a graphical app that should get a `.desktop` launcher.
    gui: bool,
}

/// `OPUS_TARGET` the `shiguredo_opus` crate needs for a native Linux build on
/// this box (Ubuntu 25.10) — without it the prebuilt-opus path panics in the
/// crate's build script. Harmless on other targets, so we always set it for
/// host Linux builds. See memory: cursio-opus-build-target.
const OPUS_TARGET_LINUX: &str = "ubuntu-24.04_x86_64";

#[derive(Parser)]
#[command(name = "xtask", about = "Cursio build & install automation")]
struct Cli {
    #[command(subcommand)]
    command: Command_,
}

#[derive(Subcommand)]
enum Command_ {
    /// Release build. Builds for this host unless another target is named.
    Build {
        /// What to build: host, windows, macos, or all.
        ///
        /// `all` is every target this machine can produce — on Linux that is
        /// the host binaries plus the cross-compiled Windows `.exe`s; elsewhere
        /// only the host, since neither Linux nor macOS binaries cross-build
        /// from the other.
        #[arg(value_enum, default_value = "host")]
        target: TargetArg,
        /// Build in debug profile instead of release.
        #[arg(long)]
        debug: bool,
        /// Only build this binary (default: all).
        #[arg(long, value_enum)]
        bin: Option<BinArg>,
    },
    /// Build release, then install binaries into ~/.local/bin (+ .desktop files).
    Install {
        /// Only install this binary (default: all).
        #[arg(long, value_enum)]
        bin: Option<BinArg>,
        /// Skip the build and install whatever's already in target/release.
        #[arg(long)]
        no_build: bool,
    },
    /// Build, then run it. Restarts on crash; Ctrl-C stops for good.
    Run {
        /// Run the debug build instead of release.
        #[arg(long)]
        debug: bool,
        /// Which binary to run.
        #[arg(long, value_enum, default_value = "cursio")]
        bin: BinArg,
        /// Don't restart the binary when it exits non-zero.
        #[arg(long)]
        once: bool,
        /// Arguments for the binary itself. Anything after `--`.
        ///
        /// For `cursio` these replace the default `mesh --send-input
        /// --clipboard`; a bare subcommand is enough (`-- peers`), since what
        /// follows is passed through untouched.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Cross-compile the Windows .exe(s) from Linux. Alias for `build windows`.
    Windows {
        /// Build in debug profile instead of release.
        #[arg(long)]
        debug: bool,
    },
    /// Build the supported macOS binaries. Alias for `build macos`.
    Macos,
    /// Build the phone app as an arm64 APK. Needs the SDK, the NDK and bun.
    Android {
        /// Build without optimizations.
        #[arg(long)]
        debug: bool,
    },
    /// Print the log directory; with --follow, tail the current log.
    Logs {
        /// `tail -f` the newest log file instead of just printing the path.
        #[arg(long)]
        follow: bool,
    },
    /// Build both halves, ship the .exe to the Windows peer and restart it there.
    Deploy(deploy::DeployArgs),
    /// Measure the two real machines over a window and print PASS/FAIL.
    LiveTest(livetest::LiveTestArgs),
    /// Put the node and the app in a versioned directory with a manifest,
    /// ready for `cursio update install`.
    Stage(stage::StageArgs),
    /// Stage this machine's, the Windows and the Ubuntu 24.04 build, sign
    /// them, and let the running nodes offer them to the mesh on the `dev`
    /// channel.
    Publish(stage::PublishArgs),
    /// live-test's child that touches the peer mount under a deadline.
    #[command(hide = true)]
    FileProbe {
        path: PathBuf,
        #[arg(long)]
        seed: u64,
        #[arg(long)]
        remove: bool,
    },
}

/// What `build` should produce.
///
/// A single command with a target, rather than one subcommand per platform:
/// `build`, `windows` and `macos` were three spellings of "compile this
/// workspace", differing only in where the output lands, and the pair that
/// still exist are kept as aliases rather than as the way in.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TargetArg {
    /// This machine.
    Host,
    /// The Windows `.exe`s, cross-compiled from Linux.
    Windows,
    /// The macOS binaries (only on macOS).
    Macos,
    /// Everything this machine can build.
    All,
}

/// clap mirror of the installable binary names.
#[derive(Clone, Copy, ValueEnum)]
enum BinArg {
    Cursio,
    Clipd,
    CursioApp,
}

impl BinArg {
    fn as_str(self) -> &'static str {
        match self {
            BinArg::Cursio => "cursio",
            BinArg::Clipd => "clipd",
            BinArg::CursioApp => "cursio-app",
        }
    }
}

fn main() -> Result<()> {
    dotenv::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env"));
    let cli = Cli::parse();
    match cli.command {
        Command_::Build { target, debug, bin } => build_target(target, !debug, bin),
        Command_::Install { bin, no_build } => install(bin, no_build),
        Command_::Run {
            debug,
            bin,
            once,
            args,
        } => run_task(!debug, bin, once, &args),
        Command_::Windows { debug } => windows(!debug).map(drop),
        Command_::Macos => macos(),
        Command_::Android { debug } => android(!debug),
        Command_::Logs { follow } => logs(follow),
        Command_::Deploy(args) => deploy::deploy(args),
        Command_::LiveTest(args) => livetest::live_test(args),
        Command_::Stage(args) => stage::stage(args),
        Command_::Publish(args) => stage::publish(args),
        Command_::FileProbe { path, seed, remove } => livetest::file_probe(&path, seed, remove),
    }
}

/// Build one target, or every target this machine can produce.
///
/// `all` skips what the host cannot make rather than failing: on Linux the
/// Windows cross-build is available and macOS is not, and a command that
/// refused to build anything because one platform is out of reach would be
/// useless exactly where it is most wanted.
fn build_target(target: TargetArg, release: bool, only: Option<BinArg>) -> Result<()> {
    match target {
        TargetArg::Host => build(release, only, None),
        TargetArg::Windows => windows(release).map(drop),
        TargetArg::Macos => macos(),
        TargetArg::All => {
            // Windows cross-builds from Linux via cargo-xwin; macOS binaries
            // can only be produced on macOS, and Linux ones only on Linux.
            let targets: &[TargetArg] = if cfg!(target_os = "linux") {
                &[TargetArg::Host, TargetArg::Windows]
            } else {
                &[TargetArg::Host]
            };
            for (i, t) in targets.iter().enumerate() {
                println!("=== building {}/{} ===", i + 1, targets.len());
                build_target(*t, release, only)?;
            }
            Ok(())
        }
    }
}

/// Build the selected binaries. `release` picks the profile; `only` narrows to
/// one binary; `target` cross-compiles when set (else host).
fn build(release: bool, only: Option<BinArg>, target: Option<&str>) -> Result<()> {
    let selected = select_for_host(only)?;
    let profile = if release { "release" } else { "debug" };
    let where_ = target.unwrap_or("host");
    println!("→ building {} ({profile}, {where_})", names(&selected));

    let mut cmd = cargo();
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    add_packages(&mut cmd, &selected)?;
    if let Some(t) = target {
        cmd.args(["--target", t]);
    }
    // Native Linux builds need OPUS_TARGET for the prebuilt-opus path; Windows
    // (gnu) builds libopus from source and don't want it. Set it only for host
    // Linux, never for a cross target.
    if target.is_none() && cfg!(target_os = "linux") {
        cmd.env("OPUS_TARGET", OPUS_TARGET_LINUX);
    }

    run(cmd).context("cargo build failed")?;
    println!("✓ built into {}", out_dir(release, target).display());
    Ok(())
}

/// Name the packages to build, preparing what `cursio-app` embeds.
///
/// The app compiles its frontend into the binary, so a stale or missing
/// `ui/.output/public` ships a stale or empty window: it is regenerated first.
fn add_packages(cmd: &mut Command, selected: &[&Binary]) -> Result<()> {
    for b in selected {
        cmd.args(["-p", b.package]);
    }
    if selected.iter().any(|b| b.bin == "cursio-app") {
        generate_app_ui()?;
    }
    Ok(())
}

/// Build the app's static frontend with bun, as `tauri.conf.json` expects.
fn generate_app_ui() -> Result<()> {
    let Some(bun) = which("bun") else {
        bail!(
            "bun not found; cursio-app embeds its frontend and bun builds it. \
             Install it with: curl -fsSL https://bun.sh/install | bash"
        );
    };
    let ui = checkout_dir().join("app/ui");
    if !ui.is_dir() {
        bail!(
            "no app frontend at {}; run this from the Cursio checkout",
            ui.display()
        );
    }
    println!("→ generating the cursio-app frontend");
    let mut install = Command::new(&bun);
    install.arg("install").current_dir(&ui);
    run(install).context("bun install in app/ui failed")?;
    let mut generate = Command::new(&bun);
    generate.args(["run", "generate"]).current_dir(&ui);
    run(generate).context("generating the cursio-app frontend failed")
}

/// The Cursio checkout this xtask was started in.
///
/// Where `cargo run` says this checkout is, not where the binary was built: a
/// target dir shared between checkouts (`CARGO_TARGET_DIR`) can hand one
/// checkout an xtask compiled in another, whose baked-in path may be gone.
fn checkout_dir() -> PathBuf {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    // `crates/xtask` → the checkout.
    manifest_dir
        .ancestors()
        .nth(2)
        .map_or_else(|| manifest_dir.join("../.."), Path::to_path_buf)
}

/// Print the log directory; with `follow`, tail the newest log file.
fn logs(follow: bool) -> Result<()> {
    let dir = cursio_log_dir();
    println!("{}", dir.display());
    if !follow {
        return Ok(());
    }
    let newest =
        newest_file(&dir)?.with_context(|| format!("no log files in {}", dir.display()))?;
    println!("→ tailing {}", newest.display());
    let mut cmd = Command::new("tail");
    cmd.arg("-f").arg(&newest);
    run(cmd)
}

// --- helpers ---------------------------------------------------------------

fn select_for_host(only: Option<BinArg>) -> Result<Vec<&'static Binary>> {
    let supported = |binary: &&Binary| binary.bin != "clipd" || cfg!(target_os = "linux");
    match only {
        Some(b) => {
            let selected: Vec<_> = BINARIES
                .iter()
                .filter(|binary| binary.bin == b.as_str())
                .filter(supported)
                .collect();
            if selected.is_empty() {
                bail!(
                    "{} is Linux-only and cannot be built on this host",
                    b.as_str()
                );
            }
            Ok(selected)
        }
        None => Ok(BINARIES.iter().filter(supported).collect()),
    }
}

fn names(bins: &[&Binary]) -> String {
    bins.iter().map(|b| b.bin).collect::<Vec<_>>().join(", ")
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
}

/// Run a command inheriting stdio, erroring if it exits non-zero.
fn run(mut cmd: Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("spawning {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} exited with {status}", cmd.get_program());
    }
    Ok(())
}

/// Where cargo puts build output: `CARGO_TARGET_DIR` when set, since cargo
/// honours it and a literal `target/` would then name a stale binary.
fn target_root() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"))
}

/// <target root>/<triple?>/<profile> — where cargo drops the binaries.
fn out_dir(release: bool, target: Option<&str>) -> PathBuf {
    let mut p = target_root();
    if let Some(t) = target {
        p.push(t);
    }
    p.push(if release { "release" } else { "debug" });
    p
}

/// The executable filename for `bin`, adding `.exe` for a Windows target.
fn exe_name(bin: &str, target: Option<&str>) -> String {
    if target.map(|t| t.contains("windows")).unwrap_or(false) {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    }
}

/// Cursio's log directory, matching `cursio_log::log_dir` (kept in sync by
/// hand — xtask deliberately has no dependency on the app crates).
fn cursio_log_dir() -> PathBuf {
    // <data_dir>/logs. On Linux data_dir is $XDG_DATA_HOME or ~/.local/share.
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cursio").join("logs")
}

/// Locate an executable on PATH (`which`-style), returning its path.
///
/// `$CARGO_HOME/bin` is searched too: that is where `cargo install` puts cargo
/// subcommands, and it is missing from PATH whenever cargo itself came from a
/// package manager rather than from rustup.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(cargo_bin_dir())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn cargo_bin_dir() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))?;
    Some(home.join("bin"))
}

/// Newest regular file in a directory by mtime, or None if the dir is empty.
fn newest_file(dir: &Path) -> Result<Option<PathBuf>> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    for entry in entries.flatten() {
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified()?;
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, entry.path()));
        }
    }
    Ok(best.map(|(_, p)| p))
}
