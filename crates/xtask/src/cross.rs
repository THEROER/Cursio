//! Builds for another machine: Windows through cargo-xwin and LLVM, macOS, and
//! the Android app, with the toolchains, sysroots and Rust targets each needs.

use crate::add_packages;
use crate::build;
use crate::cargo;
use crate::checkout_dir;
use crate::exe_name;
use crate::names;
use crate::out_dir;
use crate::run;
use crate::which;
use crate::Binary;
use crate::BINARIES;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

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
pub(crate) const WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";

/// `OPUS_TARGET` for the Windows cross-build.
///
/// Different from the Linux value: without it `shiguredo_opus` tries to build
/// libopus from source under the cross toolchain, which is both slow and
/// fragile. See memory: cursio-msvc-cross-build.
pub(crate) const OPUS_TARGET_WINDOWS: &str = "windows_x86_64";

/// Where the LLVM tools live on Ubuntu.
///
/// `cargo-xwin` links with `lld-link`, which Ubuntu ships inside the versioned
/// LLVM directory and does *not* put on `PATH`. Rather than making the user
/// export it, the toolchain directory is found here and prepended for the
/// build.
pub(crate) fn llvm_bin_dir() -> Option<std::path::PathBuf> {
    if which("lld-link").is_some() {
        return None; // already reachable; nothing to prepend
    }
    let mut candidates: Vec<std::path::PathBuf> = glob_llvm_dirs();
    // Highest version first, so a box with several LLVMs uses the newest.
    candidates.sort();
    candidates.pop()
}

pub(crate) fn glob_llvm_dirs() -> Vec<std::path::PathBuf> {
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

/// Cross-compile the Windows binaries from Linux, via `cargo-xwin`.
pub(crate) fn windows(release: bool) -> Result<PathBuf> {
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

pub(crate) fn macos() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`cargo xtask macos` must run on macOS");
    }
    build(true, None, None)
}

/// Rust target for 64-bit Android, the only ABI worth shipping to a phone.
pub(crate) const ANDROID_TARGET: &str = "aarch64-linux-android";

/// The Tauri CLI's name for that target.
pub(crate) const ANDROID_TAURI_TARGET: &str = "aarch64";

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
pub(crate) fn android(release: bool) -> Result<()> {
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

/// Ensure the *active* toolchain can compile for `target`, installing it with
/// rustup when it can't.
///
/// Asking `rustup target list` is not enough: a host where `rustc` resolves to
/// a distribution package (Homebrew, apt) while `rustup` is also installed will
/// happily report the target as present and then fail the build much later with
/// an opaque `can't find crate for core`. The standard library directory the
/// active `rustc` would actually use is the thing worth checking.
pub(crate) fn ensure_rust_target(target: &str) -> Result<()> {
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
pub(crate) const GSTREAMER_WINDOWS_SYSROOT_ENV: &str = "GSTREAMER_WINDOWS_SYSROOT";

/// Locate the GStreamer development files the Windows build links against.
///
/// `cursio-airplay` renders mirrored iPhone frames through GStreamer, so
/// `glib-sys` and the `gstreamer-*-sys` crates run pkg-config during the cross
/// build. Without a Windows sysroot they fail roughly two hundred crates in,
/// behind a wall of `cargo:rerun-if-env-changed` noise that says nothing about
/// what to install — so the requirement is checked up front instead.
pub(crate) fn windows_gstreamer_sysroot() -> Result<PathBuf> {
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
pub(crate) fn ninja_install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install ninja"
    } else {
        "sudo apt install ninja-build"
    }
}

/// Whether the active `rustc` has a standard library for `target`.
pub(crate) fn has_target_libdir(target: &str) -> bool {
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
