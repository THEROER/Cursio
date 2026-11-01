//! `cursio apps`: applications' builds on this machine, and publishing one.
//!
//! Publishing needs no running node: it signs the build into the
//! application's store under the install root, where the node's `apps`
//! module finds it and offers it to every link within
//! [`REOFFER`](crate::update::lane::REOFFER). Files are named as they are on
//! disk, flat, one directory per platform — the shape Cursio's own builds
//! have — so a build is whatever files the application needs, an installer
//! or a bare executable alike.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use cursio_app_protocol::AppId;
use cursio_protocol::update::Channel;

use super::builds::{layout_of, APPS_DIR};
use crate::config::Config;
use crate::update::layout::InstallLayout;
use crate::update::manifest::Manifest;
use crate::update::sign::{self, BuilderKey};
use crate::update::store::Store;

#[derive(Debug, clap::Subcommand)]
pub enum AppsAction {
    /// Applications whose builds this machine holds: versions, which is
    /// current, and whose builds it takes.
    Status,
    /// Offer a build of an application to the mesh, signed by this machine.
    ///
    /// Each file is `PLATFORM=PATH` (`windows-x86_64=target/app.exe`), or a
    /// bare path for this machine's own platform. A machine takes the build
    /// once its person trusts this one for that application.
    Publish {
        /// The application's id, as it registers with `cursio-sdk`.
        app: String,
        /// The version to publish it as; a time-stamped `dev.<seconds>` when
        /// absent.
        #[arg(long)]
        version: Option<String>,
        #[arg(required = true, value_name = "[PLATFORM=]FILE")]
        files: Vec<String>,
    },
}

pub fn run(action: AppsAction, config_path: &Path) -> anyhow::Result<()> {
    let root = InstallLayout::for_user()?.root().join(APPS_DIR);
    match action {
        AppsAction::Status => status(&root, config_path),
        AppsAction::Publish {
            app,
            version,
            files,
        } => publish(&root, &app, version, &files),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `[PLATFORM=]FILE` arguments, grouped by platform.
fn by_platform(files: &[String]) -> anyhow::Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut grouped: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for arg in files {
        let (platform, path) = match arg.split_once('=') {
            Some((platform, path)) => (platform.to_owned(), PathBuf::from(path)),
            None => (cursio_protocol::update::this_platform(), PathBuf::from(arg)),
        };
        if !path.is_file() {
            anyhow::bail!("{} is not a file", path.display());
        }
        grouped.entry(platform).or_default().push(path);
    }
    Ok(grouped)
}

fn publish(
    root: &Path,
    app: &str,
    version: Option<String>,
    files: &[String],
) -> anyhow::Result<()> {
    let id = AppId::parse(app).map_err(|e| anyhow::anyhow!("{e}"))?;
    let version = version.unwrap_or_else(|| format!("dev.{}", now()));
    if !crate::update::layout::valid_version(&version) {
        anyhow::bail!(
            "`{version}` is not a usable version; use letters, digits, `.`, `-`, `+` and `_`"
        );
    }
    let layout = layout_of(root, &id);
    let store = Store::of(&layout);
    let staging = store.root().join(".publishing");
    let _ = std::fs::remove_dir_all(&staging);
    let mut builds = Vec::new();
    for (platform, paths) in by_platform(files)? {
        let dir = staging.join(&platform);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let mut names = Vec::new();
        for path in &paths {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("{} has no usable file name", path.display()))?
                .to_owned();
            if names.contains(&name) {
                anyhow::bail!("two files named {name} for {platform}");
            }
            std::fs::copy(path, dir.join(&name))
                .with_context(|| format!("cannot copy {}", path.display()))?;
            names.push(name);
        }
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        Manifest::describe(&dir, &version, &refs)?.write(&dir)?;
        builds.push((platform, dir));
    }
    let key = BuilderKey::load_or_create()?;
    let statement = store.publish(&key, Channel::Dev, &builds, now());
    let _ = std::fs::remove_dir_all(&staging);
    let statement = statement?;
    let platforms: Vec<&str> = builds.iter().map(|(p, _)| p.as_str()).collect();
    println!(
        "published {id} {} for {}, signed by {}",
        statement.version,
        platforms.join(", "),
        sign::short_key(&key.public())
    );
    println!(
        "a running node offers it to its peers within {} s; each takes it once its \
         person trusts this machine for {id}",
        crate::update::lane::REOFFER.as_secs()
    );
    for gone in store.prune(3) {
        println!("stopped holding {gone}");
    }
    Ok(())
}

fn status(root: &Path, config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    let mut ids: Vec<String> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    ids.extend(config.apps.keys().cloned());
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        println!("no application's builds are here; `cursio apps publish <app> <file>` offers one");
        return Ok(());
    }
    for id in ids {
        let Ok(app) = AppId::parse(&id) else {
            continue;
        };
        let layout = layout_of(root, &app);
        let current = layout.current();
        println!("{id}");
        for version in layout.installed() {
            let mark = match current.as_deref() == Some(version.as_str()) {
                true => " (current)",
                false => "",
            };
            println!(
                "  installed {version}{mark}  {}",
                layout.version_dir(&version).display()
            );
        }
        for held in Store::of(&layout).held() {
            println!(
                "  holds {} for {}",
                held.statement.version,
                held.offer.have.join(", ")
            );
        }
        match config.apps.get(&id) {
            Some(settings) if !settings.trusted_builders.is_empty() => {
                let names: Vec<&str> = settings
                    .trusted_builders
                    .iter()
                    .map(|b| b.name.as_str())
                    .collect();
                println!("  takes builds from {}", names.join(", "));
            }
            _ => println!("  takes builds from nobody yet"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cursio-apps-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn files_are_grouped_by_the_platform_they_name() {
        let dir = scratch("group");
        let exe = dir.join("app.exe");
        let apk = dir.join("app.apk");
        std::fs::write(&exe, b"exe").unwrap();
        std::fs::write(&apk, b"apk").unwrap();
        let grouped = by_platform(&[
            format!("windows-x86_64={}", exe.display()),
            format!("android-aarch64={}", apk.display()),
        ])
        .unwrap();
        assert_eq!(grouped["windows-x86_64"], [exe]);
        assert_eq!(grouped["android-aarch64"], [apk]);
        assert!(by_platform(&[dir.join("missing").display().to_string()]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
