//! The WinRT side of [`super`]: the toast thread, the AppUserModelID and the
//! watcher of this machine's own notifications.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_protocol::notify::Notification;
use cursio_protocol::PeerId;
use tokio::sync::mpsc;
use windows::core::{IInspectable, Interface, Ref, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::{IReference, TypedEventHandler};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::UI::Notifications::Management::{
    UserNotificationListener, UserNotificationListenerAccessStatus,
};
use windows::UI::Notifications::{
    KnownNotificationBindings, NotificationKinds, ToastActivatedEventArgs, ToastDismissalReason,
    ToastDismissedEventArgs, ToastNotification, ToastNotificationManager, ToastNotifier,
};

use super::{action_from, tag, toast_xml, AUMID, GROUP, REPLY_INPUT};
use crate::notify_hub::{DisplayEvent, NotifyDisplay};

/// How often the watcher looks at the Action Center. The listener's change
/// event needs a background task, which only a packaged app has; a second is
/// well inside [`crate::notify_dedup`]'s window.
const WATCH_EVERY: Duration = Duration::from_secs(1);

type Job = Box<dyn FnOnce(&ToastNotifier) + Send>;

/// Shows toasts from one thread of its own; see the module docs.
pub struct ToastDisplay {
    jobs: Mutex<std_mpsc::Sender<Job>>,
}

impl ToastDisplay {
    /// Register the AppUserModelID and start the toast thread. `None`, with a
    /// warning, where toasts cannot be shown at all.
    pub fn start() -> Option<Arc<Self>> {
        let logo = keep_icon(include_bytes!("../../../../app/icons/128x128.png"));
        if let Err(e) = register_aumid(logo.as_deref()) {
            // Shown anyway: on some builds a toast under an unregistered id
            // still appears, only without the name and logo.
            tracing::warn!(error = %e, "cannot register Cursio for notifications; a phone's notifications may not show here");
        }
        let (jobs, inbox) = std_mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("notify-toasts".into())
            .spawn(move || {
                // SAFETY: joins this thread, which nothing else uses, to the
                // multithreaded apartment before any WinRT call on it.
                if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
                    let _ = ready_tx.send(Err(format!("cannot join a COM apartment: {e}")));
                    return;
                }
                let notifier = match ToastNotificationManager::CreateToastNotifierWithId(
                    &HSTRING::from(AUMID),
                ) {
                    Ok(notifier) => notifier,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("no toast notifier: {e}")));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                for job in inbox {
                    job(&notifier);
                }
            });
        match spawned.map_err(|e| e.to_string()).and_then(|_| {
            ready_rx
                .recv()
                .unwrap_or_else(|_| Err("the toast thread ended".into()))
        }) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(error = %e, "a phone's notifications cannot be shown here");
                return None;
            }
        }
        if let Err(e) = std::thread::Builder::new()
            .name("notify-watch".into())
            .spawn(watch_native)
        {
            tracing::warn!(error = %e, "cannot watch this machine's notifications; a phone's copies may duplicate a client's");
        }
        Some(Arc::new(ToastDisplay {
            jobs: Mutex::new(jobs),
        }))
    }

    fn run(&self, job: Job) {
        let sent = self
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(job);
        if sent.is_err() {
            tracing::debug!("the toast thread has ended; dropping a notification");
        }
    }
}

impl NotifyDisplay for ToastDisplay {
    fn show(
        &self,
        peer: PeerId,
        peer_name: &str,
        n: &Notification,
        events: mpsc::UnboundedSender<DisplayEvent>,
    ) {
        let icon = n
            .icon
            .as_deref()
            .and_then(keep_icon)
            .map(|path| file_uri(&path));
        let xml = toast_xml(n, peer_name, icon.as_deref());
        let tag = tag(peer, &n.key);
        let key = n.key.clone();
        let replies: Vec<bool> = n.actions.iter().map(|a| a.reply).collect();
        let title = if n.title.is_empty() {
            &n.app_name
        } else {
            &n.title
        };
        crate::notify_dedup::dedup().own(title, &n.text, std::time::Instant::now());
        self.run(Box::new(move |notifier| {
            if let Err(e) = show(notifier, &xml, &tag, key, replies, events) {
                tracing::warn!(error = %e, "Windows refused a phone's notification");
            }
        }));
    }

    fn close(&self, peer: PeerId, key: &str) {
        let tag = tag(peer, key);
        self.run(Box::new(move |_| {
            let removed = ToastNotificationManager::History().and_then(|history| {
                history.RemoveGroupedTagWithId(
                    &HSTRING::from(tag),
                    &HSTRING::from(GROUP),
                    &HSTRING::from(AUMID),
                )
            });
            if let Err(e) = removed {
                tracing::debug!(error = %e, "could not take a notification down");
            }
        }));
    }
}

fn show(
    notifier: &ToastNotifier,
    xml: &str,
    tag: &str,
    key: String,
    replies: Vec<bool>,
    events: mpsc::UnboundedSender<DisplayEvent>,
) -> windows::core::Result<()> {
    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    toast.SetTag(&HSTRING::from(tag))?;
    toast.SetGroup(&HSTRING::from(GROUP))?;
    {
        let key = key.clone();
        let events = events.clone();
        toast.Activated(&TypedEventHandler::new(
            move |_: Ref<'_, ToastNotification>, args: Ref<'_, IInspectable>| {
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let args: ToastActivatedEventArgs = args.cast()?;
                let Some(action) = action_from(&args.Arguments()?.to_string()) else {
                    return Ok(());
                };
                let reply = if replies.get(action as usize).copied().unwrap_or(false) {
                    match typed_reply(&args) {
                        Some(text) => Some(text),
                        // An empty reply is not sent, as on Linux.
                        None => return Ok(()),
                    }
                } else {
                    None
                };
                let _ = events.send(DisplayEvent::Invoked {
                    key: key.clone(),
                    action,
                    reply,
                });
                Ok(())
            },
        ))?;
    }
    toast.Dismissed(&TypedEventHandler::new(
        move |_: Ref<'_, ToastNotification>, args: Ref<'_, ToastDismissedEventArgs>| {
            // Timing out into the Action Center, or being removed by us, is
            // not the user dismissing it.
            if args.as_ref().map(|a| a.Reason()) == Some(Ok(ToastDismissalReason::UserCanceled)) {
                let _ = events.send(DisplayEvent::Closed { key: key.clone() });
            }
            Ok(())
        },
    ))?;
    notifier.Show(&toast)
}

