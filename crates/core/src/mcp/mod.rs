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

pub mod tools;

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
pub async fn serve<R, W>(
    input: R,
    mut output: W,
    tools: &tools::Tools<impl Ask>,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = input.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(answer) = handle(tools, &line).await {
            let mut bytes = serde_json::to_vec(&answer).expect("a JSON value always serializes");
            bytes.push(b'\n');
            output.write_all(&bytes).await?;
            output.flush().await?;
        }
    }
    Ok(())
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
                Ok(text) => Ok(text_result(text, false)),
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
