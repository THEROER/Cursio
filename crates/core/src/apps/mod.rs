//! Applications on this machine that asked the node to carry something for
//! them; see `docs/app-sdk-design.md`.
//!
//! An application links `cursio-sdk`, connects to the node's apps socket
//! ([`serve`]) and says who it is, which version runs and where it logs. The
//! node keeps that here, by [`AppId`], so a peer asking for `app:<id>`'s log
//! reaches exactly the file the application named — never a path off the
//! wire, which [`crate::diag_session::own_log`] relies on.
//!
//! What an application said is kept on disk (`apps.json` beside the logs):
//! the reason to read an application's log from another machine is usually
//! that it has stopped, and a registry that forgot it on disconnect, or on
//! the node's own restart, would answer "unknown application" exactly then.
//! Only what describes the application is kept; whether it is running and
//! how it said it was doing are live state, and start over with the node.

pub mod access;
pub mod actions;
pub mod blobs;
pub mod builds;
pub mod cli;
pub mod devtools;
pub mod lane;
pub mod launch;
pub mod mesh;
pub mod phone_open;
pub mod serve;
pub mod tunnel;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use cursio_app_protocol::{AppId, HealthState, LogSpec, ProcessName};
use serde::{Deserialize, Serialize};

/// The file the registry keeps in the node's data directory.
const FILE_NAME: &str = "apps.json";

/// One registration: an application, and which of its processes. An
/// application with a window and a daemon logs twice, and two registrations
/// under one key overwrote each other's log.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub app: AppId,
    /// `None` is the application's main process.
    pub process: Option<ProcessName>,
}

impl Key {
    pub fn main(app: AppId) -> Self {
        Key { app, process: None }
    }

    /// How the registry file names it: `app`, or `app/process`.
    fn file_key(&self) -> String {
        match &self.process {
            None => self.app.to_string(),
            Some(process) => format!("{}/{process}", self.app),
        }
    }

    fn from_file_key(text: &str) -> Option<Self> {
        let (app, process) = match text.split_once('/') {
            Some((app, process)) => (app, Some(ProcessName::parse(process).ok()?)),
            None => (text, None),
        };
        Some(Key {
            app: AppId::parse(app).ok()?,
            process,
        })
    }
}

/// One process of an application as the node knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub version: String,
    pub pid: Option<u32>,
    pub logs: Option<LogSpec>,
    pub health: HealthState,
    pub detail: Option<String>,
    /// Open connections from it: two running copies are two.
    pub connections: u32,
    /// What it lets another machine ask it to do, as its latest hello said.
    /// Live state, never saved: a process that is not running does nothing.
    pub actions: Vec<cursio_app_protocol::ActionSpec>,
    /// Topics of messages from its copies elsewhere it listens to, as its
    /// latest hello said. Live state, like `actions`.
    pub topics: Vec<cursio_app_protocol::TopicName>,
    /// It names files for its copies elsewhere to fetch (`on_blob`). Live
    /// state, like `actions`.
    pub blobs: bool,
    /// The DevTools endpoint it declared (`App::devtools`). Live state, like
    /// `actions`; checked against the socket table before it is forwarded.
    pub devtools: Option<cursio_app_protocol::Devtools>,
}

impl Registered {
    pub fn running(&self) -> bool {
        self.connections > 0
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    version: String,
    #[serde(default)]
    logs: Option<LogSpec>,
}

/// Every application process that has registered here.
#[derive(Debug, Default)]
pub struct Registry {
    apps: BTreeMap<Key, Registered>,
    file: Option<PathBuf>,
    /// Raised whenever what runs here changes — a copy connecting or going,
    /// what it listens to — so each link resends this machine's presence
    /// only then.
    generation: u64,
}

impl Registry {
    /// What `file` holds, or nothing when it is absent or unreadable; with
    /// `None`, a registry that is never saved.
    pub fn load(file: Option<PathBuf>) -> Self {
        let saved: BTreeMap<String, Saved> = file
            .as_ref()
            .and_then(|path| match std::fs::read(path) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(saved) => Some(saved),
                    Err(error) => {
                        tracing::warn!(file = %path.display(), %error, "ignoring an unreadable application registry");
                        None
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    tracing::warn!(file = %path.display(), %error, "cannot read the application registry");
                    None
                }
            })
            .unwrap_or_default();
        let apps = saved
            .into_iter()
            .filter_map(|(key, saved)| {
                let registered = Registered {
                    version: saved.version,
                    pid: None,
                    logs: saved.logs,
                    health: HealthState::default(),
                    detail: None,
                    connections: 0,
                    actions: Vec::new(),
                    topics: Vec::new(),
                    blobs: false,
                    devtools: None,
                };
                Some((Key::from_file_key(&key)?, registered))
            })
            .collect();
        Registry {
            apps,
            file,
            generation: 0,
        }
    }

