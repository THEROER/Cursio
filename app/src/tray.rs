//! The notification-area icon: the node at a glance, and the switches people
//! reach for most.
//!
//! On GNOME the icon is a StatusNotifier item through libayatana-appindicator,
//! which only shows with the AppIndicator extension (Ubuntu ships it on);
//! Windows puts it in the notification area. An appindicator reports no
//! clicks on the icon itself, only menu choices, so everything is in the menu
//! and the left click merely opens the window where clicks do arrive.
//!
//! The menu is redrawn each time the node pushes a changed status, so a toggle
//! flipped in the window, in `config.toml` or from the other machine shows
//! here at once.
//!
//! It also carries what each peer's remote-screen view shows. Those views had
//! a notification-area icon each, beside this one; with the choice here, the
//! node hides them for as long as this tray is attached (`TrayAttached`), and
//! brings them back when the app goes, so a node on its own keeps a selector.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use cursio_core::control::{Command, ModuleSwitch, Presence, Status};
use cursio_core::screen_views::ScreenView;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::node::NodeLink;

const SWITCHES: [(ModuleSwitch, &str, &str); 6] = [
    (
        ModuleSwitch::Input,
        "module.input",
        "Share keyboard and mouse",
    ),
    (
        ModuleSwitch::Clipboard,
        "module.clipboard",
        "Share clipboard",
    ),
    (ModuleSwitch::Audio, "module.audio", "Share audio"),
    (ModuleSwitch::Files, "module.files", "Share disks"),
    (ModuleSwitch::Usb, "module.usb", "Share USB devices"),
    (
        ModuleSwitch::RemoteScreen,
        "module.remote_screen",
        "Peers' screens for screen sharing",
    ),
];

struct Items {
    app: AppHandle,
    menu: Menu<Wry>,
    summary: MenuItem<Wry>,
    problems: MenuItem<Wry>,
    switches: Vec<(ModuleSwitch, CheckMenuItem<Wry>)>,
    start: MenuItem<Wry>,
    /// The remote-screen submenu, in the menu only while some view is open,
    /// and the views it was last built from.
    screens: Mutex<(Option<Submenu<Wry>>, Vec<ScreenView>)>,
}

/// Whether this app's tray is up, so the node can hide the views' own icons;
/// read by every (re)connection to the node.
static SHOWN: AtomicBool = AtomicBool::new(false);

pub fn shown() -> bool {
    SHOWN.load(Ordering::SeqCst)
}

/// Menu ids for a view's choices: the peer and the source travel in the id,
/// split by a character no name or number contains.
const SCREEN_ID: &str = "screen\u{1f}";

fn screen_id(peer: &str, source: Option<u32>) -> String {
    match source {
        Some(id) => format!("{SCREEN_ID}{peer}\u{1f}{id}"),
        None => format!("{SCREEN_ID}{peer}\u{1f}none"),
    }
}

fn parse_screen_id(id: &str) -> Option<(String, Option<u32>)> {
    let rest = id.strip_prefix(SCREEN_ID)?;
    let (peer, source) = rest.rsplit_once('\u{1f}')?;
    let source = match source {
        "none" => None,
        n => Some(n.parse().ok()?),
    };
    Some((peer.to_string(), source))
}

/// Show the main window on `route`.
pub fn open(app: &AppHandle, route: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    if let Err(e) = app.emit_to("main", "navigate", route) {
        tracing::warn!(error = %e, route, "could not switch the window's page");
    }
}

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let summary = MenuItem::with_id(app, "summary", "Cursio — connecting…", false, None::<&str>)?;
    let problems = MenuItem::with_id(app, "problems", "", false, None::<&str>)?;
    let switches: Vec<(ModuleSwitch, CheckMenuItem<Wry>)> = SWITCHES
        .iter()
        .map(|(switch, id, label)| {
            CheckMenuItem::with_id(app, *id, *label, false, false, None::<&str>)
                .map(|i| (*switch, i))
        })
        .collect::<tauri::Result<_>>()?;
    let start = MenuItem::with_id(app, "start", "Start the node", false, None::<&str>)?;
    let status = MenuItem::with_id(app, "open.status", "Open status…", true, None::<&str>)?;
    let placement = MenuItem::with_id(
        app,
        "open.placement",
        "Arrange machines…",
        true,
        None::<&str>,
    )?;
    let pair = MenuItem::with_id(app, "open.pair", "Pair a machine…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

    let menu = Menu::new(app)?;
    menu.append(&summary)?;
    menu.append(&problems)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    for (_, item) in &switches {
        menu.append(item)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&status)?;
    menu.append(&placement)?;
    menu.append(&pair)?;
    menu.append(&start)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&quit)?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("the bundle has no window icon".into()))?;
    let items = Items {
        app: app.clone(),
        menu: menu.clone(),
        summary,
        problems,
        switches: switches.clone(),
        start,
        screens: Mutex::new((None, Vec::new())),
    };

    TrayIconBuilder::with_id("cursio")
        .icon(icon)
        .tooltip("Cursio")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| {
            let id = event.id().as_ref();
            match id {
                "open.status" => open(app, "/"),
                "open.placement" => open(app, "/placement"),
                "open.pair" => open(app, "/pair"),
                // Menu events arrive on the thread that runs the event loop.
                "start" => {
                    tauri::async_runtime::spawn_blocking(|| {
                        if let Err(e) = crate::node::start_node() {
                            tracing::warn!(error = ?e, "could not start the node from the tray");
                        }
                    });
                }
                "quit" => app.exit(0),
                id if id.starts_with(SCREEN_ID) => {
                    let Some((peer, source)) = parse_screen_id(id) else {
                        return;
                    };
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let link = app.state::<NodeLink>();
                        if let Err(e) = link
                            .request(Command::ShowScreenSource { peer, source })
                            .await
                        {
                            tracing::warn!(error = ?e, "could not change what a remote-screen view shows");
                        }
                    });
                }
                _ => {
                    let Some((switch, item)) = switches.iter().find(|(_, i)| i.id().as_ref() == id)
                    else {
                        return;
                    };
                    let (switch, enabled) = (*switch, item.is_checked().unwrap_or(false));
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let link = app.state::<NodeLink>();
                        let command = cursio_core::control::Command::SetModule {
                            module: switch,
                            enabled,
                        };
                        if let Err(e) = link.request(command).await {
                            tracing::warn!(error = ?e, "could not change a module from the tray");
                        }
                    });
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                open(tray.app_handle(), "/");
            }
        })
        .build(app)?;

    SHOWN.store(true, Ordering::SeqCst);
    // The connection may already be up; tell it now rather than at the next.
    let attach = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = attach
            .state::<NodeLink>()
            .request(Command::TrayAttached)
            .await;
    });

    let mut latest = app.state::<NodeLink>().watch();
    tauri::async_runtime::spawn(async move {
        loop {
            let status = latest.borrow_and_update().clone().and_then(Result::ok);
            refresh(&items, status);
            if latest.changed().await.is_err() {
                break;
            }
        }
    });
    Ok(())
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

