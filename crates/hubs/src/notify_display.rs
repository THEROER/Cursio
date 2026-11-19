//! Showing a peer's notifications through the desktop's own notification
//! server (`org.freedesktop.Notifications`), so they look, stack and are
//! dismissed like every other notification on this machine.
//!
//! The specification carries buttons but not a reply field, and GNOME
//! implements no extension for one. A reply button therefore asks for the
//! text in a small `zenity` dialog of its own when pressed; without zenity a
//! reply button is left off rather than shown and broken.
//!
//! Icons go by path: the hint that carries pixels wants them decoded to raw
//! RGBA, and the server reads a PNG file itself. The files sit in the user's
//! runtime directory, named by content, so one per app however many
//! notifications it sends.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cursio_protocol::notify::{Notification, NotifyKind};
use cursio_protocol::PeerId;
use tokio::sync::mpsc;
use zbus::blocking::Connection;
use zbus::zvariant::Value;

use crate::notify_hub::{DisplayEvent, NotifyDisplay};

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// The specification's reason for a notification the user dismissed. The
/// others — expired, closed by a call, undefined — are not the user acting.
const CLOSED_BY_USER: u32 = 2;

struct Shown {
    peer: PeerId,
    key: String,
    title: String,
    /// Which buttons take text, in the phone's order.
    replies: Vec<bool>,
    events: mpsc::UnboundedSender<DisplayEvent>,
}

#[derive(Default)]
struct State {
    /// The server's id for each notification shown, and what it stands for.
    by_id: HashMap<u32, Shown>,
}

impl State {
    fn id_of(&self, peer: PeerId, key: &str) -> Option<u32> {
        self.by_id
            .iter()
            .find(|(_, s)| s.peer == peer && s.key == key)
            .map(|(id, _)| *id)
    }
}

/// Every call goes through [`crate::session_bus`]: `show` and `close` run on
/// a module's host thread, inside the runtime, where a blocking zbus call
/// deadlocks.
pub struct FdoDisplay {
    state: Arc<Mutex<State>>,
    zenity: bool,
}

