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

/// Cross-compilation target for the Windows `.exe`s.
///
/// MSVC rather than the gnu/mingw ABI, even though mingw needs no SDK: file
/// sharing mounts the peer's disks through WinFsp, and `winfsp-sys` refuses
/// every non-MSVC triple outright — its build script panics with "unsupported
/// triple x86_64-pc-windows-gnu". A mingw build would silently lack the feature.
///
/// Linking is done by `cargo-xwin`, which downloads the MSVC CRT and Windows SDK
/// and links with `lld-link`, so this still cross-builds from Linux with no
/// Visual Studio anywhere.
const WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";

/// `OPUS_TARGET` for the Windows cross-build.
///
/// Different from the Linux value: without it `shiguredo_opus` tries to build
/// libopus from source under the cross toolchain, which is both slow and
/// fragile. See memory: cursio-msvc-cross-build.
const OPUS_TARGET_WINDOWS: &str = "windows_x86_64";

/// Where the LLVM tools live on Ubuntu.
///
/// `cargo-xwin` links with `lld-link`, which Ubuntu ships inside the versioned
/// LLVM directory and does *not* put on `PATH`. Rather than making the user
/// export it, the toolchain directory is found here and prepended for the
/// build.
fn llvm_bin_dir() -> Option<std::path::PathBuf> {
    if which("lld-link").is_some() {
        return None; // already reachable; nothing to prepend
    }
    let mut candidates: Vec<std::path::PathBuf> = glob_llvm_dirs();
    // Highest version first, so a box with several LLVMs uses the newest.
    candidates.sort();
    candidates.pop()
}

fn glob_llvm_dirs() -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir("/usr/lib") else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("llvm-"))
        })
        .map(|p| p.join("bin"))
        .filter(|p| p.join("lld-link").exists())
        .collect()
}

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

/// The arguments `cursio` gets when the user names none.
///
/// The daily set (see memory: cursio-default-runset), not a minimal one: a
/// `run` that came up without input sharing would look broken in exactly the
/// way the tool exists to avoid.
const DEFAULT_CURSIO_ARGS: &[&str] = &["mesh", "--send-input", "--clipboard"];

/// Build one binary and run it, restarting it if it dies.
///
/// The restart loop is what makes this usable for a daemon under development:
/// `cursio` is killed by every rebuild and by its own crashes, and re-running
/// the whole command by hand each time is the friction this removes. A clean
/// exit (status 0) is taken as "it meant to stop" and ends the loop, as does
/// Ctrl-C, which the shell delivers to the child and to us alike.
fn run_task(release: bool, bin: BinArg, once: bool, args: &[String]) -> Result<()> {
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
fn terminated_by_signal(status: &std::process::ExitStatus) -> bool {
    std::os::unix::process::ExitStatusExt::signal(status).is_some()
}

/// Windows has no signals; every exit carries a code.
#[cfg(not(unix))]
fn terminated_by_signal(_status: &std::process::ExitStatus) -> bool {
    false
}

/// Ask any running copy of `bin` to quit, so the build can replace it.
///
/// Best-effort by design: no `pkill` on the box, nothing running, or a process
/// belonging to another user are all ordinary, and none of them should stop a
/// build that will fail loudly on its own if the executable really is locked.
fn stop_running(bin: &str) {
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
fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "\u{0}".to_string())
}

/// Build release binaries (unless `no_build`) and copy them into ~/.local/bin,
/// installing `.desktop` launchers for the GUI ones on Linux.
fn install(only: Option<BinArg>, no_build: bool) -> Result<()> {
    if !no_build {
        build(true, only, None)?;
    }

    let bin_dir = local_bin_dir()?;
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;

    let src_dir = out_dir(true, None);
    let selected = select_for_host(only)?;
    for b in &selected {
        let src = src_dir.join(exe_name(b.bin, None));
        if !src.exists() {
            bail!(
                "{} not found — run without --no-build, or `cargo xtask build` first",
                src.display()
            );
        }
        let dst = bin_dir.join(exe_name(b.bin, None));
        install_binary(&src, &dst)
            .with_context(|| format!("installing {} → {}", src.display(), dst.display()))?;
        println!("✓ installed {} → {}", b.bin, dst.display());

        if b.gui && cfg!(target_os = "linux") {
            install_desktop_file(b.bin, &dst)?;
        }
    }

    if cfg!(target_os = "linux") {
        install_shell_extension()?;
    }

    warn_if_not_on_path(&bin_dir);
    println!("\nDone. Logs go to {}", cursio_log_dir().display());
    Ok(())
}

