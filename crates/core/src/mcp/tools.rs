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
publishes files you built, waits until the target machine has them and starts them there. \
`phone_screenshot` shows a phone's screen and `phone_tap`/`phone_swipe`/`phone_key`/`phone_text` \
act on it, with points as fractions of the screenshot; the phone's person must have allowed screen \
sharing and turned on Cursio's accessibility service, which only they can do. Tools \
marked destructive stop or replace something running: ask the user first. Trusting a builder or \
letting an app run as administrator is the target machine's person's decision, shown there as a \
button; a tool hitting it says so — tell the user, do not retry in a loop.";

/// The long side a screenshot is scaled to unless asked otherwise: enough to
/// read a phone's text, a fraction of a full-resolution picture's tokens.
const DEFAULT_SHOT_SIDE: u32 = 1280;

/// What a tool call produced.
#[derive(Debug)]
pub enum Output {
    Text(String),
    /// A picture, with a line saying what it is.
    Image {
        png: Vec<u8>,
        note: String,
    },
}

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

    pub async fn call(&self, name: &str, args: &Value) -> Result<Output, Failure> {
        match name {
            "phone_screenshot" => self.phone_screenshot(args).await,
            "phone_tap" | "phone_swipe" | "phone_key" | "phone_text" => {
                self.phone_act(name, args).await.map(Output::Text)
            }
            "window_screenshot" => self.window_screenshot(args).await,
            "window_click" | "window_type" | "window_key" => {
                self.window_act(name, args).await.map(Output::Text)
            }
            "window_tree" => self.window_tree(args).await.map(Output::Text),
            "window_invoke" | "window_set_value" => {
                self.window_element(name, args).await.map(Output::Text)
            }
            "apps_devtools" => self.apps_devtools(args).await.map(Output::Text),
            #[cfg(feature = "web-tools")]
            name if name.starts_with("web_") => self.web(name, args).await,
            _ => self.call_text(name, args).await.map(Output::Text),
        }
    }

    /// Which window a `window_*` call means: `app` (or `app/process`) and
    /// an optional `title`.
    fn window_target(args: &Value) -> Result<cursio_protocol::diag::WindowTarget, Failure> {
        let app = need_str(args, "app")?;
        let (app, process) = match app.split_once('/') {
            Some((app, process)) => (app.to_owned(), Some(process.to_owned())),
            None => (app, None),
        };
        Ok(cursio_protocol::diag::WindowTarget {
            app,
            process,
            title: opt_str(args, "title"),
        })
    }

    /// A still of an application's window on a computer, as the image.
    async fn window_screenshot(&self, args: &Value) -> Result<Output, Failure> {
        let machine = need_str(args, "machine")?;
        let target = Self::window_target(args)?;
        let max_side = args
            .get("max_side")
            .and_then(Value::as_u64)
            .map_or(Some(DEFAULT_SHOT_SIDE), |m| (m > 0).then_some(m as u32));
        match self
            .ask
            .ask(Command::WindowShot {
                machine: machine.clone(),
                target: target.clone(),
                max_side,
            })
            .await?
        {
            Reply::Screenshot {
                path,
                width,
                height,
            } => {
                let png = std::fs::read(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                Ok(Output::Image {
                    png,
                    note: format!(
                        "{}'s window on {machine}, {width}x{height} (saved as {}). Points for \
                         window_click are fractions of this picture, 0 to 1.",
                        target.app,
                        path.display()
                    ),
                })
            }
            other => Err(answered(other)),
        }
    }

    /// This machine's listener for an application's DevTools on `machine`,
    /// opened when there is none: its port and token.
    async fn devtools_listener(&self, args: &Value) -> Result<(String, u16, String), Failure> {
        let machine = need_str(args, "machine")?;
        let app = need_str(args, "app")?;
        let (app, process) = match app.split_once('/') {
            Some((app, process)) => (app.to_owned(), Some(process.to_owned())),
            None => (app, None),
        };
        let port = args.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
        let command = Command::Devtools {
            machine,
            app: app.clone(),
            process,
            port,
            close: false,
            launch: args.get("launch").and_then(Value::as_bool).unwrap_or(false),
        };
        match self.ask.ask(command).await? {
            Reply::Devtools { port, token, .. } => Ok((app, port, token)),
            other => Err(answered(other)),
        }
    }

    async fn apps_devtools(&self, args: &Value) -> Result<String, Failure> {
        let (app, port, token) = self.devtools_listener(args).await?;
        let http = format!("http://127.0.0.1:{port}/{token}");
        Ok(format!(
            "{app}'s DevTools: {http} (Playwright: chromium.connectOverCDP('{http}'); targets: \
             {http}/json/list). The token is the only way in; it closes 15 minutes after its \
             last connection. The web_* tools use it themselves."
        ))
    }

    /// A `web_*` tool: the application's page through its DevTools. Each
    /// call is a session of its own, so `web_network` sees only what the
    /// page requests while it waits — and, for what came before, the page's
    /// own performance timeline, which has no methods or failures.
    #[cfg(feature = "web-tools")]
    async fn web(&self, name: &str, args: &Value) -> Result<Output, Failure> {
        use super::cdp::{ax_outline, console_lines, network_requests, Cdp};
        use base64::Engine as _;
        let (app, port, token) = self.devtools_listener(args).await?;
        let mut page = Cdp::page(port, &token).await?;
        let selector = || -> Result<String, Failure> { need_str(args, "selector") };
        let text = match name {
            "web_snapshot" => {
                let max = args.get("max").and_then(Value::as_u64).unwrap_or(1500) as usize;
                let tree = page.call("Accessibility.getFullAXTree", json!({})).await?;
                let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
                let (outline, truncated) = ax_outline(&nodes, max);
                match truncated {
                    true => format!("{outline}… cut at {max} lines; pass a larger `max`"),
                    false => outline,
                }
            }
            "web_query" => {
                let max = args.get("max").and_then(Value::as_u64).unwrap_or(20);
                let expression = format!(
                    "Array.from(document.querySelectorAll({sel})).slice(0, {max}).map(e => ({{ \
                     tag: e.tagName.toLowerCase(), text: (e.innerText || '').slice(0, 300), \
                     html: e.outerHTML.slice(0, 600) }}))",
                    sel = Value::String(selector()?)
                );
                let found = page.eval(&expression).await?;
                serde_json::to_string_pretty(&found).unwrap_or_default()
            }
            "web_screenshot" => {
                let shot = page
                    .call("Page.captureScreenshot", json!({ "format": "png" }))
                    .await?;
                let png = base64::engine::general_purpose::STANDARD
                    .decode(shot["data"].as_str().unwrap_or_default())
                    .map_err(|e| format!("not a picture: {e}"))?;
                return Ok(Output::Image {
                    png,
                    note: format!("{app}'s page, as its web view renders it"),
                });
            }
            "web_console" => {
                let wait = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(1000);
                page.call("Runtime.enable", json!({})).await?;
                page.call("Log.enable", json!({})).await?;
                page.listen(std::time::Duration::from_millis(wait.min(10_000)))
                    .await;
                let lines = console_lines(&page.events);
                match lines.is_empty() {
                    true => "nothing in the console".to_owned(),
                    false => lines.join("\n"),
                }
            }
            "web_network" => {
                let wait = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(2000);
                let max = args.get("max").and_then(Value::as_u64).unwrap_or(100) as usize;
                page.keep = NETWORK_EVENTS;
                page.call("Network.enable", json!({})).await?;
                page.listen(std::time::Duration::from_millis(wait.min(15_000)))
                    .await;
                let (rows, dropped) = network_requests(&page.events, max.clamp(1, 500));
                let earlier = page.eval(EARLIER_REQUESTS).await.unwrap_or(Value::Null);
                let mut out = String::new();
                if let Some(earlier) = earlier.as_array().filter(|e| !e.is_empty()) {
                    out.push_str("before this call (the page's performance timeline):\n");
                    for entry in earlier {
                        out.push_str(&format!(
                            "{} {} {:.0} ms {} B {}\n",
                            entry["status"]
                                .as_u64()
                                .map_or("?".to_owned(), |s| s.to_string()),
                            entry["kind"].as_str().unwrap_or("other"),
                            entry["ms"].as_f64().unwrap_or(0.0),
                            entry["size"].as_u64().unwrap_or(0),
                            entry["url"].as_str().unwrap_or_default(),
                        ));
                    }
                    out.push('\n');
                }
                out.push_str(&format!("during {wait} ms:\n"));
                if dropped > 0 {
                    out.push_str(&format!("… {dropped} older requests left out\n"));
                }
                if rows.is_empty() {
                    out.push_str("no requests\n");
                }
                for row in &rows {
                    out.push_str(&row.line());
                    out.push('\n');
                }
                if let Some(wanted) = args.get("body_for").and_then(Value::as_str) {
                    let row = rows
                        .iter()
                        .rev()
                        .find(|r| r.url.contains(wanted) && r.size.is_some() && r.failed.is_none())
                        .ok_or_else(|| {
                            Failure::Failed(format!(
                                "no finished request during the wait has {wanted:?} in its URL"
                            ))
                        })?;
                    let body = page
                        .call("Network.getResponseBody", json!({ "requestId": row.id }))
                        .await?;
                    let text = body["body"].as_str().unwrap_or_default();
                    out.push_str(&format!("\nbody of {}:\n", row.url));
                    match body["base64Encoded"].as_bool() == Some(true) {
                        true => out.push_str(&format!(
                            "(binary, {} bytes as base64; not shown)\n",
                            text.len()
                        )),
                        false => {
                            let mut cut = text.len().min(BODY_CAP);
                            while !text.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            out.push_str(&text[..cut]);
                            if cut < text.len() {
                                out.push_str(&format!("\n… cut at {} KiB", BODY_CAP / 1024));
                            }
                        }
                    }
                }
                out
            }
            "web_click" => {
                let expression = format!(
                    "(() => {{ const e = document.querySelector({sel}); if (!e) return null; \
                     e.scrollIntoView({{ block: 'center' }}); const r = e.getBoundingClientRect(); \
                     return [r.left + r.width / 2, r.top + r.height / 2]; }})()",
                    sel = Value::String(selector()?)
                );
                let point = page.eval(&expression).await?;
                let (Some(x), Some(y)) = (point[0].as_f64(), point[1].as_f64()) else {
                    return Err(Failure::Failed(format!("nothing matches {}", selector()?)));
                };
                for kind in ["mousePressed", "mouseReleased"] {
                    page.call(
                        "Input.dispatchMouseEvent",
                        json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
                    )
                    .await?;
                }
                format!("clicked {} at {x:.0},{y:.0}", selector()?)
            }
            "web_fill" => {
                let text = need_str(args, "text")?;
                let expression = format!(
                    "(() => {{ const e = document.querySelector({sel}); if (!e) return false; \
                     e.focus(); if (e.select) e.select(); return true; }})()",
                    sel = Value::String(selector()?)
                );
                if page.eval(&expression).await? != Value::Bool(true) {
                    return Err(Failure::Failed(format!("nothing matches {}", selector()?)));
                }
                page.call("Input.insertText", json!({ "text": text }))
                    .await?;
                format!("typed into {}", selector()?)
            }
            "web_eval" => {
                let expression = need_str(args, "expression")?;
                let value = page.eval(&expression).await?;
                let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
                if text.len() > 64 * 1024 {
                    text.truncate(64 * 1024);
                    text.push_str("\n… cut at 64 KiB");
                }
                text
            }
            _ => return Err(Failure::UnknownTool),
        };
        Ok(Output::Text(text))
    }

    /// An application's window as the tree of elements it publishes.
    async fn window_tree(&self, args: &Value) -> Result<String, Failure> {
        let machine = need_str(args, "machine")?;
        let target = Self::window_target(args)?;
        let depth = args
            .get("depth")
            .and_then(Value::as_u64)
            .map(|d| d.min(u64::from(u16::MAX)) as u16);
        let max_nodes = args
            .get("max_nodes")
            .and_then(Value::as_u64)
            .map(|n| n.min(u64::from(u32::MAX)) as u32);
        match self
            .ask
            .ask(Command::WindowTree {
                machine: machine.clone(),
                target: target.clone(),
                depth,
                max_nodes,
            })
            .await?
        {
            Reply::WindowTree {
                title,
                text,
                nodes,
                truncated,
            } => {
                let cut = match truncated {
                    true => "; cut at the limits — raise depth or max_nodes to see more",
                    false => "",
                };
                Ok(format!(
                    "{}'s window {title:?} on {machine}, {nodes} elements{cut}. Each line: role \
                     \"name\" = value [states] @centre-x,centre-y #id. Act with window_invoke by \
                     role and name, or by id.\n{text}",
                    target.app
                ))
            }
            other => Err(answered(other)),
        }
    }

    /// One element of an application's window acted on by role and name.
    async fn window_element(&self, name: &str, args: &Value) -> Result<String, Failure> {
        use cursio_protocol::diag::{ElementAct, ElementPick};
        let machine = need_str(args, "machine")?;
        let target = Self::window_target(args)?;
        let pick = match opt_str(args, "id") {
            Some(id) => ElementPick::Id(id),
            None => ElementPick::Match {
                role: need_str(args, "role")?,
                name: need_str(args, "name")?,
                nth: args
                    .get("nth")
                    .and_then(Value::as_u64)
                    .map_or(0, |n| n.min(u64::from(u32::MAX)) as u32),
            },
        };
        let act = match name {
            "window_set_value" => ElementAct::SetValue(need_str(args, "value")?),
            _ => match opt_str(args, "action").as_deref().unwrap_or("invoke") {
                "invoke" => ElementAct::Invoke,
                "toggle" => ElementAct::Toggle,
                "select" => ElementAct::Select,
                "expand" => ElementAct::Expand,
                "collapse" => ElementAct::Collapse,
                "focus" => ElementAct::Focus,
                other => {
                    return Err(Failure::Failed(format!(
                        "{other} is not an action; use invoke, toggle, select, expand, collapse or focus"
                    )))
                }
            },
        };
        match self
            .ask
            .ask(Command::WindowElement {
                machine,
                target,
                pick,
                act,
            })
            .await?
        {
            Reply::Acted { what } => Ok(what),
            other => Err(answered(other)),
        }
    }

    /// A click, text or a key into an application's window on a computer.
    async fn window_act(&self, name: &str, args: &Value) -> Result<String, Failure> {
        use cursio_protocol::diag::{window_fraction, DesktopInput};
        let machine = need_str(args, "machine")?;
        let target = Self::window_target(args)?;
        let fraction = |key: &str| -> Result<u16, Failure> {
            let value = args
                .get(key)
                .and_then(Value::as_f64)
                .ok_or_else(|| Failure::Failed(format!("{key} is required, a fraction 0 to 1")))?;
            window_fraction(value as f32).map_err(Failure::Failed)
        };
        let input = match name {
            "window_click" => DesktopInput::Click {
                x: fraction("x")?,
                y: fraction("y")?,
                right: args.get("right").and_then(Value::as_bool).unwrap_or(false),
            },
            "window_type" => DesktopInput::Type {
                text: need_str(args, "text")?,
            },
            _ => DesktopInput::Key {
                key: need_str(args, "key")?,
            },
        };
        match self
            .ask
            .ask(Command::WindowAct {
                machine,
                target,
                input,
            })
            .await?
        {
            Reply::Acted { what } => Ok(what),
            other => Err(answered(other)),
        }
    }

    /// A still of a phone's shared screen, as the image itself.
    async fn phone_screenshot(&self, args: &Value) -> Result<Output, Failure> {
        let phone = need_str(args, "phone")?;
        let max_side = args
            .get("max_side")
            .and_then(Value::as_u64)
            .map_or(Some(DEFAULT_SHOT_SIDE), |m| (m > 0).then_some(m as u32));
        let ask = args.get("ask").and_then(Value::as_bool).unwrap_or(false);
        match self
            .ask
            .ask(Command::PhoneScreenshot {
                phone: phone.clone(),
                max_side,
                ask,
            })
            .await?
        {
            Reply::Screenshot {
                path,
                width,
                height,
            } => {
                let png = std::fs::read(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                Ok(Output::Image {
                    png,
                    note: format!(
                        "{phone}'s screen, {width}x{height} (saved as {}). Points for phone_tap \
                         and phone_swipe are fractions of this picture, 0 to 1.",
                        path.display()
                    ),
                })
            }
            other => Err(answered(other)),
        }
    }

    /// A tap, swipe, key or text on a phone's shared screen.
    async fn phone_act(&self, name: &str, args: &Value) -> Result<String, Failure> {
        use crate::phone_action::{PhoneAction, PhoneKey};
        let phone = need_str(args, "phone")?;
        let num = |key: &str| -> Result<f32, Failure> {
            args.get(key)
                .and_then(Value::as_f64)
                .map(|n| n as f32)
                .ok_or_else(|| Failure::Failed(format!("{key} is required, a fraction 0 to 1")))
        };
        let ms = |key: &str| args.get(key).and_then(Value::as_u64).map(|n| n as u32);
        let action = match name {
            "phone_tap" => PhoneAction::Tap {
                x: num("x")?,
                y: num("y")?,
                hold_ms: ms("hold_ms").unwrap_or(0),
            },
            "phone_swipe" => PhoneAction::Swipe {
                x1: num("x1")?,
                y1: num("y1")?,
                x2: num("x2")?,
                y2: num("y2")?,
                ms: ms("ms").unwrap_or(300),
            },
            "phone_key" => PhoneAction::Key {
                key: match need_str(args, "key")?.as_str() {
                    "back" => PhoneKey::Back,
                    "home" => PhoneKey::Home,
                    "recents" => PhoneKey::Recents,
                    other => {
                        return Err(Failure::Failed(format!(
                            "{other:?} is not a phone key; back, home or recents"
                        )))
                    }
                },
            },
            _ => PhoneAction::Text {
                text: args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Failure::Failed("text is required".into()))?
                    .to_owned(),
            },
        };
        match self.ask.ask(Command::PhoneAct { phone, action }).await? {
            Reply::Acted { what } => Ok(what),
            other => Err(answered(other)),
        }
    }

    async fn call_text(&self, name: &str, args: &Value) -> Result<String, Failure> {
        match name {
            "machines" => self.machines().await,
            "logs" => self.logs(args).await,
            "report" => self.report(args).await,
            "path" => self.path(args).await,
            "apps_status" => apps_status(&self.config_path),
            "apps_publish" => apps_publish(args),
            "apps_launch" => apps_launch(args).await,
            "apps_ship" => apps_ship(args).await,
            "apps_actions" => apps_actions(args).await,
            "apps_call" => apps_call(args).await,
            "apps_peers" => apps_peers(args).await,
            "apps_send" => apps_send(args).await,
            "restart_machine" => {
                self.maintain(args, cursio_protocol::diag::Maintenance::Restart)
                    .await
            }
            "apply_update" => {
                self.maintain(args, cursio_protocol::diag::Maintenance::ApplyUpdate)
                    .await
            }
            "settings" => self.settings(args).await,
            "set_setting" => self.set_setting(args).await,
            _ => Err(Failure::UnknownTool),
        }
    }

    async fn settings(&self, args: &Value) -> Result<String, Failure> {
        let machine = opt_str(args, "machine").filter(|m| m != "this");
        match self.ask.ask(Command::Settings { machine }).await? {
            Reply::Settings { settings } => {
                let listed: Vec<Value> = settings
                    .iter()
                    .map(|s| {
                        json!({
                            "key": s.key,
                            "section": s.section,
                            "title": s.title,
                            "hint": s.hint,
                            "kind": s.kind,
                            "value": serde_json::from_str::<Value>(&s.value_json).unwrap_or(Value::Null),
                            "default": serde_json::from_str::<Value>(&s.default_json).unwrap_or(Value::Null),
                            "optional": s.optional,
                            "apply": s.apply,
                            "pinned": s.pinned,
                            "waits_for_restart": s.waits_for_restart,
                        })
                    })
                    .collect();
                Ok(pretty(&json!(listed)))
            }
            other => Err(answered(other)),
        }
    }

    async fn set_setting(&self, args: &Value) -> Result<String, Failure> {
        let machine = opt_str(args, "machine").filter(|m| m != "this");
        let key = need_str(args, "key")?;
        let reset = args.get("reset").and_then(Value::as_bool).unwrap_or(false);
        let command = if reset {
            Command::ResetSetting {
                machine: machine.clone(),
                key: key.clone(),
            }
        } else {
            let value = args
                .get("value")
                .cloned()
                .ok_or_else(|| Failure::Failed("value is required unless reset is true".into()))?;
            Command::SetSetting {
                machine: machine.clone(),
                key: key.clone(),
                value,
            }
        };
        let at = machine.as_deref().unwrap_or("this machine");
        match self.ask.ask(command).await? {
            Reply::Saved {
                pinned,
                restart_needed,
            } => Ok(if pinned.contains(&key) {
                format!("{key} saved on {at}; a command-line flag holds the running value until the node restarts without it")
            } else if restart_needed.contains(&key) {
                format!("{key} saved on {at}; it applies when that node restarts (restart_machine)")
            } else {
                format!("{key} changed on {at} and applied")
            }),
            other => Err(answered(other)),
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

async fn apps_actions(args: &Value) -> Result<String, Failure> {
    let machine = need_str(args, "machine")?;
    let app = need_str(args, "app")?;
    let actions = crate::apps::cli::actions(&machine, &app)
        .await
        .map_err(|e| e.to_string())?;
    Ok(crate::apps::cli::describe_actions(&actions))
}

async fn apps_call(args: &Value) -> Result<String, Failure> {
    let machine = need_str(args, "machine")?;
    let app = need_str(args, "app")?;
    let action = need_str(args, "action")?;
    let target = match opt_str(args, "process") {
        Some(process) => format!("{app}/{process}"),
        None => app,
    };
    let call_args = match args.get("args") {
        None | Some(Value::Null) => String::new(),
        Some(value) => value.to_string(),
    };
    crate::apps::cli::call(&machine, &target, &action, &call_args)
        .await
        .map_err(|e| e.to_string().into())
}

async fn apps_peers(args: &Value) -> Result<String, Failure> {
    let app = need_str(args, "app")?;
    let peers = crate::apps::cli::peers(&app)
        .await
        .map_err(|e| e.to_string())?;
    Ok(crate::apps::cli::describe_peers(&app, &peers))
}

async fn apps_send(args: &Value) -> Result<String, Failure> {
    let machine = need_str(args, "machine")?;
    let app = need_str(args, "app")?;
    let topic = need_str(args, "topic")?;
    let text = need_str(args, "text")?;
    let copies = crate::apps::cli::send(&machine, &app, &topic, &text)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "{machine}: {app} took it ({copies} running copies)"
    ))
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
        str_list(args, "args"),
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
                let start = crate::apps::cli::Start {
                    component: opt_str(args, "component"),
                    launch: args.get("launch").and_then(Value::as_bool).unwrap_or(true),
                    args: str_list(args, "args"),
                };
                crate::apps::cli::ship(
                    &to,
                    &app,
                    &version,
                    start,
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
    let phone = json!({ "type": "string", "description": "a phone's name as `machines` lists it" });
    let fraction = json!({ "type": "number", "minimum": 0, "maximum": 1 });
    let window_fraction = fraction.clone();
    let window_machine =
        json!({ "type": "string", "description": "the computer's name as `machines` lists it" });
    let window_app =
        json!({ "type": "string", "description": "the application's id, or `<id>/<process>`" });
    let window_title = json!({ "type": "string", "description": "the window whose title contains this; the largest otherwise" });
    let mut tools = vec![
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
                "files": { "type": "array", "items": { "type": "string" }, "description": "PLATFORM=PATH (windows-x86_64, linux-x86_64, android-aarch64) or a path: an .apk is android-aarch64, anything else this machine's platform" },
                "version": { "type": "string" },
                "admin": { "type": "array", "items": { "type": "string" }, "description": "file names that ask to run as administrator" },
            }),
            &["app", "files"],
        ),
        tool(
            "apps_actions",
            "List what a running application on a machine lets you ask it to do: each action's name, description and JSON Schema of its arguments, per process.",
            true, false,
            json!({
                "machine": machine.clone(),
                "app": { "type": "string" },
            }),
            &["machine", "app"],
        ),
        tool(
            "apps_call",
            "Ask a running application on a machine to do one of its declared actions (see apps_actions) and get its JSON answer. It is done by the process already running, as its own buttons would. The machine's person allows calls to an application once.",
            false, true,
            json!({
                "machine": machine.clone(),
                "app": { "type": "string" },
                "process": { "type": "string", "description": "one of the application's processes; its main one when absent" },
                "action": { "type": "string" },
                "args": { "description": "the action's arguments, as its schema describes" },
            }),
            &["machine", "app", "action"],
        ),
        tool(
            "apps_peers",
            "The connected machines where an application is registered: whether a copy runs there, its version, and the message topics its copies listen to.",
            true, false,
            json!({ "app": { "type": "string" } }),
            &["app"],
        ),
        tool(
            "apps_send",
            "Send a text message about a topic to the running copies of an application on a machine that listen to it, as its copy here would. Fails saying whether the machine is offline, no copy listens, or its person has not allowed this machine.",
            false, true,
            json!({
                "machine": machine.clone(),
                "app": { "type": "string" },
                "topic": { "type": "string" },
                "text": { "type": "string" },
            }),
            &["machine", "app", "topic", "text"],
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
                "args": { "type": "array", "items": { "type": "string" }, "description": "arguments for the started file, e.g. [\"ui\", \"--no-daemon\"]" },
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
                "args": { "type": "array", "items": { "type": "string" }, "description": "arguments for the started file" },
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
            false, true, json!({ "machine": machine.clone() }), &["machine"],
        ),
        tool(
            "settings",
            "Every setting of a machine by key: its value, default, kind (with the values it takes), what changing it takes, and whether it waits for a restart.",
            true, false, json!({ "machine": machine.clone() }), &[],
        ),
        tool(
            "set_setting",
            "Change one setting of a machine by key, as `settings` names it; applied there at once unless it waits for a restart. `reset` puts it back to its default.",
            false, false,
            json!({
                "machine": machine,
                "key": { "type": "string", "description": "e.g. audio.mic_gate, modules.clipboard" },
                "value": { "description": "JSON of the setting's kind: true, -55, \"auto\", null to unset an optional one" },
                "reset": { "type": "boolean", "description": "back to the default instead of setting value" },
            }),
            &["key"],
        ),
        tool(
            "phone_screenshot",
            "A picture of a phone's screen, from the stream it shares with this computer. Its screen must be shared: with ask, the phone is asked and its person must allow it there; this waits up to 45 s.",
            true, false,
            json!({
                "phone": phone.clone(),
                "ask": { "type": "boolean", "description": "ask the phone for its screen when it is not shared yet" },
                "max_side": { "type": "integer", "description": "scale the long side to at most this many pixels (default 1280, 0 for full size)" },
            }),
            &["phone"],
        ),
        tool(
            "phone_tap",
            "Tap a phone's screen at a point, given as fractions of the screenshot's width and height (0 to 1). hold_ms makes it a long press. Needs the phone's screen shared and Cursio's accessibility service on there.",
            false, true,
            json!({
                "phone": phone.clone(),
                "x": fraction.clone(), "y": fraction.clone(),
                "hold_ms": { "type": "integer", "description": "hold this long; 600+ is a long press" },
            }),
            &["phone", "x", "y"],
        ),
        tool(
            "phone_swipe",
            "Drag a finger across a phone's screen from (x1, y1) to (x2, y2), fractions 0 to 1, over ms (default 300). Scrolls, swipes between pages, pulls down the shade.",
            false, true,
            json!({
                "phone": phone.clone(),
                "x1": fraction.clone(), "y1": fraction.clone(), "x2": fraction.clone(), "y2": fraction,
                "ms": { "type": "integer" },
            }),
            &["phone", "x1", "y1", "x2", "y2"],
        ),
        tool(
            "phone_key",
            "Press one of a phone's buttons: back, home or recents.",
            false, true,
            json!({ "phone": phone.clone(), "key": { "type": "string", "enum": ["back", "home", "recents"] } }),
            &["phone", "key"],
        ),
        tool(
            "phone_text",
            "Type text into the field that has focus on a phone, any script; it is appended to what the field holds. Fails when no text field has focus — tap one first.",
            false, true,
            json!({ "phone": phone, "text": { "type": "string" } }),
            &["phone", "text"],
        ),
        tool(
            "window_screenshot",
            "A picture of a window of an application on a computer of the mesh. The application must be registered there through cursio-sdk and running; that machine's person allows driving it once (the same answer as for its actions). Windows, and GNOME on Linux through Cursio's Shell extension.",
            true, false,
            json!({
                "machine": window_machine.clone(),
                "app": window_app.clone(),
                "title": window_title.clone(),
                "max_side": { "type": "integer", "description": "scale the long side to at most this many pixels (default 1280, 0 for full size)" },
            }),
            &["machine", "app"],
        ),
        tool(
            "window_click",
            "Click in an application's window on a computer at a point given as fractions of its screenshot (0 to 1). Posted to the window: the person's real cursor does not move.",
            false, true,
            json!({
                "machine": window_machine.clone(), "app": window_app.clone(), "title": window_title.clone(),
                "x": window_fraction.clone(), "y": window_fraction,
                "right": { "type": "boolean", "description": "a right click" },
            }),
            &["machine", "app", "x", "y"],
        ),
        tool(
            "window_type",
            "Type text, any script, into the focused control of an application's window on a computer.",
            false, true,
            json!({ "machine": window_machine.clone(), "app": window_app.clone(), "title": window_title.clone(), "text": { "type": "string" } }),
            &["machine", "app", "text"],
        ),
        tool(
            "window_key",
            "Press a key in an application's window on a computer: enter, escape, tab, backspace, delete, up, down, left, right, home, end, pageup, pagedown, f1…f12.",
            false, true,
            json!({ "machine": window_machine.clone(), "app": window_app.clone(), "title": window_title.clone(), "key": { "type": "string" } }),
            &["machine", "app", "key"],
        ),
        tool(
            "window_tree",
            "An application's window as the tree of elements it publishes to screen readers (UI Automation on Windows, AT-SPI on Linux): role, name, value, states, centre as fractions of the window, and an id. Prefer this to a screenshot: it is small, exact, and lets window_invoke press a button by its name instead of by coordinates. Web views (WebView2, Electron) show their buttons and tabs too; on Linux the desktop's accessibility must be on, and the answer says how when it is not.",
            true, false,
            json!({
                "machine": window_machine.clone(), "app": window_app.clone(), "title": window_title.clone(),
                "depth": { "type": "integer", "description": "levels to walk (default 40, at most 64)" },
                "max_nodes": { "type": "integer", "description": "elements to list (default 3000, at most 10000)" },
            }),
            &["machine", "app"],
        ),
        tool(
            "window_invoke",
            "Act on one element of an application's window, named by role and name as window_tree prints them (or by its id): invoke (press a button, follow a link), toggle, select, expand, collapse or focus. Does what the element itself does; the real cursor does not move. A miss lists the names it could have meant.",
            false, true,
            json!({
                "machine": window_machine.clone(), "app": window_app.clone(), "title": window_title.clone(),
                "role": { "type": "string", "description": "button, edit, checkbox, tabitem, listitem, hyperlink, …" },
                "name": { "type": "string", "description": "exactly, ignoring case; or the only one containing it" },
                "nth": { "type": "integer", "description": "which of several with that role and name, from 0" },
                "id": { "type": "string", "description": "the #id from window_tree, in place of role and name" },
                "action": { "type": "string", "enum": ["invoke", "toggle", "select", "expand", "collapse", "focus"] },
            }),
            &["machine", "app"],
        ),
        tool(
            "window_set_value",
            "Set the value of one element of an application's window — a text field — named by role and name (or id), any script, without typing key by key.",
            false, true,
            json!({
                "machine": window_machine, "app": window_app, "title": window_title,
                "role": { "type": "string" }, "name": { "type": "string" },
                "nth": { "type": "integer" }, "id": { "type": "string" },
                "value": { "type": "string" },
            }),
            &["machine", "app", "value"],
        ),
    ];
    tools.extend(web_tools(&machine));
    tools
}

