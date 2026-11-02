//! `cursio mcp`: the mesh as tools for an AI agent, over the Model Context
//! Protocol (`docs/agent-tools-design.md`).
//!
//! A Claude Code (or any MCP client) session starts `cursio mcp` as a child
//! and speaks JSON-RPC to it, one message per line on stdin and stdout. Every
//! tool is a control command the running node already answers — the same the
//! CLI sends — so an agent reads a laptop's log or starts a build on it
//! exactly as the person at this desk would, and through the same consent:
//! nothing here can do what `cursio …` cannot.
//!
//! The server does not hold a connection to the node. It asks the node's
//! control socket afresh for every tool call, because the calls it exists for
//! — `apply_update`, a restart — replace the very node it would be connected
//! to, and an agent session outlives many of those.
//!
//! stdout is the protocol and nothing else writes to it: the log goes to
//! stderr and the log file, and the apps functions this calls report progress
//! through a sink rather than printing.
//!
//! ## Following the installed version
//!
//! The client starts this once per session, through the stable `cursio`,
//! which hands over to the version current then — and the session then runs
//! for days while the machine updates under it. Talking to a newer node and
//! reading a config a newer build wrote, the old server failed on things its
//! own version never knew. So before each message it looks at what is current
//! ([`successor`]) and, when that is another build, starts that build's
//! `cursio mcp` and passes the line through to it. A pass-through rather than
//! replacing the process: Windows has no exec, and the client must keep the
//! one child it started. Nothing is lost by switching mid-session, because
//! the server holds no state between messages.

#[cfg(feature = "web-tools")]
pub mod cdp;
pub mod tools;

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::control::query::Ask;

/// The MCP revision this server was written against; a client asking for
/// another is answered with this one, as the specification allows.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Longest request line accepted. A tool's arguments are a few names and
/// paths; a megabyte means something else is on the other end.
const MAX_LINE: usize = 1024 * 1024;

/// Serve MCP on `input`/`output` until the client closes `input`.
pub async fn serve<R, W>(input: R, output: W, tools: &tools::Tools<impl Ask>) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    serve_following(input, output, tools, || None).await
}

