//! `cargo xtask stage`: a build directory `cursio update install` accepts.
//!
//! The node and the app go into `target/stage/<version>/` beside a
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

use crate::deploy::{current_commit, sha256_file};

#[derive(Clone, Copy, ValueEnum)]
pub enum StageTarget {
    Host,
    Windows,
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
    let (target, out) = match args.target {
        StageTarget::Host => {
            if !args.no_build {
                crate::build(true, Some(crate::BinArg::Cursio), None)?;
                crate::build(true, Some(crate::BinArg::CursioApp), None)?;
            }
            (None, crate::out_dir(true, None))
        }
        StageTarget::Windows => {
            if args.install {
                bail!("--install stages for this machine; a Windows build installs on Windows");
            }
            if !args.no_build {
                crate::windows(true)?;
            }
            (
                Some(crate::WINDOWS_TARGET),
                crate::out_dir(true, Some(crate::WINDOWS_TARGET)),
            )
        }
    };

    let files: Vec<String> = ["cursio", "cursio-app"]
        .iter()
        .map(|bin| crate::exe_name(bin, target))
        .collect();
    let node = out.join(&files[0]);
    let commit = current_commit()?;
    let dirty_hash = match commit.dirty {
        true => Some(sha256_file(&node)?),
        false => None,
    };
    let version = version_name(env!("CARGO_PKG_VERSION"), &commit.sha, dirty_hash.as_deref());

    let dir = crate::target_root().join("stage").join(&version);
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

    if args.install {
        install_here(&dir)?;
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

    #[test]
    fn a_clean_build_is_named_by_commit_and_a_dirty_one_also_by_content() {
        assert_eq!(version_name("0.1.0", "c9798e7060", None), "0.1.0+c9798e7060");
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
        assert!(json.contains("\"cursio-app\": { \"path\": \"cursio-app.exe\", \"sha256\": \"bb\" }"));
    }
}