fn typed_reply(args: &ToastActivatedEventArgs) -> Option<String> {
    let value = args
        .UserInput()
        .ok()?
        .Lookup(&HSTRING::from(REPLY_INPUT))
        .ok()?;
    let text = value
        .cast::<IReference<HSTRING>>()
        .ok()?
        .Value()
        .ok()?
        .to_string();
    (!text.trim().is_empty()).then_some(text)
}

/// Name Cursio and give it the logo, for every toast shown under [`AUMID`].
fn register_aumid(logo: Option<&std::path::Path>) -> windows::core::Result<()> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    let path = HSTRING::from(format!(r"Software\Classes\AppUserModelId\{AUMID}"));
    let mut key = HKEY::default();
    // SAFETY: the out-pointer is a valid HKEY, closed below.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &path,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let set = |name: &str, value: &str| -> windows::core::Result<()> {
        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: REG_SZ data as UTF-16 bytes including the terminator.
        let bytes =
            unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        unsafe { RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(bytes)).ok() }
    };
    let mut result = set("DisplayName", "Cursio");
    if let (Ok(()), Some(logo)) = (&result, logo) {
        result = set("IconUri", &logo.display().to_string());
    }
    // SAFETY: `key` was opened above.
    let _ = unsafe { RegCloseKey(key) };
    result
}

/// Where an icon's PNG is kept, written on first use, named by content so one
/// file serves an app however many notifications it sends.
fn keep_icon(png: &[u8]) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    png.hash(&mut hasher);
    let dir = std::env::var_os("LOCALAPPDATA")
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

fn file_uri(path: &std::path::Path) -> String {
    format!("file:///{}", path.display().to_string().replace('\\', "/"))
}

/// Feed [`crate::notify_dedup`] what other programs show here; see the module
/// docs for why it polls.
fn watch_native() {
    // SAFETY: as on the toast thread; this thread is the watcher's alone.
    if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        tracing::warn!(error = %e, "cannot watch this machine's notifications; a phone's copies may duplicate a client's");
        return;
    }
    let listener = match allowed_listener() {
        Ok(listener) => listener,
        Err(reason) => {
            tracing::warn!(%reason, "cannot read this machine's notifications; a phone's copies of a client's will show twice");
            return;
        }
    };
    crate::notify_dedup::set_watching(true);
    let mut seen: HashSet<u32> = HashSet::new();
    loop {
        let notes = match listener
            .GetNotificationsAsync(NotificationKinds::Toast)
            .and_then(|op| op.get())
        {
            Ok(notes) => notes,
            Err(e) => {
                tracing::warn!(error = %e, "stopped reading this machine's notifications; a phone's copies may duplicate a client's");
                break;
            }
        };
        let mut now_seen = HashSet::new();
        for note in notes {
            let Ok(id) = note.Id() else { continue };
            now_seen.insert(id);
            if seen.contains(&id) {
                continue;
            }
            if let Some(native) = native_note(&note) {
                crate::notify_dedup::dedup().native(native, std::time::Instant::now());
            }
        }
        // Only what is still up is remembered, so the set stays as small as
        // the Action Center.
        seen = now_seen;
        std::thread::sleep(WATCH_EVERY);
    }
    crate::notify_dedup::set_watching(false);
}

fn allowed_listener() -> Result<UserNotificationListener, String> {
    let listener = UserNotificationListener::Current().map_err(|e| e.to_string())?;
    let mut status = listener.GetAccessStatus().map_err(|e| e.to_string())?;
    if status != UserNotificationListenerAccessStatus::Allowed {
        status = listener
            .RequestAccessAsync()
            .and_then(|op| op.get())
            .map_err(|e| e.to_string())?;
    }
    if status == UserNotificationListenerAccessStatus::Allowed {
        Ok(listener)
    } else {
        Err("notification access was not granted (Settings → Privacy → Notifications)".into())
    }
}

fn native_note(
    note: &windows::UI::Notifications::UserNotification,
) -> Option<crate::notify_dedup::NativeNote> {
    let app = note.AppInfo().ok()?;
    let entry = app.AppUserModelId().ok().map(|id| id.to_string());
    if entry.as_deref() == Some(AUMID) {
        return None;
    }
    let binding = note
        .Notification()
        .ok()?
        .Visual()
        .ok()?
        .GetBinding(&KnownNotificationBindings::ToastGeneric().ok()?)
        .ok()?;
    let mut texts = binding
        .GetTextElements()
        .ok()?
        .into_iter()
        .filter_map(|t| t.Text().ok().map(|t| t.to_string()));
    let title = texts.next().unwrap_or_default();
    let body = texts.collect::<Vec<_>>().join("\n");
    Some(crate::notify_dedup::NativeNote {
        app: app
            .DisplayInfo()
            .and_then(|d| d.DisplayName())
            .map(|n| n.to_string())
            .unwrap_or_default(),
        entry,
        title,
        body,
    })
}
