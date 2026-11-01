//! A small Chrome DevTools Protocol client for `cursio mcp`'s `web_*` tools.
//!
//! It talks to an application's web view on another machine through the
//! DevTools tunnel's loopback listener (`apps::tunnel`), token included, so an
//! agent reads and drives the page without installing Playwright. It does
//! only what the tools need: find the page target, send a command and wait
//! for its answer, keep the events that arrive meanwhile.
//!
//! The owning machine uses it too, without the tunnel: while a DevTools
//! session of an application is allowed, its node reads the page's console
//! straight from the loopback port and keeps it as the application's
//! `console` log (`apps::devtools_console`).

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as Ws;

/// How long one command waits for its answer.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Events kept while waiting, newest last; older ones are dropped.
pub const KEEP_EVENTS: usize = 500;

pub struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next: u64,
    pub events: Vec<Value>,
    /// How many events [`Cdp::events`] holds at most.
    pub keep: usize,
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
        Self::connect(port, &format!("/{token}/json/list")).await
    }

    /// Connect to the page of the DevTools listening on this machine's
    /// `port` itself: the owning side, which needs no tunnel and no token.
    pub async fn local(port: u16) -> Result<Self, String> {
        Self::connect(port, "/json/list").await
    }

    async fn connect(port: u16, list: &str) -> Result<Self, String> {
        let list = get(port, list).await?;
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
            keep: KEEP_EVENTS,
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

    /// Collect events for `wait`; false when the page's connection ended
    /// meanwhile — the application closed or the page went away.
    pub async fn listen(&mut self, wait: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.ws.next()).await {
                Err(_) => return true,
                Ok(None | Some(Err(_)) | Some(Ok(Ws::Close(_)))) => return false,
                Ok(Some(Ok(Ws::Text(text)))) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if value.get("method").is_some() {
                            self.keep(value);
                        }
                    }
                }
                Ok(Some(Ok(_))) => {}
            }
        }
    }

    fn keep(&mut self, event: Value) {
        keep_newest(&mut self.events, self.keep, event);
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

/// Push `item`, dropping the oldest so at most `max` stay.
fn keep_newest<T>(ring: &mut Vec<T>, max: usize, item: T) {
    if max == 0 {
        return;
    }
    if ring.len() >= max {
        ring.drain(..=ring.len() - max);
    }
    ring.push(item);
}

/// One line of a page's console: how severe, what it said, and when.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleEntry {
    /// As Cursio's own log names it: `ERROR`, `WARN`, `INFO` or `DEBUG`.
    pub level: &'static str,
    pub text: String,
    /// Milliseconds since the Unix epoch, as the page stamped it.
    pub at_ms: Option<f64>,
}

/// Console calls, exceptions and log entries among `events`, one per line.
pub fn console_lines(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(console_entry)
        .map(|e| e.text)
        .collect()
}

/// `event` as a console line, when it is one.
pub fn console_entry(event: &Value) -> Option<ConsoleEntry> {
    let params = &event["params"];
    let (level, at_ms, text) = match event["method"].as_str()? {
        "Runtime.consoleAPICalled" => {
            let level = match params["type"].as_str()? {
                "error" | "assert" => "ERROR",
                "warning" => "WARN",
                "debug" | "trace" => "DEBUG",
                _ => "INFO",
            };
            (level, params["timestamp"].as_f64(), console_call(params)?)
        }
        "Runtime.exceptionThrown" => (
            "ERROR",
            params["timestamp"].as_f64(),
            format!(
                "exception: {}",
                params["exceptionDetails"]["exception"]["description"]
                    .as_str()
                    .or(params["exceptionDetails"]["text"].as_str())?
            ),
        ),
        "Log.entryAdded" => {
            let entry = &params["entry"];
            let level = entry["level"].as_str()?;
            (
                match level {
                    "error" => "ERROR",
                    "warning" => "WARN",
                    "verbose" => "DEBUG",
                    _ => "INFO",
                },
                entry["timestamp"].as_f64(),
                format!(
                    "{} {level}: {}",
                    entry["source"].as_str()?,
                    entry["text"].as_str()?
                ),
            )
        }
        _ => return None,
    };
    Some(ConsoleEntry { level, text, at_ms })
}

