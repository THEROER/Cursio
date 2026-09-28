//! The machine-wide half of mesh updates: which offer to take, what happened
//! to it, and when to switch.
//!
//! Links come and go and each carries its own [`Lane`](super::lane::Lane), but
//! the answers here belong to the machine: one build is fetched at a time
//! however many peers offer it, a refusal is reported once, and the switch
//! happens once. So the lanes consult one [`Agent`], installed at startup.
//!
//! **Installing is not switching.** A verified build goes into `versions/`
//! the moment its last file arrives — that costs nothing and changes nothing
//! that runs. Switching restarts the node, so it follows the machine's policy:
//! `auto` waits for a [quiet moment](super::quiet), `ask` waits for the user,
//! `never` fetches nothing at all.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use cursio_protocol::update::{Offer, Statement};
use serde::{Deserialize, Serialize};

use crate::config::{UpdateConfig, UpdatePolicy};
use crate::diagnostics::{self, Problem, Severity};

use super::layout::InstallLayout;
use super::store::Store;
use super::UpdateError;

/// What the app shows about updates. Part of the control status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// `auto`, `ask` or `never`.
    pub policy: String,
    /// `stable` or `dev`.
    pub channel: String,
    /// The build this node runs.
    pub running: String,
    /// A build being fetched right now.
    pub progress: Option<Progress>,
    /// Installed and waiting to be switched to.
    pub ready: Option<Ready>,
    /// Why a ready build is not being switched to yet, under `auto`.
    pub waiting_for: Option<String>,
    /// The last thing that happened, in words.
    pub last: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub version: String,
    pub from: String,
    pub received: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub version: String,
    pub from: String,
    pub published_at: u64,
}

/// What a lane should do with an offer.
#[derive(Debug)]
pub enum Decision {
    /// Fetch this platform's files of the statement.
    Fetch(Statement),
    /// Leave it: already here, older, somebody else fetching, or policy.
    Skip,
}

pub struct Agent {
    layout: InstallLayout,
    store: Store,
    platform: String,
    config: Box<dyn Fn() -> UpdateConfig + Send + Sync>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    fetching: Option<String>,
    /// Versions refused or failed here; not tried again this run.
    given_up: HashSet<String>,
    status: UpdateStatus,
}

const REFUSED: &str = "update.refused";
const FAILED: &str = "update.failed";
const READY: &str = "update.ready";

