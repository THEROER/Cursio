//! The node's own notification-area icon on Linux: the node at a glance, the
//! switches people reach for most, and the way into the app's window.
//!
//! It used to belong to the app, through libayatana-appindicator. That is
//! GTK 3, and an app drawing its window with Chromium (CEF, on a machine whose
//! WebKitGTK has no GPU path) is GTK 4 — the two cannot share a process. So
//! the icon moved to the process that is always running anyway, and speaks
//! the StatusNotifierItem protocol over D-Bus itself (`ksni`, on the zbus the
//! node already links), which needs no toolkit at all. The app then has no
//! reason to outlive its window: a click here starts it on the page asked
//! for, and a running one takes that over its single-instance hand-off.
//!
//! The menu is fed the way every other client is: a control-socket
//! subscription to this very node, so it shows exactly what the window and
//! the GNOME tile show, and changes the same way.
//!
//! GNOME shows a StatusNotifierItem only with the AppIndicator extension,
//! which Ubuntu ships on; without one the icon is simply not drawn, and
//! nothing else changes.

use std::time::Duration;

use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{MenuItem, TrayMethods};

use crate::control::{transport, Command, ModuleSwitch, Presence, Reply, Status};

const SWITCHES: [(ModuleSwitch, &str); 6] = [
    (ModuleSwitch::Input, "Share keyboard and mouse"),
    (ModuleSwitch::Clipboard, "Share clipboard"),
    (ModuleSwitch::Audio, "Share audio"),
    (ModuleSwitch::Files, "Share disks"),
    (ModuleSwitch::Usb, "Share USB devices"),
    (
        ModuleSwitch::RemoteScreen,
        "Peers' screens for screen sharing",
    ),
];

/// Longest a problem's text runs in the menu, in characters. A menu item is
/// as wide as its text: a crash report with a file path in it made the menu
/// wider than two screens. The whole text is in the status.
const PROBLEM_CHARS: usize = 60;

/// How long the icon waits before subscribing again after its connection to
/// the node's control socket ended.
const RECONNECT: Duration = Duration::from_secs(2);

struct NodeTray {
    status: Option<Box<Status>>,
    /// Requests to the node, from menu clicks; answered on the subscription's
    /// task, since a click must not wait on a socket.
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
}

/// Start the app on `page` (`None` is its status page).
fn open_app(page: Option<&str>) {
    let Some(app) = crate::update::layout::stable_executable("cursio-app") else {
        tracing::warn!("the tray cannot open a window: no cursio-app is installed");
        return;
    };
    let mut command = std::process::Command::new(&app);
    if let Some(page) = page {
        command.args(["--page", page]);
    }
    match command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // A running app takes the request and this copy exits at once;
            // reaping it keeps that exit from lingering as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(app = %app.display(), error = %e, "the tray cannot open a window"),
    }
}

fn clipped(text: &str) -> String {
    match text.char_indices().nth(PROBLEM_CHARS) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text.to_owned(),
    }
}

/// The one-line summary: who is here and how the cursor feels.
pub fn summary(status: &Status) -> String {
    let online = status
        .peers
        .iter()
        .filter(|p| p.presence == Presence::Online)
        .count();
    let mut line = format!("Cursio — {online}/{} peers online", status.peers.len());
    if let Some(p99) = status.latency.iter().map(|r| r.p99_us).max() {
        line.push_str(&format!(" · p99 {:.1} ms", p99 as f64 / 1000.0));
    }
    line
}

fn problems_line(status: &Status) -> String {
    match status.problems.len() {
        0 => "No problems".to_owned(),
        1 => format!("1 problem: {}", clipped(&status.problems[0].summary)),
        n => format!("{n} problems — open status for details"),
    }
}

fn switched_on(status: &Status, switch: ModuleSwitch) -> bool {
    let m = &status.modules;
    match switch {
        ModuleSwitch::Input => m.input,
        ModuleSwitch::Clipboard => m.clipboard,
        ModuleSwitch::Audio => m.audio,
        ModuleSwitch::Files => m.files,
        ModuleSwitch::Usb => m.usb,
        ModuleSwitch::RemoteScreen => m.remote_screen,
        ModuleSwitch::ScreenHost => m.screen_host,
    }
}

fn label(text: String) -> MenuItem<NodeTray> {
    StandardItem {
        label: text,
        enabled: false,
        ..Default::default()
    }
    .into()
}

fn opener(text: &str, page: Option<&'static str>) -> MenuItem<NodeTray> {
    StandardItem {
        label: text.into(),
        activate: Box::new(move |_: &mut NodeTray| open_app(page)),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for NodeTray {
    fn id(&self) -> String {
        "cursio".into()
    }

    fn title(&self) -> String {
        self.status
            .as_deref()
            .map_or_else(|| "Cursio".into(), summary)
    }

    fn icon_name(&self) -> String {
        // Installed into the user's hicolor theme with the app.
        "cursio".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        open_app(None);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::new();
        match self.status.as_deref() {
            Some(status) => {
                items.push(label(summary(status)));
                items.push(label(problems_line(status)));
                items.push(MenuItem::Separator);
                for (switch, text) in SWITCHES {
                    if !status.available.is_empty() && !status.available.contains(&switch) {
                        continue;
                    }
                    let on = switched_on(status, switch);
                    items.push(
                        CheckmarkItem {
                            label: text.into(),
                            checked: on,
                            activate: Box::new(move |tray: &mut NodeTray| {
                                let _ = tray.commands.send(Command::SetModule {
                                    module: switch,
                                    enabled: !on,
                                });
                            }),
                            ..Default::default()
                        }
                        .into(),
                    );
                }
            }
            None => items.push(label("Cursio — starting…".into())),
        }
        items.push(MenuItem::Separator);
        items.push(opener("Open status…", None));
        items.push(opener("Arrange machines…", Some("placement")));
        items.push(opener("Pair a machine…", Some("pair")));
        items
    }
}

/// Show the icon for as long as the node runs. A desktop without a
/// StatusNotifier host is told nothing and keeps the node as it is.
pub fn start() {
    tokio::spawn(async move {
        let (commands, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let tray = NodeTray {
            status: None,
            commands,
        };
        let handle = match tray.spawn().await {
            Ok(handle) => handle,
            Err(e) => {
                tracing::info!(error = %e, "no notification-area icon: the desktop shows none");
                return;
            }
        };
        let endpoint = match transport::endpoint() {
            Ok(endpoint) => endpoint,
            Err(e) => {
                tracing::warn!(error = %e, "the tray cannot reach the node's control socket");
                return;
            }
        };
        loop {
            // The socket comes up beside the tray; a first refusal is the
            // ordinary case at start.
            let Ok((client, mut pushes)) = transport::connect(&endpoint).await else {
                tokio::time::sleep(RECONNECT).await;
                continue;
            };
            if !matches!(
                client.request(Command::Subscribe).await,
                Ok(Reply::Subscribed)
            ) {
                tokio::time::sleep(RECONNECT).await;
                continue;
            }
            loop {
                tokio::select! {
                    pushed = pushes.recv() => match pushed {
                        Some(Reply::Status(status)) => {
                            handle.update(|tray| tray.status = Some(status)).await;
                        }
                        Some(_) => {}
                        None => break,
                    },
                    Some(command) = asked.recv() => {
                        match client.request(command).await {
                            Ok(Reply::Error { message }) => {
                                tracing::warn!(%message, "a tray switch was refused");
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "a tray switch did not reach the node");
                                break;
                            }
                        }
                    }
                }
            }
            handle.update(|tray| tray.status = None).await;
            tokio::time::sleep(RECONNECT).await;
        }
    });
}
