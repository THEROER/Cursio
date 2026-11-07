//! The Cursio app: a window in front of a node.
//!
//! On a desktop it does not run the mesh (see `cursio_core::control` for why):
//! it is a client of the `cursio` node's control socket, showing what the node
//! is doing and changing its config, with a tray icon that is the app while
//! the window is closed. Quitting the app leaves the node running, because the
//! mesh is not the app's to stop.
//!
//! On a phone there is no separate node to be a client of: Android runs apps,
//! not daemons. The node runs in this process, kept alive by a foreground
//! service (`mobile.rs`), and the window talks to it through the same
//! control commands a desktop sends over its socket — so one UI draws both.

/// Starting the node and the tray at login.
#[cfg(desktop)]
mod autostart;
/// A phone's call, in a small window of its own.
#[cfg(desktop)]
mod call_window;
/// Opening, showing and copying a gallery item once it is on this computer.
#[cfg(desktop)]
mod gallery_files;
/// Installed versions and rolling back to the previous one.
#[cfg(desktop)]
mod install;
/// The node's and this app's log, for the window's log drawer.
#[cfg(desktop)]
mod logs;
/// The phone's foreground service and the permissions it needs.
#[cfg(mobile)]
mod mobile;
/// The line to the running node.
mod node;
/// Restarting this window as the version the install now names.
#[cfg(desktop)]
mod relaunch;
/// The platform webview or Chromium, picked before the first window.
#[cfg(desktop)]
mod renderer;
/// Driving a peer's cursor from a phone's touch surface.
#[cfg(mobile)]
mod touchpad;
#[cfg(all(desktop, not(target_os = "linux")))]
mod tray;

/// On Linux the node draws the notification-area icon (`node_tray`), whose
/// menu starts this app; a running copy takes that over its single-instance
/// hand-off. The app has no icon of its own and is nothing but its windows:
/// the main one is made when asked for and destroyed when closed, and the app
/// ends with its last window. Made, not shown from hidden, also because the
/// CEF runtime never maps a window that was created hidden and shown later.
#[cfg(target_os = "linux")]
mod tray {
    use tauri::{AppHandle, Emitter, Manager};

    pub fn shown() -> bool {
        false
    }

    /// Show the main window on `route`, making it when there is none.
    pub fn open(app: &AppHandle, route: &str) {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
            if let Err(e) = app.emit_to("main", "navigate", route) {
                tracing::warn!(error = %e, route, "could not switch the window's page");
            }
            return;
        }
        // A page still loading hears no `navigate`; it asks for this instead.
        *app.state::<crate::StartPage>()
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(route.to_owned());
        let Some(config) = app.config().app.windows.iter().find(|w| w.label == "main") else {
            tracing::warn!("the app's configuration has no main window");
            return;
        };
        let built = tauri::WebviewWindowBuilder::from_config(app, config)
            .and_then(|builder| builder.visible(true).build());
        if let Err(e) = built {
            tracing::warn!(error = %e, "could not open the window");
        }
    }
}

/// Which face of the app this build shows, so the UI picks its first page.
#[tauri::command]
fn app_platform() -> &'static str {
    if cfg!(mobile) {
        "mobile"
    } else {
        "desktop"
    }
}

/// The page `--page` asked for when this process started, for the window to
/// take once it is listening: a `navigate` event sent while the page is still
/// loading reaches nobody.
#[cfg(desktop)]
#[derive(Default)]
struct StartPage(std::sync::Mutex<Option<String>>);

