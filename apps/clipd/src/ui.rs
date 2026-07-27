//! The popup: a GTK4 + libadwaita window listing the history.
//!
//! Built with libadwaita rather than a hand-styled toolkit or a web view for
//! one reason — this window appears *on top of* the user's desktop, right
//! beside real GNOME surfaces, where anything slightly off reads as foreign.
//! Adwaita gives the system's light/dark theme, the user's accent colour
//! (GNOME 47+), the system font and its size, and the platform's rounding,
//! shadows and animations, all of which keep matching after a GNOME upgrade.
//! Reimplementing that by hand means approximating it once and drifting
//! afterwards.
//!
//! ## Living in the daemon
//!
//! The window is built once, at daemon startup, and then shown and hidden. It
//! is not a process the hotkey launches: constructing a GTK application and
//! connecting to the display costs hundreds of milliseconds, which is far too
//! slow for something summoned by a keypress. `clipd pick` just sends a line to
//! the daemon (see [`crate::ipc`]).
//!
//! ## Presenting only what is on screen
//!
//! Rows are built lazily through `ListView` + a `ListItemFactory`, so a history
//! of hundreds of entries costs only the dozen widgets actually visible.
//! Thumbnails were already generated when each item was recorded, so a row
//! loads a couple of kilobytes rather than decoding a multi-megabyte PNG.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;

use crate::store::{HistoryItem, ItemKind, Store};

/// The window's title, shown in the header bar.
const WINDOW_TITLE: &str = "Clipboard History";

/// Application id. Also the D-Bus name and the key GNOME matches a window to
/// its `.desktop` entry by, so it must stay in step with the packaging.
pub const APP_ID: &str = "dev.cursio.clipd";

/// How many entries the popup lists.
const VISIBLE_LIMIT: usize = 200;

/// Styling that Adwaita doesn't provide on its own.
///
/// Deliberately tiny: every colour here comes from a named Adwaita variable
/// rather than a literal, so the window follows the system theme and the user's
/// accent colour instead of freezing one designer's choices.
const CSS: &str = "
.clip-row {
    padding: 4px 6px;
    border-radius: 8px;
}
.clip-thumb {
    border-radius: 5px;
    background: alpha(currentColor, 0.06);
    min-width: 56px;
    min-height: 40px;
}
.clip-preview {
    font-size: 0.94em;
}
.clip-meta {
    font-size: 0.76em;
    opacity: 0.55;
}
.clip-empty {
    opacity: 0.55;
}
";

/// Handle to the popup, used by the daemon to show and hide it.
#[derive(Clone)]
pub struct Popup {
    window: adw::ApplicationWindow,
    refresh: Rc<dyn Fn()>,
}

impl Popup {
    /// Bring the window up, refreshed and focused.
    pub fn show(&self) {
        (self.refresh)();
        self.window.present();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn toggle(&self) {
        if self.window.is_visible() {
            self.hide();
        } else {
            self.show();
        }
    }
}

/// One row's data, as a GObject so `ListView` can bind to it.
mod row_data {
    use super::*;
    use glib::subclass::prelude::*;

    #[derive(Default)]
    pub struct Inner {
        pub item: RefCell<Option<HistoryItem>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Inner {
        const NAME: &'static str = "ClipdRowData";
        type Type = RowData;
    }

    impl ObjectImpl for Inner {}

    glib::wrapper! {
        pub struct RowData(ObjectSubclass<Inner>);
    }

    impl RowData {
        pub fn new(item: HistoryItem) -> Self {
            let object: Self = glib::Object::new();
            object.imp().item.replace(Some(item));
            object
        }