fn refresh(items: &Items, status: Option<Status>) {
    let Some(status) = status else {
        let _ = items.summary.set_text("Cursio — node not running");
        let _ = items
            .problems
            .set_text("Start it here or with `cursio mesh`");
        let _ = items.start.set_enabled(true);
        for (_, item) in &items.switches {
            let _ = item.set_enabled(false);
        }
        return;
    };
    let _ = items.summary.set_text(summary(&status));
    let _ = items.problems.set_text(match status.problems.len() {
        0 => "No problems".to_owned(),
        1 => format!("1 problem: {}", status.problems[0].summary),
        n => format!("{n} problems — open status for details"),
    });
    let _ = items.start.set_enabled(false);
    refresh_screens(items, &status.screens);
    let m = status.modules;
    for (switch, item) in &items.switches {
        let on = match switch {
            ModuleSwitch::Input => m.input,
            ModuleSwitch::Clipboard => m.clipboard,
            ModuleSwitch::Audio => m.audio,
            ModuleSwitch::Files => m.files,
            ModuleSwitch::Usb => m.usb,
            ModuleSwitch::RemoteScreen => m.remote_screen,
            ModuleSwitch::ScreenHost => m.screen_host,
        };
        let _ = item.set_checked(on);
        let _ = item.set_enabled(true);
    }
}

/// Rebuild the remote-screen submenu when the views or their sources change:
/// a peer per submenu, its monitors, then its windows, then "show nothing".
fn refresh_screens(items: &Items, views: &[ScreenView]) {
    let mut held = items.screens.lock().unwrap_or_else(|p| p.into_inner());
    if held.1 == views {
        return;
    }
    if let Some(old) = held.0.take() {
        let _ = items.menu.remove(&old);
    }
    held.1 = views.to_vec();
    if views.is_empty() {
        return;
    }
    match screens_menu(&items.app, views) {
        // After the switches, which are where screen sharing is turned on.
        Ok(sub) => {
            let at = 3 + items.switches.len();
            if items.menu.insert(&sub, at).is_ok() {
                held.0 = Some(sub);
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not build the remote-screen menu"),
    }
}

fn screens_menu(app: &AppHandle, views: &[ScreenView]) -> tauri::Result<Submenu<Wry>> {
    let top = Submenu::new(app, "Show in Discord/OBS", true)?;
    for view in views {
        let peer = Submenu::new(app, &view.peer, true)?;
        for monitor in [true, false] {
            let mut any = false;
            for source in view.sources.iter().filter(|s| s.monitor == monitor) {
                any = true;
                let item = CheckMenuItem::with_id(
                    app,
                    screen_id(&view.peer, Some(source.id)),
                    &source.title,
                    true,
                    view.showing == Some(source.id),
                    None::<&str>,
                )?;
                peer.append(&item)?;
            }
            if any {
                peer.append(&PredefinedMenuItem::separator(app)?)?;
            }
        }
        if view.sources.is_empty() {
            peer.append(&MenuItem::new(
                app,
                "Waiting for the peer's list…",
                false,
                None::<&str>,
            )?)?;
            peer.append(&PredefinedMenuItem::separator(app)?)?;
        }
        peer.append(&CheckMenuItem::with_id(
            app,
            screen_id(&view.peer, None),
            "Show nothing",
            true,
            view.showing.is_none(),
            None::<&str>,
        )?)?;
        top.append(&peer)?;
    }
    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_menu_id_carries_its_peer_and_source_back() {
        assert_eq!(
            parse_screen_id(&screen_id("Lightmoon PC", Some(7))),
            Some(("Lightmoon PC".to_string(), Some(7)))
        );
        assert_eq!(
            parse_screen_id(&screen_id("dark", None)),
            Some(("dark".to_string(), None))
        );
        assert_eq!(parse_screen_id("module.audio"), None);
    }
}
