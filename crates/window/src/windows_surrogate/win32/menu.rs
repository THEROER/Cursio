//! A remote-screen view's selector: the peer's screens and windows as a
//! context menu on the notification-area icon.

use crate::windows_surrogate::win32::ViewState;
use crate::SourcePick;
use cursio_protocol::ScreenSource;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;

/// What an idle view shows — seen only in a capture app's preview, which is
/// exactly where someone wonders why the picture is empty.
/// It names both places the choice lives: the view's own icon, and the app
/// tray's "Show in Discord/OBS", which hides that icon while it is attached.
pub(super) const VIEW_HINT: &str = "Choose what to show: Cursio icon → Show in Discord/OBS";

/// The menu command for "show nothing"; sources use their index plus one.
pub(super) const STOP_COMMAND: usize = 0xFFFF;

/// One line of the view's context menu.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MenuEntry {
    Item { command: usize, label: String },
    Disabled(String),
    Separator,
}

/// The view's menu: screens, then windows, then a way to show nothing.
pub(super) fn menu_entries(sources: &[ScreenSource]) -> Vec<MenuEntry> {
    use cursio_protocol::SourceKind;
    let mut out = Vec::new();
    for kind in [SourceKind::Monitor, SourceKind::Window] {
        let before = out.len();
        for (index, source) in sources.iter().enumerate().filter(|(_, s)| s.kind == kind) {
            out.push(MenuEntry::Item {
                command: index + 1,
                label: source.title.clone(),
            });
        }
        if out.len() > before {
            out.push(MenuEntry::Separator);
        }
    }
    if sources.is_empty() {
        out.push(MenuEntry::Disabled("Waiting for the peer's list…".into()));
        out.push(MenuEntry::Separator);
    }
    out.push(MenuEntry::Item {
        command: STOP_COMMAND,
        label: "Show nothing".into(),
    });
    out
}

/// Show the view's context menu at the cursor and report the choice.
///
/// `TrackPopupMenu` opens a window of its own (class `#32768`), and window
/// capture records only the captured `HWND`, so the menu stays out of what a
/// screen-sharing application sends.
pub(super) unsafe fn show_source_menu(hwnd: HWND, view: &ViewState) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, SetForegroundWindow,
        TrackPopupMenu, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    };

    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let sources = view.sources.borrow().clone();
    for entry in menu_entries(&sources) {
        let appended = match entry {
            MenuEntry::Item { command, label } => {
                let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
                AppendMenuW(menu, MF_STRING, command, PCWSTR(wide.as_ptr()))
            }
            MenuEntry::Disabled(label) => {
                let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(wide.as_ptr()))
            }
            MenuEntry::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()),
        };
        if let Err(e) = appended {
            tracing::debug!(error = %e, "could not build the source menu");
        }
    }
    let mut at = POINT::default();
    let _ = GetCursorPos(&mut at);
    // Without this the menu does not close when the user clicks elsewhere — a
    // documented `TrackPopupMenu` quirk.
    let _ = SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        at.x,
        at.y,
        None,
        hwnd,
        None,
    )
    .0 as usize;
    let _ = DestroyMenu(menu);

    let pick = match chosen {
        0 => return,
        STOP_COMMAND => SourcePick::Stop,
        n => match sources.get(n - 1) {
            Some(source) => SourcePick::Source(source.id),
            None => return,
        },
    };
    let _ = view.picks.send(pick);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_menu_lists_screens_before_windows_and_ends_with_show_nothing() {
        use cursio_protocol::SourceKind;
        let source = |id, kind, title: &str| ScreenSource {
            id,
            kind,
            title: title.into(),
            width: 0,
            height: 0,
            picked_on_host: false,
        };
        let entries = menu_entries(&[
            source(7, SourceKind::Window, "Firefox"),
            source(0x8000_0000, SourceKind::Monitor, "Screen 1"),
        ]);
        assert_eq!(
            entries,
            vec![
                MenuEntry::Item {
                    command: 2,
                    label: "Screen 1".into()
                },
                MenuEntry::Separator,
                MenuEntry::Item {
                    command: 1,
                    label: "Firefox".into()
                },
                MenuEntry::Separator,
                MenuEntry::Item {
                    command: STOP_COMMAND,
                    label: "Show nothing".into()
                },
            ]
        );
    }
}