        pub fn item(&self) -> HistoryItem {
            self.imp()
                .item
                .borrow()
                .clone()
                .expect("row data is always populated on construction")
        }
    }
}

use row_data::RowData;

/// Build the popup window.
///
/// `on_choose` runs when the user picks an entry; it receives the item id and
/// is responsible for putting the content back on the clipboard.
pub fn build<F>(app: &adw::Application, store: Rc<RefCell<Store>>, on_choose: F) -> Popup
where
    F: Fn(i64) + 'static,
{
    // Shared so the deferred write (see `connect_activate`) can own a handle.
    let on_choose = Rc::new(on_choose);
    load_css();

    let model = gio::ListStore::new::<RowData>();
    let filter_text = Rc::new(RefCell::new(String::new()));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(WINDOW_TITLE)
        .default_width(420)
        .default_height(480)
        .resizable(true)
        // A popup, not a document window: it floats above whatever the user is
        // working in, so the entry they want is visible next to the place they
        // intend to paste it.
        .modal(false)
        .build();

    // Hiding must not destroy the window: it is built once and reused for the
    // life of the daemon, which is what keeps reopening instant.
    window.set_hide_on_close(true);

    // No entry in the window list or the dock while it is up.
    window.set_deletable(false);

    keep_above(&window);

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search clipboard history…")
        .hexpand(true)
        .build();

    let selection = gtk::SingleSelection::new(Some(model.clone()));
    let factory = build_factory();

    let list = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        // One click pastes and dismisses. A picker exists to be used in a
        // single gesture — press the hotkey, click the entry, done — and making
        // it a double-click would put an extra beat in the one action the
        // window is for.
        .single_click_activate(true)
        .build();
    list.add_css_class("navigation-sidebar");

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    let empty = adw::StatusPage::builder()
        .icon_name("edit-paste-symbolic")
        .title("Nothing here yet")
        .description("Copy something and it will show up.")
        .build();
    empty.add_css_class("clip-empty");

    // Swapped for the status page when there is nothing to show, so an empty
    // history explains itself instead of looking broken.
    let content_stack = gtk::Stack::new();
    content_stack.add_named(&scroller, Some("list"));
    content_stack.add_named(&empty, Some("empty"));

    let header = adw::HeaderBar::builder()
        .title_widget(&search)
        .show_end_title_buttons(false)
        .build();

    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    layout.append(&header);
    layout.append(&content_stack);
    window.set_content(Some(&layout));

    // Reload the model from the store, honouring the current search text.
    let refresh: Rc<dyn Fn()> = {
        let model = model.clone();
        let store = store.clone();
        let filter_text = filter_text.clone();
        let content_stack = content_stack.clone();
        Rc::new(move || {
            let needle = filter_text.borrow().clone();
            let items = {
                let store = store.borrow();
                let result = if needle.trim().is_empty() {
                    store.recent(VISIBLE_LIMIT)
                } else {
                    store.search(needle.trim(), VISIBLE_LIMIT)
                };
                match result {
                    Ok(items) => items,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not read history");
                        Vec::new()
                    }
                }
            };

            model.remove_all();
            for item in &items {
                model.append(&RowData::new(item.clone()));
            }
            content_stack.set_visible_child_name(if items.is_empty() { "empty" } else { "list" });
        })
    };

    // Typing filters the list as you go.
    {
        let filter_text = filter_text.clone();
        let refresh = refresh.clone();
        search.connect_search_changed(move |entry| {
            *filter_text.borrow_mut() = entry.text().to_string();
            refresh();
        });
    }

    // Activating a row (Enter or a click) pastes it and dismisses the popup.
    {
        let selection = selection.clone();
        let window = window.clone();
        let search = search.clone();
        list.connect_activate(move |_, position| {
            let chosen = selection.item(position).map(|object| {
                object
                    .downcast_ref::<RowData>()
                    .expect("model holds RowData")
                    .item()
                    .id
            });

            // Dismiss *first*, then put the content on the clipboard. Writing
            // can take a moment — a large image has to be decoded and handed to
            // the X server — and doing it before hiding left the window sitting
            // on screen for that whole time, which felt like a hang. Deferring
            // the write by one main-loop iteration lets the window disappear on
            // the same frame as the click.
            search.set_text("");
            window.set_visible(false);

            if let Some(id) = chosen {
                let on_choose = on_choose.clone();
                glib::idle_add_local_once(move || on_choose(id));
            }
        });
    }

    install_key_handling(&window, &search, &list, &selection, store, refresh.clone());

    // Closing hides rather than destroys: the window is reused for the life of
    // the daemon, which is what keeps reopening instant.
    {
        let search = search.clone();
        window.connect_close_request(move |window| {
            search.set_text("");
            window.set_visible(false);
            glib::Propagation::Stop
        });
    }

    Popup { window, refresh }
}

/// Rows: thumbnail (or type icon), preview text, and a muted metadata line.
fn build_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("factory items are ListItems");

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("clip-row");

        let thumb = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .width_request(56)
            .height_request(40)
            .build();
        thumb.add_css_class("clip-thumb");

        let icon = gtk::Image::builder().pixel_size(20).build();
        icon.add_css_class("dim-label");

        // Only one of the two is ever shown: a real thumbnail for images, a
        // symbolic icon for everything else.
        let media = gtk::Stack::new();
        media.add_named(&thumb, Some("thumb"));
        media.add_named(&icon, Some("icon"));

