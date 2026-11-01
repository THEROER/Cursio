//! Calling what a running application declared it can be asked to do, from
//! another machine (`cursio apps call`, `cursio mcp`'s `apps_call`).
//!
//! An application declares its actions in its hello (`cursio-sdk`'s
//! `App::action`), each with a description and a JSON Schema, and the node
//! lists them for a peer and passes a peer's call to the running process over
//! its apps socket. Nothing here runs a program or names a path: an action is
//! only ever something the application chose to expose, done by the process
//! that is already running — which is the point, since what is being tested
//! is usually exactly that process (a launcher's own runner, not a second
//! copy started from a shell).
//!
//! **Its own consent.** Trusting a machine's builds of an application does not
//! extend to calling it: a build is installed and started by the person, an
//! action does something the moment it is called. `[apps.<id>] actions` is
//! `ask` until this machine's person answers, raised once as a problem with
//! both answers as buttons, the way an application asking for administrator
//! rights is (`launch.rs`).

use std::time::Duration;

use cursio_app_protocol::{no_such_action, ActionName, AppId, ProcessName};
use cursio_protocol::diag::AppAction;

use super::Key;
use crate::config::{AdminConsent, ConfigEdit};
use crate::diagnostics::{Problem, ProblemAction, Severity};

/// How long a call waits for the application's answer. Under the asking
/// side's own wait for a diag answer (30 s), so a slow action is reported as
/// slow rather than as a peer that never answered.
pub const CALL_WAIT: Duration = Duration::from_secs(25);

/// Every action `app`'s registered processes declare, with whether each runs.
pub fn list(app: &str) -> Result<Vec<AppAction>, String> {
    let id = AppId::parse(app).map_err(|e| e.to_string())?;
    let registry = super::registry().lock().unwrap_or_else(|p| p.into_inner());
    let processes = registry.processes(&id);
    if processes.is_empty() {
        return Err(format!(
            "no application {id} has registered on this machine"
        ));
    }
    Ok(processes
        .into_iter()
        .flat_map(|(key, registered)| {
            let running = registered.running();
            registered.actions.iter().map(move |spec| AppAction {
                process: key.process.as_ref().map(|p| p.to_string()),
                name: spec.name.to_string(),
                description: spec.description.clone(),
                schema_json: spec.schema.to_string(),
                running,
            })
        })
        .collect())
}

/// `peer` asks `app` (or one `process` of it) to do `action` with the JSON
/// `args`; the answer's JSON, or why not.
pub fn call(
    peer: &str,
    app: &str,
    process: Option<&str>,
    action: &str,
    args_json: &str,
) -> Result<String, String> {
    let id = AppId::parse(app).map_err(|e| e.to_string())?;
    let process = process
        .map(ProcessName::parse)
        .transpose()
        .map_err(|e| e.to_string())?;
    let action_name = ActionName::parse(action).map_err(|e| e.to_string())?;
    let args: serde_json::Value = match args_json.trim() {
        "" => serde_json::Value::Null,
        text => {
            serde_json::from_str(text).map_err(|e| format!("the arguments are not JSON: {e}"))?
        }
    };
    let key = Key {
        app: id.clone(),
        process,
    };
    {
        let registry = super::registry().lock().unwrap_or_else(|p| p.into_inner());
        let Some(registered) = registry.get(&key).filter(|r| r.running()) else {
            return Err(format!("{} is not running on this machine", describe(&key)));
        };
        if !registered.actions.iter().any(|a| a.name == action_name) {
            let names: Vec<ActionName> =
                registered.actions.iter().map(|a| a.name.clone()).collect();
            return Err(no_such_action(id.as_str(), action, &names));
        }
    }
    allowed(peer, &id)?;
    tracing::info!(%peer, app = %describe(&key), %action_name, "a peer called an application's action");
    let answer = super::serve::call(&key, &action_name, args, CALL_WAIT)?;
    Ok(answer.to_string())
}

/// Whether this machine's person lets `peer` drive `id` — call its actions,
/// or read and click its windows ([`crate::desktop_window`]): asked once,
/// kept per application.
pub fn allowed(peer: &str, id: &AppId) -> Result<(), String> {
    let consent = super::builds::builds()
        .map(|b| b.app_config(id.as_str()).actions)
        .unwrap_or_default();
    match consent {
        AdminConsent::Allow => {
            crate::diagnostics::clear(&problem(id));
            Ok(())
        }
        AdminConsent::Deny => Err(format!(
            "this machine's person does not let other machines drive {id}"
        )),
        AdminConsent::Ask => Err(ask(id, peer)),
    }
}

fn problem(id: &AppId) -> String {
    format!("app.{id}.actions")
}

/// `app`, or `app/process`, as a person reads it.
fn describe(key: &Key) -> String {
    match &key.process {
        Some(process) => format!("{}/{process}", key.app),
        None => key.app.to_string(),
    }
}

/// Raise the question of `peer` calling `id`'s actions here, with both
/// answers as buttons, and say what the caller waits for.
fn ask(id: &AppId, peer: &str) -> String {
    let answer = |label: String, consent| ProblemAction {
        label,
        edit: ConfigEdit::AppActions {
            app: id.as_str().to_owned(),
            consent,
        },
    };
    crate::diagnostics::raise(Problem {
        id: problem(id),
        severity: Severity::Warning,
        summary: format!("{peer} wants to call {id}'s actions on this machine"),
        hint: Some(
            "the answer is kept for this application; `[apps.<id>] actions` in the config changes it"
                .into(),
        ),
        actions: vec![
            answer(
                format!("Allow {peer} to call {id}'s actions"),
                AdminConsent::Allow,
            ),
            answer("Deny".into(), AdminConsent::Deny),
        ],
        since: std::time::SystemTime::now(),
    });
    format!(
        "{id}'s actions wait for this machine's person to allow calling them, once, in Cursio's problems there"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calling_an_applications_actions_waits_for_an_answer_kept_for_it() {
        let id = AppId::parse("asks-actions").unwrap();
        let why = ask(&id, "Nocturne");
        assert!(why.contains("wait for this machine's person"));
        let raised = crate::diagnostics::snapshot()
            .into_iter()
            .find(|p| p.id == "app.asks-actions.actions")
            .expect("the question is raised");
        assert_eq!(
            raised.actions[0].label,
            "Allow Nocturne to call asks-actions's actions"
        );
        let mut config = crate::config::Config::default();
        raised.actions[0].edit.clone().apply(&mut config);
        assert_eq!(config.apps["asks-actions"].actions, AdminConsent::Allow);
        raised.actions[1].edit.clone().apply(&mut config);
        assert_eq!(config.apps["asks-actions"].actions, AdminConsent::Deny);
        crate::diagnostics::clear("app.asks-actions.actions");
    }

    #[test]
    fn a_call_to_an_application_that_is_not_running_says_so() {
        let error = call("Nocturne", "never-registered", None, "launch", "{}").unwrap_err();
        assert!(error.contains("not running"), "{error}");
        assert!(call("Nocturne", "x", None, "launch", "{not json").is_err());
    }
}
