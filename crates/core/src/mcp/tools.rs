//! The tools `cursio mcp` offers, each a control command or a `cursio apps`
//! function, and what each says about itself.
//!
//! Every tool carries MCP's annotations: `readOnlyHint` for those that only
//! read, `destructiveHint` for those that stop or replace something running
//! (a restart, a switch to another build, `--restart` on a launch), so a
//! client can ask its person before calling them. None of them changes a
//! setting: what the node would ask its person about — trusting a builder,
//! letting an application run as administrator — stays a button on that
//! machine, and a tool reaching it answers with the refusal that says so.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::control::query::{self, Ask};
use crate::control::{Command, Reply};

/// What the server tells the agent about using it, once, at `initialize`.
pub const INSTRUCTIONS: &str = "Cursio links this computer with the user's other machines and \
phones into one mesh. `machines` lists them; `logs` and `report` read any of them without SSH or \
adb (`app` reads an application registered through cursio-sdk); `apps_ship` builds nothing but \
publishes files you built, waits until the target machine has them and starts them there. Tools \
marked destructive stop or replace something running: ask the user first. Trusting a builder or \
letting an app run as administrator is the target machine's person's decision, shown there as a \
button; a tool hitting it says so — tell the user, do not retry in a loop.";

/// Why a tool call produced no result.
#[derive(Debug)]
pub enum Failure {
    UnknownTool,
    /// The tool ran and could not do it; the text says why.
    Failed(String),
}

impl From<String> for Failure {
    fn from(why: String) -> Self {
        Failure::Failed(why)
    }
}

/// The tools, over a node reached through `A`.
pub struct Tools<A> {
    ask: A,
    config_path: PathBuf,
}

impl<A: Ask> Tools<A> {
    pub fn new(ask: A, config_path: PathBuf) -> Self {
        Tools { ask, config_path }
    }

    #[cfg(test)]
    pub fn ask_ref(&self) -> &A {
        &self.ask
    }

    pub async fn call(&self, name: &str, args: &Value) -> Result<String, Failure> {
        match name {
            "machines" => self.machines().await,
            "logs" => self.logs(args).await,
            "report" => self.report(args).await,
            "path" => self.path(args).await,
            "apps_status" => apps_status(&self.config_path),
            "apps_publish" => apps_publish(args),
            "apps_launch" => apps_launch(args).await,
            "apps_ship" => apps_ship(args).await,
            "restart_machine" => {
                self.maintain(args, cursio_protocol::diag::Maintenance::Restart)
                    .await
            }
            "apply_update" => {
                self.maintain(args, cursio_protocol::diag::Maintenance::ApplyUpdate)
                    .await
            }
            _ => Err(Failure::UnknownTool),
        }
    }

    async fn machines(&self) -> Result<String, Failure> {
        let Reply::Status(status) = self.ask.ask(Command::Status).await? else {
            return Err("the running node did not answer with its status"
                .to_owned()
                .into());
        };
        let mut machines = vec![json!({
            "name": status.node.name,
            "this": true,
            "online": true,
            "status": status.node.status.to_string(),
            "build": status.node.build,
        })];
        for peer in &status.peers {
            machines.push(json!({
                "name": peer.name,
                "online": format!("{:?}", peer.presence),
                "kind": peer.link.as_ref().map(|l| format!("{:?}", l.device_kind)),
                "status": peer.link.as_ref().map(|l| l.status.to_string()),
                "modules": peer.link.as_ref().map(|l| l.modules.clone()),
            }));
        }
        let problems: Vec<&str> = status.problems.iter().map(|p| p.summary.as_str()).collect();
        Ok(pretty(
            &json!({ "machines": machines, "problems_here": problems }),
        ))
    }

    async fn logs(&self, args: &Value) -> Result<String, Failure> {
        let machine = opt_str(args, "machine").unwrap_or_else(|| "this".into());
        let since = opt_str(args, "since").unwrap_or_else(|| "10m".into());
        let within = query::parse_since(&since)?;
        let lines = args
            .get("lines")
            .and_then(Value::as_u64)
            .map_or(200, |n| n.clamp(1, 2000) as u32);
        let log = match (opt_str(args, "app"), opt_str(args, "log")) {
            (Some(app), _) => Some(format!("app:{app}")),
            (None, log) => log,
        };
        let read = query::read_logs(
            &self.ask,
            &[machine],
            Some(within),
            opt_str(args, "grep"),
            lines,
            log.as_deref(),
        )
        .await?;
        Ok(match read.is_empty() {
            true => format!("no lines in the last {since}"),
            false => read.join("\n"),
        })
    }