        let preview = gtk::Label::builder()
            .xalign(0.0)
            // One line, ellipsised: rows stay a uniform height, so more of the
            // history is visible at a glance and the list doesn't jump around
            // as entries of different lengths scroll past.
            .single_line_mode(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        preview.add_css_class("clip-preview");

        let meta = gtk::Label::builder().xalign(0.0).build();
        meta.add_css_class("clip-meta");

        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        text.append(&preview);
        text.append(&meta);

        let pin = gtk::Image::from_icon_name("view-pin-symbolic");
        pin.set_valign(gtk::Align::Start);

        row.append(&media);
        row.append(&text);
        row.append(&pin);
        list_item.set_child(Some(&row));
    });

    factory.connect_bind(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("factory items are ListItems");
        let Some(object) = list_item.item() else { return };
        let data = object.downcast_ref::<RowData>().expect("model holds RowData");
        let item = data.item();

        let Some(row) = list_item.child().and_downcast::<gtk::Box>() else { return };
        let Some(media) = row.first_child().and_downcast::<gtk::Stack>() else { return };
        let Some(text) = media.next_sibling().and_downcast::<gtk::Box>() else { return };
        let Some(preview) = text.first_child().and_downcast::<gtk::Label>() else { return };
        let Some(meta) = preview.next_sibling().and_downcast::<gtk::Label>() else { return };
        let Some(pin) = text.next_sibling().and_downcast::<gtk::Image>() else { return };

        preview.set_text(&item.preview);
        // Size is only worth the line for images; for a text snippet the byte
        // count is noise next to the text itself.
        meta.set_text(&if item.kind == ItemKind::Image {
            format!(
                "{} · {}",
                relative_time(item.copied_at),
                crate::store::human_bytes(item.size)
            )
        } else {
            relative_time(item.copied_at)
        });
        pin.set_visible(item.pinned);

        match item.thumbnail.as_ref().filter(|path| path.exists()) {
            Some(path) => {
                if let Some(thumb) = media.child_by_name("thumb").and_downcast::<gtk::Picture>() {
                    thumb.set_file(Some(&gio::File::for_path(path)));
                }
                media.set_visible_child_name("thumb");
            }
            None => {
                if let Some(icon) = media.child_by_name("icon").and_downcast::<gtk::Image>() {
                    icon.set_icon_name(Some(icon_for(item.kind)));
                }
                media.set_visible_child_name("icon");
            }
        }
    });

    factory
}

/// Keyboard behaviour: the popup is meant to be driven without the mouse.
///
/// Escape dismisses. Delete removes the highlighted entry, Ctrl+P pins it —
/// both act on the selection and then refresh in place. Typing anywhere goes to
/// the search box, so the window is usable the instant it appears without
/// having to click into a field first.
fn install_key_handling(
    window: &adw::ApplicationWindow,
    search: &gtk::SearchEntry,
    list: &gtk::ListView,
    selection: &gtk::SingleSelection,
    store: Rc<RefCell<Store>>,
    refresh: Rc<dyn Fn()>,
) {
    let keys = gtk::EventControllerKey::new();
    // Capture phase, so Escape reaches us before the search entry consumes it.
    // GtkSearchEntry treats Escape as "clear the field" and stops it there, so
    // in the default (bubble) phase the first Escape would only empty the box
    // instead of dismissing the popup.
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);

    let window_weak = window.downgrade();
    let search_clone = search.clone();
    let selection = selection.clone();
    let list = list.clone();

    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let selected = || {
            selection
                .selected_item()
                .and_then(|object| object.downcast::<RowData>().ok())
                .map(|data| data.item())
        };

        match key {
            gdk::Key::Escape => {
                search_clone.set_text("");
                if let Some(window) = window_weak.upgrade() {
                    window.set_visible(false);
                }
                glib::Propagation::Stop
            }
            gdk::Key::Delete | gdk::Key::KP_Delete => {
                if let Some(item) = selected() {
                    if let Err(e) = store.borrow_mut().remove(item.id) {
                        tracing::warn!(error = %e, "could not delete history item");
                    }
                    refresh();
                }
                glib::Propagation::Stop
            }
            gdk::Key::p | gdk::Key::P if modifiers.contains(gdk::ModifierType::CONTROL_MASK) => {
                if let Some(item) = selected() {
                    if let Err(e) = store.borrow().set_pinned(item.id, !item.pinned) {
                        tracing::warn!(error = %e, "could not pin history item");
                    }
                    refresh();
                }
                glib::Propagation::Stop
            }
            // Let the list handle movement keys itself.
            gdk::Key::Up | gdk::Key::Down | gdk::Key::Page_Up | gdk::Key::Page_Down
            | gdk::Key::Home | gdk::Key::End | gdk::Key::Return | gdk::Key::KP_Enter => {
                glib::Propagation::Proceed
            }
            _ => glib::Propagation::Proceed,
        }
    });

    window.add_controller(keys);

    // Dismiss when focus moves to another window — but only for a *real*
    // switch, not the flicker that happens constantly on GNOME.
    //
    // This is subtler than it looks. GNOME hands focus around during ordinary
    // interactions (a menu opening, the compositor settling a freshly mapped
    // window, an always-on-top terminal like ddterm sliding in), and an earlier
    // version that hid on the first `is_active == false` made the popup vanish
    // seconds after opening — which reads as the window "freezing", since what
    // is left on screen is the last painted frame.
    //
    // So a dismissal is only honoured once the window has actually held focus,
    // and only if focus has not come back a moment later.
    {
        let window_weak = window.downgrade();
        let search = search.clone();
        let had_focus = Rc::new(std::cell::Cell::new(false));

        window.connect_is_active_notify(move |window| {
            if window.is_active() {
                had_focus.set(true);
                return;
            }
            // Never focused yet: this is the compositor still settling the
            // window, not the user clicking away.
            if !had_focus.get() || !window.is_visible() {
                return;
            }

            let window_weak = window_weak.clone();
            let search = search.clone();
            let had_focus = had_focus.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                let Some(window) = window_weak.upgrade() else { return };
                if window.is_active() || !window.is_visible() {
                    return; // focus came back; it was a flicker
                }
                had_focus.set(false);
                search.set_text("");
                window.set_visible(false);
            });
        });
    }

    // Focus the search box on open, so typing filters immediately.
    let search_clone = search.clone();
    let list_clone = list.clone();
    window.connect_map(move |_| {
        search_clone.grab_focus();
        list_clone.scroll_to(0, gtk::ListScrollFlags::SELECT, None);
    });
}