/// Cross-compile the Windows binaries from Linux, via `cargo-xwin`.
fn windows(release: bool) -> Result<PathBuf> {
    if !cfg!(target_os = "linux") {
        println!("note: `windows` is meant for cross-compiling from Linux.");
    }
    ensure_rust_target(WINDOWS_TARGET)?;

    let xwin = which("cargo-xwin").with_context(|| {
        format!(
            "cargo-xwin not found. Install it with:\n  \
             cargo install cargo-xwin\n\
             (it downloads the MSVC CRT and Windows SDK so {WINDOWS_TARGET} \
             links without Visual Studio)"
        )
    })?;
    // libopus is built with cmake, which invokes ninja specifically.
    if which("ninja").is_none() {
        bail!(
            "ninja not found. Install it with:\n  \
             {}\n\
             (cmake builds libopus with it; without it the build fails with \
             \"unable to find a build program corresponding to Ninja\")",
            ninja_install_hint()
        );
    }
    let gstreamer = windows_gstreamer_sysroot()?;

    // clipd is GTK/Linux-only — never cross-build it for Windows.
    let selected: Vec<&Binary> = BINARIES.iter().filter(|b| b.bin != "clipd").collect();
    let profile = if release { "release" } else { "debug" };
    println!(
        "→ cross-building {} for Windows ({profile})",
        names(&selected)
    );

    let mut cmd = cargo();
    cmd.arg("xwin");
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    add_packages(&mut cmd, &selected)?;
    cmd.args(["--target", WINDOWS_TARGET]);
    // The prebuilt libopus for Windows; without this the crate tries to compile
    // it from source under the cross toolchain.
    cmd.env("OPUS_TARGET", OPUS_TARGET_WINDOWS);
    // AirPlay video pulls in GStreamer, whose -sys crates resolve through
    // pkg-config. Cross-compiling means pointing it at the Windows development
    // files instead of the host's, which pkg-config refuses to do by default.
    cmd.env("PKG_CONFIG_ALLOW_CROSS", "1");
    cmd.env("PKG_CONFIG_SYSROOT_DIR", &gstreamer);
    cmd.env("PKG_CONFIG_PATH", gstreamer.join("lib").join("pkgconfig"));
    // Ubuntu keeps lld-link out of PATH, and xwin links with it. `cargo-xwin`
    // itself may also live outside PATH, in `$CARGO_HOME/bin`.
    let mut extra_dirs: Vec<PathBuf> = Vec::new();
    extra_dirs.extend(llvm_bin_dir());
    extra_dirs.extend(xwin.parent().map(Path::to_path_buf));
    if !extra_dirs.is_empty() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined =
            std::env::join_paths(extra_dirs.into_iter().chain(std::env::split_paths(&path)))
                .context("building the cross-build PATH")?;
        cmd.env("PATH", joined);
    }
    run(cmd).context("windows cross-build failed")?;

    let dir = out_dir(release, Some(WINDOWS_TARGET));
    for b in &selected {
        println!(
            "✓ {}",
            dir.join(exe_name(b.bin, Some(WINDOWS_TARGET))).display()
        );
    }
    Ok(dir)
}

fn macos() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`cargo xtask macos` must run on macOS");
    }
    build(true, None, None)
}

/// Rust target for 64-bit Android, the only ABI worth shipping to a phone.
const ANDROID_TARGET: &str = "aarch64-linux-android";

/// The Tauri CLI's name for that target.
const ANDROID_TAURI_TARGET: &str = "aarch64";

