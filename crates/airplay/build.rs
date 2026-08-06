fn main() {
    let sources = [
        "native/playfair/fairplay_playfair.c",
        "native/playfair/playfair/hand_garble.c",
        "native/playfair/playfair/modified_md5.c",
        "native/playfair/playfair/omg_hax.c",
        "native/playfair/playfair/playfair.c",
        "native/playfair/playfair/sap_hash.c",
    ];

    let mut build = cc::Build::new();
    build
        .files(sources)
        .include("native/playfair")
        .include("native/playfair/playfair")
        .warnings(false)
        .flag_if_supported("-std=c99")
        .compile("cursio_playfair");

    println!("cargo:rerun-if-changed=native/playfair");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build_macos_video_sink();
    }
}

fn build_macos_video_sink() {
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo always sets OUT_DIR"))
            .join("cursio-airplay-video-sink");
    let status = std::process::Command::new("xcrun")
        .args([
            "--sdk",
            "macosx",
            "clang",
            "-fobjc-arc",
            "-fblocks",
            "-O2",
            "native/macos/video_sink.m",
            "-o",
        ])
        .arg(&output)
        .args([
            "-framework",
            "AppKit",
            "-framework",
            "AVFoundation",
            "-framework",
            "CoreMedia",
            "-framework",
            "QuartzCore",
        ])
        .status()
        .expect("failed to launch the macOS Objective-C compiler");
    assert!(status.success(), "failed to compile the macOS video sink");
    println!("cargo:rerun-if-changed=native/macos/video_sink.m");
}
