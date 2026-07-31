fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build_macos_window_capture();
    }
}

/// Compile the ScreenCaptureKit helper and leave it in `OUT_DIR`, where the
/// crate embeds it. Same arrangement as `cursio-airplay`'s presentation helper:
/// the binary ships inside the Rust one, so there is nothing to install
/// alongside it.
fn build_macos_window_capture() {
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo always sets OUT_DIR"))
            .join("cursio-window-capture");
    let status = std::process::Command::new("xcrun")
        .args([
            "--sdk",
            "macosx",
            "clang",
            "-fobjc-arc",
            "-fblocks",
            "-O2",
            "native/macos/window_capture.m",
            "-o",
        ])
        .arg(&output)
        .args([
            "-framework",
            "AppKit",
            "-framework",
            "CoreMedia",
            "-framework",
            "CoreVideo",
            "-framework",
            "ScreenCaptureKit",
        ])
        .status()
        .expect("failed to launch the macOS Objective-C compiler");
    assert!(
        status.success(),
        "failed to compile the macOS window capture helper"
    );
    println!("cargo:rerun-if-changed=native/macos/window_capture.m");
}