/// Network events one `web_network` call keeps: a request is four or five.
#[cfg(feature = "web-tools")]
const NETWORK_EVENTS: usize = 4000;

/// Most of one response body `web_network` returns.
#[cfg(feature = "web-tools")]
const BODY_CAP: usize = 64 * 1024;

/// The newest 50 resources the page fetched before the call, from its
/// performance timeline (`responseStatus` is absent before Chromium 109).
#[cfg(feature = "web-tools")]
const EARLIER_REQUESTS: &str = "performance.getEntriesByType('resource').slice(-50).map(e => ({ \
     url: e.name, kind: e.initiatorType, ms: e.duration, size: e.transferSize, \
     status: e.responseStatus || null }))";

/// The DevTools tools: the strongest reach there is — they run code inside
/// the application — so every one that acts is marked destructive, and the
/// one that runs arbitrary JavaScript says so in its title.
fn web_tools(machine: &Value) -> Vec<Value> {
    let app = json!({ "type": "string", "description": "the application's id, or `<id>/<process>`; it must declare DevTools (cursio-sdk App::devtools) and that machine's person must allow inspecting it, for a limited time" });
    let selector = json!({ "type": "string", "description": "a CSS selector" });
    let mut tools = vec![tool(
        "apps_devtools",
        "Open (or reuse) this machine's loopback address for an application's DevTools on another machine and return it, token included, for Playwright's connectOverCDP or a browser. Grants code execution inside the application: ask the user first. The web_* tools do this themselves.",
        false, true,
        json!({ "machine": machine, "app": app, "port": { "type": "integer" }, "launch": { "type": "boolean", "description": "when no running copy offers DevTools, start the application with --devtools there (ending the copies running without them) and wait up to 30 s" } }),
        &["machine", "app"],
    )];
    if !cfg!(feature = "web-tools") {
        return tools;
    }
    tools.extend([
        tool(
            "web_snapshot",
            "An application's page as its accessibility tree (roles and names, one line each) — the cheapest way to read a web view's interface.",
            true, false,
            json!({ "machine": machine, "app": app, "max": { "type": "integer", "description": "most lines (default 1500)" } }),
            &["machine", "app"],
        ),
        tool(
            "web_query",
            "Elements of an application's page matching a CSS selector: tag, text and HTML, bounded.",
            true, false,
            json!({ "machine": machine, "app": app, "selector": selector, "max": { "type": "integer" } }),
            &["machine", "app", "selector"],
        ),
        tool(
            "web_screenshot",
            "A picture of an application's page as its web view renders it.",
            true, false,
            json!({ "machine": machine, "app": app }),
            &["machine", "app"],
        ),
        tool(
            "web_console",
            "What an application's page logs: console calls, uncaught exceptions and browser log entries, collected for a moment (wait_ms, default 1000).",
            true, false,
            json!({ "machine": machine, "app": app, "wait_ms": { "type": "integer" } }),
            &["machine", "app"],
        ),
        tool(
            "web_network",
            "Requests an application's page makes while this waits (wait_ms, default 2000, at most 15000): method, status, type, time, size and URL, the newest `max` (default 100); plus the resources it fetched before, from its performance timeline. `body_for` returns the response body (up to 64 KiB) of the newest finished request whose URL contains it.",
            true, false,
            json!({ "machine": machine, "app": app, "wait_ms": { "type": "integer" }, "max": { "type": "integer" }, "body_for": { "type": "string", "description": "part of a request's URL whose response body to return" } }),
            &["machine", "app"],
        ),
        tool(
            "web_click",
            "Click the element of an application's page matching a CSS selector, as a mouse would.",
            false, true,
            json!({ "machine": machine, "app": app, "selector": selector }),
            &["machine", "app", "selector"],
        ),
        tool(
            "web_fill",
            "Type text (any script) into the field of an application's page matching a CSS selector, replacing what it holds.",
            false, true,
            json!({ "machine": machine, "app": app, "selector": selector, "text": { "type": "string" } }),
            &["machine", "app", "selector", "text"],
        ),
    ]);
    let mut eval = tool(
        "web_eval",
        "Run JavaScript inside an application's page and return its value as JSON. Arbitrary code execution in the application: ask the user first, and prefer web_snapshot/web_query/web_click.",
        false, true,
        json!({ "machine": machine, "app": app, "expression": { "type": "string" } }),
        &["machine", "app", "expression"],
    );
    eval["annotations"]["title"] = json!("Run JavaScript inside the application");
    tools.push(eval);
    tools
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
