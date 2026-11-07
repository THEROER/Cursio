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
//!
//! **A phone switches differently.** Its node lives in the app's process, and
//! Android replaces an app only through its own installer, which asks the
//! person. So the phone's agent is given a [`Switcher`] that hands the APK to
//! that installer, and offers it as soon as it is in: the "switch" is a
//! notification the person taps, which is already the asking `ask` means.
//!
//! **Other applications' builds are agents too.** An application on the mesh
//! (`docs/app-sdk-design.md`) gets an agent of its own — its own install root,
//! store and trusted builders — named by a [`Subject`], so its refusals, its
//! failures and its button stand apart from Cursio's own. Trusting a builder
//! for Cursio never extends to an application, nor the other way.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use cursio_protocol::update::{Offer, Statement};
use cursio_protocol::PeerId;
use serde::{Deserialize, Serialize};

use crate::config::{ConfigEdit, TrustedBuilder, UpdateConfig, UpdatePolicy};
use crate::diagnostics::{self, Problem, ProblemAction, Severity};

/// What switches this machine to an installed build, in place of the update
/// helper; see the module docs.
pub type Switcher = Box<dyn Fn(&InstallLayout, &str) -> Result<(), UpdateError> + Send + Sync>;

/// Whose builds an agent takes: Cursio's own, or one application's.
pub struct Subject {
    /// How the person knows it: `Cursio`, or the application's id.
    pub name: String,
    /// Prefix of the agent's problem ids: `update`, `app.<id>`.
    problems: String,
    /// The config change "accept builds from this machine" makes.
    trust: Box<dyn Fn(TrustedBuilder) -> ConfigEdit + Send + Sync>,
}

impl Subject {
    pub fn cursio() -> Self {
        Subject {
            name: "Cursio".into(),
            problems: "update".into(),
            trust: Box::new(ConfigEdit::TrustBuilder),
        }
    }

    pub fn app(id: &str) -> Self {
        let app = id.to_owned();
        Subject {
            name: id.to_owned(),
            problems: format!("app.{id}"),
            trust: Box::new(move |builder| ConfigEdit::TrustAppBuilder {
                app: app.clone(),
                builder,
            }),
        }
    }
}

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
    /// The machine it came from; empty for one found on disk at startup,
    /// whose sender was not kept.
    pub from: String,
    pub published_at: u64,
}

/// A build installed here that only the person can put on: on a phone, one
/// handed to the system installer and not installed yet. Listed so the app's
/// window can offer it again — a notification swiped away, or an installer
/// left by mistake, was otherwise the end of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingBuild {
    /// The application, or `None` for Cursio itself.
    pub app: Option<String>,
    pub version: String,
    pub from: String,
    pub published_at: u64,
    /// What the person knows the application as — on a phone, its APK's
    /// label rather than its id. Absent when nothing here can say, and from
    /// an older node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The application's icon as base64 PNG, small; absent as `label` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// A build coming in over the mesh right now, for the app's progress bar and
/// a phone's notification: what was sent there was otherwise invisible on
/// it until the installer opened, minutes later for an APK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchingBuild {
    /// The application, or `None` for Cursio itself.
    pub app: Option<String>,
    pub version: String,
    pub from: String,
    pub received: u64,
    pub total: u64,
}

/// Every build this machine is fetching now, Cursio's first.
pub fn fetching() -> Vec<FetchingBuild> {
    let apps = crate::apps::builds::builds()
        .map(|b| b.known())
        .unwrap_or_default();
    global()
        .into_iter()
        .chain(apps)
        .filter_map(|agent| {
            let progress = agent.status().progress?;
            Some(FetchingBuild {
                app: agent.app().map(str::to_owned),
                version: progress.version,
                from: progress.from,
                received: progress.received,
                total: progress.total,
            })
        })
        .collect()
}

/// How an application's build shows to the person.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Look {
    pub label: Option<String>,
    /// Base64 PNG.
    pub icon: Option<String>,
}

/// Says how an application's waiting build (its id and version) looks.
type Looker = Box<dyn Fn(&str, &str) -> Look + Send + Sync>;

static LOOKER: OnceLock<Looker> = OnceLock::new();

