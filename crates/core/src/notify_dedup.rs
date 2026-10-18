//! Not showing a phone's copy of what a desktop client already showed.
//!
//! Telegram, WhatsApp, Discord and their like run a client on the computer
//! too, and their servers push every message to every client at once. The
//! user at the desk got the desktop client's notification and then Cursio's
//! copy of the phone's, a second or so apart, for every message.
//!
//! So the machine that shows the phone's notification watches its own
//! notification server, and a copy that matches something a local client
//! showed moments earlier — same app, same sender, same text — is dropped.
//! Order is not fixed: the two pushes race, and the phone's copy also crosses
//! the Wi-Fi, so either may land first.
//!
//! - **The native one came first.** Recent native notifications are kept for
//!   [`LOOKBACK`]; a copy that matches one is not shown.
//! - **The copy came first.** For an app this machine has not yet seen
//!   duplicate, the copy is shown at once and watched for [`WAIT`]; a native
//!   match arriving inside that takes the copy down again. An app that has
//!   matched [`LEARN_AFTER`] times is known to have a client here, and its
//!   copies are held back for [`WAIT`] instead of flashing up and away. If no
//!   native one arrives — the client is closed today — the copy is shown
//!   after all: holding back must never become losing.
//!
//! What this machine learned is kept in the config directory and forgotten
//! after [`FORGET_AFTER`] without a match, so an app uninstalled from the
//! computer stops delaying its copies.
//!
//! Calls are never held: a second of ringing nobody can answer is worse than
//! a duplicate.
//!
//! Matching is deliberately loose about form and strict about content. A
//! desktop client words a notification its own way — "Alice" vs "Alice
//! (Work)", an emoji the phone dropped, the chat's name as a prefix — so text
//! is compared after reducing it to lowercase letters and digits, and a title
//! or body that contains the other counts as the same. The app is matched by
//! name against the native notification's app name and desktop entry; a copy
//! of an app shown by a browser ("Google Chrome" showing WhatsApp Web) can
//! still match, but only on near-identical text.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cursio_protocol::notify::{Notification, NotifyKind};
use serde::{Deserialize, Serialize};

/// How long a copy waits for, or is watched for, its native twin. Both come
/// from one server push to two clients, which arrive within a few hundred
/// milliseconds of each other; the phone's copy then crosses the link. Two
/// seconds covers a desktop client that is slow to draw while still reading
/// as "now" when the copy is the only one coming.
pub const WAIT: Duration = Duration::from_secs(2);

/// How far back a native notification still counts as the twin of a copy
/// arriving now. Longer than [`WAIT`]: the phone side is the slower path
/// (push to the phone, the listener, then the link), and a phone that was
/// dozing delivers late.
pub const LOOKBACK: Duration = Duration::from_secs(10);

/// Matches before an app is taken to have a client on this machine. One
/// could be a coincidence of two apps showing the same text.
pub const LEARN_AFTER: u32 = 2;

/// Without a match for this long, an app is no longer taken to have a client
/// here.
pub const FORGET_AFTER: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// How long Cursio's own notifications are remembered, so the watcher does not
/// take them for a client's.
const OWN_FOR: Duration = Duration::from_secs(5);

/// One notification some program on this machine asked the server to show.
#[derive(Debug, Clone)]
pub struct NativeNote {
    pub app: String,
    /// The `desktop-entry` hint, or the sending app's id where the API names
    /// it (GTK and portal notifications).
    pub entry: Option<String>,
    pub title: String,
    pub body: String,
}

/// What to do with a copy from the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// A native twin is already on screen.
    Duplicate,
    /// Show it, and watch for a twin until the given moment.
    ShowWatching(Instant),
    /// Hold it until the given moment, then show it unless a twin came.
    Hold(Instant),
    /// Show it; there is nothing to compare it with.
    Show,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Learned {
    matches: u32,
    /// Unix seconds of the last match.
    last: u64,
}

