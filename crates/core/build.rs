//! Build-time linker configuration.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Delay-load WinFsp so the binary starts without it.
    //
    // `winfsp-sys` already asks for this, but `cargo:rustc-link-arg` reaches
    // only its immediate dependent — cargo does not propagate link args
    // transitively (rust-lang/cargo#9554) — and our binary is two crates
    // further along. Without the flag repeated here, `winfsp-x64.dll` lands in
    // the normal import table, and Windows refuses to start the process at all
    // when the DLL is missing: the user sees "winfsp-x64.dll is missing" before
    // `main` runs, whether or not they passed `--files`.
    //
    // Delay-loading moves that failure to the first actual call, which is
    // exactly where it belongs — file sharing then reports "WinFsp is not
    // available" and the session carries on without it.
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") && target.contains("msvc") {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:winfsp-x64.dll");

        // GStreamer, for the same reason and with more at stake.
        //
        // It is linked for one thing only: decoding the video an AirPlay
        // receiver accepts. Yet its DLLs sat in the ordinary import table, so
        // Windows refused to start the process without all eleven of them
        // present — `glib-2.0-0.dll est introuvable`, before `main`, for a user
        // who only wanted a mouse and a clipboard.
        //
        // Delay-loaded, the binary runs alone and AirPlay stays available to
        // whoever does install GStreamer. The catch is that an unresolved
        // delay-load raises a structured exception rather than returning an
        // error, which took the process down silently until
        // `cursio_airplay::gstreamer_present` learned to probe for the DLLs
        // before anything calls into them.
        for dll in [
            "glib-2.0-0.dll",
            "gobject-2.0-0.dll",
            "gstreamer-1.0-0.dll",
            "gstapp-1.0-0.dll",
            "gstvideo-1.0-0.dll",
        ] {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
        }

        // The delay-load helper the linker needs to resolve those imports at
        // first use.
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }

    platform_abi();
}

/// Name the FFmpeg this build links against, for the update lane's platform.
///
/// A Linux build links the system's libav* by soname, and the soname moves
/// with FFmpeg's major version: Ubuntu 25.10 ships `libavutil.so.59`, 24.04
/// `libavutil.so.58`. A build from one does not start on the other, yet both
/// are `linux-x86_64` — so a machine took a build it could not run, failed the
/// health check and rolled back, again at every offer. The major version is
/// therefore part of the platform (`linux-x86_64-av59`), and a machine takes
/// only builds made against the FFmpeg it has.
///
/// Read here from the same `pkg-config` the `ffmpeg-sys` build uses, so the
/// name cannot disagree with what was actually linked. Only the window module
/// links FFmpeg; a build without it names no ABI and needs none.
fn platform_abi() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" || std::env::var_os("CARGO_FEATURE_WINDOW").is_none() {
        return;
    }
    let output = std::process::Command::new("pkg-config")
        .args(["--modversion", "libavutil"])
        .output();
    let version = match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) => panic!(
            "pkg-config does not know libavutil ({}); install the FFmpeg development \
             packages (libavutil-dev on Debian/Ubuntu)",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => panic!("cannot run pkg-config to find libavutil: {e}; install pkg-config"),
    };
    let major = version
        .split('.')
        .next()
        .filter(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or_else(|| panic!("pkg-config gave libavutil version `{version}`, not N.N.N"));
    println!("cargo:rustc-env=CURSIO_PLATFORM_ABI=av{major}");
}
