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
        /// Mark this file of the build as asking to run as an administrator;
        /// each receiving machine's person answers that once.
        #[arg(long, value_name = "FILE")]
        admin: Vec<String>,
        #[arg(required = true, value_name = "[PLATFORM=]FILE")]
        files: Vec<String>,
    },
    /// Start an application's current build on a connected machine.
    Launch {
        machine: String,
        app: String,
        /// Start it only once this version is current there.
        #[arg(long)]
        version: Option<String>,
        /// The file of the build to run, when it has several.
        #[arg(long)]
        component: Option<String>,
        /// End the copies already running from its install first.
        #[arg(long)]
        restart: bool,
        /// Arguments for the started file, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Publish a build, wait until it is current on `--to`, and start it
    /// there: the whole round from a build on this machine to it running on
    /// another.
    Ship {
        app: String,
        /// The machine to run it on (`all` for every connected one that
        /// takes it).
        #[arg(long)]
        to: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, value_name = "FILE")]
        admin: Vec<String>,
        #[arg(long)]
        component: Option<String>,
        /// Install only; start nothing.
        #[arg(long)]
        no_launch: bool,
        /// How long to wait for the build to arrive, in seconds.
        #[arg(long, default_value_t = 300)]
        wait: u64,
        #[arg(required = true, value_name = "[PLATFORM=]FILE")]
        files: Vec<String>,
        /// Arguments for the started file, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// The actions a running application declared on a connected machine.
    Actions { machine: String, app: String },
    /// Ask a running application on a connected machine to do one of its
    /// declared actions; prints its JSON answer.
    Call {
        machine: String,
        /// `<app>`, or `<app>/<process>` for one of its processes.
        target: String,
        action: String,
        /// The arguments, as JSON; none when absent.
        json: Option<String>,
    },
}

/// Where progress lines go: the terminal for the CLI, a tool's answer for
/// `cursio mcp`, whose stdout is its protocol.
pub type Say<'a> = &'a mut (dyn FnMut(String) + Send);

/// Where applications' builds live on this machine.
pub fn apps_root() -> anyhow::Result<PathBuf> {
    Ok(InstallLayout::for_user()?.root().join(APPS_DIR))
}

pub async fn run(action: AppsAction, config_path: &Path) -> anyhow::Result<()> {
    let root = apps_root()?;
    let mut print = |line: String| println!("{line}");
    match action {
        AppsAction::Status => {
            print!("{}", status(&root, config_path)?);
            Ok(())
        }
        AppsAction::Publish {
            app,
            version,
            admin,
            files,
        } => publish(&root, &app, version, &admin, &files, &mut print).map(drop),
        AppsAction::Launch {
            machine,
            app,
            version,
            component,
            restart,
            args,
        } => {
            let what = launch(&machine, &app, version, component, restart, true, args).await?;
            println!("{machine}: {what}");
            Ok(())
        }
        AppsAction::Ship {
            app,
            to,
            version,
            admin,
            component,
            no_launch,
            wait,
            files,
            args,
        } => {
            let version = publish(&root, &app, version, &admin, &files, &mut print)?;
            let start = Start {
                component,
                launch: !no_launch,
                args,
            };
            ship(&to, &app, &version, start, wait, &mut print).await
        }
        AppsAction::Actions { machine, app } => {
            print!("{}", describe_actions(&actions(&machine, &app).await?));
            Ok(())
        }
        AppsAction::Call {
            machine,
            target,
            action,
            json,
        } => {
            let answer = call(&machine, &target, &action, json.as_deref().unwrap_or("")).await?;
            println!("{answer}");
            Ok(())
        }
    }
}

/// The actions `app` declared on `machine`.
pub async fn actions(
    machine: &str,
    app: &str,
) -> anyhow::Result<Vec<cursio_protocol::diag::AppAction>> {
    use crate::control::{Command, Reply};
    let command = Command::AppActions {
        machine: machine.to_owned(),
        app: app.to_owned(),
    };
    match client().await?.request(command).await? {
        Reply::Actions { actions } => Ok(actions),
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the running node answered {other:?}"),
    }
}

/// Actions as a person reads them: one per line, its arguments' schema
/// beneath.
pub fn describe_actions(actions: &[cursio_protocol::diag::AppAction]) -> String {
    if actions.is_empty() {
        return "it declares no actions\n".to_owned();
    }
    let mut out = String::new();
    for action in actions {
        let process = action
            .process
            .as_deref()
            .map(|p| format!(" [{p}]"))
            .unwrap_or_default();
        let running = if action.running { "" } else { " (not running)" };
        out.push_str(&format!(
            "{}{process}{running} — {}\n    args: {}\n",
            action.name, action.description, action.schema_json
        ));
    }
    out
}

/// Ask `target` (`app` or `app/process`) on `machine` to do `action` with the
/// JSON `args_json`; its answer, as JSON text.
pub async fn call(
    machine: &str,
    target: &str,
    action: &str,
    args_json: &str,
) -> anyhow::Result<String> {
    use crate::control::{Command, Reply};
    let (app, process) = match target.split_once('/') {
        Some((app, process)) => (app, Some(process.to_owned())),
        None => (target, None),
    };
    let command = Command::CallAction {
        machine: machine.to_owned(),
        app: app.to_owned(),
        process,
        action: action.to_owned(),
        args_json: args_json.to_owned(),
    };
    match client().await?.request(command).await? {
        Reply::Called { result_json } => Ok(result_json),
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the running node answered {other:?}"),
    }
}

async fn client() -> anyhow::Result<crate::control::transport::Client> {
    let endpoint = crate::control::transport::endpoint()?;
    let (client, _pushes) = crate::control::transport::connect(&endpoint)
        .await
        .map_err(|e| anyhow::anyhow!("no running node to ask ({e}); start `cursio mesh` first"))?;
    Ok(client)
}