fn console_call(params: &Value) -> Option<String> {
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

/// One request the page made, as `web_network` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub url: String,
    /// `Document`, `XHR`, `Fetch`, `Script`… as the browser classed it.
    pub kind: String,
    pub status: Option<u64>,
    pub mime: Option<String>,
    /// Bytes as they crossed the wire, once it finished.
    pub size: Option<u64>,
    pub ms: Option<f64>,
    pub failed: Option<String>,
    started: Option<f64>,
}

impl Request {
    pub fn finished(&self) -> bool {
        self.size.is_some() || self.failed.is_some()
    }

    /// One line: method, status, kind, time, size, URL.
    pub fn line(&self) -> String {
        let status = match (&self.failed, self.status) {
            (Some(why), _) => format!("failed ({why})"),
            (None, Some(status)) => status.to_string(),
            (None, None) => "pending".to_owned(),
        };
        let ms = self.ms.map(|ms| format!(" {ms:.0} ms")).unwrap_or_default();
        let size = self.size.map(|b| format!(" {b} B")).unwrap_or_default();
        format!("{} {status} {}{ms}{size} {}", self.method, self.kind, self.url)
    }
}

/// The requests among `events` (`Network.*`), in the order they started,
/// the newest `max` of them; and how many older ones were left out. A
/// redirect is its own row, answered by the status that redirected it.
pub fn network_requests(events: &[Value], max: usize) -> (Vec<Request>, usize) {
    let mut rows: Vec<Request> = Vec::new();
    let mut dropped = 0;
    fn open<'a>(rows: &'a mut [Request], id: &str) -> Option<&'a mut Request> {
        rows.iter_mut().rev().find(|r| r.id == id && !r.finished())
    }
    for event in events {
        let params = &event["params"];
        let Some(id) = params["requestId"].as_str() else {
            continue;
        };
        let at = params["timestamp"].as_f64();
        match event["method"].as_str().unwrap_or_default() {
            "Network.requestWillBeSent" => {
                if let Some(redirect) = params.get("redirectResponse") {
                    if let Some(row) = open(&mut rows, id) {
                        row.status = redirect["status"].as_u64();
                        row.size = Some(redirect["encodedDataLength"].as_u64().unwrap_or(0));
                        row.ms = elapsed(row.started, at);
                    }
                }
                let request = Request {
                    id: id.to_owned(),
                    method: params["request"]["method"].as_str().unwrap_or("GET").to_owned(),
                    url: params["request"]["url"].as_str().unwrap_or_default().to_owned(),
                    kind: params["type"].as_str().unwrap_or("Other").to_owned(),
                    status: None,
                    mime: None,
                    size: None,
                    ms: None,
                    failed: None,
                    started: at,
                };
                if max > 0 && rows.len() >= max {
                    dropped += 1;
                }
                keep_newest(&mut rows, max, request);
            }
            "Network.responseReceived" => {
                if let Some(row) = open(&mut rows, id) {
                    row.status = params["response"]["status"].as_u64();
                    row.mime = params["response"]["mimeType"].as_str().map(str::to_owned);
                    if let Some(kind) = params["type"].as_str() {
                        row.kind = kind.to_owned();
                    }
                }
            }
            "Network.loadingFinished" => {
                if let Some(row) = open(&mut rows, id) {
                    row.size = Some(params["encodedDataLength"].as_f64().unwrap_or(0.0) as u64);
                    row.ms = elapsed(row.started, at);
                }
            }
            "Network.loadingFailed" => {
                if let Some(row) = open(&mut rows, id) {
                    let why = params["errorText"].as_str().unwrap_or("failed");
                    row.failed = Some(match params["canceled"].as_bool() == Some(true) {
                        true => format!("canceled, {why}"),
                        false => why.to_owned(),
                    });
                    row.ms = elapsed(row.started, at);
                }
            }
            _ => {}
        }
    }
    (rows, dropped)
}