pub struct Dedup {
    natives: VecDeque<(NativeNote, Instant)>,
    own: VecDeque<(String, Instant)>,
    learned: HashMap<String, Learned>,
    /// Where `learned` is kept; `None` keeps it in memory only.
    store: Option<PathBuf>,
}

/// Whether anything feeds [`Dedup::native`]. Without a watcher every copy is
/// shown at once, as before this existed.
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Say that a watcher feeds this machine's native notifications.
pub fn set_watching(on: bool) {
    WATCHING.store(on, Ordering::Relaxed);
}

fn watching() -> bool {
    WATCHING.load(Ordering::Relaxed)
}

/// The machine's one record, shared by the watcher and every link.
pub fn dedup() -> std::sync::MutexGuard<'static, Dedup> {
    static DEDUP: OnceLock<Mutex<Dedup>> = OnceLock::new();
    DEDUP
        .get_or_init(|| {
            // As `identity.rs` resolves it: an explicit root, else the platform's.
            let store = cursio_protocol::paths::config_dir()
                .or_else(|| {
                    directories::ProjectDirs::from("dev", "cursio", "cursio")
                        .map(|d| d.config_dir().to_path_buf())
                })
                .map(|d| d.join("notify-native-apps.json"));
            Mutex::new(Dedup::load(store))
        })
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

