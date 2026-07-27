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
        // The delay-load helper the linker needs to resolve those imports at
        // first use.
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
