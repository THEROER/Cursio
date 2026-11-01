//! One update agent per application whose builds reach this machine.
//!
//! Each application gets the install root `apps/<id>/` under Cursio's own
//! (`versions/`, `current`, `previous`, `store/` — the layout of
//! `docs/update-design.md`, rooted per application), its own trusted builders
//! (`[apps.<id>]` in the config) and its own problems. Agents are made the
//! first time an application is named — by an offer from a peer, or by a
//! build published here — and found again on disk after a restart, so a build
//! published while the node was down is still offered.
//!
//! An offer for an application this machine has never trusted a builder of
//! is refused with a button, exactly as an untrusted Cursio build is: that is
//! how a laptop takes the first build of something it has never run.
//!
//! "Switching" an application is pointing its `current` at the new build.
//! What runs is the application's business: the running copy is told
//! through its apps socket and decides when to restart into it. A phone
//! hands the build to its system installer instead ([`start_with`]), as it
//! does Cursio's own APK.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use cursio_app_protocol::AppId;

use crate::config::Config;
use crate::update::agent::{Agent, Subject};
use crate::update::layout::InstallLayout;
use crate::update::UpdateError;

/// Where every application's builds live, under Cursio's install root.
pub const APPS_DIR: &str = "apps";

/// What hands an application's installed build on, in place of telling its
/// running copies: a phone's is its system installer (`crates/android`).
pub type AppSwitcher =
    Arc<dyn Fn(&AppId, &InstallLayout, &str) -> Result<(), UpdateError> + Send + Sync>;

pub struct Builds {
    root: PathBuf,
    config_path: PathBuf,
    agents: Mutex<BTreeMap<String, Arc<Agent>>>,
    switcher: Option<AppSwitcher>,
    opener: Option<super::phone_open::AppOpener>,
}

static BUILDS: OnceLock<Builds> = OnceLock::new();

/// Start taking applications' builds, reading `[apps]` from `config_path` at
/// every decision. Without an install root this machine takes none.
pub fn start(config_path: PathBuf) {
    let root = match InstallLayout::for_user() {
        Ok(layout) => layout.root().join(APPS_DIR),
        Err(error) => {
            tracing::warn!(%error, "no install root; this machine takes no applications' builds");
            return;
        }
    };
    let _ = BUILDS.set(Builds::at(root, config_path));
}

/// [`start`] with the builds under `root` and handed on by `switcher`: a
/// phone, whose writable directory the system chooses per install and whose
/// applications only its installer may replace — and are opened by `opener`.
pub fn start_with(
    root: PathBuf,
    config_path: PathBuf,
    switcher: AppSwitcher,
    opener: super::phone_open::AppOpener,
) {
    let mut builds = Builds::at(root, config_path).with_switcher(switcher);
    builds.opener = Some(opener);
    let _ = BUILDS.set(builds);
}

/// The machine's applications' builds, once [`start`]ed.
pub fn builds() -> Option<&'static Builds> {
    BUILDS.get()
}

/// The install layout of `app` under `root`, for the CLI publishing into it
/// without a node.
pub fn layout_of(root: &Path, app: &AppId) -> InstallLayout {
    InstallLayout::at(root.join(app.as_str()))
}

impl Builds {
    pub fn at(root: PathBuf, config_path: PathBuf) -> Self {
        Builds {
            root,
            config_path,
            agents: Mutex::new(BTreeMap::new()),
            switcher: None,
            opener: None,
        }
    }

    /// What opens an application here, where builds go to an installer.
    pub fn opener(&self) -> Option<&super::phone_open::AppOpener> {
        self.opener.as_ref()
    }

    /// Which packages this phone's person allowed as which application, read
    /// now: a button pressed takes effect at once.
    pub fn package_grants(&self) -> Vec<crate::config::PackageGrant> {
        Config::load(&self.config_path)
            .map(|c| c.package_access)
            .unwrap_or_default()
    }

    pub fn with_switcher(mut self, switcher: AppSwitcher) -> Self {
        self.switcher = Some(switcher);
        self
    }

    /// Whether builds here are handed to an installer rather than run from
    /// their install root: what a phone does.
    pub fn hands_to_installer(&self) -> bool {
        self.switcher.is_some()
    }

    fn agents(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<Agent>>> {
        self.agents.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The agent for `app`, made now if this is the first time it is named;
    /// `None` for a name that is not an application id, which is all a peer
    /// gets for sending one.
    pub fn agent(&self, app: &str) -> Option<Arc<Agent>> {
        let id = AppId::parse(app).ok()?;
        let mut agents = self.agents();
        if let Some(agent) = agents.get(app) {
            return Some(agent.clone());
        }
        let agent = Arc::new(self.make(&id));
        agents.insert(app.to_owned(), agent.clone());
        Some(agent)
    }

    /// This machine's settings for `app`, read now.
    pub fn app_config(&self, app: &str) -> crate::config::AppConfig {
        Config::load(&self.config_path)
            .ok()
            .and_then(|config| config.apps.get(app).cloned())
            .unwrap_or_default()
    }

    /// Every application with an agent, and every one with builds on disk.
    pub fn known(&self) -> Vec<Arc<Agent>> {
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if entry.path().is_dir() {
                        let _ = self.agent(name);
                    }
                }
            }
        }
        self.agents().values().cloned().collect()
    }

    fn make(&self, id: &AppId) -> Agent {
        let config_path = self.config_path.clone();
        let app = id.as_str().to_owned();
        let running = crate::apps::registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .version(id);
        let notify = id.clone();
        let handed_on = self.switcher.clone();
        Agent::new(
            layout_of(&self.root, id),
            cursio_protocol::update::this_platform(),
            move || match Config::load(&config_path) {
                Ok(config) => config.apps.get(&app).cloned().unwrap_or_default().as_update(),
                Err(error) => {
                    tracing::warn!(%error, app, "cannot read the config; taking none of this application's builds");
                    crate::config::AppConfig {
                        policy: crate::config::UpdatePolicy::Never,
                        ..Default::default()
                    }
                    .as_update()
                }
            },
        )
        .for_subject(Subject::app(id.as_str()), running)
        .with_switcher(Box::new(move |layout, version| match &handed_on {
            Some(hand_on) => {
                layout.activate(version)?;
                hand_on(&notify, layout, version)
            }
            None => switch(&notify, layout, version),
        }))
    }
}

/// Point `app`'s `current` at `version`, and tell its running copies.
fn switch(app: &AppId, layout: &InstallLayout, version: &str) -> Result<(), UpdateError> {
    layout.activate(version)?;
    let dir = layout.version_dir(version);
    tracing::info!(%app, %version, dir = %dir.display(), "an application's build is installed and current");
    crate::apps::serve::build_ready(app, version, &dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cursio-builds-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_application_is_known_by_its_id_and_found_again_on_disk() {
        let root = scratch("known");
        let builds = Builds::at(root.clone(), root.join("config.toml"));
        assert!(builds.agent("../etc").is_none(), "not an application id");
        let agent = builds.agent("leavepulse").unwrap();
        assert_eq!(agent.subject(), "leavepulse");
        assert!(Arc::ptr_eq(&agent, &builds.agent("leavepulse").unwrap()));

        std::fs::create_dir_all(root.join("other-app")).unwrap();
        let again = Builds::at(root.clone(), root.join("config.toml"));
        let mut names: Vec<String> = again
            .known()
            .iter()
            .map(|a| a.subject().to_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["other-app"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
