//! `cursio config list|get|set|reset`: settings by key, through the running
//! node, of this machine or of another one (`--on`).
//!
//! Through the node rather than by editing the file here, so a change is
//! checked against the setting's kind and applied at once exactly as the app's
//! is, and so another machine — a phone with no shell — is reached the same
//! way.

use cursio_core::control::{Command, Reply};
use cursio_protocol::settings::{SettingApply, SettingKind, SettingView};

use crate::cli::ConfigAction;
use crate::client::control_client;

pub(crate) async fn run(action: ConfigAction) -> anyhow::Result<()> {
    let client = control_client()
        .await
        .map_err(|e| anyhow::anyhow!("no running node to ask ({e}); start `cursio mesh` first"))?;
    match action {
        ConfigAction::List { on, json } => {
            let settings = list(&client, on).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&settings)?);
            } else {
                print!("{}", table(&settings));
            }
            Ok(())
        }
        ConfigAction::Get { key, on } => {
            let settings = list(&client, on).await?;
            let setting = settings.iter().find(|s| s.key == key).ok_or_else(|| {
                anyhow::anyhow!(
                    "no setting is called {key} there; `cursio config list` names them all"
                )
            })?;
            println!("{}", setting.value_json);
            Ok(())
        }
        ConfigAction::Set { key, value, on } => {
            let value = parse_value(&value);
            let reply = client
                .request(Command::SetSetting {
                    machine: on.clone(),
                    key: key.clone(),
                    value,
                })
                .await?;
            saved(reply, &key, on.as_deref())
        }
        ConfigAction::Reset { key, on } => {
            let reply = client
                .request(Command::ResetSetting {
                    machine: on.clone(),
                    key: key.clone(),
                })
                .await?;
            saved(reply, &key, on.as_deref())
        }
    }
}

async fn list(
    client: &cursio_core::control::transport::Client,
    on: Option<String>,
) -> anyhow::Result<Vec<SettingView>> {
    match client.request(Command::Settings { machine: on }).await? {
        Reply::Settings { settings } => Ok(settings),
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the running node answered {other:?}"),
    }
}

/// A value as typed: JSON when it reads as JSON (`true`, `-55`, `"auto"`,
/// `null`, a table), and otherwise the text itself, so `auto` needs no quotes.
fn parse_value(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_owned()))
}

fn saved(reply: Reply, key: &str, on: Option<&str>) -> anyhow::Result<()> {
    let at = on.unwrap_or("this machine");
    match reply {
        Reply::Saved {
            pinned,
            restart_needed,
        } => {
            if pinned.iter().any(|p| p == key) {
                println!("{key} saved on {at}, but a flag holds the running value until the node restarts without it");
            } else if restart_needed.iter().any(|k| k == key) {
                println!(
                    "{key} saved on {at}; it applies when the node restarts (`cursio restart{}`)",
                    on.map(|m| format!(" {m}")).unwrap_or_default()
                );
            } else {
                println!("{key} changed on {at}");
            }
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the running node answered {other:?}"),
    }
}

/// The settings grouped by section, one per line: key, value, and what is
/// worth knowing about it.
fn table(settings: &[SettingView]) -> String {
    let width = settings.iter().map(|s| s.key.len()).max().unwrap_or(0);
    let mut out = String::new();
    let mut section = "";
    for s in settings {
        if s.section != section {
            section = &s.section;
            out.push_str(&format!("\n[{section}]\n"));
        }
        let value = match s.kind {
            SettingKind::Structured if s.value_json.len() > 40 => "{…}".to_owned(),
            _ => s.value_json.clone(),
        };
        let mut notes = Vec::new();
        if s.value_json != s.default_json {
            notes.push(format!("default {}", short(&s.default_json)));
        }
        if s.waits_for_restart {
            notes.push("waits for a restart".to_owned());
        } else if s.apply == SettingApply::Restart {
            notes.push("applies at restart".to_owned());
        }
        if s.pinned {
            notes.push("held by a flag".to_owned());
        }
        if let Some(choices) = choices(&s.kind) {
            notes.push(choices);
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("  ({})", notes.join("; "))
        };
        out.push_str(&format!("{:width$}  {value}{notes}\n", s.key));
    }
    out
}

fn short(json: &str) -> String {
    if json.len() > 40 {
        "{…}".to_owned()
    } else {
        json.to_owned()
    }
}

fn choices(kind: &SettingKind) -> Option<String> {
    match kind {
        SettingKind::Choice { options } => Some(options.join("|")),
        SettingKind::Level {
            words,
            min,
            max,
            unit,
        } => Some(format!("{}|{min}..{max} {unit}", words.join("|"))),
        SettingKind::Number { min, max } => Some(format!("{min}..{max}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_value_is_json_when_it_reads_as_json_and_text_otherwise() {
        assert_eq!(parse_value("true"), serde_json::json!(true));
        assert_eq!(parse_value("-55"), serde_json::json!(-55));
        assert_eq!(parse_value("auto"), serde_json::json!("auto"));
        assert_eq!(parse_value("null"), serde_json::Value::Null);
        assert_eq!(
            parse_value("{\"left\":[0,50]}"),
            serde_json::json!({"left": [0, 50]})
        );
    }
}
