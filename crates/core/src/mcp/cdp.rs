//! A small Chrome DevTools Protocol client for `cursio mcp`'s `web_*` tools.
//!
//! It talks to an application's web view on another machine through the
//! DevTools tunnel's loopback listener (`apps::tunnel`), token included, so an
//! agent reads and drives the page without installing Playwright. It does
//! only what the tools need: find the page target, send a command and wait
//! for its answer, keep the events that arrive meanwhile.

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as Ws;

/// How long one command waits for its answer.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Events kept while waiting, newest last; older ones are dropped.
const KEEP_EVENTS: usize = 500;

pub struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next: u64,
    pub events: Vec<Value>,
}

/// GET `path` from the listener at `127.0.0.1:port`, answering the body.
async fn get(port: u16, path: &str) -> Result<String, String> {
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot reach the DevTools listener: {e}"))?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut answer = Vec::new();
    socket
        .read_to_end(&mut answer)
        .await
        .map_err(|e| e.to_string())?;
    let answer = String::from_utf8_lossy(&answer);
    let (head, body) = answer
        .split_once("\r\n\r\n")
        .ok_or("the DevTools listener answered no HTTP")?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        let status = head.lines().next().unwrap_or_default();
        return Err(format!("{status}: {}", body.trim()));
    }
    Ok(body.to_owned())
}

/// The first page among `targets` (`/json/list`), by its WebSocket URL.
fn page_url(targets: &Value) -> Option<String> {
    targets
        .as_array()?
        .iter()
        .find(|t| t["type"] == "page")
        .and_then(|t| t["webSocketDebuggerUrl"].as_str())
        .map(str::to_owned)
}

impl Cdp {
    /// Connect to the application's page through the listener at `port`
    /// whose path starts with `token`.
    pub async fn page(port: u16, token: &str) -> Result<Self, String> {
        let list = get(port, &format!("/{token}/json/list")).await?;
        let targets: Value =
            serde_json::from_str(&list).map_err(|e| format!("not a DevTools target list: {e}"))?;
        let url = page_url(&targets).ok_or("the application shows no page to inspect")?;
        let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|e| format!("cannot open the page's DevTools: {e}"))?;
        Ok(Cdp {
            ws,
            next: 1,
            events: Vec::new(),
        })
    }

    /// Send `method` with `params` and wait for its result.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        let command = json!({ "id": id, "method": method, "params": params });
        self.ws
            .send(Ws::text(command.to_string()))
            .await
            .map_err(|e| format!("the DevTools connection closed: {e}"))?;
        let deadline = tokio::time::Instant::now() + ANSWER_WAIT;
        loop {
            let message = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .map_err(|_| format!("{method} got no answer within {} s", ANSWER_WAIT.as_secs()))?
                .ok_or("the DevTools connection closed")?
                .map_err(|e| e.to_string())?;
            let Ws::Text(text) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if value["id"] == id {
                if let Some(error) = value.get("error") {
                    return Err(format!(
                        "{method}: {}",
                        error["message"].as_str().unwrap_or("failed")
                    ));
                }
                return Ok(value["result"].clone());
            }
            if value.get("method").is_some() {
                self.keep(value);
            }
        }
    }

    /// Collect events for `wait`.
    pub async fn listen(&mut self, wait: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(Ok(Ws::Text(text)))) =
            tokio::time::timeout_at(deadline, self.ws.next()).await
        {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if value.get("method").is_some() {
                    self.keep(value);
                }
            }
        }
    }

    fn keep(&mut self, event: Value) {
        if self.events.len() == KEEP_EVENTS {
            self.events.remove(0);
        }
        self.events.push(event);
    }

    /// Evaluate `expression` in the page, answering its value as JSON.
    pub async fn eval(&mut self, expression: &str) -> Result<Value, String> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details["exception"]["description"]
                .as_str()
                .or(details["text"].as_str())
                .unwrap_or("it threw");
            return Err(text.to_owned());
        }
        Ok(result["result"]["value"].clone())
    }
}

