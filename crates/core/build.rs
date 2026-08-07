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
}
