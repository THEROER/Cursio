//! Remote-screen views as the Cursio app's tray sees them.
//!
//! On Windows each peer's view is a window nobody sees, and its source was
//! picked from a notification-area icon of its own — one per peer, beside the
//! app's tray icon, which read as the same program twice. So the app's tray
//! now carries the choice for every view, and the node's icons stand aside
//! while an app is attached to say so ([`Command::TrayAttached`]). A node with
//! no app keeps them: headless, they are the only selector there is.
//!
//! This module is the meeting point: each viewer publishes its view's sources
//! and what it shows, the control socket lists them and hands a choice back.
//! It knows nothing of windows, so the status that carries it builds on a
//! phone too, where there are no views.
//!
//! [`Command::TrayAttached`]: crate::control::Command::TrayAttached

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use cursio_protocol::{ScreenSource, SourceId, SourceKind};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// One peer's view, for a menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenView {
    pub peer: String,
    pub sources: Vec<ScreenChoice>,
    /// What the view shows now, per the peer.
    pub showing: Option<SourceId>,
}

/// One thing a view can show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenChoice {
    pub id: SourceId,
    pub title: String,
    /// A whole monitor rather than one window, so a menu can group them.
    pub monitor: bool,
}

/// A choice made from the tray: a source, or `None` to show nothing.
pub type TrayPick = Option<SourceId>;

struct Entry {
    sources: Vec<ScreenSource>,
    showing: Option<SourceId>,
    picks: mpsc::UnboundedSender<TrayPick>,
}

fn views() -> &'static Mutex<BTreeMap<String, Entry>> {
    static VIEWS: OnceLock<Mutex<BTreeMap<String, Entry>>> = OnceLock::new();
    VIEWS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, BTreeMap<String, Entry>> {
    views().lock().unwrap_or_else(|p| p.into_inner())
}

/// A view opened for `peer`; what the tray picks for it arrives on the
/// returned channel.
pub fn open(peer: &str) -> mpsc::UnboundedReceiver<TrayPick> {
    let (picks, rx) = mpsc::unbounded_channel();
    lock().insert(
        peer.to_string(),
        Entry {
            sources: Vec::new(),
            showing: None,
            picks,
        },
    );
    rx
}

/// What `peer`'s view offers and shows now.
pub fn update(peer: &str, sources: &[ScreenSource], showing: Option<SourceId>) {
    if let Some(entry) = lock().get_mut(peer) {
        entry.sources = sources.to_vec();
        entry.showing = showing;
    }
}

pub fn close(peer: &str) {
    lock().remove(peer);
}

/// Every open view, by peer name.
pub fn list() -> Vec<ScreenView> {
    lock()
        .iter()
        .map(|(peer, entry)| ScreenView {
            peer: peer.clone(),
            sources: entry
                .sources
                .iter()
                .map(|s| ScreenChoice {
                    id: s.id,
                    title: s.title.clone(),
                    monitor: s.kind == SourceKind::Monitor,
                })
                .collect(),
            showing: entry.showing,
        })
        .collect()
}

/// Show `source` (or nothing) in `peer`'s view.
pub fn pick(peer: &str, source: TrayPick) -> Result<(), String> {
    let views = lock();
    let entry = views
        .get(peer)
        .ok_or_else(|| format!("no remote-screen view of {peer} is open here"))?;
    if let Some(id) = source {
        if !entry.sources.iter().any(|s| s.id == id) {
            return Err(format!("{peer} no longer offers that source"));
        }
    }
    entry
        .picks
        .send(source)
        .map_err(|_| format!("the view of {peer} is closing"))
}

/// Counts the apps whose tray stands in for the views' own icons, and tells
/// the views when that count moves between none and some.
pub struct Trays {
    count: Mutex<usize>,
    apply: fn(bool),
}

impl Trays {
    pub const fn new(apply: fn(bool)) -> Self {
        Self {
            count: Mutex::new(0),
            apply,
        }
    }

    /// An app attached; its icons stay hidden until the guard drops.
    pub fn attach(&'static self) -> TrayGuard {
        let mut count = self.count.lock().unwrap_or_else(|p| p.into_inner());
        *count += 1;
        if *count == 1 {
            (self.apply)(false);
        }
        TrayGuard(self)
    }

    fn detach(&self) {
        let mut count = self.count.lock().unwrap_or_else(|p| p.into_inner());
        *count = count.saturating_sub(1);
        if *count == 0 {
            (self.apply)(true);
        }
    }
}

/// Held for as long as an app's tray is attached.
pub struct TrayGuard(&'static Trays);

impl Drop for TrayGuard {
    fn drop(&mut self) {
        self.0.detach();
    }
}

#[cfg(feature = "window")]
fn apply_icons(shown: bool) {
    cursio_window::set_remote_screen_icons_shown(shown);
}

#[cfg(not(feature = "window"))]
fn apply_icons(_shown: bool) {}

/// This node's trays.
pub static TRAYS: Trays = Trays::new(apply_icons);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    fn source(id: SourceId, kind: SourceKind, title: &str) -> ScreenSource {
        ScreenSource {
            id,
            kind,
            title: title.into(),
            width: 0,
            height: 0,
            picked_on_host: false,
        }
    }

    // 0 = never told, 1 = shown, 2 = hidden.
    static SEEN: AtomicU8 = AtomicU8::new(0);
    fn record(shown: bool) {
        SEEN.store(if shown { 1 } else { 2 }, Ordering::SeqCst);
    }
    static TEST_TRAYS: Trays = Trays::new(record);

    #[test]
    fn the_views_icons_hide_while_any_tray_is_attached_and_return_after_the_last() {
        let first = TEST_TRAYS.attach();
        assert_eq!(SEEN.load(Ordering::SeqCst), 2);
        let second = TEST_TRAYS.attach();
        drop(first);
        assert_eq!(SEEN.load(Ordering::SeqCst), 2, "one tray is still there");
        drop(second);
        assert_eq!(SEEN.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_tray_pick_reaches_the_view_it_names() {
        let mut picks = open("Tray Peer A");
        update(
            "Tray Peer A",
            &[
                source(1, SourceKind::Monitor, "Screen 1"),
                source(2, SourceKind::Window, "Notes"),
            ],
            Some(1),
        );
        let listed = list()
            .into_iter()
            .find(|v| v.peer == "Tray Peer A")
            .unwrap();
        assert_eq!(listed.showing, Some(1));
        assert!(listed.sources[0].monitor && !listed.sources[1].monitor);
        pick("Tray Peer A", Some(2)).unwrap();
        pick("Tray Peer A", None).unwrap();
        assert_eq!(picks.try_recv().unwrap(), Some(2));
        assert_eq!(picks.try_recv().unwrap(), None);
        close("Tray Peer A");
    }

    #[test]
    fn a_pick_for_a_source_the_peer_dropped_is_refused() {
        let _picks = open("Tray Peer B");
        update(
            "Tray Peer B",
            &[source(1, SourceKind::Monitor, "Screen 1")],
            None,
        );
        assert!(pick("Tray Peer B", Some(9)).is_err());
        assert!(pick("Nobody", Some(1)).is_err());
        close("Tray Peer B");
    }
}
