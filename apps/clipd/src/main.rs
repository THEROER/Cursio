//! `clipd` — clipboard history for Linux.
//!
//! What Win+V is on Windows and what GNOME does not ship: everything you copy
//! is remembered, and a hotkey brings up a searchable list to paste any of it
//! back. It runs standalone — no pairing, no network, no other component.
//!
//! It is built on `clipboard-core`, which also backs Cursio's cross-machine
//! clipboard sharing. The two never talk to each other: the system clipboard is
//! the meeting point, so when both run, content arriving from another machine
//! shows up here as an ordinary copy, with no integration code at all.
//!
//! ```text
//! clipd daemon          # watch the clipboard and record what's copied
//! clipd list            # print the history
//! clipd pick            # choose an entry (fuzzel/rofi/…) and put it back
//! clipd copy <id>       # put a specific entry back on the clipboard
//! clipd install-hotkey  # bind the picker to a key under GNOME
//! clipd install-service # run the daemon automatically at login
//! ```

mod hotkey;
mod ipc;
mod picker;
mod service;
mod store;
mod ui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use clipboard_core::Selection;
use gtk::glib;
use store::Store;

#[derive(Parser)]
#[command(name = "clipd", about = "Clipboard history for Linux", version)]
struct Cli {
    /// Where history is kept. Defaults to the XDG data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Watch the clipboard and record everything copied.
    Daemon {
        /// Maximum items to keep.
        #[arg(long, default_value_t = store::DEFAULT_MAX_ITEMS)]
        max_items: usize,
        /// Maximum total bytes to keep.
        #[arg(long, default_value_t = store::DEFAULT_MAX_BYTES)]
        max_bytes: u64,
    },
    /// Print the history, newest first.
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Only items whose preview contains this text.
        #[arg(long)]
        search: Option<String>,
    },
    /// Open the popup (or close it, if it is already open).
    Pick {
        /// Use an external picker instead of the built-in popup, e.g. for a
        /// session with no running daemon.
        #[arg(long)]
        with: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Put a specific history entry back on the clipboard.
    Copy { id: i64 },
    /// Pin an entry so it is never evicted.
    Pin { id: i64 },
    /// Unpin an entry.
    Unpin { id: i64 },
    /// Delete one entry.
    Remove { id: i64 },
    /// Delete everything except pinned entries.
    Clear,
    /// Show where history is stored and how big it is.
    Status,
    /// Check whether the daemon is running.
    Ping,
    /// Install a systemd user service so the daemon starts at login.
    InstallService,
    /// Remove the systemd user service.
    UninstallService,
    /// Bind the picker to a keyboard shortcut (GNOME).
    InstallHotkey {
        /// The key combination, in GNOME's syntax.
        #[arg(long, default_value = "<Super><Shift>v")]
        binding: String,
        /// Take the binding even if GNOME already uses it for something else.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

fn main() -> Result<()> {
    // Console (info) + rotating file (debug) logging, shared with `cursio`.
    // `_log` must live for the whole run — dropping it flushes the log file.
    let _log = cursio_log::init("clipd");

    let cli = Cli::parse();
    // Only a *explicitly* chosen directory is baked into the service unit;
    // otherwise the daemon resolves the default itself at run time.
    let explicit_data_dir = cli.data_dir.clone();
    let dir = match cli.data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };

    match cli.command {
        Command::Daemon {
            max_items,
            max_bytes,
        } => daemon(&dir, max_items, max_bytes),
        Command::List { limit, search } => list(&dir, limit, search.as_deref()),
        Command::Pick { with, limit } => pick(&dir, with.as_deref(), limit),
        Command::Ping => {
            println!("{}", ipc::send(ipc::Request::Ping)?);
            Ok(())
        }
        Command::Copy { id } => copy(&dir, id),
        Command::Pin { id } => Store::open(&dir)?.set_pinned(id, true),
        Command::Unpin { id } => Store::open(&dir)?.set_pinned(id, false),
        Command::Remove { id } => Store::open(&dir)?.remove(id),
        Command::Clear => Store::open(&dir)?.clear(),
        Command::Status => status(&dir),
        Command::InstallService => service::install(explicit_data_dir.as_deref()),
        Command::UninstallService => service::uninstall(),
        Command::InstallHotkey { binding, force } => hotkey::install(&binding, force),
    }
}

fn default_data_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "clipd")
        .context("could not determine a data directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Watch the clipboard, record what is copied, and host the popup.
///
/// Both jobs live in one process on purpose. The window has to be built ahead
/// of time for the hotkey to feel instant (see [`ui`]), and the daemon is
/// already running, so it hosts the window and the hotkey just sends it a line
/// over the socket (see [`ipc`]).
///
/// GTK owns the main loop, so the clipboard watch runs on its own thread and
/// hands changes back through a channel. The store is only ever touched from
/// the GTK thread — hence `Rc<RefCell<…>>` rather than a mutex.
///
/// Content is read **eagerly** (`read_loaded`), the opposite of the rest of the
/// system. X11 clipboard content is served by the process that copied it, so it
/// vanishes when that application exits; a history holding a lazy reference
/// would find the bytes gone precisely when the user reaches for them.
fn daemon(dir: &Path, max_items: usize, max_bytes: u64) -> Result<()> {
    use adw::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    let store = Store::open(dir)?.with_limits(max_items, max_bytes);
    let (count, bytes) = store.stats()?;
    tracing::info!(
        dir = %dir.display(),
        items = count,
        size = %store::human_bytes(bytes),
        "clipd watching the clipboard"
    );

    // libadwaita must be initialised before any of its types are constructed;
    // without this the application object exists but never activates.
    adw::init().map_err(|e| anyhow::anyhow!("initialising libadwaita: {e}"))?;

    let app = adw::Application::builder()
        .application_id(ui::APP_ID)
        .build();

    let store = Rc::new(RefCell::new(store));
    let data_dir = dir.to_path_buf();

    app.connect_activate(move |app| {
        // Keep the process alive with no window on screen. GTK quits once the
        // last window closes, and this popup spends nearly all its time hidden
        // — without this guard the daemon would exit the first time the user
        // dismissed it. Leaked deliberately: the hold must outlive this
        // closure, for the life of the daemon.
        std::mem::forget(app.hold());

        let popup = ui::build(app, store.clone(), {
            let data_dir = data_dir.clone();
            move |id| {
                if let Err(e) = copy(&data_dir, id) {
                    tracing::warn!(id, error = %e, "could not put the entry back on the clipboard");
                }
            }
        });

        spawn_clipboard_watch(store.clone());

        if let Err(e) = spawn_ipc(popup) {
            tracing::error!(error = %e, "no command socket; the hotkey will not work");
        }
    });

    // GTK parses argv itself and would choke on our subcommand.
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// Watch the clipboard on a background thread, recording changes on the GTK one.
///
/// The watch cannot run on the GTK thread — it blocks — and the store must not
/// be touched off it, so changes cross back through a channel that GTK drains
/// in its own loop.
fn spawn_clipboard_watch(store: std::rc::Rc<std::cell::RefCell<Store>>) {
    // The captured content rides the channel itself. An earlier version parked
    // it in a `thread_local` and sent only a wakeup — which silently dropped
    // everything, because a thread-local is per *thread*: the watch thread
    // filled its own copy and the GTK thread always read an empty one.
    let (tx, rx) = std::sync::mpsc::channel::<clipboard_core::ClipboardContent>();

    std::thread::spawn(move || {
        let mut clipboard = match clipboard_core::open() {
            Ok(clipboard) => clipboard,
            Err(e) => {
                tracing::error!(error = %e, "cannot read the clipboard; history will stay empty");
                return;
            }
        };
        let mut watch = match clipboard_core::watch(Selection::Clipboard) {
            Ok(watch) => watch,
            Err(e) => {
                tracing::error!(error = %e, "cannot watch the clipboard");
                return;
            }
        };

        // Record whatever is already on the clipboard, so a copy made before
        // the daemon started isn't missed.
        let mut pending: Vec<clipboard_core::ClipboardContent> = Vec::new();
        if let Ok(content) = clipboard.read_loaded(Selection::Clipboard) {
            pending.push(content);
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("building the watch runtime");

        runtime.block_on(async move {
            for content in pending.drain(..) {
                let _ = tx.send(content);
            }

            while let Some(change) = watch.changes.recv().await {
                match clipboard.read_loaded(change.selection) {
                    Ok(content) => {
                        if tx.send(content).is_err() {
                            break; // GTK side gone
                        }
                    }
                    // An empty clipboard is routine (it was cleared); other
                    // failures are worth a line but must not stop the watch.
                    Err(clipboard_core::ClipboardError::Empty) => {}
                    Err(e) => tracing::warn!(error = %e, "could not read the clipboard"),
                }
            }
        });
    });

    // Drain captures on the GTK thread, where the store lives.
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        while let Ok(content) = rx.try_recv() {
            match store.borrow_mut().insert(&content) {
                Ok(id) => tracing::debug!(id, "recorded a copy"),
                Err(e) => tracing::warn!(error = %e, "could not record a copy"),
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Listen for hotkey commands and drive the popup from the GTK thread.
fn spawn_ipc(popup: ui::Popup) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<ipc::Request>();

    ipc::listen(move |request| {
        // Widgets may only be touched on the GTK thread, so the socket thread
        // just forwards the request and answers immediately.
        if tx.send(request).is_err() {
            return "error: daemon is shutting down".to_string();
        }
        "ok".to_string()
    })?;

    // 120 ms is imperceptible for a keypress but wakes the loop far less often
    // than the 50 ms it used to, which showed up as steady idle CPU.
    glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
        while let Ok(request) = rx.try_recv() {
            match request {
                ipc::Request::Show => popup.show(),
                ipc::Request::Hide => popup.hide(),
                ipc::Request::Toggle => popup.toggle(),
                ipc::Request::Ping => {}
            }
        }
        glib::ControlFlow::Continue
    });

    Ok(())
}

fn list(dir: &Path, limit: usize, search: Option<&str>) -> Result<()> {
    let store = Store::open(dir)?;
    let items = match search {
        Some(needle) => store.search(needle, limit)?,
        None => store.recent(limit)?,
    };

    if items.is_empty() {
        println!("(history is empty)");
        return Ok(());
    }
    for item in items {
        println!(
            "{:>5}  {}{}  {:>9}  {}",
            item.id,
            item.kind.marker(),
            if item.pinned { "*" } else { " " },
            store::human_bytes(item.size),
            item.preview
        );
    }
    Ok(())
}

/// Open the popup.
///
/// The window lives in the daemon, so this only sends it a line — which is what
/// makes the hotkey feel instant (see [`ipc`]). With `--with`, or when no
/// daemon is running, it falls back to driving an external picker in this
/// process, so `clipd pick` still works in a bare session.
fn pick(dir: &Path, with: Option<&str>, limit: usize) -> Result<()> {
    if with.is_none() {
        match ipc::send(ipc::Request::Toggle) {
            Ok(_) => return Ok(()),
            Err(e) => tracing::debug!(error = %e, "no daemon; using an external picker"),
        }
    }

    let store = Store::open(dir)?;
    let items = store.recent(limit)?;
    if items.is_empty() {
        anyhow::bail!("history is empty");
    }

    match picker::choose(&items, with)? {
        Some(id) => copy(dir, id),
        // The user dismissed the picker; that is a normal outcome.
        None => Ok(()),
    }
}

fn copy(dir: &Path, id: i64) -> Result<()> {
    let store = Store::open(dir)?;
    let content = store
        .load(id)
        .with_context(|| format!("loading history item {id}"))?;

    let mut clipboard = clipboard_core::open().context("opening the clipboard")?;
    clipboard
        .write(Selection::Clipboard, &content)
        .context("writing to the clipboard")?;

    // The backend hands ownership to a thread that must outlive this call, or
    // the clipboard would be empty the moment the process exits (X11 serves
    // content from its owner). Staying alive briefly lets the paste target
    // fetch it; a desktop paste happens well inside this window.
    std::thread::sleep(std::time::Duration::from_millis(200));
    Ok(())
}

fn status(dir: &Path) -> Result<()> {
    let store = Store::open(dir)?;
    let (count, bytes) = store.stats()?;
    println!("history:  {}", dir.display());
    println!("items:    {count}");
    println!("size:     {}", store::human_bytes(bytes));
    Ok(())
}
