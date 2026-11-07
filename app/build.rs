/// The CEF build `cursio-app-cef` links, as its binary distribution names it.
///
/// One CEF serves every installed version: it is the bulk of the Chromium
/// build (`libcef.so` alone is over a gigabyte), changes only when
/// `cef-dll-sys` moves, and so lives once under the install root at
/// `cef/<this>/` (`cargo xtask cef-runtime`) instead of inside each version.
/// The platform-webview binary, which does not link CEF, reads it from here to
/// find that directory. A CEF build bumped without this fails the build below.
const CEF_RUNTIME: &str = "152.0.6+g708dc14+chromium-152.0.7977.83";

fn main() {
    register_android_plugin();
    println!("cargo:rustc-env=CURSIO_CEF_RUNTIME={CEF_RUNTIME}");
    if std::env::var_os("CARGO_FEATURE_CEF").is_some() {
        link_shared_cef();
    }
    tauri_build::build()
}

/// Let `cursio-app-cef` load the shared CEF from `versions/<v>/`.
///
/// Chromium finds its paks, ICU data and locales beside `libcef.so`, not
/// beside the executable, so a library path is all the binary needs. The
/// `$ORIGIN` that `tauri-build` adds stays after it, for a build run from the
/// target directory with CEF beside it.
fn link_shared_cef() {
    let dir = std::env::var("DEP_TAURI_RUNTIME_CEF_CEF_DIR")
        .expect("tauri-runtime-cef names the CEF distribution it built against");
    let archive = std::path::Path::new(&dir).join("archive.json");
    let text = std::fs::read_to_string(&archive)
        .unwrap_or_else(|e| panic!("reading {}: {e}", archive.display()));
    let linked = cef_runtime_name(&text)
        .unwrap_or_else(|| panic!("{} names no CEF build", archive.display()));
    assert_eq!(
        linked, CEF_RUNTIME,
        "cef-dll-sys now brings CEF {linked}; set CEF_RUNTIME in app/build.rs to it"
    );
    println!("cargo:rerun-if-changed={}", archive.display());
    println!("cargo:rustc-link-arg-bin=cursio-app-cef=-Wl,-rpath,$ORIGIN/../../cef/{CEF_RUNTIME}");
}

/// `"name": "cef_binary_<build>_linux64_minimal.tar.bz2"` → `<build>`.
fn cef_runtime_name(archive_json: &str) -> Option<&str> {
    let name = archive_json.split("\"cef_binary_").nth(1)?;
    name.split('_').next()
}

/// Put the phone's Kotlin plugin (service, boot receiver, permissions) into the
/// generated Android project.
///
/// `tauri-build` includes a Gradle library module for every
/// `DEP_<NAME>_ANDROID_LIBRARY_PATH` it finds, which is how a plugin crate
/// ships one; this app's plugin is not a crate, so the variable is set here,
/// as the LeavePulse launcher does for its runtime.
fn register_android_plugin() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let plugin_dir = manifest_dir.join("../crates/android/plugin");
    println!("cargo:rerun-if-changed={}", plugin_dir.display());
    // SAFETY: a build script is single-threaded; nothing reads the environment
    // concurrently with this.
    unsafe { std::env::set_var("DEP_CURSIO_MESH_PLUGIN_ANDROID_LIBRARY_PATH", plugin_dir) };
}
