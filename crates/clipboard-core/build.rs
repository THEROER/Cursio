//! Compiles the macOS pasteboard shim into the crate.
//!
//! Built in, not spawned: unlike screen capture there is no permission at
//! stake, but a clipboard backend that shelled out to a helper would pay a
//! process launch on every read — and reads happen on every copy the user
//! makes anywhere on the machine.

fn main() {
    println!("cargo:rerun-if-changed=native/macos/pasteboard.m");

    // The build script is compiled for the host whatever the target is, so the
    // platform question is answered here rather than by a `cfg` on the crate.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    cc::Build::new()
        .file("native/macos/pasteboard.m")
        .flag("-fobjc-arc")
        .compile("cursio_pasteboard");

    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=AppKit");
}