impl Agent {
    pub fn new(
        layout: InstallLayout,
        platform: String,
        config: impl Fn() -> UpdateConfig + Send + Sync + 'static,
    ) -> Self {
        let store = Store::of(&layout);
        Self {
            layout,
            store,
            platform,
            config: Box::new(config),
            state: Mutex::new(State {
                status: UpdateStatus {
                    running: super::running_build(),
                    ..UpdateStatus::default()
                },
                ..State::default()
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn layout(&self) -> &InstallLayout {
        &self.layout
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn config(&self) -> UpdateConfig {
        (self.config)()
    }

    pub fn status(&self) -> UpdateStatus {
        let config = self.config();
        let mut status = self.state().status.clone();
        status.policy = policy_name(config.policy).into();
        status.channel = match config.channel {
            crate::config::UpdateChannel::Stable => "stable",
            crate::config::UpdateChannel::Dev => "dev",
        }
        .into();
        status
    }

    /// Whether to fetch an offered build. Claims the fetch when the answer is
    /// yes, so a second link offering the same build is told to skip.
    pub fn consider(&self, offer: &Offer, from: &str) -> Decision {
        let config = self.config();
        if config.policy == UpdatePolicy::Never || !offer.have.contains(&self.platform) {
            return Decision::Skip;
        }
        let statement = match super::sign::verify(offer, &config) {
            Ok(statement) => statement,
            Err(refusal) => {
                let version = cursio_protocol::update::Statement::from_bytes(&offer.statement)
                    .map(|s| s.version)
                    .unwrap_or_default();
                let mut state = self.state();
                if state.given_up.insert(version.clone()) {
                    tracing::warn!(%from, %version, %refusal, "refused a build offered over the mesh");
                    diagnostics::raise(Problem {
                        id: REFUSED.into(),
                        severity: Severity::Warning,
                        summary: format!(
                            "{from} offered build {version}, and it was refused: {refusal}"
                        ),
                        hint: None,
                        since: SystemTime::now(),
                    });
                    state.status.last = Some(format!("refused {version} from {from}: {refusal}"));
                }
                return Decision::Skip;
            }
        };
        let version = statement.version.clone();
        let mut state = self.state();
        if state.given_up.contains(&version)
            || state.fetching.is_some()
            || version == state.status.running
            || self.layout.installed().contains(&version)
            || self.store.is_complete(&version, &self.platform)
            || statement.published_at <= self.newest_known()
        {
            return Decision::Skip;
        }
        state.fetching = Some(version.clone());
        state.status.progress = Some(Progress {
            version,
            from: from.to_owned(),
            received: 0,
            total: statement.for_platform(&self.platform).map(|a| a.size).sum(),
        });
        Decision::Fetch(statement)
    }

    /// The newest build this machine has for its own platform, by the time it
    /// was published: an offer has to be newer than that to be worth taking.
    fn newest_known(&self) -> u64 {
        self.store
            .held()
            .into_iter()
            .filter(|h| h.offer.have.contains(&self.platform))
            .map(|h| h.statement.published_at)
            .max()
            .unwrap_or(0)
    }

    pub fn progressed(&self, received: u64) {
        if let Some(progress) = self.state().status.progress.as_mut() {
            progress.received = received;
        }
    }

    /// A fetch ended without a build; it is not tried again this run.
    pub fn failed(&self, version: &str, from: &str, reason: &str) {
        let mut state = self.state();
        release(&mut state, version);
        state.given_up.insert(version.to_owned());
        state.status.last = Some(format!("fetching {version} from {from} failed: {reason}"));
        tracing::warn!(%version, %from, %reason, "a build from the mesh could not be installed");
        diagnostics::raise(Problem {
            id: FAILED.into(),
            severity: Severity::Warning,
            summary: format!("build {version} from {from} could not be installed: {reason}"),
            hint: None,
            since: SystemTime::now(),
        });
    }

    /// A link went away mid-fetch: another peer may offer the build again.
    pub fn abandoned(&self, version: &str) {
        release(&mut self.state(), version);
    }

    /// Every file arrived and checked out: install it, then act on policy.
    pub fn installed(&self, statement: &Statement, from: &str) {
        let version = statement.version.clone();
        {
            let mut state = self.state();
            release(&mut state, &version);
            state.status.ready = Some(Ready {
                version: version.clone(),
                from: from.to_owned(),
                published_at: statement.published_at,
            });
            state.status.last = Some(format!("installed {version} from {from}"));
        }
        diagnostics::clear(FAILED);
        tracing::info!(%version, %from, "installed a build from the mesh");
        if self.config().policy == UpdatePolicy::Ask {
            diagnostics::raise(Problem {
                id: READY.into(),
                severity: Severity::Info,
                summary: format!("Cursio {version} is installed and ready to switch to"),
                hint: Some(
                    "switch from the app, or run `cursio update apply` on this machine".into(),
                ),
                since: SystemTime::now(),
            });
        }
    }

    /// Under `auto`: switch to a ready build if now is quiet. Answers what it
    /// did, for the log and the status.
    pub fn tick(&self) -> Option<String> {
        if self.config().policy != UpdatePolicy::Auto {
            self.state().status.waiting_for = None;
            return None;
        }
        let ready = self.state().status.ready.clone()?;
        if let Some(why) = super::quiet::busy() {
            self.state().status.waiting_for = Some(why);
            return None;
        }
        self.state().status.waiting_for = None;
        Some(match self.switch(&ready.version) {
            Ok(()) => format!("switching to {}", ready.version),
            Err(e) => e.to_string(),
        })
    }

    /// Switch to the ready build now, whatever the policy: the user asked.
    pub fn apply_ready(&self) -> Result<String, UpdateError> {
        let ready = self.state().status.ready.clone().ok_or_else(|| {
            UpdateError::Refused("no build is installed and waiting to be switched to".into())
        })?;
        self.switch(&ready.version)?;
        Ok(ready.version)
    }

    /// Hand over to the apply helper, which stops this node, flips `current`,
    /// starts the new one and rolls back if it does not answer.
    fn switch(&self, version: &str) -> Result<(), UpdateError> {
        let exe = super::layout::stable_executable("cursio")
            .or_else(|| std::env::current_exe().ok())
            .ok_or(UpdateError::NoRoot)?;
        let mut args: Vec<std::ffi::OsString> = vec![
            "update".into(),
            "apply".into(),
            "--version".into(),
            version.into(),
            "--wait-pid".into(),
            std::process::id().to_string().into(),
            "--stop".into(),
            "--".into(),
        ];
        args.extend(std::env::args_os().skip(1));
        let mut command = std::process::Command::new(&exe);
        command.args(&args);
        super::apply::detach(&mut command);
        command.spawn().map_err(|e| UpdateError::io(&exe, e))?;
        diagnostics::clear(READY);
        self.state().status.last = Some(format!("switching to {version}"));
        tracing::info!(%version, "handed over to the update helper; this node will stop");
        Ok(())
    }
}

fn release(state: &mut State, version: &str) {
    if state.fetching.as_deref() == Some(version) {
        state.fetching = None;
        state.status.progress = None;
    }
}

fn policy_name(policy: UpdatePolicy) -> &'static str {
    match policy {
        UpdatePolicy::Auto => "auto",
        UpdatePolicy::Ask => "ask",
        UpdatePolicy::Never => "never",
    }
}

static AGENT: OnceLock<Arc<Agent>> = OnceLock::new();

/// Make `agent` the machine's; the first call wins.
pub fn install(agent: Agent) -> Arc<Agent> {
    AGENT.get_or_init(|| Arc::new(agent)).clone()
}

pub fn global() -> Option<Arc<Agent>> {
    AGENT.get().cloned()
}

/// What the control status reports, empty when updates are not running.
pub fn status() -> UpdateStatus {
    global().map(|a| a.status()).unwrap_or_default()
}

/// Start taking and handing on builds for this node, reading the `[update]`
/// table of `config_path` afresh at every decision.
pub fn start(config_path: std::path::PathBuf) {
    let layout = match InstallLayout::for_user() {
        Ok(layout) => layout,
        Err(e) => {
            tracing::warn!(error = %e, "no install root; this node neither takes nor hands on builds");
            return;
        }
    };
    let agent = install(Agent::new(
        layout,
        cursio_protocol::update::this_platform(),
        move || match crate::config::Config::load(&config_path) {
            Ok(config) => config.update,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the config; updates fall back to asking");
                UpdateConfig::default()
            }
        },
    ));
    watch(agent);
}

/// Check for a quiet moment every so often, for as long as the process runs.
pub fn watch(agent: Arc<Agent>) {
    const EVERY: Duration = Duration::from_secs(15);
    let spawned = std::thread::Builder::new()
        .name("update-switch".into())
        .spawn(move || loop {
            std::thread::sleep(EVERY);
            if let Some(what) = agent.tick() {
                tracing::info!(%what, "update");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the update watcher; builds will only switch when asked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TrustedBuilder, UpdateChannel};
    use crate::update::sign::{hex, BuilderKey};
    use cursio_protocol::update::{Artifact, Channel};

    fn agent_at(root: &std::path::Path, key: &BuilderKey, policy: UpdatePolicy) -> Agent {
        let trusted = hex(&key.public());
        Agent::new(InstallLayout::at(root), "linux-x86_64".into(), move || {
            UpdateConfig {
                policy,
                channel: UpdateChannel::Dev,
                trusted_builders: vec![TrustedBuilder {
                    peer: cursio_protocol::PeerId::nil(),
                    name: "builder".into(),
                    key: trusted.clone(),
                }],
            }
        })
    }

    fn offer(key: &BuilderKey, version: &str, at: u64) -> Offer {
        key.sign(
            &Statement {
                version: version.into(),
                channel: Channel::Dev,
                published_at: at,
                artifacts: vec![Artifact {
                    platform: "linux-x86_64".into(),
                    name: "cursio".into(),
                    size: 1,
                    sha256: [0; 32],
                }],
            },
            vec!["linux-x86_64".into()],
        )
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cursio-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn one_build_is_fetched_once_however_many_peers_offer_it() {
        let root = scratch("once");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = agent_at(&root, &key, UpdatePolicy::Ask);
        let o = offer(&key, "0.2.0+b", 9);
        assert!(matches!(agent.consider(&o, "a"), Decision::Fetch(_)));
        assert!(matches!(agent.consider(&o, "b"), Decision::Skip));
        agent.abandoned("0.2.0+b");
        assert!(matches!(agent.consider(&o, "b"), Decision::Fetch(_)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_machine_that_never_updates_fetches_nothing() {
        let root = scratch("never");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = agent_at(&root, &key, UpdatePolicy::Never);
        assert!(matches!(
            agent.consider(&offer(&key, "v", 9), "a"),
            Decision::Skip
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_build_is_not_fetched_again() {
        let root = scratch("failed");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = agent_at(&root, &key, UpdatePolicy::Ask);
        let o = offer(&key, "0.3.0+c", 9);
        assert!(matches!(agent.consider(&o, "a"), Decision::Fetch(_)));
        agent.failed("0.3.0+c", "a", "hash mismatch");
        assert!(matches!(agent.consider(&o, "a"), Decision::Skip));
        assert!(agent.status().last.unwrap().contains("hash mismatch"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn under_auto_a_ready_build_waits_for_a_quiet_moment() {
        let root = scratch("auto");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = agent_at(&root, &key, UpdatePolicy::Auto);
        agent.state().status.ready = Some(Ready {
            version: "v".into(),
            from: "a".into(),
            published_at: 1,
        });
        let mut driving = crate::update::quiet::DrivingMark::default();
        driving.set(true);
        assert_eq!(agent.tick(), None);
        assert!(agent.status().waiting_for.unwrap().contains("cursor"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
