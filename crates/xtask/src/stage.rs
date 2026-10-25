//! `cargo xtask stage`: a build directory `cursio update install` accepts.
//!
//! The node and the app go into `target/stage/<version>/<platform>/` beside a
//! `manifest.json` naming each file's sha256 — the same manifest the installer
//! verifies before and after copying (see `docs/update-design.md`). The version
//! is the crate version plus the commit, so two stages of one commit are the
//! same version, and a dirty tree adds the node's own hash: two different
//! uncommitted builds must never install over each other under one name.
//!
//! The manifest is written by hand rather than with `serde_json`: xtask keeps
//! its dependencies to `anyhow` and `clap`, and names and hex digests need no
//! escaping.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};

use crate::cross::WINDOWS_TARGET;
use crate::deploy::{current_commit, sha256_file};

#[derive(Clone, Copy, ValueEnum)]
pub enum StageTarget {
    Host,
    Windows,
    /// Linux for machines on Ubuntu 24.04, built in a container.
    Ubuntu24,
}

#[derive(Args)]
pub struct StageArgs {
    #[arg(value_enum, default_value = "host")]
    target: StageTarget,
    /// Stage what is already built instead of building first.
    #[arg(long)]
    no_build: bool,
    /// Install the staged build on this machine and make it current (host
    /// only). The running node is left alone; switch with `cursio update apply`.
    #[arg(long)]
    install: bool,
}

pub fn stage(args: StageArgs) -> Result<()> {
    if args.install && !matches!(args.target, StageTarget::Host) {
        bail!("--install stages for this machine; install another platform's build there");
    }
    let (_, dir) = stage_build(args.target, args.no_build, None)?;
    if args.install {
        install_here(&dir)?;
    }
    Ok(())
}

/// The platform name a build for `target` runs on, as the update lane spells
/// it: `<os>-<arch>` in Rust's names, and on Linux the FFmpeg ABI the build
/// links (`linux-x86_64-av59`), read the way `crates/core/build.rs` reads it
/// so the two cannot disagree.
pub fn platform_name(target: StageTarget) -> Result<String> {
    Ok(match target {
        StageTarget::Host => {
            let base = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
            if cfg!(target_os = "linux") {
                format!("{base}-{}", host_abi()?)
            } else {
                base
            }
        }
        StageTarget::Windows => "windows-x86_64".into(),
        StageTarget::Ubuntu24 => format!("linux-x86_64-{}", crate::container::abi()?),
    })
}