impl FdoDisplay {
    /// Start listening for what the user does with what is shown. `None`, with
    /// a warning, where there is no session bus.
    pub fn start() -> Option<Arc<Self>> {
        let Some(bus) = crate::session_bus::connection() else {
            tracing::warn!("no session bus; a phone's notifications cannot be shown here");
            return None;
        };
        let state = Arc::new(Mutex::new(State::default()));
        let listener = {
            let state = Arc::clone(&state);
            std::thread::Builder::new()
                .name("notify-signals".into())
                .spawn(move || listen(&bus, &state))
        };
        if let Err(e) = listener {
            tracing::warn!(error = %e, "cannot listen for notification buttons; showing none");
            return None;
        }
        if let Err(e) = std::thread::Builder::new()
            .name("notify-watch".into())
            .spawn(watch_native)
        {
            tracing::warn!(error = %e, "cannot watch this machine's notifications; a phone's copies may duplicate a client's");
        }
        Some(Arc::new(FdoDisplay {
            state,
            zenity: which("zenity"),
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl NotifyDisplay for FdoDisplay {
    fn show(
        &self,
        peer: PeerId,
        peer_name: &str,
        n: &Notification,
        events: mpsc::UnboundedSender<DisplayEvent>,
    ) {
        let replaces = self.lock().id_of(peer, &n.key).unwrap_or(0);
        let icon = n
            .icon
            .as_deref()
            .and_then(icon_path)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut actions: Vec<String> = Vec::new();
        let mut replies = Vec::new();
        for (i, action) in n.actions.iter().enumerate() {
            replies.push(action.reply);
            if action.reply && !self.zenity {
                continue;
            }
            actions.push(i.to_string());
            actions.push(action.title.clone());
        }
        let urgency: u8 = if n.kind == NotifyKind::Call { 2 } else { 1 };
        let message = n.kind == NotifyKind::Message;
        let summary = if n.title.is_empty() {
            n.app_name.clone()
        } else {
            n.title.clone()
        };
        let app = format!("{} · {}", n.app_name, peer_name);
        let body = n.text.clone();
        // A call stays until the phone withdraws it; anything else follows
        // the server's own timing.
        let expire: i32 = if n.kind == NotifyKind::Call { 0 } else { -1 };
        let title = summary.clone();
        crate::notify_dedup::dedup().own(&title, &body, std::time::Instant::now());
        let sent = crate::session_bus::with_bus(move |bus| {
            let mut hints: HashMap<&str, Value> = HashMap::new();
            hints.insert("urgency", Value::U8(urgency));
            if message {
                hints.insert("category", Value::from("im.received"));
            }
            let actions: Vec<&str> = actions.iter().map(String::as_str).collect();
            bus.call_method(
                Some(DEST),
                PATH,
                Some(DEST),
                "Notify",
                &(
                    app.as_str(),
                    replaces,
                    icon.as_str(),
                    title.as_str(),
                    body.as_str(),
                    actions,
                    hints,
                    expire,
                ),
            )
            .and_then(|reply| reply.body().deserialize::<u32>())
        });
        let id: u32 = match sent {
            Some(Ok(id)) => id,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "the desktop refused a phone's notification");
                return;
            }
            None => return,
        };
        let mut state = self.lock();
        if replaces != 0 && replaces != id {
            state.by_id.remove(&replaces);
        }
        state.by_id.insert(
            id,
            Shown {
                peer,
                key: n.key.clone(),
                title: summary,
                replies,
                events,
            },
        );
    }

    fn close(&self, peer: PeerId, key: &str) {
        let id = {
            let mut state = self.lock();
            let id = state.id_of(peer, key);
            if let Some(id) = id {
                state.by_id.remove(&id);
            }
            id
        };
        if let Some(id) = id {
            let closed = crate::session_bus::with_bus(move |bus| {
                bus.call_method(Some(DEST), PATH, Some(DEST), "CloseNotification", &id)
                    .map(drop)
            });
            if let Some(Err(e)) = closed {
                tracing::debug!(error = %e, "could not take a notification down");
            }
        }
    }
}

/// Follow the server's signals for as long as the bus is up.
fn listen(bus: &Connection, state: &Arc<Mutex<State>>) {
    let rule = match zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(DEST)
        .map(|b| b.build())
    {
        Ok(rule) => rule,
        Err(e) => {
            tracing::warn!(error = %e, "cannot match notification signals");
            return;
        }
    };
    let signals = match zbus::blocking::MessageIterator::for_match_rule(rule, bus, None) {
        Ok(signals) => signals,
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for notification signals");
            return;
        }
    };
    for msg in signals.flatten() {
        let header = msg.header();
        let Some(member) = header.member() else {
            continue;
        };
        match member.as_str() {
            "ActionInvoked" => {
                let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() else {
                    continue;
                };
                let Ok(index) = action.parse::<u8>() else {
                    continue;
                };
                let shown = state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .by_id
                    .remove(&id);
                if let Some(shown) = shown {
                    invoked(shown, index);
                }
            }
            "NotificationClosed" => {
                let Ok((id, reason)) = msg.body().deserialize::<(u32, u32)>() else {
                    continue;
                };
                let shown = state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .by_id
                    .remove(&id);
                if let (Some(shown), CLOSED_BY_USER) = (shown, reason) {
                    let _ = shown.events.send(DisplayEvent::Closed { key: shown.key });
                }
            }
            _ => {}
        }
    }
}

/// What every program on this machine asks the notification server to show,
/// for [`crate::notify_dedup`].
///
/// A bus monitor rather than a name of our own: the server is someone else's,
/// and only a monitor sees the calls made to it. A monitor connection may not
/// send anything once it is one — the bus drops it — so it is its own
/// connection, opened here on this thread: a blocking zbus call made from the
/// node's runtime panics. Three ways in are watched, because apps use all of
/// them: the freedesktop `Notify` call, GTK's `AddNotification`, and the
/// portal's, which sandboxed apps go through.
fn watch_native() {
    const RULES: [&str; 3] = [
        "type='method_call',interface='org.freedesktop.Notifications',member='Notify'",
        "type='method_call',interface='org.gtk.Notifications',member='AddNotification'",
        "type='method_call',interface='org.freedesktop.portal.Notification',member='AddNotification'",
    ];
    let bus = match Connection::session().and_then(|bus| {
        bus.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus.Monitoring"),
            "BecomeMonitor",
            &(RULES.to_vec(), 0u32),
        )
        .map(|_| bus)
    }) {
        Ok(bus) => bus,
        Err(e) => {
            tracing::warn!(error = %e, "cannot watch this machine's notifications; a phone's copies may duplicate a client's");
            return;
        }
    };
    crate::notify_dedup::set_watching(true);
    for msg in zbus::blocking::MessageIterator::from(&bus) {
        let Ok(msg) = msg else { break };
        if let Some(note) = native_note(&msg) {
            crate::notify_dedup::dedup().native(note, std::time::Instant::now());
        }
    }
    crate::notify_dedup::set_watching(false);
    tracing::warn!(
        "stopped watching this machine's notifications; a phone's copies may duplicate a client's"
    );
}

/// A notification request, read from whichever of the three calls it is.
fn native_note(msg: &zbus::Message) -> Option<crate::notify_dedup::NativeNote> {
    use crate::notify_dedup::NativeNote;
    type Hints = HashMap<String, zbus::zvariant::OwnedValue>;
    let text = |map: &Hints, key: &str| -> Option<String> {
        map.get(key)
            .and_then(|v| v.downcast_ref::<&str>().ok())
            .map(str::to_string)
    };
    let header = msg.header();
    match (header.interface()?.as_str(), header.member()?.as_str()) {
        ("org.freedesktop.Notifications", "Notify") => {
            let (app, _replaces, _icon, title, body, _actions, hints, _expire): (
                String,
                u32,
                String,
                String,
                String,
                Vec<String>,
                Hints,
                i32,
            ) = msg.body().deserialize().ok()?;
            Some(NativeNote {
                entry: text(&hints, "desktop-entry"),
                app,
                title,
                body,
            })
        }
        ("org.gtk.Notifications", "AddNotification") => {
            let (app_id, _id, n): (String, String, Hints) = msg.body().deserialize().ok()?;
            Some(NativeNote {
                app: app_id.clone(),
                entry: Some(app_id),
                title: text(&n, "title").unwrap_or_default(),
                body: text(&n, "body").unwrap_or_default(),
            })
        }
        ("org.freedesktop.portal.Notification", "AddNotification") => {
            // The portal names no app; the sandbox it came from is not on the
            // message, so this one matches on its text alone.
            let (_id, n): (String, Hints) = msg.body().deserialize().ok()?;
            Some(NativeNote {
                app: String::new(),
                entry: None,
                title: text(&n, "title").unwrap_or_default(),
                body: text(&n, "body").unwrap_or_default(),
            })
        }
        _ => None,
    }
}

/// A button was pressed. A reply asks for its text first, off this thread:
/// the dialog waits on the user, and other notifications' buttons must not.
fn invoked(shown: Shown, action: u8) {
    if !shown.replies.get(action as usize).copied().unwrap_or(false) {
        let _ = shown.events.send(DisplayEvent::Invoked {
            key: shown.key,
            action,
            reply: None,
        });
        return;
    }
    std::thread::spawn(move || {
        let typed = std::process::Command::new("zenity")
            .args(["--entry", "--title", "Cursio", "--text"])
            .arg(format!("Reply to {}", shown.title))
            .output();
        match typed {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                if !text.is_empty() {
                    let _ = shown.events.send(DisplayEvent::Invoked {
                        key: shown.key,
                        action,
                        reply: Some(text),
                    });
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "could not ask for the reply"),
        }
    });
}

/// Where an icon's PNG is kept, written on first use.
fn icon_path(png: &[u8]) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    png.hash(&mut hasher);
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("cursio")
        .join("notify-icons");
    let path = dir.join(format!("{:016x}.png", hasher.finish()));
    if !path.exists() {
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, png)) {
            tracing::debug!(error = %e, "could not keep a notification icon");
            return None;
        }
    }
    Some(path)
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}
