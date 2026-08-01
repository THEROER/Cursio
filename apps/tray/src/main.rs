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

// No console window. The whole point is to start hidden in the tray, and on
// Windows a console subsystem binary opens one whether or not anything is
// written to it. Diagnostics are the log, which is one click away.
#![windows_subsystem = "windows"]

mod icon;
mod logs;

use std::process::{Child, Command};

use anyhow::{Context, Result};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Pixels square. Large enough that a 2x display has real detail to scale from,
/// and small enough that drawing it is free.
const ICON_SIZE: u32 = 64;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut daemon = Some(spawn_daemon(&args)?);

    let menu = Menu::new();
    let show_logs = MenuItem::new("Show logs", true, None);
    let quit = MenuItem::new("Quit Cursio", true, None);
    menu.append_items(&[&show_logs, &PredefinedMenuItem::separator(), &quit])?;
    let show_logs_id = show_logs.id().clone();
    let quit_id = quit.id().clone();

    let event_loop = build_event_loop();
    let menu_events = MenuEvent::receiver();
    let tray_events = TrayIconEvent::receiver();

    // Built inside the loop rather than before it, because macOS refuses a
    // status item until `NSApplication` has finished launching — created any
    // earlier it simply never appears, with no error anywhere. The other
    // platforms do not care, so there is one construction site rather than a
    // `cfg` around two.
    // Underscore-prefixed because nothing ever reads it: the icon exists for as
    // long as this binding does, and dropping it takes the icon out of the tray.
    let mut _tray: Option<TrayIcon> = None;

    event_loop.run(move |event, _target, control_flow| {
        // Polled rather than waited on: the daemon is checked on every turn, so
        // its death shows up as the tray exiting rather than as an icon that
        // stays put with nothing behind it.
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(250),
        );

        if matches!(event, Event::NewEvents(StartCause::Init)) {
            match build_tray(menu.clone()) {
                Ok(built) => _tray = Some(built),
                Err(error) => {
                    eprintln!("could not add the tray icon: {error:#}");
                    stop_daemon(&mut daemon);
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
        }

        if let Ok(event) = menu_events.try_recv() {
            if event.id == show_logs_id {
                show_logs_now();
            } else if event.id == quit_id {
                stop_daemon(&mut daemon);
                *control_flow = ControlFlow::Exit;
                return;
            }
        }

        if let Ok(TrayIconEvent::Click { button, .. }) = tray_events.try_recv() {
            if button == tray_icon::MouseButton::Left {
                show_logs_now();
            }
        }

        if let Some(child) = daemon.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    eprintln!("cursio exited ({status}); the tray is closing too");
                    daemon = None;
                    *control_flow = ControlFlow::Exit;
                }
                Ok(None) => {}
                Err(error) => eprintln!("cannot check on the daemon: {error}"),
            }
        }
    })
}

/// An event loop that does not make this a foreground application.
///
/// On macOS a process that opens a window — and a status item counts — becomes
/// a Dock application with a menu bar of its own unless it says otherwise.
/// `Accessory` is what every tray-only application uses.
fn build_event_loop() -> tao::event_loop::EventLoop<()> {
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut event_loop = EventLoopBuilder::new().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    event_loop
}

/// Put the icon in the tray.
fn build_tray(menu: Menu) -> Result<TrayIcon> {
    let drawn = icon::draw(ICON_SIZE);
    let image = tray_icon::Icon::from_rgba(drawn.rgba, drawn.width, drawn.height)
        .context("building the tray icon")?;

    let builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Cursio — click for logs")
        .with_icon(image);
    // A left click must open the console, so it has to reach us as an event
    // instead of dropping the menu down. The menu is still there on a right
    // click, which is where every platform's convention puts it anyway.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let builder = builder.with_menu_on_left_click(false);

    builder.build().context("adding the tray icon")
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

    let mut command = Command::new(&program);
    command.arg("mesh").args(args);
    // Detached from whatever started us. The daemon writes to the rotating log
    // regardless, so inheriting these buys nothing and costs the thing the tray
    // exists for: on macOS the output lands in the terminal the tray was
    // launched from, which is exactly the window the user wanted gone.
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The daemon is a console binary, and a console binary started from a
    // windowed one is given a console window of its own. Suppressing it is the
    // difference between "starts hidden in the tray" and "starts hidden in the
    // tray, next to a black rectangle".
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command.spawn().with_context(|| {
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
    if let Err(error) = logs::newest_log().and_then(|path| logs::open_console(&path)) {
        eprintln!("could not open the log console: {error:#}");
    }
}

/// Ask the daemon to stop, then wait for it.
///
/// Killed rather than signalled politely, because there is nothing to be polite
/// with on Windows and the daemon's own shutdown path is driven by Ctrl-C,
/// which a child process does not receive from its parent.
fn stop_daemon(daemon: &mut Option<Child>) {
    let Some(child) = daemon.as_mut() else {
        return;
    };
    if child.kill().is_ok() {
        // Reaped, so the daemon is really gone before the icon disappears —
        // otherwise quitting looks instant while the mesh is still up.
        let _ = child.wait();
    }
    *daemon = None;
}
