//! A tray front end for the Cursio daemon.
//!
//! Deliberately not a GUI. It runs `cursio mesh`, puts an icon in the tray, and
//! opens a terminal on the log when that icon is clicked — enough to leave the
//! daemon running without a terminal window parked on the desktop, and to look
//! at what it is doing when something seems wrong. Everything a real interface
//! would do (pairing, layout, which modules run) is still the CLI's job.
//!
//! ## Why the daemon is a child process
//!
//! Every platform wants its tray on the main thread and inside its own event
//! loop: a Win32 message pump, an `NSApplication` run loop, GTK's main loop.
//! Cursio's peer sessions want the main thread too — they are deliberately
//! `!Send` and run on a `LocalSet` (see the mesh design notes). Rather than
//! make one of those give way, the tray spawns the `cursio` binary it sits
//! next to and supervises it. The daemon stays exactly the thing that is
//! already tested, and a crash in the tray cannot take the mesh down with it.
//!
//! Arguments are passed straight through, so this is `cursio mesh` with an
//! icon:
//!
//! ```sh
//! cursio-tray --send-input --at 1,0
//! ```

mod icon;
mod logs;

use std::process::{Child, Command};

use anyhow::{Context, Result};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

/// Pixels square. Large enough that a 2x display has real detail to scale from,
/// and small enough that drawing it is free.
const ICON_SIZE: u32 = 64;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut daemon = spawn_daemon(&args)?;

    // Built before the event loop so a failure here is a plain error on stderr
    // rather than something swallowed by a loop that has already started.
    let drawn = icon::draw(ICON_SIZE);
    let image = tray_icon::Icon::from_rgba(drawn.rgba, drawn.width, drawn.height)
        .context("building the tray icon")?;

    let menu = Menu::new();
    let show_logs = MenuItem::new("Show logs", true, None);
    let quit = MenuItem::new("Quit Cursio", true, None);
    menu.append_items(&[&show_logs, &PredefinedMenuItem::separator(), &quit])?;

    let event_loop = EventLoopBuilder::new().build();

    // Held for as long as the loop runs: dropping it removes the icon.
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Cursio")
        .with_icon(image)
        .build()
        .context("adding the tray icon")?;

    let menu_events = MenuEvent::receiver();
    let tray_events = TrayIconEvent::receiver();
    let show_logs_id = show_logs.id().clone();
    let quit_id = quit.id().clone();

    event_loop.run(move |_event, _target, control_flow| {
        // Polled rather than waited on: the daemon is checked on every turn, so
        // a crash shows up as the tray exiting rather than as an icon that
        // stays put with nothing behind it.
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(200),
        );

        if let Ok(event) = menu_events.try_recv() {
            if event.id == show_logs_id {
                show_logs_now();
            } else if event.id == quit_id {
                stop_daemon(&mut daemon);
                *control_flow = ControlFlow::Exit;
                return;
            }
        }

        // A left click opens the log directly. The menu still has the entry,
        // because on Linux a StatusNotifierItem's activation is at the panel's
        // discretion and some panels only ever show the menu.
        if let Ok(TrayIconEvent::Click { button, .. }) = tray_events.try_recv() {
            if button == tray_icon::MouseButton::Left {
                show_logs_now();
            }
        }

        match daemon.try_wait() {
            Ok(Some(status)) => {
                tracing::error!(?status, "the cursio daemon exited");
                eprintln!("cursio exited ({status}); the tray is closing too");
                *control_flow = ControlFlow::Exit;
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "cannot check on the daemon"),
        }
    })
}

/// Start `cursio mesh`, passing our own arguments through.
///
/// Looked for beside this binary first: a build tree and an installed tree both
/// put the two together, and finding the wrong `cursio` on `PATH` would run a
/// different version than the one just built.
fn spawn_daemon(args: &[String]) -> Result<Child> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join(daemon_name())))
        .filter(|path| path.exists());
    let program = beside.unwrap_or_else(|| daemon_name().into());

    Command::new(&program)
        .arg("mesh")
        .args(args)
        .spawn()
        .with_context(|| {
            format!(
                "starting {} — build it with `cargo build -p cursio-core`",
                program.display()
            )
        })
}

const fn daemon_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "cursio.exe"
    } else {
        "cursio"
    }
}

/// Open the console, reporting a failure where the user can see it.
///
/// A click that does nothing is the worst outcome here: the reason to click is
/// that something already seems wrong.
fn show_logs_now() {
    match logs::newest_log().and_then(|path| logs::open_console(&path)) {
        Ok(()) => {}
        Err(error) => {
            tracing::error!(%error, "could not open the log console");
            eprintln!("could not open the log console: {error:#}");
        }
    }
}

/// Ask the daemon to stop, then wait for it.
///
/// Killed rather than signalled politely, because there is nothing to be polite
/// with on Windows and the daemon's own shutdown path is driven by Ctrl-C,
/// which a child process does not receive from its parent.
fn stop_daemon(daemon: &mut Child) {
    if let Err(error) = daemon.kill() {
        tracing::warn!(%error, "could not stop the daemon");
        return;
    }
    // Reaped, so the daemon is really gone before the tray icon disappears —
    // otherwise quitting looks instant while the mesh is still up.
    let _ = daemon.wait();
}
