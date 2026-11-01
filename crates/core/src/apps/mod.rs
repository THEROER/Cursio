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

pub mod builds;
pub mod cli;
pub mod lane;
pub mod launch;
pub mod serve;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use cursio_app_protocol::{AppId, HealthState, LogSpec};
use serde::{Deserialize, Serialize};

/// The file the registry keeps in the node's data directory.
const FILE_NAME: &str = "apps.json";

/// One application as the node knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub version: String,
    pub pid: Option<u32>,
    pub logs: Option<LogSpec>,
    pub health: HealthState,
    pub detail: Option<String>,
    /// Open connections from it: two running copies are two.
    pub connections: u32,
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

/// Every application that has registered here.
#[derive(Debug, Default)]
pub struct Registry {
    apps: BTreeMap<AppId, Registered>,
    file: Option<PathBuf>,
}

impl Registry {
    /// What `file` holds, or nothing when it is absent or unreadable; with
    /// `None`, a registry that is never saved.
    pub fn load(file: Option<PathBuf>) -> Self {
        let saved: BTreeMap<AppId, Saved> = file
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
            .map(|(id, saved)| {
                let registered = Registered {
                    version: saved.version,
                    pid: None,
                    logs: saved.logs,
                    health: HealthState::default(),
                    detail: None,
                    connections: 0,
                };
                (id, registered)
            })
            .collect();
        Registry { apps, file }
    }

    /// A copy of `app` connected and said this.
    pub fn connected(&mut self, app: &AppId, version: String, pid: u32, logs: Option<LogSpec>) {
        let entry = self.apps.entry(app.clone()).or_insert(Registered {
            version: String::new(),
            pid: None,
            logs: None,
            health: HealthState::default(),
            detail: None,
            connections: 0,
        });
        let changed = entry.version != version || entry.logs != logs;
        entry.version = version;
        entry.pid = Some(pid);
        entry.logs = logs;
        entry.connections += 1;
        if changed {
            self.save();
        }
    }

    pub fn health(&mut self, app: &AppId, state: HealthState, detail: Option<String>) {
        if let Some(entry) = self.apps.get_mut(app) {
            entry.health = state;
            entry.detail = detail;
        }
    }

    /// A copy of `app` closed its connection.
    pub fn disconnected(&mut self, app: &AppId) {
        if let Some(entry) = self.apps.get_mut(app) {
            entry.connections = entry.connections.saturating_sub(1);
            if entry.connections == 0 {
                entry.pid = None;
            }
        }
    }

    /// Where `app` said it logs.
    pub fn logs(&self, app: &AppId) -> Option<LogSpec> {
        self.apps.get(app)?.logs.clone()
    }

    pub fn get(&self, app: &AppId) -> Option<&Registered> {
        self.apps.get(app)
    }

    fn save(&self) {
        let Some(path) = &self.file else {
            return;
        };
        let saved: BTreeMap<&AppId, Saved> = self
            .apps
            .iter()
            .map(|(id, app)| {
                let saved = Saved {
                    version: app.version.clone(),
                    logs: app.logs.clone(),
                };
                (id, saved)
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
        let file = cursio_protocol::paths::data_dir().map(|dir| dir.join(FILE_NAME));
        Arc::new(Mutex::new(Registry::load(file)))
    })
}

/// Where `app` said it logs, on this node.
pub fn logs_of(app: &AppId) -> Option<LogSpec> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .logs(app)
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

    #[test]
    fn an_application_is_running_while_any_copy_is_connected() {
        let mut registry = Registry::load(None);
        let app = AppId::parse("demo").unwrap();
        registry.connected(&app, "1".into(), 10, None);
        registry.connected(&app, "1".into(), 11, None);
        registry.disconnected(&app);
        assert!(registry.get(&app).unwrap().running());
        registry.disconnected(&app);
        assert!(!registry.get(&app).unwrap().running());
        assert_eq!(registry.get(&app).unwrap().pid, None);
    }

    #[test]
    fn where_an_application_logs_survives_the_node_restarting() {
        let file = scratch().join("apps.json");
        let app = AppId::parse("demo").unwrap();
        {
            let mut registry = Registry::load(Some(file.clone()));
            registry.connected(&app, "2".into(), 1, Some(spec()));
            registry.health(&app, HealthState::Failing, Some("down".into()));
            registry.disconnected(&app);
        }
        let registry = Registry::load(Some(file));
        let again = registry.get(&app).unwrap();
        assert_eq!(registry.logs(&app), Some(spec()));
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