/// Build the phone app: the Tauri app in `app/` as an arm64 APK.
///
/// The app's library carries the node (`crates/android`, linked in for
/// `target_os = "android"`) and its JNI natives, and the Kotlin half — the
/// foreground service, boot receiver and permissions — is the Gradle library
/// in `crates/android/plugin`, which `app/build.rs` hands to the generated
/// project in `app/gen/android`.
///
/// The minimum API level is `bundle.android.minSdkVersion` in
/// `app/tauri.conf.json`, which the CLI also passes to the NDK's clang: 28
/// (Pie), because audio needs AAudio's input presets — the echo canceller that
/// keeps a phone playing the PC's sound from sending it straight back.
///
/// What crosses is the mesh core plus the modules with an Android backend
/// (audio). Turning another on does not fail at the feature — it fails deep
/// inside PipeWire or FUSE, which is a much worse error to read.
fn android(release: bool) -> Result<()> {
    ensure_rust_target(ANDROID_TARGET)?;

    let Some(bun) = which("bun") else {
        bail!(
            "bun not found; the Tauri CLI and the app's frontend both run on it. \
             Install it with: curl -fsSL https://bun.sh/install | bash"
        );
    };

    // Gradle finds the SDK only through these; the NDK alone is not enough to
    // package an APK.
    let sdk = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .find_map(|k| std::env::var_os(k).map(PathBuf::from))
        .with_context(|| {
            "ANDROID_HOME is not set.\n\
             Install the SDK through Android Studio (or its command-line tools), then:\n  \
             export ANDROID_HOME=$HOME/Android/Sdk"
                .to_string()
        })?;

    // The NDK ships its own clang and sysroot; nothing here can be built with
    // the host's. Both spellings are in circulation — Android Studio sets the
    // first, most CI images the second.
    let ndk = ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT"]
        .iter()
        .find_map(|k| std::env::var_os(k).map(PathBuf::from))
        .with_context(|| {
            "ANDROID_NDK_HOME is not set.\n\
             Install the NDK through Android Studio (SDK Manager → SDK Tools → \
             NDK) or from https://developer.android.com/ndk/downloads, then:\n  \
             export ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/<version>"
                .to_string()
        })?;
    if !ndk.join("toolchains/llvm/prebuilt").is_dir() {
        bail!(
            "{} does not look like an NDK: it has no toolchains/llvm/prebuilt.\n\
             Point ANDROID_NDK_HOME at a versioned NDK directory, not at the SDK.",
            ndk.display()
        );
    }

    let app = checkout_dir().join("app");
    if !app.join("gen/android").is_dir() {
        bail!(
            "no Android project at {}; run this from the Cursio checkout",
            app.join("gen/android").display()
        );
    }

    // The CLI is the app's own devDependency: `bun x tauri` without it would
    // fetch the unrelated npm package named `tauri`. The frontend's packages
    // are installed too, since the CLI generates it as `beforeBuildCommand`.
    for dir in [app.clone(), app.join("ui")] {
        let mut install = Command::new(&bun);
        install.arg("install").current_dir(&dir);
        run(install).with_context(|| format!("bun install in {} failed", dir.display()))?;
    }

    let profile = if release { "release" } else { "debug" };
    println!("→ building the Android app for {ANDROID_TARGET} ({profile})");

    let mut cmd = Command::new(&bun);
    cmd.args(["run", "tauri", "android", "build", "--apk"]);
    cmd.args(["--target", ANDROID_TAURI_TARGET]);
    // The version lives once, in the workspace's Cargo.toml. The desktop app
    // reads it from there when tauri.conf.json names none, but the Android
    // build does not: it keeps whatever `tauri.properties` held from an
    // earlier build, so the phone reported 0.1.0 after the release was
    // renumbered.
    cmd.args([
        "--config",
        &format!(r#"{{"version":"{}"}}"#, env!("CARGO_PKG_VERSION")),
    ]);
    if !release {
        cmd.arg("--debug");
    }
    cmd.current_dir(&app);
    cmd.env("ANDROID_HOME", &sdk);
    // The Tauri CLI reads NDK_HOME; cargo's build scripts read the other two.
    cmd.env("NDK_HOME", &ndk);
    cmd.env("ANDROID_NDK_HOME", &ndk);
    // libopus is built by CMake, whose own Android support finds the NDK only
    // through ANDROID_NDK_ROOT ("Neither the NDK or a standalone toolchain was
    // found" otherwise), and whose default Makefile generator `cmake-rs` cannot
    // locate a make for under this toolchain.
    cmd.env("ANDROID_NDK_ROOT", &ndk);
    cmd.env("CMAKE_GENERATOR", "Ninja");
    run(cmd)?;

    let apk = app
        .join("gen/android/app/build/outputs/apk/universal")
        .join(profile)
        .join(format!("app-universal-{profile}.apk"));
    if !apk.is_file() {
        bail!(
            "the build reported success but {} is missing",
            apk.display()
        );
    }
    println!("✓ {}", apk.display());
    println!("  install it with: adb install -r {}", apk.display());
    Ok(())
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

/// Copy `src` over `dst` atomically: write to a temp file in the same dir, then
/// `rename` it into place. `rename` swaps the directory entry to a new inode, so
/// a copy of the binary that's *currently running* (which holds the old inode)
/// keeps working — a plain `fs::copy` onto it fails with "Text file busy".
fn install_binary(src: &Path, dst: &Path) -> Result<()> {
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.new",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or("bin")
    ));
    // Clean up any leftover temp from a previous interrupted run.
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(src, &tmp).with_context(|| format!("staging {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, dst).with_context(|| format!("replacing {}", dst.display()))?;
    Ok(())
}

/// `~/.local/bin` via HOME. Kept simple: this is the user-install prefix chosen
/// for Cursio; `/usr/local/bin` and packaging are out of scope here.
fn local_bin_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
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

/// Write a `.desktop` launcher into ~/.local/share/applications for a GUI app.
fn install_desktop_file(bin: &str, exec: &Path) -> Result<()> {
    let apps_dir = {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .context("HOME not set")?;
        base.join("applications")
    };
    std::fs::create_dir_all(&apps_dir)?;

    let (name, comment) = match bin {
        "clipd" => ("Cursio Clipboard", "Clipboard history (Win+V for Wayland)"),
        "cursio-app" => ("Cursio", "LAN-first cross-machine workspace"),
        _ => (bin, "Cursio"),
    };
    // The window's Wayland app id is the binary's name, not the file name of
    // this launcher, so without `StartupWMClass` GNOME cannot tie the two
    // together and the dock shows "cursio-app" with a generic icon.
    let icon = if bin == "cursio-app" {
        install_app_icons(&base_data_dir()?)?;
        "Icon=cursio\n".to_string()
    } else {
        String::new()
    };
    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Comment={comment}\n\
         Exec={exec}\n\
         {icon}\
         StartupWMClass={bin}\n\
         Terminal=false\n\
         Categories=Utility;\n",
        exec = exec.display(),
    );
    let path = apps_dir.join(format!("dev.cursio.{bin}.desktop"));
    std::fs::write(&path, desktop).with_context(|| format!("writing {}", path.display()))?;
    println!("  ↳ launcher {}", path.display());
    // Best effort: both only refresh caches a desktop would also rebuild on
    // its own, and neither is installed everywhere.
    let _ = Command::new("update-desktop-database")
        .arg(&apps_dir)
        .status();
    Ok(())
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
fn base_data_dir() -> Result<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("HOME not set")
}

/// Put the logo into the user's hicolor theme as `cursio`, which is the name
/// the launcher asks for.
fn install_app_icons(data_dir: &Path) -> Result<()> {
    let src = checkout_dir().join("app/icons");
    let theme = data_dir.join("icons/hicolor");
    for (file, size) in [
        ("32x32.png", "32x32"),
        ("128x128.png", "128x128"),
        ("128x128@2x.png", "256x256"),
        ("icon.png", "512x512"),
    ] {
        let dir = theme.join(size).join("apps");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::copy(src.join(file), dir.join("cursio.png"))
            .with_context(|| format!("installing the {size} icon"))?;
    }
    let scalable = theme.join("scalable/apps");
    std::fs::create_dir_all(&scalable)?;
    std::fs::copy(src.join("logo.svg"), scalable.join("cursio.svg"))
        .context("installing the scalable icon")?;
    let _ = Command::new("gtk-update-icon-cache")
        .args(["-f", "-t"])
        .arg(&theme)
        .status();
    Ok(())
}

/// Copy the GNOME Shell extension (drag bridge and quick settings tile) into
/// the user's extensions directory.
///
/// Skipped without GNOME Shell. The running Shell does not reload an
/// extension's code on Wayland, so a changed copy only takes effect at the
/// next login — said here because nothing else would.
fn install_shell_extension() -> Result<()> {
    if which("gnome-shell").is_none() {
        return Ok(());
    }
    let src = checkout_dir().join("native/cursio-shell-extension");
    let dst = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("HOME not set")?
        .join("gnome-shell/extensions/cursio@cursio.dev");
    std::fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in std::fs::read_dir(&src).with_context(|| format!("reading {}", src.display()))? {
        let path = entry?.path();
        if path.is_file() {
            let to = dst.join(path.file_name().context("extension file has no name")?);
            std::fs::copy(&path, &to)
                .with_context(|| format!("copying {} → {}", path.display(), to.display()))?;
        }
    }
    println!(
        "✓ installed the GNOME Shell extension → {}\n  \
         enable it with `gnome-extensions enable cursio@cursio.dev`; a changed one \
         loads at the next login",
        dst.display()
    );
    Ok(())
}

/// Warn (don't fail) if the install dir isn't on PATH, with the fix.
fn warn_if_not_on_path(bin_dir: &Path) {
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == bin_dir))
        .unwrap_or(false);
    if !on_path {
        eprintln!(
            "\n! {} is not on your PATH. Add it, e.g.:\n  \
             echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc",
            bin_dir.display()
        );
    }
}

