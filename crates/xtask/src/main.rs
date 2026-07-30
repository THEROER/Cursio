//! `cargo xtask` — Cursio's build / install / cross-compile automation.
//!
//! Wraps the handful of commands that otherwise need env vars and target
//! triples remembered by hand:
//! - `build`   — release build for this host (Linux/macOS/Windows).
//! - `install` — build release, then copy binaries into `~/.local/bin` and
//!               install `.desktop` launchers (Linux).
//! - `windows` — cross-compile the `.exe`s from Linux (mingw), no Windows box.
//! - `logs`    — print the log directory (and `tail -f` it with `--follow`).
//!
//! Android is declared but not wired up yet; it prints what's missing instead
//! of pretending to work.
//!
//! Run `cargo xtask <cmd> --help` for flags.

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
    /// Release build for this host.
    Build {
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
    /// Cross-compile the Windows .exe(s) from Linux (mingw).
    Windows {
        /// Build in debug profile instead of release.
        #[arg(long)]
        debug: bool,
    },
    /// Build the supported macOS binaries.
    Macos,
    /// Android build (future — prints what's needed).
    Android,
    /// Print the log directory; with --follow, tail the current log.
    Logs {
        /// `tail -f` the newest log file instead of just printing the path.
        #[arg(long)]
        follow: bool,
    },
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
    let cli = Cli::parse();
    match cli.command {
        Command_::Build { debug, bin } => build(!debug, bin, None),
        Command_::Install { bin, no_build } => install(bin, no_build),
        Command_::Windows { debug } => windows(!debug),
        Command_::Macos => macos(),
        Command_::Android => android(),
        Command_::Logs { follow } => logs(follow),
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
    for b in &selected {
        cmd.args(["-p", b.package]);
    }
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

    warn_if_not_on_path(&bin_dir);
    println!("\nDone. Logs go to {}", cursio_log_dir().display());
    Ok(())
}

/// Cross-compile the Windows binaries from Linux, via `cargo-xwin`.
fn windows(release: bool) -> Result<()> {
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
    for b in &selected {
        cmd.args(["-p", b.package]);
    }
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
    Ok(())
}

fn macos() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`cargo xtask macos` must run on macOS");
    }
    build(true, None, None)
}

fn android() -> Result<()> {
    println!(
        "Android is a future target — no build yet.\n\
         It'll need the NDK and cargo-ndk, and the input/audio/capture backends \n\
         ported to Android APIs before there's anything to build."
    );
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

/// target/<triple?>/<profile> — where cargo drops the binaries.
fn out_dir(release: bool, target: Option<&str>) -> PathBuf {
    let mut p = PathBuf::from("target");
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
    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Comment={comment}\n\
         Exec={exec}\n\
         Terminal=false\n\
         Categories=Utility;\n",
        exec = exec.display(),
    );
    let path = apps_dir.join(format!("dev.cursio.{bin}.desktop"));
    std::fs::write(&path, desktop).with_context(|| format!("writing {}", path.display()))?;
    println!("  ↳ launcher {}", path.display());
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
