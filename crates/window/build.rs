fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build_macos_window_capture();
    }
}

/// Compile the ScreenCaptureKit capture **into** the binary.
///
/// Not a spawned helper: macOS grants Screen Recording to the responsible
/// application, and a separate executable unpacked to a temporary path is not
/// covered by the approval the user gave their terminal. Linked in, the request
/// belongs to the process the user actually launched.
fn build_macos_window_capture() {
    cc::Build::new()
        .file("native/macos/window_capture.m")
        .flag("-fobjc-arc")
        .flag("-fblocks")
        .compile("cursio_window_capture");

    for framework in ["CoreMedia", "CoreVideo", "Foundation", "ScreenCaptureKit"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rerun-if-changed=native/macos/window_capture.m");
}