    /// What has changed counts this high; see [`Registry::presence`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Every application registered here, as a peer is told of it: whether a
    /// copy runs, its version, and what its running copies listen to.
    pub fn presence(&self) -> Vec<cursio_protocol::apps::AppPresence> {
        let mut apps: BTreeMap<&AppId, cursio_protocol::apps::AppPresence> = BTreeMap::new();
        for (key, r) in &self.apps {
            let entry =
                apps.entry(&key.app)
                    .or_insert_with(|| cursio_protocol::apps::AppPresence {
                        app: key.app.to_string(),
                        running: false,
                        version: r.version.clone(),
                        topics: Vec::new(),
                    });
            if r.running() {
                entry.running = true;
                for topic in &r.topics {
                    if !entry.topics.iter().any(|t| t == topic.as_str()) {
                        entry.topics.push(topic.to_string());
                    }
                }
            }
        }
        apps.into_values().collect()
    }

    /// The running processes of `app` listening to `topic`.
    pub fn listening(&self, app: &AppId, topic: &cursio_app_protocol::TopicName) -> Vec<Key> {
        self.apps
            .iter()
            .filter(|(key, r)| key.app == *app && r.running() && r.topics.contains(topic))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Whether `key`'s process names files for its copies elsewhere.
    pub fn set_blobs(&mut self, key: &Key, blobs: bool) {
        if let Some(entry) = self.apps.get_mut(key) {
            entry.blobs = blobs;
        }
    }

    /// The DevTools endpoint `key`'s process declared.
    pub fn set_devtools(&mut self, key: &Key, devtools: Option<cursio_app_protocol::Devtools>) {
        if let Some(entry) = self.apps.get_mut(key) {
            entry.devtools = devtools;
        }
    }

    /// The running process of `app` (only `process` when named) that
    /// declared a DevTools endpoint, with its pid.
    pub fn devtools(
        &self,
        app: &AppId,
        process: Option<&ProcessName>,
    ) -> Option<(Key, u32, cursio_app_protocol::Devtools)> {
        self.apps
            .iter()
            .filter(|(key, r)| key.app == *app && r.running())
            .filter(|(key, _)| process.is_none_or(|p| key.process.as_ref() == Some(p)))
            .find_map(|(key, r)| Some((key.clone(), r.pid?, r.devtools?)))
    }

    /// Every pid `app`'s running processes registered with.
    pub fn pids(&self, app: &AppId) -> Vec<u32> {
        self.apps
            .iter()
            .filter(|(key, r)| key.app == *app && r.running())
            .filter_map(|(_, r)| r.pid)
            .collect()
    }

    /// The running processes of `app` that name files, the main one first.
    pub fn blob_servers(&self, app: &AppId) -> Vec<Key> {
        self.apps
            .iter()
            .filter(|(key, r)| key.app == *app && r.running() && r.blobs)
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// What `key`'s process listens to, as its hello said.
    pub fn set_topics(&mut self, key: &Key, topics: Vec<cursio_app_protocol::TopicName>) {
        if let Some(entry) = self.apps.get_mut(key) {
            if entry.topics != topics {
                entry.topics = topics;
                self.generation += 1;
            }
        }
    }

    /// A copy of `key`'s process connected and said this.
    pub fn connected(&mut self, key: &Key, version: String, pid: u32, logs: Option<LogSpec>) {
        let entry = self.apps.entry(key.clone()).or_insert(Registered {
            version: String::new(),
            pid: None,
            logs: None,
            health: HealthState::default(),
            detail: None,
            connections: 0,
            actions: Vec::new(),
            topics: Vec::new(),
            blobs: false,
            devtools: None,
        });
        let changed = entry.version != version || entry.logs != logs;
        entry.version = version;
        entry.pid = Some(pid);
        entry.logs = logs;
        entry.connections += 1;
        self.generation += 1;
        if changed {
            self.save();
        }
    }

    /// What `key`'s process declared it can be asked to do.
    pub fn set_actions(&mut self, key: &Key, actions: Vec<cursio_app_protocol::ActionSpec>) {
        if let Some(entry) = self.apps.get_mut(key) {
            entry.actions = actions;
        }
    }

    /// Every registered process of `app`, the main one first, with whether it
    /// runs and what it declared.
    pub fn processes(&self, app: &AppId) -> Vec<(Key, &Registered)> {
        self.apps
            .iter()
            .filter(|(key, _)| key.app == *app)
            .map(|(key, r)| (key.clone(), r))
            .collect()
    }

    pub fn health(&mut self, key: &Key, state: HealthState, detail: Option<String>) {
        if let Some(entry) = self.apps.get_mut(key) {
            entry.health = state;
            entry.detail = detail;
        }
    }

    /// A copy of `key`'s process closed its connection.
    pub fn disconnected(&mut self, key: &Key) {
        if let Some(entry) = self.apps.get_mut(key) {
            entry.connections = entry.connections.saturating_sub(1);
            if entry.connections == 0 {
                entry.pid = None;
                entry.topics.clear();
            }
            self.generation += 1;
        }
    }

    /// Where each process of `app` said it logs, the main process first.
    pub fn logs(&self, app: &AppId) -> Vec<(Option<ProcessName>, LogSpec)> {
        self.apps
            .iter()
            .filter(|(key, _)| key.app == *app)
            .filter_map(|(key, r)| Some((key.process.clone(), r.logs.clone()?)))
            .collect()
    }

    pub fn get(&self, key: &Key) -> Option<&Registered> {
        self.apps.get(key)
    }

    /// The version `app` runs as here: its main process's, else any one's.
    pub fn version(&self, app: &AppId) -> Option<String> {
        self.apps
            .iter()
            .find(|(key, _)| key.app == *app)
            .map(|(_, r)| r.version.clone())
    }

    fn save(&self) {
        let Some(path) = &self.file else {
            return;
        };
        let saved: BTreeMap<String, Saved> = self
            .apps
            .iter()
            .map(|(key, app)| {
                let saved = Saved {
                    version: app.version.clone(),
                    logs: app.logs.clone(),
                };
                (key.file_key(), saved)
            })
            .collect();
        let bytes = serde_json::to_vec_pretty(&saved).expect("the registry always serializes");
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let partial = path.with_extension("json.partial");
                std::fs::write(&partial, bytes)?;
                std::fs::rename(&partial, path)
            });
        if let Err(error) = written {
            tracing::warn!(file = %path.display(), %error, "cannot save the application registry; it is forgotten at the next restart");
        }
    }
}