/// Ensure the *active* toolchain can compile for `target`, installing it with
/// rustup when it can't.
///
/// Asking `rustup target list` is not enough: a host where `rustc` resolves to
/// a distribution package (Homebrew, apt) while `rustup` is also installed will
/// happily report the target as present and then fail the build much later with
/// an opaque `can't find crate for core`. The standard library directory the
/// active `rustc` would actually use is the thing worth checking.
fn ensure_rust_target(target: &str) -> Result<()> {
    if has_target_libdir(target) {
        return Ok(());
    }
    if which("rustup").is_none() {
        bail!(
            "the active rust toolchain cannot build for {target} and rustup is not installed.\n\
             Install rustup, or build on a host whose toolchain ships {target}."
        );
    }
    println!("→ installing rust target {target}");
    let mut cmd = Command::new("rustup");
    cmd.args(["target", "add", target]);
    run(cmd)?;
    if has_target_libdir(target) {
        return Ok(());
    }
    bail!(
        "rustup installed {target} but the active `rustc` still cannot use it.\n\
         `rustc` is most likely a distribution package rather than rustup's shim; \
         check `which rustc` against `rustup which rustc`."
    )
}

/// Env var pointing at the unpacked GStreamer *Windows* development files.
const GSTREAMER_WINDOWS_SYSROOT_ENV: &str = "GSTREAMER_WINDOWS_SYSROOT";