/// Ask the window manager to keep the popup above other windows.
///
/// GTK4 removed `set_keep_above`, and under Wayland a client genuinely cannot
/// demand to be on top — the compositor owns stacking. But this application
/// runs through XWayland (see the clipboard backend for why), where the
/// long-standing X11 hint `_NET_WM_STATE_ABOVE` still applies and GNOME's
/// window manager honours it.
///
/// Setting it needs the X11 window id, which GTK4 does not expose through the
/// safe API when the `x11` feature isn't compiled in, so it is set with
/// `xprop`, matching on the window's unique title. Best-effort throughout: if
/// the tool is missing or the surface is a native Wayland one, the popup simply
/// behaves like an ordinary window instead of failing.
fn keep_above(window: &adw::ApplicationWindow) {
    window.connect_map(|_| {
        // The window has to be mapped before the WM knows about it, so this
        // runs just after it appears rather than at realize time.
        glib::timeout_add_local_once(std::time::Duration::from_millis(60), || {
            // Three hints in one property: float above other windows, and stay
            // out of both the dock and the Alt-Tab switcher. A popup summoned
            // by a hotkey is not a place the user "switches to" — listing it
            // there would clutter both, and it is dismissed as soon as an entry
            // is chosen.
            let result = std::process::Command::new("xprop")
                .args([
                    // Matched by title, which is kept plain ASCII for exactly
                    // this reason: an earlier title containing an em dash was
                    // never matched by `xprop -name`.
                    "-name",
                    WINDOW_TITLE,
                    "-f",
                    "_NET_WM_STATE",
                    "32a",
                    "-set",
                    "_NET_WM_STATE",
                    "_NET_WM_STATE_ABOVE,_NET_WM_STATE_SKIP_TASKBAR,_NET_WM_STATE_SKIP_PAGER",
                ])
                .output();

            match result {
                Ok(output) if output.status.success() => {
                    tracing::debug!("popup set above, hidden from dock and switcher")
                }
                Ok(output) => tracing::debug!(
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "could not set the popup's window hints"
                ),
                Err(e) => tracing::debug!(error = %e, "xprop unavailable; popup will stack normally"),
            }
        });
    });
}

fn icon_for(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Text => "text-x-generic-symbolic",
        ItemKind::Image => "image-x-generic-symbolic",
        ItemKind::Files => "folder-symbolic",
    }
}

/// "just now", "5 min ago", "3 h ago", "2 d ago".
///
/// A wall-clock timestamp would make the user do the arithmetic; for a list
/// ordered by recency, the distance is the useful part.
pub fn relative_time(copied_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(copied_at);
    let seconds = (now - copied_at).max(0);

    match seconds {
        s if s < 45 => "just now".to_string(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => format!("{} d ago", s / 86_400),
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_reads_naturally() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        assert_eq!(relative_time(now), "just now");
        assert_eq!(relative_time(now - 300), "5 min ago");
        assert_eq!(relative_time(now - 7200), "2 h ago");
        assert_eq!(relative_time(now - 3 * 86_400), "3 d ago");
    }

    #[test]
    fn a_future_timestamp_does_not_produce_negative_time() {
        // Clock skew shouldn't render "-4 min ago".
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(relative_time(now + 600), "just now");
    }

    #[test]
    fn every_item_kind_has_an_icon() {
        for kind in [ItemKind::Text, ItemKind::Image, ItemKind::Files] {
            assert!(icon_for(kind).ends_with("-symbolic"));
        }
    }
}
