//! Settings by key, for every reader at once: the control socket, the CLI and
//! the MCP tools through it, and another machine of the mesh through the diag
//! module.
//!
//! Each change goes the way a saved file always went — written, then applied
//! to the running node by the same applier — so a setting changed from the
//! app, from the shell or from across the mesh lands exactly as an edit of
//! `config.toml` would, and the file stays the one record of what is set.

use cursio_protocol::diag::{DiagRequest, DiagResponse};
use cursio_protocol::settings::{SettingSaved, SettingView};

use crate::config::Config;
use crate::control::NodeState;
use crate::settings::{self, DeviceRole, Here};

impl NodeState {
    /// Every setting that exists on this node, with what it runs.
    pub fn settings(&self) -> Vec<SettingView> {
        let running = self.running_config().clone();
        let restart_needed = crate::live::restart_needed(&self.started, &running);
        let modules = self.inner().modules.clone();
        settings::view(&Here {
            running: &running,
            overrides: &self.overrides,
            restart_needed: &restart_needed,
            has_module: &|id| modules.contains(&id),
            devices: &devices,
        })
    }

    /// Set `key` to `value` (`None` unsets an optional one) in the file and
    /// on the running node.
    pub fn set_setting(
        &self,
        key: &str,
        value: Option<toml::Value>,
    ) -> Result<SettingSaved, String> {
        self.change_setting(key, |file| settings::set(file, key, value))
    }

    /// Take `key` out of the file, back to its default.
    pub fn reset_setting(&self, key: &str) -> Result<SettingSaved, String> {
        self.change_setting(key, |file| settings::reset(file, key))
    }

    /// Answer a peer's settings request, as `peer` asked it.
    pub(crate) fn answer_settings(
        &self,
        peer: &str,
        request: &DiagRequest,
    ) -> Result<DiagResponse, String> {
        match request {
            DiagRequest::Settings => Ok(DiagResponse::Settings(self.settings())),
            DiagRequest::SetSetting {
                key,
                value_json,
                reset,
            } => {
                let saved = if *reset {
                    self.reset_setting(key)
                } else {
                    let value = match value_json {
                        Some(json) => serde_json::from_str(json)
                            .map_err(|e| format!("{key}: not JSON: {e}"))
                            .and_then(|json| {
                                settings::from_json(json).map_err(|e| e.to_string())
                            })?,
                        None => None,
                    };
                    self.set_setting(key, value)
                };
                match &saved {
                    Ok(_) => {
                        tracing::info!(%peer, %key, reset, value = ?value_json, "a peer changed a setting here")
                    }
                    Err(error) => {
                        tracing::warn!(%peer, %key, %error, "a peer's change of a setting here was refused")
                    }
                }
                saved.map(DiagResponse::SettingSaved)
            }
            other => Err(format!("{other:?} is not a settings request")),
        }
    }

    fn change_setting(
        &self,
        key: &str,
        change: impl FnOnce(&Config) -> Result<Config, crate::config::ConfigError>,
    ) -> Result<SettingSaved, String> {
        let file = Config::load(&self.config_path).map_err(|e| e.to_string())?;
        let next = change(&file).map_err(|e| e.to_string())?;
        next.save(&self.config_path).map_err(|e| e.to_string())?;
        let restart_needed = self.apply(next.with_overrides(&self.overrides));
        Ok(SettingSaved {
            pinned: self.overrides.pinned().contains(&key),
            restart_needed,
        })
    }
}

/// The sound devices a setting of `role` may name, as the calls hub sees
/// them now; none where it does not run.
fn devices(role: DeviceRole) -> Vec<String> {
    let Some(setup) = crate::calls_hub::hub().setup() else {
        return Vec::new();
    };
    let list = match role {
        DeviceRole::Output => setup.outputs,
        DeviceRole::Input => setup.inputs,
    };
    list.into_iter().map(|d| d.id).collect()
}

#[cfg(test)]
mod tests {
    use crate::config::Overrides;
    use crate::control::test_support::{state, temp_dir};
    use crate::control::NodeState;

    fn node_with_config(tag: &str) -> (NodeState, std::path::PathBuf) {
        let dir = temp_dir(tag);
        (state(&dir, Overrides::default()), dir.join("config.toml"))
    }

    #[test]
    fn a_setting_changed_by_key_is_written_and_running_at_once() {
        let (node, path) = node_with_config("setting-by-key");
        let saved = node
            .set_setting("modules.clipboard", Some(toml::Value::Boolean(false)))
            .unwrap();
        assert!(!saved.pinned);
        assert!(
            !crate::config::Config::load(&path)
                .unwrap()
                .modules
                .clipboard
        );
        assert!(!node.running_config().modules.clipboard);
        let listed = node.settings();
        let clipboard = listed.iter().find(|s| s.key == "modules.clipboard");
        assert!(
            clipboard.is_none() || clipboard.unwrap().value_json == "false",
            "listed as it runs"
        );
    }

    #[test]
    fn a_wrong_value_changes_nothing_and_says_why() {
        let (node, path) = node_with_config("setting-wrong");
        let error = node
            .set_setting("bluetooth.mesh", Some(toml::Value::String("maybe".into())))
            .unwrap_err();
        assert!(error.contains("auto, on, off"), "{error}");
        assert_eq!(
            crate::config::Config::load(&path).unwrap(),
            crate::config::Config::default()
        );
    }

    #[test]
    fn a_setting_that_waits_for_a_restart_says_so() {
        let (node, _path) = node_with_config("setting-restart");
        let saved = node
            .set_setting("port", Some(toml::Value::Integer(40000)))
            .unwrap();
        assert_eq!(saved.restart_needed, vec!["port".to_owned()]);
        let reset = node.reset_setting("port").unwrap();
        assert!(reset.restart_needed.is_empty());
    }
}