/// Start (or, with `start` false, only check) `app` on `machine`, answering
/// what that machine did.
pub async fn launch(
    machine: &str,
    app: &str,
    version: Option<String>,
    component: Option<String>,
    restart: bool,
    start: bool,
    args: Vec<String>,
) -> anyhow::Result<String> {
    use crate::control::{Command, Reply};
    let command = Command::Launch {
        machine: machine.to_owned(),
        app: app.to_owned(),
        version,
        component,
        restart,
        start,
        args,
    };
    match client().await?.request(command).await? {
        Reply::Launched { what } => Ok(what),
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the running node answered {other:?}"),
    }
}

/// Machines `to` names: one, or every connected desktop for `all`.
async fn targets(to: &str) -> anyhow::Result<Vec<String>> {
    use crate::control::{Command, Reply};
    if to != "all" {
        return Ok(vec![to.to_owned()]);
    }
    let Reply::Status(status) = client().await?.request(Command::Status).await? else {
        anyhow::bail!("the running node did not answer with its status");
    };
    // Machines whose link carries the apps module: those that can take it.
    Ok(status
        .peers
        .iter()
        .filter(|p| {
            p.link
                .as_ref()
                .is_some_and(|l| l.modules.iter().any(|m| m == "apps"))
        })
        .map(|p| p.name.clone())
        .collect())
}

/// How a shipped build is started once it is current.
#[derive(Debug, Clone, Default)]
pub struct Start {
    /// The file to run, when the build has several.
    pub component: Option<String>,
    /// `false` installs only.
    pub launch: bool,
    pub args: Vec<String>,
}

/// Wait until `version` of `app` is current on each target, then start it.
pub async fn ship(
    to: &str,
    app: &str,
    version: &str,
    start: Start,
    wait: u64,
    say: Say<'_>,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    let mut failed = Vec::new();
    for machine in targets(to).await? {
        say(format!("{machine}: waiting for {app} {version}…"));
        let mut said = String::new();
        loop {
            // A launch that names the version is refused until it is current
            // there, with why, which is the progress worth showing.
            let attempt = launch(
                &machine,
                app,
                Some(version.to_owned()),
                start.component.clone(),
                start.launch,
                start.launch,
                start.args.clone(),
            )
            .await;
            match attempt {
                Ok(what) => {
                    say(format!("{machine}: {what}"));
                    break;
                }
                Err(error) => {
                    let message = error.to_string();
                    let waiting = message.contains("not current here yet");
                    if !waiting || std::time::Instant::now() >= deadline {
                        say(format!("{machine}: {message}"));
                        failed.push(machine.clone());
                        break;
                    }
                    if message != said {
                        say(format!("{machine}: {message}"));
                        said = message;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        }
    }
    match failed.is_empty() {
        true => Ok(()),
        false => anyhow::bail!("{app} {version} did not reach {}", failed.join(", ")),
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

/// Sign `files` into `app`'s store as one build, answering its version.
/// `admin` names files (by file name) that ask to run as an administrator.
pub fn publish(
    root: &Path,
    app: &str,
    version: Option<String>,
    admin: &[String],
    files: &[String],
    say: Say<'_>,
) -> anyhow::Result<String> {
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
        let mut manifest = Manifest::describe(&dir, &version, &refs)?;
        for component in manifest.components.values_mut() {
            component.admin = admin.contains(&component.path);
        }
        manifest.write(&dir)?;
        builds.push((platform, dir));
    }
    if let Some(unknown) = admin.iter().find(|name| {
        !builds
            .iter()
            .any(|(_, dir)| dir.join(name.as_str()).is_file())
    }) {
        let _ = std::fs::remove_dir_all(&staging);
        anyhow::bail!("--admin {unknown} names no file of this build; name it by file name");
    }
    let key = BuilderKey::load_or_create()?;
    let statement = store.publish(&key, Channel::Dev, &builds, now());
    let _ = std::fs::remove_dir_all(&staging);
    let statement = statement?;
    let platforms: Vec<&str> = builds.iter().map(|(p, _)| p.as_str()).collect();
    say(format!(
        "published {id} {} for {}, signed by {}",
        statement.version,
        platforms.join(", "),
        sign::short_key(&key.public())
    ));
    say(format!(
        "a running node offers it to its peers within {} s; each takes it once its \
         person trusts this machine for {id}",
        crate::update::lane::REOFFER.as_secs()
    ));
    for gone in store.prune(3) {
        say(format!("stopped holding {gone}"));
    }
    Ok(statement.version)
}

/// Applications whose builds this machine holds, as lines of text.
pub fn status(root: &Path, config_path: &Path) -> anyhow::Result<String> {
    use std::fmt::Write as _;
    let mut out = String::new();
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
        out.push_str(
            "no application's builds are here; `cursio apps publish <app> <file>` offers one\n",
        );
        return Ok(out);
    }
    for id in ids {
        let Ok(app) = AppId::parse(&id) else {
            continue;
        };
        let layout = layout_of(root, &app);
        let current = layout.current();
        let _ = writeln!(out, "{id}");
        for version in layout.installed() {
            let mark = match current.as_deref() == Some(version.as_str()) {
                true => " (current)",
                false => "",
            };
            let _ = writeln!(
                out,
                "  installed {version}{mark}  {}",
                layout.version_dir(&version).display()
            );
        }
        for held in Store::of(&layout).held() {
            let _ = writeln!(
                out,
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
                let _ = writeln!(out, "  takes builds from {}", names.join(", "));
            }
            _ => out.push_str("  takes builds from nobody yet\n"),
        }
    }
    Ok(out)
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