impl Dedup {
    fn load(store: Option<PathBuf>) -> Self {
        let learned = store
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| match serde_json::from_slice(&bytes) {
                Ok(learned) => Some(learned),
                Err(e) => {
                    tracing::warn!(error = %e, "ignoring an unreadable record of desktop notification clients");
                    None
                }
            })
            .unwrap_or_default();
        Dedup {
            natives: VecDeque::new(),
            own: VecDeque::new(),
            learned,
            store,
        }
    }

    /// Cursio itself is about to show this; the watcher will see it too.
    pub fn own(&mut self, title: &str, body: &str, now: Instant) {
        self.own.push_back((fingerprint(title, body), now));
        while self
            .own
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) > OWN_FOR)
        {
            self.own.pop_front();
        }
    }

    /// The watcher saw a program ask for this notification.
    pub fn native(&mut self, note: NativeNote, now: Instant) {
        let print = fingerprint(&note.title, &note.body);
        if self
            .own
            .iter()
            .any(|(p, at)| *p == print && now.duration_since(*at) <= OWN_FOR)
        {
            return;
        }
        self.natives.push_back((note, now));
        while self
            .natives
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) > LOOKBACK)
        {
            self.natives.pop_front();
        }
    }

    /// What to do with a copy arriving now.
    pub fn decide(&mut self, n: &Notification, now: Instant) -> Decision {
        if n.kind == NotifyKind::Call || !watching() {
            return Decision::Show;
        }
        if self.twin_since(n, now.checked_sub(LOOKBACK).unwrap_or(now)) {
            self.matched(&n.app_id);
            return Decision::Duplicate;
        }
        if self.knows(&n.app_id) {
            Decision::Hold(now + WAIT)
        } else {
            Decision::ShowWatching(now + WAIT)
        }
    }

    /// Whether a twin of a copy has arrived since it was decided on; counts
    /// it when one has.
    pub fn arrived(&mut self, n: &Notification, since: Instant) -> bool {
        let found = self.twin_since(n, since);
        if found {
            self.matched(&n.app_id);
        }
        found
    }

    fn twin_since(&self, n: &Notification, since: Instant) -> bool {
        self.natives
            .iter()
            .any(|(note, at)| *at >= since && same(n, note))
    }

    fn knows(&self, app: &str) -> bool {
        self.learned.get(app).is_some_and(|l| {
            l.matches >= LEARN_AFTER && unix_now().saturating_sub(l.last) < FORGET_AFTER.as_secs()
        })
    }

    fn matched(&mut self, app: &str) {
        let now = unix_now();
        let entry = self.learned.entry(app.to_string()).or_default();
        // A long silence starts the count over rather than resuming it.
        if now.saturating_sub(entry.last) >= FORGET_AFTER.as_secs() {
            entry.matches = 0;
        }
        entry.matches = entry.matches.saturating_add(1);
        entry.last = now;
        if entry.matches == LEARN_AFTER {
            tracing::info!(
                app,
                "this app has a client on this machine; holding its phone copies back briefly"
            );
        }
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.store else { return };
        let written = serde_json::to_vec_pretty(&self.learned)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(path, bytes)
            });
        if let Err(e) = written {
            tracing::warn!(error = %e, path = %path.display(), "cannot keep which apps have a client here");
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lowercase letters and digits, words separated by single spaces: what is
/// left of a text once emoji, punctuation and spacing are set aside.
fn norm(s: &str) -> String {
    let kept: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fingerprint(title: &str, body: &str) -> String {
    format!("{}\n{}", norm(title), norm(body))
}

/// The same text, worded a little differently: equal, one inside the other,
/// or mostly the same words.
fn similar(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.chars().count() >= 4 && long.contains(short) {
        return true;
    }
    let wa: std::collections::HashSet<&str> = a.split(' ').collect();
    let wb: std::collections::HashSet<&str> = b.split(' ').collect();
    // Every word of one inside the other: a name with the chat added, which
    // is too short for the substring test above.
    if wa.is_subset(&wb) || wb.is_subset(&wa) {
        return true;
    }
    let common = wa.intersection(&wb).count();
    let all = wa.union(&wb).count();
    all > 0 && common * 10 >= all * 6
}

/// Whether `note` names the same app as the phone does, by any word of the
/// app's name (three letters or more) in the native app name or entry.
fn same_app(phone_app: &str, note: &NativeNote) -> bool {
    let native = norm(&format!(
        "{} {}",
        note.app,
        note.entry.as_deref().unwrap_or_default()
    ));
    norm(phone_app)
        .split(' ')
        .filter(|w| w.chars().count() >= 3)
        .any(|w| native.contains(w))
}

/// Whether a native notification is the twin of a phone's copy.
fn same(n: &Notification, note: &NativeNote) -> bool {
    let (pt, pb) = (norm(&n.title), norm(&n.text));
    let (nt, nb) = (norm(&note.title), norm(&note.body));
    let body = similar(&pb, &nb);
    let title = pt.is_empty() || nt.is_empty() || similar(&pt, &nt);
    if same_app(&n.app_name, note) {
        // The app vouches for it; the words only have to agree.
        return (body && title) || (pb.is_empty() && nb.is_empty() && !pt.is_empty() && pt == nt);
    }
    // Another app showing it (a browser, a wrapper): only near-identical text.
    body && similar(&pt, &nt) && pb.chars().count() >= 8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(app: &str, title: &str, text: &str) -> Notification {
        Notification {
            key: format!("{app}|{title}|{text}"),
            app_id: format!("pkg.{}", norm(app).replace(' ', ".")),
            app_name: app.into(),
            title: title.into(),
            text: text.into(),
            icon: None,
            when_ms: 0,
            kind: NotifyKind::Message,
            actions: vec![],
        }
    }

    fn native(app: &str, title: &str, body: &str) -> NativeNote {
        NativeNote {
            app: app.into(),
            entry: None,
            title: title.into(),
            body: body.into(),
        }
    }

    fn fresh() -> Dedup {
        set_watching(true);
        Dedup::load(None)
    }

    #[test]
    fn telegram_desktop_twin_matches_despite_emoji_and_spacing() {
        let n = phone("Telegram", "Alice", "see you at 8 👋");
        assert!(same(
            &n,
            &native("Telegram Desktop", "Alice", "see you at 8")
        ));
    }

    #[test]
    fn a_different_message_from_the_same_app_is_not_a_twin() {
        let n = phone("Telegram", "Alice", "see you at 8");
        assert!(!same(
            &n,
            &native("Telegram Desktop", "Alice", "running late, sorry")
        ));
    }

    #[test]
    fn a_title_with_the_chat_prefixed_still_matches() {
        let n = phone("Discord", "Bob", "patch is up");
        assert!(same(&n, &native("discord", "Bob (#dev)", "patch is up")));
    }

    #[test]
    fn a_browser_twin_needs_near_identical_text() {
        let n = phone("WhatsApp", "Carol", "dinner tonight at mine?");
        assert!(same(
            &n,
            &native("Google Chrome", "Carol", "dinner tonight at mine?")
        ));
        assert!(!same(
            &phone("WhatsApp", "Carol", "ok"),
            &native("Google Chrome", "Carol", "ok")
        ));
    }

    #[test]
    fn a_native_twin_already_shown_drops_the_copy() {
        let mut d = fresh();
        let now = Instant::now();
        d.native(native("Telegram Desktop", "Alice", "hi"), now);
        let n = phone("Telegram", "Alice", "hi");
        assert_eq!(
            d.decide(&n, now + Duration::from_millis(800)),
            Decision::Duplicate
        );
    }

    #[test]
    fn a_twin_older_than_the_lookback_does_not_count() {
        let mut d = fresh();
        let now = Instant::now();
        d.native(native("Telegram Desktop", "Alice", "hi"), now);
        let n = phone("Telegram", "Alice", "hi");
        assert!(matches!(
            d.decide(&n, now + LOOKBACK + Duration::from_secs(1)),
            Decision::ShowWatching(_)
        ));
    }

    #[test]
    fn an_unknown_app_is_shown_at_once_and_watched() {
        let mut d = fresh();
        let now = Instant::now();
        let n = phone("Telegram", "Alice", "hi");
        assert_eq!(d.decide(&n, now), Decision::ShowWatching(now + WAIT));
        d.native(
            native("Telegram Desktop", "Alice", "hi"),
            now + Duration::from_millis(500),
        );
        assert!(d.arrived(&n, now));
    }

    #[test]
    fn an_app_that_matched_twice_is_held_back_instead() {
        let mut d = fresh();
        let now = Instant::now();
        for i in 0..LEARN_AFTER {
            let at = now + Duration::from_secs(u64::from(i) * 20);
            d.native(native("Telegram Desktop", "Alice", "hi"), at);
            assert_eq!(
                d.decide(&phone("Telegram", "Alice", "hi"), at),
                Decision::Duplicate
            );
        }
        let later = now + Duration::from_secs(100);
        let n = phone("Telegram", "Bob", "new one");
        assert_eq!(d.decide(&n, later), Decision::Hold(later + WAIT));
        assert!(
            !d.arrived(&n, later),
            "with no twin the held copy is shown after all"
        );
    }

    #[test]
    fn calls_are_never_held() {
        let mut d = fresh();
        let mut n = phone("Phone", "Mum", "Incoming call");
        n.kind = NotifyKind::Call;
        d.native(native("Phone", "Mum", "Incoming call"), Instant::now());
        assert_eq!(d.decide(&n, Instant::now()), Decision::Show);
    }

    #[test]
    fn cursios_own_notifications_are_not_taken_for_a_clients() {
        let mut d = fresh();
        let now = Instant::now();
        d.own("Alice", "hi", now);
        d.native(native("Telegram · POCO", "Alice", "hi"), now);
        let n = phone("Telegram", "Alice", "hi");
        assert!(matches!(d.decide(&n, now), Decision::ShowWatching(_)));
    }

    #[test]
    fn a_match_after_a_long_silence_starts_the_count_over() {
        let mut d = fresh();
        d.learned.insert(
            "pkg.telegram".into(),
            Learned {
                matches: 5,
                last: unix_now() - FORGET_AFTER.as_secs() - 60,
            },
        );
        assert!(!d.knows("pkg.telegram"));
        d.matched("pkg.telegram");
        assert_eq!(d.learned["pkg.telegram"].matches, 1);
    }
}