/// How this machine shows an application's build; a phone reads its APK's
/// label and icon. The first call wins.
pub fn set_looker(looker: impl Fn(&str, &str) -> Look + Send + Sync + 'static) {
    let _ = LOOKER.set(Box::new(looker));
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
    /// Switches in place of the update helper, at once when installed.
    switcher: Option<Switcher>,
    /// When the running build was made, where its store cannot say: a
    /// phone's app installed by hand holds no statement of itself.
    built_at: Option<u64>,
    subject: Subject,
    /// The store's builds and when they were read; see [`Agent::held_builds`].
    held: Mutex<Option<(std::time::Instant, Vec<super::store::Held>)>>,
}

#[derive(Default)]
struct State {
    /// Which peer each linked machine's name is, so trusting its builds
    /// from a refusal can name it.
    peers: HashMap<String, PeerId>,
    fetching: Option<String>,
    /// Versions that failed here; not tried again this run.
    given_up: HashSet<String>,
    /// Versions whose refusal has been reported.
    reported: HashSet<String>,
    status: UpdateStatus,
}

const REFUSED: &str = "refused";
const FAILED: &str = "failed";
const READY: &str = "ready";

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
            switcher: None,
            built_at: None,
            subject: Subject::cursio(),
            held: Mutex::new(None),
        }
    }

    /// Take `subject`'s builds rather than Cursio's, with `running` as the
    /// build of it this machine has now.
    pub fn for_subject(mut self, subject: Subject, running: Option<String>) -> Self {
        self.subject = subject;
        self.state().status.running = running.unwrap_or_default();
        self
    }

    pub fn subject(&self) -> &str {
        &self.subject.name
    }

    fn problem(&self, what: &str) -> String {
        format!("{}.{what}", self.subject.problems)
    }

    /// The running build was made at `at` (Unix seconds): take nothing
    /// published before. A phone's app put on by hand had an empty store, so
    /// every build its computer still held looked new — it took an older one
    /// than it ran and offered to install it.
    pub fn with_built_at(mut self, at: Option<u64>) -> Self {
        self.built_at = at;
        self
    }

    /// Whether builds are handed to `switcher` rather than to the update
    /// helper: what a phone does.
    pub fn hands_on(&self) -> bool {
        self.switcher.is_some()
    }

    /// The build installed and waiting, if any.
    pub fn ready(&self) -> Option<Ready> {
        self.status().ready
    }

    /// The application whose builds these are, or `None` for Cursio's own.
    pub fn app(&self) -> Option<&str> {
        self.subject
            .problems
            .starts_with("app.")
            .then_some(self.subject.name.as_str())
    }

    /// The ready build, when only the person can put it on.
    pub fn waiting(&self) -> Option<WaitingBuild> {
        if !self.hands_on() {
            return None;
        }
        let ready = self.ready()?;
        let look = self
            .app()
            .and_then(|app| LOOKER.get().map(|look| look(app, &ready.version)))
            .unwrap_or_default();
        Some(WaitingBuild {
            app: self.app().map(str::to_owned),
            version: ready.version,
            from: ready.from,
            published_at: ready.published_at,
            label: look.label,
            icon: look.icon,
        })
    }

    /// `version` is what this machine has now: the person put it on.
    pub fn now_running(&self, version: &str) {
        self.state().status.running = version.to_owned();
    }

    /// Pick up a build a previous run installed and handed on but that was
    /// never put on. `ready` lives in memory and the store already holds the
    /// build's statement, so a phone whose app restarted before the person
    /// tapped Install treated every later offer of it as old news, and the
    /// build sat in `versions/` with nothing left to start its installer.
    ///
    /// The newest installed build published after the running one was made,
    /// other than the running one and the one a rollback left behind.
    pub fn recover_ready(self) -> Self {
        let running = self.state().status.running.clone();
        let previous = self.layout.previous();
        let floor = self.built_at.unwrap_or(0);
        let found = self
            .layout
            .installed()
            .into_iter()
            .filter(|v| *v != running && previous.as_deref() != Some(v.as_str()))
            .filter_map(|v| {
                let at = self.store.load(&v)?.statement.published_at;
                (at > floor).then_some((at, v))
            })
            .max();
        if let Some((published_at, version)) = found {
            tracing::info!(subject = %self.subject.name, %version, "a build installed by an earlier run is still waiting");
            self.state().status.ready = Some(Ready {
                version,
                from: String::new(),
                published_at,
            });
        }
        self
    }

    /// Switch through `switcher`, as soon as a build is installed, rather
    /// than through the update helper at a quiet moment; see the module docs.
    pub fn with_switcher(mut self, switcher: Switcher) -> Self {
        self.switcher = Some(switcher);
        self
    }

    /// The machine called `name` is `peer`.
    pub fn know_peer(&self, name: &str, peer: PeerId) {
        self.state().peers.insert(name.to_owned(), peer);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The builds the store holds, as last read.
    ///
    /// Reading the store is a directory walk with a file read and a parse per
    /// build, and every link's update module asks for it every
    /// [`REOFFER`](super::lane::REOFFER). Done in each module's pump that cost
    /// 50–250 ms a time on a Windows machine, once 14 s, three links at once.
    /// So the node's watcher thread reads it ([`Self::look_at_store`]) and the
    /// links take the answer; a listing older than `REOFFER` — no watcher,
    /// as on a phone — is read where it is asked for, as before.
    pub fn held_builds(&self) -> Vec<super::store::Held> {
        let cached = self.held.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match cached {
            Some((at, held)) if at.elapsed() < super::lane::REOFFER => held,
            _ => self.look_at_store(),
        }
    }

    /// Read the store now, for [`Self::held_builds`] to answer from.
    pub fn look_at_store(&self) -> Vec<super::store::Held> {
        let held = self.store.held();
        *self.held.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((std::time::Instant::now(), held.clone()));
        held
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
        let mut status = {
            let mut state = self.state();
            // Once what runs is the ready build it is no longer waiting: a
            // phone installs over the running process, and an application
            // reports its version only after it has been put on.
            if state
                .status
                .ready
                .as_ref()
                .is_some_and(|r| r.version == state.status.running)
            {
                state.status.ready = None;
                diagnostics::clear(&self.problem(READY));
            }
            state.status.clone()
        };
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
                // Both mean "not chosen yet" rather than "wrong": a machine
                // that has never taken a dev build follows stable, and says so
                // before it ever checks who signed.
                let a_choice = matches!(
                    refusal,
                    super::sign::Refusal::Untrusted { .. }
                        | super::sign::Refusal::WrongChannel { .. }
                );
                if a_choice {
                    self.store.note_signer(from, &offer.signer);
                }
                // Reported once, but asked again every time: trusting the
                // builder afterwards has to work without a restart.
                let mut state = self.state();
                if state.reported.insert(version.clone()) {
                    let subject = &self.subject.name;
                    tracing::warn!(%subject, %from, %version, %refusal, "refused a build offered over the mesh");
                    // A choice not made yet is offered as one — accepting
                    // trusts the key and follows dev, which answers both;
                    // any other refusal is a fault.
                    let label = match self.subject.problems.as_str() {
                        "update" => format!("Accept builds from {from}"),
                        _ => format!("Take {subject} builds from {from}"),
                    };
                    let actions = match a_choice {
                        true => vec![ProblemAction {
                            label,
                            edit: (self.subject.trust)(TrustedBuilder {
                                peer: state.peers.get(from).copied().unwrap_or(PeerId::nil()),
                                name: from.to_owned(),
                                key: super::sign::hex(&offer.signer),
                            }),
                        }],
                        false => Vec::new(),
                    };
                    // The refusal's own words name `cursio update
                    // trust-builder`, which trusts a machine for Cursio; for
                    // an application the button is the one way, so say it
                    // without them.
                    let summary = match (self.subject.problems.as_str(), a_choice) {
                        ("update", _) => {
                            format!("{from} offered build {version}, and it was refused: {refusal}")
                        }
                        (_, true) => format!(
                            "{from} offered {subject} {version}; this machine takes {subject} \
                             only from machines trusted for it"
                        ),
                        (_, false) => format!(
                            "{from} offered {subject} {version}, and it was refused: {refusal}"
                        ),
                    };
                    diagnostics::raise(Problem {
                        id: self.problem(REFUSED),
                        severity: Severity::Warning,
                        summary,
                        hint: None,
                        actions,
                        since: SystemTime::now(),
                    });
                    state.status.last = Some(format!("refused {version} from {from}: {refusal}"));
                }
                return Decision::Skip;
            }
        };
        // Accepted now: an earlier refusal (an untrusted builder since
        // trusted) is over.
        diagnostics::clear(&self.problem(REFUSED));
        let version = statement.version.clone();
        let newest = self.newest_known();
        let installed_here = self.layout.installed().contains(&version);
        // A build already in `versions` but not running — put there by hand
        // with `cursio update install`, or fetched by a run that stopped
        // before switching — is not fetched again, and used to be skipped
        // outright, so the node stayed on its older build for good. It is as
        // ready as one just fetched. Not when it is `previous`: that is where
        // a rollback leaves a build that did not come up, and switching to it
        // again would only fail again.
        if installed_here
            && statement.published_at > newest
            && self.layout.previous().as_deref() != Some(version.as_str())
        {
            let state = self.state();
            let fresh = version != state.status.running
                && !state.given_up.contains(&version)
                && state
                    .status
                    .ready
                    .as_ref()
                    .is_none_or(|r| r.published_at < statement.published_at);
            drop(state);
            if fresh {
                tracing::info!(%version, %from, "an offered build is already installed here; ready to switch to it");
                self.mark_ready(&version, from, statement.published_at);
            }
            return Decision::Skip;
        }
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
            .max(self.built_at.unwrap_or(0))
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
        let subject = &self.subject.name;
        tracing::warn!(%subject, %version, %from, %reason, "a build from the mesh could not be installed");
        diagnostics::raise(Problem {
            id: self.problem(FAILED),
            severity: Severity::Warning,
            summary: format!(
                "{subject} build {version} from {from} could not be installed: {reason}"
            ),
            hint: None,
            actions: Vec::new(),
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
        release(&mut self.state(), &version);
        diagnostics::clear(&self.problem(FAILED));
        tracing::info!(subject = %self.subject.name, %version, %from, "installed a build from the mesh");
        self.mark_ready(&version, from, statement.published_at);
    }

    /// `version`, installed, is the one to switch to: under `auto` at the
    /// next quiet moment, under `ask` when the person says so.
    fn mark_ready(&self, version: &str, from: &str, published_at: u64) {
        {
            let mut state = self.state();
            state.status.ready = Some(Ready {
                version: version.to_owned(),
                from: from.to_owned(),
                published_at,
            });
            state.status.last = Some(format!("installed {version} from {from}"));
        }
        if self.switcher.is_some() {
            if let Err(error) = self.switch(version) {
                tracing::warn!(%version, %error, "could not offer the installed build");
            }
            return;
        }
        if self.config().policy == UpdatePolicy::Ask {
            diagnostics::raise(Problem {
                id: self.problem(READY),
                severity: Severity::Info,
                summary: format!(
                    "{} {version} is installed and ready to switch to",
                    self.subject.name
                ),
                hint: Some(
                    "switch from the app, or run `cursio update apply` on this machine".into(),
                ),
                actions: Vec::new(),
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
        if let Some(switcher) = &self.switcher {
            switcher(&self.layout, version)?;
            diagnostics::clear(&self.problem(READY));
            self.state().status.last = Some(format!("switched to {version}"));
            return Ok(());
        }
        let exe = super::layout::stable_executable("cursio")
            .or_else(|| std::env::current_exe().ok())
            .ok_or(UpdateError::NoRoot)?;
        let unit = super::systemd::own_unit();
        let mut args: Vec<std::ffi::OsString> = vec![
            "update".into(),
            "apply".into(),
            "--version".into(),
            version.into(),
            "--wait-pid".into(),
            std::process::id().to_string().into(),
            "--stop".into(),
        ];
        if let Some(unit) = &unit {
            args.extend(["--unit".into(), unit.into()]);
        }
        args.push("--".into());
        args.extend(std::env::args_os().skip(1));
        // Under a unit the helper must leave its cgroup, or stopping the node
        // takes the helper with it (see `super::systemd`). A scope rather than
        // a service: the helper re-runs itself from a copy and exits, and a
        // service would end — killing the copy — when its first process did.
        let mut command = match &unit {
            Some(_) => {
                let mut c = std::process::Command::new("systemd-run");
                c.args(["--user", "--scope", "--quiet", "--collect"])
                    .arg(format!("--unit=cursio-update-{}", std::process::id()))
                    .arg("--")
                    .arg(&exe);
                c
            }
            None => std::process::Command::new(&exe),
        };
        command.args(&args);
        super::apply::detach(&mut command);
        command.spawn().map_err(|e| UpdateError::io(&exe, e))?;
        diagnostics::clear(&self.problem(READY));
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

/// Every build on this machine waiting for the person: Cursio's own, then
/// each application's. Empty on a computer, which switches by itself.
pub fn waiting() -> Vec<WaitingBuild> {
    let apps = crate::apps::builds::builds()
        .filter(|b| b.hands_to_installer())
        .map(|b| b.known())
        .unwrap_or_default();
    global()
        .into_iter()
        .chain(apps)
        .filter_map(|agent| agent.waiting())
        .collect()
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
    let agent = install(Agent::new(layout, super::platform(), move || {
        match crate::config::Config::load(&config_path) {
            Ok(config) => config.update,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the config; updates fall back to asking");
                UpdateConfig::default()
            }
        }
    }));
    watch(agent);
}

/// Check for a quiet moment every so often, for as long as the process runs.
pub fn watch(agent: Arc<Agent>) {
    const EVERY: Duration = Duration::from_secs(15);
    let spawned = std::thread::Builder::new()
        .name("update-switch".into())
        .spawn(move || loop {
            std::thread::sleep(EVERY);
            // Every link's module offers from this; reading it here keeps the
            // walk off their pumps (see `Agent::held_builds`).
            agent.look_at_store();
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

    #[test]
    fn accepting_a_builder_from_its_refusal_lets_its_next_offer_through() {
        let key = BuilderKey::from_seed([6; 32]);
        let o = offer(&key, "0.9.0+phone", 9);
        let mut config = crate::config::Config::default();
        config.update.trusted_builders.clear();
        assert!(crate::update::sign::verify(&o, &config.update).is_err());
        ConfigEdit::TrustBuilder(TrustedBuilder {
            peer: PeerId::nil(),
            name: "Nocturne".into(),
            key: hex(&o.signer),
        })
        .apply(&mut config);
        assert_eq!(config.update.channel, UpdateChannel::Dev);
        assert!(crate::update::sign::verify(&o, &config.update).is_ok());
    }

    #[test]
    fn an_application_builder_is_trusted_for_that_application_alone() {
        let key = BuilderKey::from_seed([7; 32]);
        let o = offer(&key, "0.6.0+launcher", 9);
        let mut config = crate::config::Config::default();
        let refusal = Subject::app("leavepulse");
        let builder = TrustedBuilder {
            peer: PeerId::nil(),
            name: "Nocturne".into(),
            key: hex(&o.signer),
        };
        (refusal.trust)(builder).apply(&mut config);
        let app = config.apps["leavepulse"].as_update();
        assert!(crate::update::sign::verify(&o, &app).is_ok());
        assert!(
            crate::update::sign::verify(&o, &config.update).is_err(),
            "Cursio's own builds are not taken from it"
        );
        assert!(!config.apps.contains_key("other"));
    }

    #[test]
    fn an_application_refusal_offers_its_own_button() {
        let root = scratch("app-refusal");
        let key = BuilderKey::from_seed([8; 32]);
        let agent = Agent::new(InstallLayout::at(&root), "linux-x86_64".into(), || {
            crate::config::AppConfig::default().as_update()
        })
        .for_subject(Subject::app("leavepulse"), None);
        assert!(matches!(
            agent.consider(&offer(&key, "0.6.0+x", 9), "Nocturne"),
            Decision::Skip
        ));
        let problem = crate::diagnostics::snapshot()
            .into_iter()
            .find(|p| p.id == "app.leavepulse.refused")
            .expect("the refusal is the application's own");
        assert_eq!(
            problem.actions[0].label,
            "Take leavepulse builds from Nocturne"
        );
        assert!(matches!(
            problem.actions[0].edit,
            ConfigEdit::TrustAppBuilder { ref app, .. } if app == "leavepulse"
        ));
        crate::diagnostics::clear("app.leavepulse.refused");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_build_published_before_the_running_one_was_made_is_not_taken() {
        let root = scratch("built-at");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = agent_at(&root, &key, UpdatePolicy::Ask).with_built_at(Some(100));
        assert!(matches!(
            agent.consider(&offer(&key, "0.9.0+older", 90), "a"),
            Decision::Skip
        ));
        assert!(matches!(
            agent.consider(&offer(&key, "0.9.0+newer", 110), "a"),
            Decision::Fetch(_)
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_switcher_is_offered_the_build_the_moment_it_is_installed() {
        let root = scratch("switcher");
        let key = BuilderKey::from_seed([5; 32]);
        let offered: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = offered.clone();
        let agent =
            agent_at(&root, &key, UpdatePolicy::Ask).with_switcher(Box::new(move |_, version| {
                seen.lock().unwrap().push(version.to_owned());
                Ok(())
            }));
        let statement = Statement::from_bytes(&offer(&key, "0.9.0+apk", 9).statement).unwrap();
        agent.installed(&statement, "Nocturne");
        assert_eq!(*offered.lock().unwrap(), vec!["0.9.0+apk".to_string()]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A version directory as `cursio update install` leaves it.
    fn install_by_hand(root: &std::path::Path, version: &str) {
        let dir = root.join("versions").join(version);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), "{}").unwrap();
    }

    #[test]
    fn a_build_installed_by_hand_is_switched_to_when_a_peer_offers_it() {
        let root = scratch("by-hand");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = agent_at(&root, &key, UpdatePolicy::Ask);
        install_by_hand(&root, "0.4.0+d");
        let o = offer(&key, "0.4.0+d", 9);
        assert!(
            matches!(agent.consider(&o, "a"), Decision::Skip),
            "already installed: nothing to fetch"
        );
        assert_eq!(agent.status().ready.unwrap().version, "0.4.0+d");
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn handing_on(agent: Agent) -> Agent {
        agent.with_switcher(Box::new(|_, _| Ok(())))
    }

    #[test]
    fn a_build_handed_to_an_installer_is_listed_as_waiting_until_it_runs() {
        let root = scratch("waiting");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = handing_on(agent_at(&root, &key, UpdatePolicy::Ask));
        let statement = Statement::from_bytes(&offer(&key, "0.9.0+apk", 9).statement).unwrap();
        agent.installed(&statement, "Nocturne");
        assert_eq!(
            agent.waiting(),
            Some(WaitingBuild {
                app: None,
                version: "0.9.0+apk".into(),
                from: "Nocturne".into(),
                published_at: 9,
                label: None,
                icon: None,
            })
        );
        agent.now_running("0.9.0+apk");
        assert_eq!(agent.waiting(), None);
        assert_eq!(agent.status().ready, None, "ready means not running yet");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_applications_waiting_build_names_the_application() {
        let root = scratch("waiting-app");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = handing_on(
            agent_at(&root, &key, UpdatePolicy::Ask).for_subject(Subject::app("leavepulse"), None),
        );
        let statement = Statement::from_bytes(&offer(&key, "0.6.0+x", 9).statement).unwrap();
        agent.installed(&statement, "Nocturne");
        let waiting = agent.waiting().unwrap();
        assert_eq!(waiting.app.as_deref(), Some("leavepulse"));
        assert_eq!(waiting.version, "0.6.0+x");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_computer_lists_no_waiting_build_since_it_switches_by_itself() {
        let root = scratch("waiting-desktop");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = agent_at(&root, &key, UpdatePolicy::Ask);
        let statement = Statement::from_bytes(&offer(&key, "0.9.0+d", 9).statement).unwrap();
        agent.installed(&statement, "Nocturne");
        assert!(agent.status().ready.is_some());
        assert_eq!(agent.waiting(), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_build_an_earlier_run_handed_on_is_still_waiting_after_a_restart() {
        let root = scratch("recover");
        let key = BuilderKey::from_seed([5; 32]);
        let first = agent_at(&root, &key, UpdatePolicy::Ask);
        for (version, at) in [("0.8.0+old", 5), ("0.9.0+new", 9)] {
            install_by_hand(&root, version);
            first.store().remember(&offer(&key, version, at)).unwrap();
        }
        let again = handing_on(agent_at(&root, &key, UpdatePolicy::Ask))
            .with_built_at(Some(7))
            .recover_ready();
        let waiting = again.waiting().expect("the newer build still waits");
        assert_eq!(waiting.version, "0.9.0+new");
        assert_eq!(waiting.from, "");

        let caught_up = handing_on(agent_at(&root, &key, UpdatePolicy::Ask))
            .with_built_at(Some(9))
            .recover_ready();
        assert_eq!(caught_up.waiting(), None, "nothing newer than what runs");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_build_a_rollback_left_behind_is_not_switched_to_again() {
        let root = scratch("rolled-back");
        let key = BuilderKey::from_seed([5; 32]);
        let agent = agent_at(&root, &key, UpdatePolicy::Auto);
        install_by_hand(&root, "0.4.0+old");
        install_by_hand(&root, "0.5.0+broken");
        let layout = InstallLayout::at(&root);
        layout.activate("0.5.0+broken").unwrap();
        layout.activate("0.4.0+old").unwrap();
        assert_eq!(layout.previous().as_deref(), Some("0.5.0+broken"));
        let o = offer(&key, "0.5.0+broken", 9);
        assert!(matches!(agent.consider(&o, "a"), Decision::Skip));
        assert!(agent.status().ready.is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
