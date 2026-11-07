//! `cargo xtask cef-runtime`: the CEF `cursio-app-cef` loads, installed once
//! under the install root as `cef/<build>/`.
//!
//! CEF is most of the Chromium app — `libcef.so` alone is over a gigabyte as
//! downloaded — and changes only when `cef-dll-sys` does, so it is not a
//! component of each version: every update would carry it, and the install
//! copies each component to the root as well. The binary finds it through its
//! `$ORIGIN/../../cef/<build>` library path (`app/build.rs`); Chromium then
//! finds its paks, ICU data and locales beside the library, not beside the
//! executable, which is what lets one directory serve every version.
//!
//! Only this machine for now: Chromium is needed on one machine, and the
//! update lane has no kind of build for a shared runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

#[derive(Args)]
pub struct CefRuntimeArgs {
    /// Take CEF from the debug build instead of the release one.
    #[arg(long)]
    debug: bool,
    /// Install what is already built instead of building `cursio-app-cef` first.
    #[arg(long)]
    no_build: bool,
}

/// What CEF puts beside the binary that the browser needs at run time; the
/// rest of its output directory is the build's own.
const FILES: &[&str] = &[
    "libcef.so",
    "libEGL.so",
    "libGLESv2.so",
    "libvk_swiftshader.so",
    "libvulkan.so.1",
    "vk_swiftshader_icd.json",
    "chrome_100_percent.pak",
    "chrome_200_percent.pak",
    "resources.pak",
    "icudtl.dat",
    "v8_context_snapshot.bin",
    "chrome-sandbox",
];
const LOCALES: &str = "locales";

pub fn cef_runtime(args: CefRuntimeArgs) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("the shared CEF is laid out for Linux only");
    }
    let release = !args.debug;
    if !args.no_build {
        crate::build_cef(release)?;
    }
    let out = crate::out_dir(release, None);
    let archive = out.join("archive.json");
    let text = std::fs::read_to_string(&archive)
        .with_context(|| format!("reading {}; build cursio-app-cef first", archive.display()))?;
    let build = cef_build_name(&text)
        .with_context(|| format!("{} names no CEF build", archive.display()))?;

    let cef_root = install_root()?.join("cef");
    let target = cef_root.join(build);
    let partial = cef_root.join(format!(".{build}.partial"));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(partial.join(LOCALES))
        .with_context(|| format!("creating {}", partial.display()))?;
    for file in FILES {
        let from = out.join(file);
        if !from.is_file() {
            bail!("{} is missing from the CEF build", from.display());
        }
        std::fs::copy(&from, partial.join(file))
            .with_context(|| format!("copying {}", from.display()))?;
    }
    for entry in std::fs::read_dir(out.join(LOCALES)).context("reading CEF's locales")? {
        let entry = entry?;
        std::fs::copy(entry.path(), partial.join(LOCALES).join(entry.file_name()))
            .with_context(|| format!("copying {}", entry.path().display()))?;
    }
    strip_symbols(&partial.join("libcef.so"));

    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&partial, &target)
        .with_context(|| format!("moving CEF into {}", target.display()))?;
    println!("✓ CEF {build} installed in {}", target.display());
    println!(
        "  Chromium runs only with a sandbox. Once, as root: install the AppArmor profile \
         /etc/apparmor.d/cursio-app-cef, or `chown root:root {0} && chmod 4755 {0}`.",
        target.join("chrome-sandbox").display()
    );
    Ok(())
}

/// The download keeps its symbols: 1.4 GB of library, 270 MB without them.
/// The dynamic symbols the loader needs stay. Left as it is when `strip` is
/// not installed.
fn strip_symbols(lib: &Path) {
    match Command::new("strip")
        .arg("--strip-unneeded")
        .arg(lib)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("strip {} exited with {status}; kept as is", lib.display()),
        Err(e) => eprintln!("strip not run ({e}); {} kept as is", lib.display()),
    }
}

/// The install root, as `cursio_core::update::layout` finds it on Linux.
fn install_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("CURSIO_INSTALL_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/lib/cursio"))
}

/// `"name": "cef_binary_<build>_linux64_minimal.tar.bz2"` → `<build>`, the
/// same reading as `app/build.rs`, which names the directory the binary loads.
fn cef_build_name(archive_json: &str) -> Option<&str> {
    let name = archive_json.split("\"cef_binary_").nth(1)?;
    name.split('_').next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cef_build_is_named_as_its_archive_names_it() {
        let json = r#"{ "type": "minimal",
            "name": "cef_binary_152.0.6+g708dc14+chromium-152.0.7977.83_linux64_minimal.tar.bz2" }"#;
        assert_eq!(
            cef_build_name(json),
            Some("152.0.6+g708dc14+chromium-152.0.7977.83")
        );
        assert_eq!(cef_build_name("{}"), None);
    }
}