/// The page's accessibility tree as one line per node, like `window_tree`:
/// ignored nodes are skipped (their children kept), at most `max` lines.
pub fn ax_outline(nodes: &[Value], max: usize) -> (String, bool) {
    use std::collections::HashMap;
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| Some((n["nodeId"].as_str()?, n)))
        .collect();
    let root = nodes.iter().find(|n| n.get("parentId").is_none());
    let mut out = String::new();
    let mut lines = 0;
    let mut truncated = false;
    let mut stack: Vec<(&Value, usize)> = root.map(|r| vec![(r, 0)]).unwrap_or_default();
    while let Some((node, depth)) = stack.pop() {
        let ignored = node["ignored"].as_bool().unwrap_or(false);
        let role = node["role"]["value"].as_str().unwrap_or("");
        let name = node["name"]["value"].as_str().unwrap_or("");
        let shown = !ignored && !matches!(role, "" | "none" | "generic" | "InlineTextBox");
        let child_depth = if shown {
            if lines == max {
                truncated = true;
                break;
            }
            lines += 1;
            out.push_str(&"  ".repeat(depth));
            out.push_str("- ");
            out.push_str(role);
            if !name.is_empty() {
                out.push_str(&format!(" {name:?}"));
            }
            out.push('\n');
            depth + 1
        } else {
            depth
        };
        if let Some(children) = node["childIds"].as_array() {
            for child in children.iter().rev() {
                if let Some(child) = child.as_str().and_then(|id| by_id.get(id)) {
                    stack.push((child, child_depth));
                }
            }
        }
    }
    (out, truncated)
}

/// Console calls, exceptions and log entries among `events`, one per line.
pub fn console_lines(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| {
            let params = &e["params"];
            match e["method"].as_str()? {
                "Runtime.consoleAPICalled" => {
                    let args: Vec<String> = params["args"]
                        .as_array()?
                        .iter()
                        .map(|a| {
                            a.get("value")
                                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                                .or_else(|| a["description"].as_str().map(str::to_owned))
                                .unwrap_or_default()
                        })
                        .collect();
                    Some(format!(
                        "console.{}: {}",
                        params["type"].as_str()?,
                        args.join(" ")
                    ))
                }
                "Runtime.exceptionThrown" => Some(format!(
                    "exception: {}",
                    params["exceptionDetails"]["exception"]["description"]
                        .as_str()
                        .or(params["exceptionDetails"]["text"].as_str())?
                )),
                "Log.entryAdded" => Some(format!(
                    "{} {}: {}",
                    params["entry"]["source"].as_str()?,
                    params["entry"]["level"].as_str()?,
                    params["entry"]["text"].as_str()?
                )),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_page_target_is_inspected() {
        let targets = json!([
            { "type": "service_worker", "webSocketDebuggerUrl": "ws://x/sw" },
            { "type": "page", "webSocketDebuggerUrl": "ws://127.0.0.1:1/t/devtools/page/A" }
        ]);
        assert_eq!(
            page_url(&targets).as_deref(),
            Some("ws://127.0.0.1:1/t/devtools/page/A")
        );
    }

    #[test]
    fn the_accessibility_outline_skips_ignored_nodes_but_keeps_their_children() {
        let nodes = vec![
            json!({ "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "Launcher" }, "childIds": ["2"] }),
            json!({ "nodeId": "2", "parentId": "1", "ignored": true, "childIds": ["3", "4"] }),
            json!({ "nodeId": "3", "parentId": "2", "role": { "value": "button" }, "name": { "value": "Play Create+" } }),
            json!({ "nodeId": "4", "parentId": "2", "role": { "value": "link" }, "name": { "value": "Builds" } }),
        ];
        let (text, truncated) = ax_outline(&nodes, 100);
        assert_eq!(
            text,
            "- RootWebArea \"Launcher\"\n  - button \"Play Create+\"\n  - link \"Builds\"\n"
        );
        assert!(!truncated);
        let (text, truncated) = ax_outline(&nodes, 2);
        assert_eq!(text.lines().count(), 2);
        assert!(truncated);
    }

    #[test]
    fn console_calls_and_exceptions_read_as_lines() {
        let events = vec![
            json!({ "method": "Runtime.consoleAPICalled", "params": { "type": "error", "args": [{ "type": "string", "value": "launch failed" }, { "type": "number", "value": 3 }] } }),
            json!({ "method": "Runtime.exceptionThrown", "params": { "exceptionDetails": { "text": "Uncaught", "exception": { "description": "TypeError: x is undefined" } } } }),
            json!({ "method": "Page.loadEventFired", "params": {} }),
        ];
        assert_eq!(
            console_lines(&events),
            [
                "console.error: launch failed 3",
                "exception: TypeError: x is undefined"
            ]
        );
    }
}