/// This node's registry, kept in its data directory.
pub fn registry() -> &'static Arc<Mutex<Registry>> {
    static REGISTRY: OnceLock<Arc<Mutex<Registry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        // A test's registry is never saved: tests register throwaway
        // applications, and saved they landed in the person's own registry.
        let file = match cfg!(test) {
            true => None,
            false => crate::data_dir::data_dir().map(|dir| dir.join(FILE_NAME)),
        };
        Arc::new(Mutex::new(Registry::load(file)))
    })
}

/// Where `app`'s processes said they log, on this node — only `process`'s
/// when one is named.
pub fn logs_of(app: &AppId, process: Option<&ProcessName>) -> Vec<(Option<ProcessName>, LogSpec)> {
    let mut logs = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .logs(app);
    if let Some(process) = process {
        logs.retain(|(p, _)| p.as_ref() == Some(process));
    }
    logs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cursio-apps-registry-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn spec() -> LogSpec {
        LogSpec {
            dir: "/var/log/demo".into(),
            file: "demo.log".into(),
        }
    }

    fn key(app: &str, process: Option<&str>) -> Key {
        Key {
            app: AppId::parse(app).unwrap(),
            process: process.map(|p| ProcessName::parse(p).unwrap()),
        }
    }

    #[test]
    fn an_application_is_running_while_any_copy_is_connected() {
        let mut registry = Registry::load(None);
        let app = key("demo", None);
        registry.connected(&app, "1".into(), 10, None);
        registry.connected(&app, "1".into(), 11, None);
        registry.disconnected(&app);
        assert!(registry.get(&app).unwrap().running());
        registry.disconnected(&app);
        assert!(!registry.get(&app).unwrap().running());
        assert_eq!(registry.get(&app).unwrap().pid, None);
    }

    #[test]
    fn a_window_and_a_daemon_of_one_application_keep_their_own_logs() {
        let file = scratch().join("apps.json");
        let window = key("launcher", None);
        let daemon = key("launcher", Some("daemon"));
        let daemon_log = LogSpec {
            dir: "/var/log/demo".into(),
            file: "daemon".into(),
        };
        {
            let mut registry = Registry::load(Some(file.clone()));
            registry.connected(&window, "1".into(), 1, Some(spec()));
            registry.connected(&daemon, "1".into(), 2, Some(daemon_log.clone()));
            assert_eq!(registry.get(&daemon).unwrap().pid, Some(2));
        }
        let registry = Registry::load(Some(file));
        assert_eq!(
            registry.logs(&window.app),
            vec![(None, spec()), (daemon.process.clone(), daemon_log),],
            "both survive a restart, the main process first"
        );
        assert_eq!(registry.version(&window.app).as_deref(), Some("1"));
    }

    #[test]
    fn where_an_application_logs_survives_the_node_restarting() {
        let file = scratch().join("apps.json");
        let app = key("demo", None);
        {
            let mut registry = Registry::load(Some(file.clone()));
            registry.connected(&app, "2".into(), 1, Some(spec()));
            registry.health(&app, HealthState::Failing, Some("down".into()));
            registry.disconnected(&app);
        }
        let registry = Registry::load(Some(file));
        let again = registry.get(&app).unwrap();
        assert_eq!(registry.logs(&app.app), vec![(None, spec())]);
        assert_eq!(again.version, "2");
        assert_eq!(again.health, HealthState::Ok);
        assert!(!again.running());
    }

    #[test]
    fn an_unreadable_registry_file_starts_empty() {
        let dir = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("apps.json");
        std::fs::write(&file, b"not json").unwrap();
        assert!(Registry::load(Some(file)).apps.is_empty());
    }
}