fn host_abi() -> Result<String> {
    let out = Command::new("pkg-config")
        .args(["--modversion", "libavutil"])
        .output()
        .context("running pkg-config; install it and the FFmpeg development packages")?;
    if !out.status.success() {
        bail!(
            "pkg-config does not know libavutil: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    abi_from_version(&String::from_utf8_lossy(&out.stdout))
}

/// `59.39.100` → `av59`.
pub fn abi_from_version(version: &str) -> Result<String> {
    let version = version.trim();
    match version.split('.').next() {
        Some(major) if !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()) => {
            Ok(format!("av{major}"))
        }
        _ => bail!("libavutil version `{version}` is not N.N.N"),
    }
}

/// Build (unless `no_build`) and stage one target, answering the version and
/// directory. `version` names it instead of deriving a name: builds for two
/// platforms published together must carry one version, and a dirty tree's
/// name hashes the node, which differs per platform.
pub fn stage_build(
    target_kind: StageTarget,
    no_build: bool,
    version: Option<&str>,
) -> Result<(String, PathBuf)> {
    let (target, out) = match target_kind {
        StageTarget::Host => {
            if !no_build {
                crate::build(true, Some(crate::BinArg::Cursio), None)?;
                crate::build(true, Some(crate::BinArg::CursioApp), None)?;
            }
            (None, crate::out_dir(true, None))
        }
        StageTarget::Windows => {
            if !no_build {
                crate::windows(true)?;
            }
            (
                Some(WINDOWS_TARGET),
                crate::out_dir(true, Some(WINDOWS_TARGET)),
            )
        }
        StageTarget::Ubuntu24 => {
            if !no_build {
                crate::container::build()?;
            }
            (None, crate::container::out_dir())
        }
    };

    let files: Vec<String> = ["cursio", "cursio-app"]
        .iter()
        .map(|bin| crate::exe_name(bin, target))
        .collect();
    let node = out.join(&files[0]);
    let version = match version {
        Some(v) => v.to_owned(),
        None => {
            let commit = current_commit()?;
            let dirty_hash = match commit.dirty {
                true => Some(sha256_file(&node)?),
                false => None,
            };
            version_name(
                env!("CARGO_PKG_VERSION"),
                &commit.sha,
                dirty_hash.as_deref(),
            )
        }
    };

    // One directory per platform: a Linux and a Windows build of one commit
    // are the same version, and staging the second must not delete the first.
    let dir = crate::target_root()
        .join("stage")
        .join(&version)
        .join(platform_name(target_kind)?);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut components = Vec::new();
    for file in &files {
        let from = out.join(file);
        if !from.is_file() {
            bail!("{} is missing; build it or drop --no-build", from.display());
        }
        std::fs::copy(&from, dir.join(file))
            .with_context(|| format!("copying {}", from.display()))?;
        components.push((component_name(file), file.clone(), sha256_file(&from)?));
    }
    let manifest = manifest_json(&version, &components);
    std::fs::write(dir.join("manifest.json"), manifest).context("writing manifest.json")?;
    println!("✓ staged {version} in {}", dir.display());
    Ok((version, dir))
}

#[derive(Args)]
pub struct PublishArgs {
    /// Publish only this machine's build, not the Windows and Ubuntu 24.04
    /// ones too.
    #[arg(long)]
    host_only: bool,
    /// Publish what is already built instead of building first.
    #[arg(long)]
    no_build: bool,
}

/// Stage this machine's build, the Windows one and — from a Linux host whose
/// FFmpeg differs — the Ubuntu 24.04 one as one version, sign them with this
/// machine's key and put them in the local node's store, from which every
/// running node offers them to its peers on the `dev` channel. Each machine
/// takes the one whose platform name is its own.
///
/// Signing is done by the staged node itself (`cursio update publish`), so the
/// key and the store are handled by the same code that later verifies them.
pub fn publish(args: PublishArgs) -> Result<()> {
    let (version, host_dir) = stage_build(StageTarget::Host, args.no_build, None)?;
    let host_platform = platform_name(StageTarget::Host)?;
    let mut builds = vec![(host_platform.clone(), host_dir.clone())];
    if !args.host_only && !cfg!(windows) {
        let (_, dir) = stage_build(StageTarget::Windows, args.no_build, Some(&version))?;
        builds.push((platform_name(StageTarget::Windows)?, dir));
    }
    if !args.host_only && cfg!(target_os = "linux") {
        if !crate::container::available() {
            println!(
                "note: docker is not installed, so there is no Ubuntu 24.04 build in this \
                 version; machines on 24.04 keep what they run"
            );
        } else if platform_name(StageTarget::Ubuntu24)? == host_platform {
            // This machine links the same FFmpeg: its build already is theirs.
        } else {
            let (_, dir) = stage_build(StageTarget::Ubuntu24, args.no_build, Some(&version))?;
            builds.push((platform_name(StageTarget::Ubuntu24)?, dir));
        }
    }
    let node = host_dir.join(crate::exe_name("cursio", None));
    let mut command = Command::new(&node);
    command.args(["update", "publish"]);
    for (platform, dir) in &builds {
        command
            .arg("--build")
            .arg(format!("{platform}={}", dir.display()));
    }
    let status = command
        .status()
        .with_context(|| format!("running {}", node.display()))?;
    if !status.success() {
        bail!("{} update publish failed", node.display());
    }
    Ok(())
}

/// Run the staged node's own installer, so the install is done by the code
/// that will read it.
fn install_here(dir: &Path) -> Result<()> {
    let node: PathBuf = dir.join(crate::exe_name("cursio", None));
    let status = Command::new(&node)
        .args(["update", "install"])
        .arg(dir)
        .arg("--activate")
        .status()
        .with_context(|| format!("running {}", node.display()))?;
    if !status.success() {
        bail!("{} update install failed", node.display());
    }
    Ok(())
}

fn version_name(crate_version: &str, sha: &str, dirty_hash: Option<&str>) -> String {
    match dirty_hash {
        Some(hash) => format!("{crate_version}+{sha}.dirty.{}", &hash[..hash.len().min(8)]),
        None => format!("{crate_version}+{sha}"),
    }
}

fn component_name(file: &str) -> String {
    file.strip_suffix(".exe").unwrap_or(file).to_owned()
}

fn manifest_json(version: &str, components: &[(String, String, String)]) -> String {
    let entries: Vec<String> = components
        .iter()
        .map(|(name, path, sha)| {
            format!("    \"{name}\": {{ \"path\": \"{path}\", \"sha256\": \"{sha}\" }}")
        })
        .collect();
    format!(
        "{{\n  \"version\": \"{version}\",\n  \"components\": {{\n{}\n  }}\n}}\n",
        entries.join(",\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the major version names the soname a build links, so only it
    /// separates one Linux platform from another.
    #[test]
    fn an_ffmpeg_version_names_its_major_abi() {
        assert_eq!(abi_from_version("59.39.100\n").unwrap(), "av59");
        assert_eq!(abi_from_version("58.29.100").unwrap(), "av58");
        assert!(abi_from_version("").is_err());
        assert!(abi_from_version("v59.1").is_err());
    }

    #[test]
    fn a_clean_build_is_named_by_commit_and_a_dirty_one_also_by_content() {
        assert_eq!(
            version_name("0.1.0", "c9798e7060", None),
            "0.1.0+c9798e7060"
        );
        assert_eq!(
            version_name("0.1.0", "c9798e7060", Some("0123456789abcdef")),
            "0.1.0+c9798e7060.dirty.01234567"
        );
    }

    #[test]
    fn windows_executables_are_named_without_their_extension() {
        assert_eq!(component_name("cursio.exe"), "cursio");
        assert_eq!(component_name("cursio-app"), "cursio-app");
    }

    #[test]
    fn the_manifest_names_every_component_with_its_hash() {
        let json = manifest_json(
            "0.1.0+abc",
            &[
                ("cursio".into(), "cursio.exe".into(), "aa".into()),
                ("cursio-app".into(), "cursio-app.exe".into(), "bb".into()),
            ],
        );
        assert!(json.contains("\"version\": \"0.1.0+abc\""));
        assert!(json.contains("\"cursio\": { \"path\": \"cursio.exe\", \"sha256\": \"aa\" }"));
        assert!(
            json.contains("\"cursio-app\": { \"path\": \"cursio-app.exe\", \"sha256\": \"bb\" }")
        );
    }
}