/// Locate the GStreamer development files the Windows build links against.
///
/// `cursio-airplay` renders mirrored iPhone frames through GStreamer, so
/// `glib-sys` and the `gstreamer-*-sys` crates run pkg-config during the cross
/// build. Without a Windows sysroot they fail roughly two hundred crates in,
/// behind a wall of `cargo:rerun-if-env-changed` noise that says nothing about
/// what to install — so the requirement is checked up front instead.
fn windows_gstreamer_sysroot() -> Result<PathBuf> {
    let sysroot = std::env::var_os(GSTREAMER_WINDOWS_SYSROOT_ENV)
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "{GSTREAMER_WINDOWS_SYSROOT_ENV} is not set.\n\
                 The Windows build needs GStreamer's MSVC *development* files \
                 (the `-devel` package from https://gstreamer.freedesktop.org/download/), \
                 unpacked somewhere this host can read:\n  \
                 export {GSTREAMER_WINDOWS_SYSROOT_ENV}=/path/to/gstreamer/1.0/msvc_x86_64\n\
                 That directory must contain lib/pkgconfig/gstreamer-1.0.pc."
            )
        })?;
    let pkgconfig = sysroot.join("lib").join("pkgconfig");
    if !pkgconfig.join("gstreamer-1.0.pc").is_file() {
        bail!(
            "{} does not look like a GStreamer Windows sysroot: {} is missing.\n\
             Point {GSTREAMER_WINDOWS_SYSROOT_ENV} at the prefix that contains \
             lib/pkgconfig (for the MSVC devel package that is .../1.0/msvc_x86_64).",
            sysroot.display(),
            pkgconfig.join("gstreamer-1.0.pc").display()
        );
    }
    Ok(sysroot)
}

/// Package-manager hint for the host running the cross-build. `windows` is
/// documented as a Linux workflow but does run elsewhere, and an apt command on
/// a Mac is worse than no hint at all.
fn ninja_install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install ninja"
    } else {
        "sudo apt install ninja-build"
    }
}

/// Whether the active `rustc` has a standard library for `target`.
fn has_target_libdir(target: &str) -> bool {
    let Ok(out) = Command::new("rustc")
        .args(["--print", "target-libdir", "--target", target])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let dir = String::from_utf8_lossy(&out.stdout);
    Path::new(dir.trim()).is_dir()
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