    async fn report(&self, args: &Value) -> Result<String, Failure> {
        let since = opt_str(args, "since").unwrap_or_else(|| "1d".into());
        let machine = opt_str(args, "machine").filter(|m| m != "this");
        let reply = self
            .ask
            .ask(Command::Report {
                machine,
                since_secs: query::parse_since(&since)?,
            })
            .await?;
        match reply {
            Reply::Report(report) => Ok(pretty(&json!(report))),
            other => Err(answered(other)),
        }
    }

    async fn path(&self, args: &Value) -> Result<String, Failure> {
        let peer = need_str(args, "machine")?;
        match self.ask.ask(Command::Paths { peer, choice: None }).await? {
            reply @ Reply::Paths { .. } => Ok(pretty(&json!(reply))),
            other => Err(answered(other)),
        }
    }

    async fn maintain(
        &self,
        args: &Value,
        what: cursio_protocol::diag::Maintenance,
    ) -> Result<String, Failure> {
        let machine = need_str(args, "machine")?;
        match self
            .ask
            .ask(Command::Maintain {
                machine: machine.clone(),
                what,
            })
            .await?
        {
            Reply::Done => Ok(format!(
                "{machine} took it; its link drops while it comes back"
            )),
            other => Err(answered(other)),
        }
    }
}

fn apps_status(config_path: &std::path::Path) -> Result<String, Failure> {
    let root = crate::apps::cli::apps_root().map_err(|e| e.to_string())?;
    crate::apps::cli::status(&root, config_path).map_err(|e| Failure::Failed(e.to_string()))
}

fn apps_publish(args: &Value) -> Result<String, Failure> {
    let app = need_str(args, "app")?;
    let files = str_list(args, "files");
    if files.is_empty() {
        return Err("files: name at least one, as PLATFORM=PATH or a path"
            .to_owned()
            .into());
    }
    let root = crate::apps::cli::apps_root().map_err(|e| e.to_string())?;
    let mut said = Vec::new();
    let mut say = |line: String| said.push(line);
    crate::apps::cli::publish(
        &root,
        &app,
        opt_str(args, "version"),
        &str_list(args, "admin"),
        &files,
        &mut say,
    )
    .map_err(|e| e.to_string())?;
    Ok(said.join("\n"))
}

async fn apps_launch(args: &Value) -> Result<String, Failure> {
    let machine = need_str(args, "machine")?;
    let app = need_str(args, "app")?;
    let what = crate::apps::cli::launch(
        &machine,
        &app,
        opt_str(args, "version"),
        opt_str(args, "component"),
        args.get("restart")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        true,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(format!("{machine}: {what}"))
}

async fn apps_ship(args: &Value) -> Result<String, Failure> {
    let app = need_str(args, "app")?;
    let to = need_str(args, "to")?;
    let files = str_list(args, "files");
    if files.is_empty() {
        return Err("files: name at least one, as PLATFORM=PATH or a path"
            .to_owned()
            .into());
    }
    let root = crate::apps::cli::apps_root().map_err(|e| e.to_string())?;
    let mut said = Vec::new();
    let outcome = {
        let mut say = |line: String| said.push(line);
        match crate::apps::cli::publish(
            &root,
            &app,
            opt_str(args, "version"),
            &str_list(args, "admin"),
            &files,
            &mut say,
        ) {
            Ok(version) => {
                crate::apps::cli::ship(
                    &to,
                    &app,
                    &version,
                    opt_str(args, "component"),
                    args.get("launch").and_then(Value::as_bool).unwrap_or(true),
                    args.get("wait").and_then(Value::as_u64).unwrap_or(300),
                    &mut say,
                )
                .await
            }
            Err(e) => Err(e),
        }
    };
    match outcome {
        Ok(()) => Ok(said.join("\n")),
        Err(e) => {
            said.push(e.to_string());
            Err(Failure::Failed(said.join("\n")))
        }
    }
}

fn answered(reply: Reply) -> Failure {
    match reply {
        Reply::Error { message } => Failure::Failed(message),
        other => Failure::Failed(format!("the running node answered {other:?}")),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn need_str(args: &Value, key: &str) -> Result<String, Failure> {
    opt_str(args, key).ok_or_else(|| Failure::Failed(format!("{key} is required")))
}

fn str_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A tool's listing entry. `read_only` and `destructive` become MCP's
/// annotations; every tool reaches beyond this process (the mesh), so
/// `openWorldHint` is always set.
fn tool(
    name: &str,
    description: &str,
    read_only: bool,
    destructive: bool,
    properties: Value,
    required: &[&str],
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": read_only,
            "openWorldHint": true,
        },
    })
}

