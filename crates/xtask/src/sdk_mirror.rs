//! `cargo xtask sdk-mirror`: the public `cursio-sdk` repository, made from
//! this one.
//!
//! The SDK and its protocol are developed here, beside the node's side of
//! that protocol, so a change to both lands in one commit and the two never
//! disagree. Applications of any license depend on them, though, and Cursio's
//! repository is neither public nor permissively licensed — so the two crates
//! are mirrored into a repository of their own, with its own workspace root,
//! and published to crates.io from there by a tag (its `publish` workflow).
//!
//! What the mirror holds is [`ENTRIES`]; anything else in it is removed at
//! every sync, so a file deleted here disappears there too.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Where the public repository lives.
const REMOTE: &str = "git@github.com:THEROER/cursio-sdk.git";

/// What the mirror is made of: a path here, where it goes there, and what to
/// leave out of it.
const ENTRIES: &[(&str, &str, &[&str])] = &[
    ("crates/sdk/mirror/workspace.toml", "Cargo.toml", &[]),
    ("crates/sdk/mirror/README.md", "README.md", &[]),
    ("crates/sdk/mirror/LICENSE-MIT", "LICENSE-MIT", &[]),
    ("crates/sdk/mirror/LICENSE-APACHE", "LICENSE-APACHE", &[]),
    ("crates/sdk/mirror/gitignore", ".gitignore", &[]),
    ("crates/sdk/mirror/github", ".github", &[]),
    ("crates/app-protocol", "crates/app-protocol", &[]),
    ("crates/sdk", "crates/sdk", &["mirror"]),
];

pub struct Options {
    pub push: bool,
    pub tag: bool,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("cannot run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn copy_tree(from: &Path, to: &Path, skip: &[&str]) -> Result<()> {
    if from.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to).with_context(|| format!("cannot copy {}", from.display()))?;
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip.iter().any(|s| name == **s) {
            continue;
        }
        copy_tree(&entry.path(), &to.join(&name), &[])?;
    }
    Ok(())
}

/// Empty `dir` but for its `.git`.
fn clear(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// The SDK's version, which the tag names.
fn sdk_version(root: &Path) -> Result<String> {
    let manifest = std::fs::read_to_string(root.join("crates/sdk/Cargo.toml"))?;
    manifest
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        .context("crates/sdk/Cargo.toml names no version")
}

pub fn run(root: &Path, options: Options) -> Result<()> {
    let mirror: PathBuf = root.join("target/sdk-mirror");
    if !mirror.join(".git").is_dir() {
        let parent = mirror.parent().context("no target directory")?;
        std::fs::create_dir_all(parent)?;
        git(parent, &["clone", REMOTE, "sdk-mirror"])
            .with_context(|| format!("cannot clone {REMOTE}; create the repository first"))?;
    }
    git(&mirror, &["pull", "--ff-only"]).ok();
    clear(&mirror)?;
    for (from, to, skip) in ENTRIES {
        copy_tree(&root.join(from), &mirror.join(to), skip)?;
    }

    // The mirror must stand on its own before anything leaves this machine.
    let target = root.join("target/sdk-mirror-build");
    for args in [
        vec!["fmt", "--all", "--check"],
        vec![
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        vec!["test", "--workspace"],
    ] {
        let status = Command::new("cargo")
            .current_dir(&mirror)
            .env("CARGO_TARGET_DIR", &target)
            .args(&args)
            .status()
            .with_context(|| format!("cannot run cargo {}", args.join(" ")))?;
        if !status.success() {
            bail!(
                "the mirror fails `cargo {}`; nothing was pushed",
                args.join(" ")
            );
        }
    }

    let source = git(root, &["rev-parse", "--short", "HEAD"])?;
    git(&mirror, &["add", "-A"])?;
    let changed = !git(&mirror, &["status", "--porcelain"])?.is_empty();
    if changed {
        git(
            &mirror,
            &["commit", "-m", &format!("chore: sync from cursio {source}")],
        )?;
        println!("mirror: committed the SDK as of cursio {source}");
    } else {
        println!("mirror: already as of cursio {source}");
    }
    if options.push {
        git(&mirror, &["push", "origin", "HEAD:main"])?;
        println!("mirror: pushed to {REMOTE}");
    }
    if options.tag {
        let tag = format!("v{}", sdk_version(root)?);
        let exists = git(&mirror, &["ls-remote", "--tags", "origin", &tag])?;
        if !exists.is_empty() {
            bail!("{tag} is already published; raise the version in crates/sdk and crates/app-protocol first");
        }
        git(&mirror, &["tag", &tag])?;
        git(&mirror, &["push", "origin", &tag])?;
        println!("mirror: tagged {tag}; its publish workflow puts both crates on crates.io");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mirrored_path_exists_here() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for (from, _, _) in ENTRIES {
            assert!(root.join(from).exists(), "{from} is mirrored but missing");
        }
        assert!(
            ENTRIES.iter().any(|(_, to, _)| *to == "Cargo.toml"),
            "the mirror needs a workspace root"
        );
    }

    #[test]
    fn the_sdk_version_is_read_from_its_manifest() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let version = sdk_version(&root).unwrap();
        assert!(version.split('.').count() == 3, "{version}");
    }
}