#[cfg(desktop)]
#[tauri::command]
fn app_start_page(start: tauri::State<'_, StartPage>) -> Option<String> {
    start.0.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// `--page call` opens the call window rather than a page of the main one.
#[cfg(desktop)]
const CALL_ROUTE: &str = "/call";

/// The route `--page <name>` names, e.g. `--page messages` → `/messages`, and
/// `--peer <machine>` beside it opens that page on one machine
/// (`/gallery?peer=dark`). Only a plain page name is taken, and the machine
/// is percent-encoded whole: this is an argument any program may pass.
#[cfg(desktop)]
fn page_route(args: impl IntoIterator<Item = String>) -> Option<String> {
    let args: Vec<String> = args.into_iter().collect();
    let after = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    let page = after("--page")?;
    if page.is_empty() || !page.chars().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    let peer = after("--peer").filter(|p| !p.is_empty() && p.len() <= 128);
    Some(match peer {
        None => format!("/{page}"),
        Some(peer) => {
            let encoded: String = peer
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect();
            format!("/{page}?peer={encoded}")
        }
    })
}

/// Steer WebKitGTK away from the renderer paths that crash it, before any
/// thread exists and before Tauri creates the webview.
///
/// Taken from the LeavePulse launcher, which met the same crashes first: on
/// Wayland the DMABUF renderer takes the web process down, so it is off there
/// and X11 keeps the GPU path. Anything the user exported is honoured, so
/// `WEBKIT_DISABLE_DMABUF_RENDERER=0` brings it back.
#[cfg(all(target_os = "linux", feature = "wry"))]
fn tune_webkitgtk_renderer() {
    fn set_if_unset(key: &str, value: &str) {
        if std::env::var_os(key).is_none() {
            // SAFETY: called first thing in `run`, before any thread is
            // started, so nothing can be reading the environment concurrently.
            unsafe { std::env::set_var(key, value) };
        }
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t.eq_ignore_ascii_case("wayland"));

    set_if_unset("WEBKIT_FORCE_COMPOSITING_MODE", "1");
    if wayland {
        set_if_unset("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(desktop)]
    let (_log, hidden) = {
        // Started as the install's stable copy: run the current version instead.
        if let Some(target) = cursio_core::update::layout::handover("cursio-app") {
            match cursio_core::update::layout::hand_over(&target) {
                Ok(()) => return,
                Err(e) => eprintln!(
                    "could not start {}: {e}; running this copy",
                    target.display()
                ),
            }
        }
        // After the install's handover, so the version that runs is the one
        // whose Chromium binary takes over.
        #[cfg(feature = "wry")]
        renderer::hand_over_to_chromium();
        #[cfg(all(target_os = "linux", feature = "wry"))]
        tune_webkitgtk_renderer();

        // Console (info) + rotating file (debug) logging, shared with the CLI.
        // `_log` must outlive the UI — dropping it flushes the log file.
        let log = cursio_core::logging::init("cursio-app");

        // Ensure this node has a stable identity before the UI comes up; the
        // same identity the `cursio` CLI uses, so the app and CLI act as one
        // node.
        match cursio_core::NodeIdentity::load_or_create() {
            Ok(identity) => {
                tracing::info!(
                    name = %identity.display_name,
                    peer_id = %identity.peer_id,
                    "cursio node"
                );
            }
            Err(e) => tracing::warn!(error = %e, "could not load node identity"),
        }

        // Started to replace an older window: let it go before the
        // single-instance check below would hand it this launch instead.
        relaunch::wait_for_predecessor(&std::env::args().collect::<Vec<_>>());

        // Started at login by autostart: come up as the tray alone.
        let hidden = std::env::args().any(|a| a == "--hidden");
        // Linux has no tray of the app's to come up as (see `tray`): a start
        // asking for no window — an autostart entry an older app wrote, a
        // restart of an app nobody was looking at — has nothing to do.
        if cfg!(target_os = "linux") && hidden {
            return;
        }
        (log, hidden)
    };

    let builder = tauri::Builder::default();
    #[cfg(feature = "cef")]
    let builder = builder.runtime(cef_runtime());
    #[cfg(not(feature = "cef"))]
    let builder = builder.runtime(tauri_runtime_wry::Wry::default());
    let builder = builder.manage(node::NodeLink::default());

    #[cfg(desktop)]
    let first_page = page_route(std::env::args());

    #[cfg(desktop)]
    let builder = builder
        // First, as the plugin requires: a second launch must be caught before
        // it builds a window and a tray of its own.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if argv.iter().any(|a| a == "--hidden") {
                return;
            }
            match page_route(argv) {
                Some(route) if route == CALL_ROUTE => call_window::open(app),
                route => tray::open(app, &route.unwrap_or_else(|| "/".into())),
            }
        }))
        .setup(move |app| {
            node::follow(app.handle().clone());
            relaunch::follow_install(app.handle().clone());
            // Started by the node for a call: the call window alone, the main
            // window left in the tray as if started at login.
            let for_call = first_page.as_deref() == Some(CALL_ROUTE);
            let hidden = hidden || for_call;
            let first_page = if for_call { None } else { first_page.clone() };
            if for_call {
                call_window::open(app.handle());
            }
            // `reg.exe` / `systemctl`: off the thread that draws the window.
            tauri::async_runtime::spawn_blocking(autostart::refresh);
            // On Linux the node draws the icon itself (see `tray`).
            #[cfg(target_os = "linux")]
            if !hidden {
                tray::open(app.handle(), "/");
            }
            #[cfg(not(target_os = "linux"))]
            if let Err(e) = tray::install(app.handle()) {
                // No tray (GNOME without the AppIndicator extension) must not
                // leave an app nobody can reach: show the window instead.
                tracing::warn!(
                    error = %e,
                    "no tray icon; on GNOME install the AppIndicator extension"
                );
                tray::open(app.handle(), "/");
            } else if !hidden {
                tray::open(app.handle(), "/");
            }
            *tauri::Manager::state::<StartPage>(app)
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = first_page.clone();
            Ok(())
        })
        .manage(StartPage::default())
        .on_window_event(|window, event| {
            // Where the tray is the app, a closed window only hides. On
            // Linux it goes, and the app with the last one (see `tray`).
            if cfg!(not(target_os = "linux")) {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_platform,
            app_start_page,
            node::commands::messages_conversations,
            node::commands::messages_thread,
            node::commands::messages_send,
            node::commands::messages_mark_read,
            node::commands::gallery_list,
            node::commands::gallery_thumbnail,
            node::commands::gallery_read,
            node::commands::gallery_save,
            node::commands::gallery_save_as,
            node::commands::gallery_fetch,
            node::commands::file_open,
            node::commands::file_reveal,
            node::commands::files_copy,
            node::commands::file_send,
            node::commands::node_call,
            node::commands::node_phone_screen,
            node::commands::node_ring,
            node::commands::node_wake,
            node::commands::node_status,
            node::commands::node_config,
            node::commands::node_set_module,
            node::commands::node_set_position,
            node::commands::node_arrange,
            node::commands::node_arrange_screen,
            node::commands::node_set_barrier,
            node::commands::node_set_name,
            node::commands::node_set_mic_gate,
            node::commands::node_set_calls,
            node::commands::node_set_bluetooth_standby,
            node::commands::node_apply_config_edit,
            node::commands::node_set_stream_volume,
            node::commands::node_pair_show,
            node::commands::node_pair_enter,
            node::commands::node_pair_cancel,
            node::commands::node_pairing,
            node::commands::node_restart,
            node::commands::node_update_apply,
            node::commands::node_forget,
            node::commands::node_start,
            autostart::commands::autostart_get,
            autostart::commands::autostart_set,
            install::commands::install_status,
            install::commands::install_rollback,
            relaunch::commands::app_relaunch,
            logs::commands::log_tail,
        ]);

    #[cfg(mobile)]
    let builder = builder
        .manage(touchpad::Touchpad::default())
        .plugin(mobile::init())
        .setup(|app| {
            node::follow(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_platform,
            touchpad::commands::touchpad_status,
            touchpad::commands::touchpad_targets,
            touchpad::commands::touchpad_connect,
            touchpad::commands::touchpad_motion,
            touchpad::commands::touchpad_button,
            touchpad::commands::touchpad_scroll,
            touchpad::commands::touchpad_gesture,
            node::commands::node_status,
            node::commands::node_config,
            node::commands::node_set_module,
            node::commands::node_set_name,
            node::commands::node_set_mic_gate,
            node::commands::node_set_calls,
            node::commands::node_set_bluetooth_standby,
            node::commands::node_apply_config_edit,
            node::commands::node_set_stream_volume,
            node::commands::node_pair_show,
            node::commands::node_pair_enter,
            node::commands::node_pair_cancel,
            node::commands::node_pairing,
            node::commands::node_forget,
            node::commands::node_start,
            node::commands::gallery_list,
            node::commands::gallery_thumbnail,
            node::commands::gallery_read,
            mobile::commands::mobile_setup,
            mobile::commands::mobile_install_build,
            mobile::commands::mobile_request,
            mobile::commands::mobile_restart,
            mobile::commands::mobile_system_bars,
            mobile::commands::phone_audio_targets,
            mobile::commands::phone_audio_route,
            mobile::commands::phone_audio_state,
            mobile::commands::phone_audio_volume,
            mobile::commands::phone_screen_targets,
            mobile::commands::phone_screen_share,
            mobile::commands::phone_screen_answer,
        ]);

    let result = builder.run(context());
    if let Err(error) = result {
        // Chromium that cannot start must not leave the machine without a
        // window: back to the platform webview, told to stay on it.
        #[cfg(all(desktop, feature = "cef"))]
        if let Some(exe) = renderer::webkit_exe() {
            tracing::warn!(%error, "Chromium did not start; using the platform webview");
            // SAFETY: the runtime has shut down; no other thread reads the
            // environment any more.
            unsafe { std::env::set_var(renderer::ENV, "webkit") };
            let error = renderer::hand_over(&exe);
            panic!("could not start the platform webview either: {error}");
        }
        panic!("error while running tauri application: {error}");
    }
}

/// The app's context. On Linux the main window is not made at start but when
/// it is asked for (see `tray`): a start for a call must not show it.
fn context() -> tauri::Context {
    let context = tauri::generate_context!();
    #[cfg(target_os = "linux")]
    let context = {
        let mut context = context;
        for window in &mut context.config_mut().app.windows {
            if window.label == "main" {
                window.create = false;
            }
        }
        context
    };
    context
}

#[cfg(feature = "cef")]
fn cef_runtime() -> tauri_runtime_cef::Cef {
    // A debug build runs from the target directory, where no setuid
    // `chrome-sandbox` can be, and Ubuntu holds back the user namespaces
    // Chromium would use instead, so there it runs unsandboxed rather than
    // aborting. A release build is only handed over to when a sandbox is
    // there (`renderer::chromium_sandbox_usable`).
    tauri_runtime_cef::Cef::default().sandbox(if cfg!(debug_assertions) {
        tauri_runtime_cef::SandboxPolicy::Disabled
    } else {
        tauri_runtime_cef::SandboxPolicy::Auto
    })
}

#[cfg(all(test, desktop))]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_page_argument_opens_only_a_plain_page_name() {
        assert_eq!(
            page_route(args(&["cursio-app", "--page", "messages"])),
            Some("/messages".into())
        );
        assert_eq!(page_route(args(&["cursio-app", "--page", "../etc"])), None);
        assert_eq!(page_route(args(&["cursio-app", "--page"])), None);
        assert_eq!(page_route(args(&["cursio-app", "--hidden"])), None);
    }

    #[test]
    fn a_peer_argument_opens_the_page_on_that_machine_encoded_whole() {
        assert_eq!(
            page_route(args(&[
                "cursio-app",
                "--page",
                "gallery",
                "--peer",
                "POCO X7 Pro"
            ])),
            Some("/gallery?peer=POCO%20X7%20Pro".into())
        );
        assert_eq!(
            page_route(args(&[
                "cursio-app",
                "--peer",
                "a&page=x",
                "--page",
                "gallery"
            ])),
            Some("/gallery?peer=a%26page%3Dx".into())
        );
    }
}