/// [`serve`], passing each message to the `cursio mcp` of the executable
/// `successor` names whenever it names one, and answering itself otherwise.
pub async fn serve_following<R, W>(
    input: R,
    mut output: W,
    tools: &tools::Tools<impl Ask>,
    successor: impl Fn() -> Option<PathBuf>,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = input.lines();
    let mut ahead: Option<Successor> = None;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match successor() {
            Some(exe) if ahead.as_ref().is_none_or(|s| s.exe != exe) => {
                ahead = match Successor::start(&exe) {
                    Ok(next) => {
                        tracing::info!(exe = %exe.display(), "a newer build is current; passing tool calls to its server");
                        Some(next)
                    }
                    Err(e) => {
                        tracing::warn!(exe = %exe.display(), error = %e, "could not start the current build's server; answering here");
                        None
                    }
                };
            }
            Some(_) => {}
            None => ahead = None,
        }
        let answer = match ahead.as_mut() {
            Some(next) => match next.pass(&line).await {
                Ok(answer) => answer,
                Err(e) => {
                    tracing::warn!(error = %e, "the current build's server went away; answering here");
                    ahead = None;
                    handle(tools, &line).await.map(|a| a.to_string())
                }
            },
            None => handle(tools, &line).await.map(|a| a.to_string()),
        };
        if let Some(answer) = answer {
            output.write_all(answer.as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
    Ok(())
}

/// The installed build's `cursio` when it is not the one running: what a
/// long-lived server should pass its messages to. `None` for a development
/// build, or when this is current.
pub fn successor() -> Option<PathBuf> {
    let current = crate::update::layout::restart_target("cursio")?;
    let own = std::env::current_exe().ok()?;
    match (current.canonicalize(), own.canonicalize()) {
        (Ok(current), Ok(own)) if current == own => None,
        _ => Some(current),
    }
}

/// A newer build's server, started with this one's arguments.
struct Successor {
    exe: PathBuf,
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl Successor {
    fn start(exe: &Path) -> std::io::Result<Self> {
        let mut child = tokio::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let missing = || std::io::Error::other("the server's pipes were not opened");
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        Ok(Self {
            exe: exe.to_owned(),
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout).lines(),
        })
    }

    /// Hand `line` on, and bring back its answer when it gets one.
    async fn pass(&mut self, line: &str) -> std::io::Result<Option<String>> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        if !answered(line) {
            return Ok(None);
        }
        loop {
            match self.stdout.next_line().await? {
                Some(answer) if answer.trim().is_empty() => continue,
                Some(answer) => return Ok(Some(answer)),
                None => {
                    let status = self.child.try_wait().ok().flatten();
                    return Err(std::io::Error::other(format!(
                        "it closed its output ({status:?})"
                    )));
                }
            }
        }
    }
}

/// Whether [`handle`] answers `line`, so a passed-on line is waited for
/// exactly when the server it went to will reply.
fn answered(line: &str) -> bool {
    if line.len() > MAX_LINE {
        return true;
    }
    match serde_json::from_str::<Value>(line) {
        Ok(message) => message.get("id").is_some(),
        Err(_) => true,
    }
}

/// The answer to one line, or `None` for a notification.
pub async fn handle(tools: &tools::Tools<impl Ask>, line: &str) -> Option<Value> {
    if line.len() > MAX_LINE {
        return Some(error(Value::Null, -32700, "message too long"));
    }
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(e) => return Some(error(Value::Null, -32700, &format!("not JSON: {e}"))),
    };
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    // A message without an id is a notification and is never answered.
    let id = id?;
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "cursio", "version": env!("CARGO_PKG_VERSION") },
            "instructions": tools::INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::list() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            // A tool that fails answers as a tool result with `isError`, so
            // the agent reads why; only an unknown tool is a protocol error.
            match tools.call(name, &arguments).await {
                Ok(tools::Output::Text(text)) => Ok(text_result(text, false)),
                Ok(tools::Output::Image { png, note }) => Ok(image_result(&png, note)),
                Err(tools::Failure::UnknownTool) => {
                    Err((-32602, format!("there is no tool named {name:?}")))
                }
                Err(tools::Failure::Failed(why)) => Ok(text_result(why, true)),
            }
        }
        other => Err((-32601, format!("method {other:?} is not served here"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error(id, code, &message),
    })
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn image_result(png: &[u8], note: String) -> Value {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    json!({
        "content": [
            { "type": "image", "data": data, "mimeType": "image/png" },
            { "type": "text", "text": note },
        ],
        "isError": false,
    })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Command, Reply};

    /// A node that answers every command with one reply, and records what
    /// it was asked.
    struct Fake {
        reply: Reply,
        asked: std::sync::Mutex<Vec<Command>>,
    }

    impl Ask for Fake {
        async fn ask(&self, command: Command) -> Result<Reply, String> {
            self.asked.lock().unwrap().push(command);
            Ok(self.reply.clone())
        }
    }

    fn tools(reply: Reply) -> tools::Tools<Fake> {
        tools::Tools::new(
            Fake {
                reply,
                asked: Default::default(),
            },
            std::env::temp_dir().join("cursio-mcp-test-config.toml"),
        )
    }

    #[tokio::test]
    async fn initialize_names_the_server_and_offers_tools() {
        let t = tools(Reply::Done);
        let answer = handle(&t, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#)
            .await
            .unwrap();
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["result"]["serverInfo"]["name"], "cursio");
        assert!(answer["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn a_notification_is_not_answered() {
        let t = tools(Reply::Done);
        assert!(handle(
            &t,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn every_listed_tool_says_whether_it_changes_anything() {
        let t = tools(Reply::Done);
        let answer = handle(&t, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .unwrap();
        let listed = answer["result"]["tools"].as_array().unwrap();
        assert!(listed.iter().any(|t| t["name"] == "logs"));
        for tool in listed {
            assert!(
                tool["annotations"]["readOnlyHint"].is_boolean(),
                "{} says nothing about changing things",
                tool["name"]
            );
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[tokio::test]
    async fn an_unknown_method_and_garbage_are_protocol_errors() {
        let t = tools(Reply::Done);
        let unknown = handle(&t, r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#)
            .await
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        let garbage = handle(&t, "{not json").await.unwrap();
        assert_eq!(garbage["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn a_log_read_asks_the_node_and_answers_its_lines() {
        let t = tools(Reply::LogLines {
            lines: vec!["2026-11-01T10:00:00Z  INFO hello".into()],
            truncated: false,
        });
        let answer = handle(
            &t,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"logs","arguments":{"machine":"Lightmoon","app":"leavepulse","since":"5m","grep":"hello","lines":10}}}"#,
        )
        .await
        .unwrap();
        assert_eq!(answer["result"]["isError"], false, "{answer}");
        assert!(answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hello"));
        let asked = t.ask_ref().asked.lock().unwrap().clone();
        assert!(asked.iter().any(|c| matches!(
            c,
            Command::Logs { machine: Some(m), log: Some(l), within_secs: Some(300), max_lines: 10, .. }
                if m == "Lightmoon" && l == "app:leavepulse"
        )));
    }

    #[tokio::test]
    async fn a_tool_the_node_refuses_reads_as_a_failed_result_with_why() {
        let t = tools(Reply::Error {
            message: "no session to Lightmoon".into(),
        });
        let answer = handle(
            &t,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"path","arguments":{"machine":"Lightmoon"}}}"#,
        )
        .await
        .unwrap();
        assert_eq!(answer["result"]["isError"], true);
        assert!(answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no session"));
    }

    #[tokio::test]
    async fn a_window_tree_comes_back_as_text_with_how_to_act_on_it() {
        let t = tools(Reply::WindowTree {
            title: "LeavePulse".into(),
            text: "- window \"LeavePulse\" #1\n  - button \"Play\" @0.80,0.12 #1-2\n".into(),
            nodes: 2,
            truncated: false,
        });
        let answer = handle(
            &t,
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"window_tree","arguments":{"machine":"Lightmoon","app":"leavepulse","depth":5}}}"#,
        )
        .await
        .unwrap();
        let text = answer["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("button \"Play\""), "{text}");
        assert!(text.contains("window_invoke"), "it says how to act: {text}");
        let asked = t.ask_ref().asked.lock().unwrap().clone();
        assert!(asked.iter().any(|c| matches!(
            c,
            Command::WindowTree { machine, depth: Some(5), max_nodes: None, .. } if machine == "Lightmoon"
        )));
    }

    #[tokio::test]
    async fn an_element_is_invoked_by_role_and_name_or_by_id() {
        use cursio_protocol::diag::{ElementAct, ElementPick};
        let t = tools(Reply::Acted {
            what: "invoked button \"Play\"".into(),
        });
        for call in [
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"window_invoke","arguments":{"machine":"Lightmoon","app":"leavepulse","role":"button","name":"Play"}}}"#,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"window_invoke","arguments":{"machine":"Lightmoon","app":"leavepulse","id":"1-2","action":"focus"}}}"#,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"window_set_value","arguments":{"machine":"Lightmoon","app":"leavepulse","role":"edit","name":"Search","value":"Привет"}}}"#,
        ] {
            let answer = handle(&t, call).await.unwrap();
            assert_eq!(answer["result"]["isError"], false, "{answer}");
        }
        let asked = t.ask_ref().asked.lock().unwrap().clone();
        let elements: Vec<_> = asked
            .iter()
            .filter_map(|c| match c {
                Command::WindowElement { pick, act, .. } => Some((pick.clone(), act.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            elements,
            [
                (
                    ElementPick::Match {
                        role: "button".into(),
                        name: "Play".into(),
                        nth: 0
                    },
                    ElementAct::Invoke
                ),
                (ElementPick::Id("1-2".into()), ElementAct::Focus),
                (
                    ElementPick::Match {
                        role: "edit".into(),
                        name: "Search".into(),
                        nth: 0
                    },
                    ElementAct::SetValue("Привет".into())
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_missing_argument_is_named() {
        let t = tools(Reply::Done);
        let answer = handle(
            &t,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"apps_launch","arguments":{"app":"leavepulse"}}}"#,
        )
        .await
        .unwrap();
        assert_eq!(answer["result"]["isError"], true);
        assert!(answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("machine"));
    }

    /// An executable standing in for another build's server.
    #[cfg(unix)]
    fn fake_server(body: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cursio-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("cursio");
        std::fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, exe)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_newer_build_answers_once_it_is_current() {
        let (dir, exe) = fake_server(
            "while read line; do case \"$line\" in *'\"id\"'*) \
             echo '{\"jsonrpc\":\"2.0\",\"id\":8,\"result\":\"newer\"}';; esac; done",
        );
        let t = tools(Reply::Done);
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"ping\"}\n";
        let mut output = Vec::new();
        serve_following(
            tokio::io::BufReader::new(&input[..]),
            &mut output,
            &t,
            || Some(exe.clone()),
        )
        .await
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [r#"{"jsonrpc":"2.0","id":8,"result":"newer"}"#]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_newer_build_that_dies_leaves_the_answer_to_this_one() {
        let (dir, exe) = fake_server("exit 0");
        let t = tools(Reply::Done);
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n";
        let mut output = Vec::new();
        serve_following(
            tokio::io::BufReader::new(&input[..]),
            &mut output,
            &t,
            || Some(exe.clone()),
        )
        .await
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains("\"id\":9") && text.contains("\"result\":{}"),
            "{text}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_requests_and_unreadable_lines_are_waited_for() {
        assert!(answered(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#));
        assert!(!answered(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        ));
        assert!(answered("not json"));
    }

    #[tokio::test]
    async fn lines_in_are_answered_lines_out() {
        let t = tools(Reply::Done);
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let mut output = Vec::new();
        serve(tokio::io::BufReader::new(&input[..]), &mut output, &t)
            .await
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(
            text.lines().count(),
            1,
            "one answer for one request: {text}"
        );
        assert!(text.contains("\"id\":7"));
    }
}