/// Milliseconds between two of the browser's monotonic stamps, in seconds.
fn elapsed(from: Option<f64>, to: Option<f64>) -> Option<f64> {
    Some(((to? - from?) * 1000.0).max(0.0))
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

    #[test]
    fn console_entries_carry_their_level_and_the_pages_own_time() {
        let warn = json!({ "method": "Runtime.consoleAPICalled", "params": { "type": "warning", "timestamp": 1_761_998_400_123.5, "args": [{ "type": "string", "value": "slow" }] } });
        let entry = console_entry(&warn).expect("a console call");
        assert_eq!(entry.level, "WARN");
        assert_eq!(entry.text, "console.warning: slow");
        assert_eq!(entry.at_ms, Some(1_761_998_400_123.5));
        let log = json!({ "method": "Log.entryAdded", "params": { "entry": { "source": "network", "level": "error", "text": "404", "timestamp": 5.0 } } });
        assert_eq!(console_entry(&log).expect("a log entry").level, "ERROR");
    }

    fn sent(id: &str, url: &str, at: f64) -> Value {
        json!({ "method": "Network.requestWillBeSent", "params": { "requestId": id, "timestamp": at, "type": "Fetch", "request": { "method": "POST", "url": url } } })
    }

    #[test]
    fn requests_read_with_status_size_and_time() {
        let events = vec![
            sent("1", "https://api/builds", 10.0),
            json!({ "method": "Network.responseReceived", "params": { "requestId": "1", "type": "Fetch", "response": { "status": 200, "mimeType": "application/json" } } }),
            json!({ "method": "Network.loadingFinished", "params": { "requestId": "1", "timestamp": 10.25, "encodedDataLength": 512.0 } }),
            sent("2", "https://api/down", 11.0),
            json!({ "method": "Network.loadingFailed", "params": { "requestId": "2", "timestamp": 11.5, "errorText": "net::ERR_CONNECTION_REFUSED" } }),
            sent("3", "https://api/slow", 12.0),
        ];
        let (rows, dropped) = network_requests(&events, 10);
        assert_eq!(dropped, 0);
        let lines: Vec<String> = rows.iter().map(Request::line).collect();
        assert_eq!(
            lines,
            [
                "POST 200 Fetch 250 ms 512 B https://api/builds",
                "POST failed (net::ERR_CONNECTION_REFUSED) Fetch 500 ms https://api/down",
                "POST pending Fetch https://api/slow",
            ]
        );
    }

    #[test]
    fn a_redirect_is_its_own_row() {
        let mut second = sent("1", "https://b/", 1.5);
        second["params"]["redirectResponse"] = json!({ "status": 302, "encodedDataLength": 90 });
        let (rows, _) = network_requests(&[sent("1", "https://a/", 1.0), second], 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, Some(302));
        assert_eq!(rows[0].ms, Some(500.0));
        assert_eq!(rows[1].url, "https://b/");
        assert!(!rows[1].finished());
    }

    #[test]
    fn only_the_newest_requests_are_kept() {
        let events: Vec<Value> = (0..7)
            .map(|i| sent(&i.to_string(), &format!("https://x/{i}"), f64::from(i)))
            .collect();
        let (rows, dropped) = network_requests(&events, 3);
        assert_eq!(dropped, 4);
        let urls: Vec<&str> = rows.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://x/4", "https://x/5", "https://x/6"]);
    }

    #[test]
    fn the_event_ring_drops_the_oldest() {
        let mut ring = vec![1, 2, 3];
        keep_newest(&mut ring, 3, 4);
        assert_eq!(ring, [2, 3, 4]);
        let mut shrunk = vec![1, 2, 3, 4, 5];
        keep_newest(&mut shrunk, 2, 6);
        assert_eq!(shrunk, [5, 6]);
    }
}