/// Every tool, for `tools/list`.
pub fn list() -> Vec<Value> {
    let machine = json!({ "type": "string", "description": "a machine's name as `machines` lists it; `this` for this one" });
    vec![
        tool(
            "machines",
            "The machines and phones on the mesh: online or not, kind, battery and network, modules, and this machine's problems.",
            true, false, json!({}), &[],
        ),
        tool(
            "logs",
            "Read a machine's log (Cursio's own, another Cursio log, or an application's registered through cursio-sdk). `machine: all` merges every machine by time.",
            true, false,
            json!({
                "machine": { "type": "string", "description": "name, `this` (default) or `all`" },
                "app": { "type": "string", "description": "an application id: read its log instead of Cursio's" },
                "log": { "type": "string", "description": "another of Cursio's logs, e.g. `cursio-app` or `mount-helper`" },
                "since": { "type": "string", "description": "how far back: 30s, 10m (default), 2h, 1d" },
                "grep": { "type": "string", "description": "only lines containing this" },
                "lines": { "type": "integer", "description": "most lines per machine (default 200, at most 2000)" },
            }),
            &[],
        ),
        tool(
            "report",
            "A machine's day counted from its log: starts, link drops, warnings, slow modules, memory, largest processes, problems.",
            true, false,
            json!({ "machine": machine.clone(), "since": { "type": "string", "description": "1d (default), 6h, …" } }),
            &[],
        ),
        tool(
            "path",
            "Which paths (network, Bluetooth) carry the session with a machine, from both ends.",
            true, false, json!({ "machine": machine.clone() }), &["machine"],
        ),
        tool(
            "apps_status",
            "Applications whose builds this machine holds: versions, which is current, whose builds it takes.",
            true, false, json!({}), &[],
        ),
        tool(
            "apps_publish",
            "Sign files you built as a build of an application and offer it to the mesh. Each machine takes it only once its person trusts this machine for that application.",
            false, false,
            json!({
                "app": { "type": "string", "description": "the application's id" },
                "files": { "type": "array", "items": { "type": "string" }, "description": "PLATFORM=PATH (windows-x86_64, linux-x86_64, android-aarch64) or a path for this machine's platform" },
                "version": { "type": "string" },
                "admin": { "type": "array", "items": { "type": "string" }, "description": "file names that ask to run as administrator" },
            }),
            &["app", "files"],
        ),
        tool(
            "apps_launch",
            "Start an application's current build on a machine. With restart, copies running from its install are ended first.",
            false, true,
            json!({
                "machine": machine.clone(),
                "app": { "type": "string" },
                "version": { "type": "string", "description": "start only once this version is current there" },
                "component": { "type": "string", "description": "which file of the build, when it has several" },
                "restart": { "type": "boolean" },
            }),
            &["machine", "app"],
        ),
        tool(
            "apps_ship",
            "Publish files as a build, wait until it is current on `to`, and start it there (replacing running copies). The whole round from a local build to it running on another machine.",
            false, true,
            json!({
                "app": { "type": "string" },
                "to": { "type": "string", "description": "a machine's name, or `all`" },
                "files": { "type": "array", "items": { "type": "string" } },
                "version": { "type": "string" },
                "admin": { "type": "array", "items": { "type": "string" } },
                "component": { "type": "string" },
                "launch": { "type": "boolean", "description": "start it (default true)" },
                "wait": { "type": "integer", "description": "seconds to wait for it to arrive (default 300)" },
            }),
            &["app", "to", "files"],
        ),
        tool(
            "restart_machine",
            "Restart Cursio on a machine; its link drops while it comes back.",
            false, true, json!({ "machine": machine.clone() }), &["machine"],
        ),
        tool(
            "apply_update",
            "Switch a machine's Cursio to the build it has downloaded and is waiting to switch to.",
            false, true, json!({ "machine": machine }), &["machine"],
        ),
    ]
}

/// The running node, reached afresh for every call: the calls this exists
/// for replace the node, and an agent session outlives many of them.
pub struct NodeAsk;

impl Ask for NodeAsk {
    async fn ask(&self, command: Command) -> Result<Reply, String> {
        let endpoint = crate::control::transport::endpoint().map_err(|e| e.to_string())?;
        let (client, _pushes) = crate::control::transport::connect(&endpoint)
            .await
            .map_err(|e| format!("no running Cursio node to ask ({e}); start it first"))?;
        client.request(command).await.map_err(|e| e.to_string())
    }
}
