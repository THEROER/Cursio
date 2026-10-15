fn main() {
    register_android_plugin();
    tauri_build::build()
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
