//! Choosing an entry: the popup the hotkey brings up.
//!
//! Rather than ship a GUI, `clipd` drives a picker that is already installed.
//! They all reduce to the same trivial exchange — a menu in on stdin, the
//! chosen entry out on stdout — and they already solve what is genuinely hard
//! here: a window that takes keyboard focus, search, and theming that matches
//! the desktop.
//!
//! ## Two families, and why the choice is not cosmetic
//!
//! **dmenu-style** (`fuzzel`, `wofi`, `tofi`, `rofi`, `dmenu`) is the nicer
//! experience — type-to-filter, keyboard-only, instant. But the Wayland ones
//! draw their overlay with `wlr-layer-shell`, a wlroots protocol **GNOME does
//! not implement**. On GNOME, `fuzzel` exits immediately with "compositor is
//! missing support for the Wayland layer surface protocol" and no window ever
//! appears (verified on the target system — the same wlroots-vs-GNOME gap that
//! forces the X11 clipboard path).
//!
//! **GTK dialogs** (`zenity`, `yad`) are ordinary application windows, so they
//! work anywhere GTK does, GNOME included. They are heavier and mouse-oriented,
//! but they *appear*, which beats an elegant menu that never opens.
//!
//! So detection prefers whichever family suits the session: under GNOME the GTK
//! dialogs come first, elsewhere the dmenu-style ones do. `--with` overrides
//! the choice entirely.
//!
//! The trade-off common to both is that a text menu can't show image
//! thumbnails; an image appears as `[image, 2.1 MiB]`. A purpose-built GTK
//! popup could do better, and this module is the seam where one would slot in —
//! everything above it only asks "which id did the user choose?".

use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

use crate::store::HistoryItem;

/// Dmenu-style pickers, best first. Preferred when the compositor supports
/// them; on GNOME the Wayland ones cannot open a window at all (module docs).
const DMENU_PICKERS: &[&str] = &["fuzzel", "wofi", "tofi", "rofi", "dmenu"];

/// GTK dialogs, best first. Plain application windows, so they work under any
/// compositor — the only ones that reliably open on GNOME.
const GTK_PICKERS: &[&str] = &["yad", "zenity"];

/// Show the picker and return the chosen item's id.
///
/// `Ok(None)` means the user dismissed it (Escape), which is a normal outcome
/// and not an error.
pub fn choose(items: &[HistoryItem], with: Option<&str>) -> Result<Option<i64>> {
    let program = match with {
        Some(explicit) => explicit.to_string(),
        None => detect().context(
            "no picker found. Install one of: zenity, yad (work everywhere), \
             or fuzzel, wofi, tofi, rofi, dmenu (not on GNOME/KDE — they need \
             wlr-layer-shell). Or pass --with <command>.",
        )?,
    };

    // The id travels in the menu itself so the chosen entry identifies the item
    // exactly, rather than being matched back by position — a picker may filter
    // or reorder what it shows.
    //
    // The two families want different shapes: dmenu-style takes one line per
    // entry, GTK dialogs take one *cell* per line (id, then label, repeating).
    let menu: String = if is_gtk_dialog(&program) {
        items
            .iter()
            .map(|item| format!("{}\n{}\n", item.id, label(item)))
            .collect()
    } else {
        items
            .iter()
            .map(|item| format!("{}\t{}\n", item.id, label(item)))
            .collect()
    };

    let mut child = build_command(&program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting the picker `{program}`"))?;

    child
        .stdin
        .take()
        .context("picker has no stdin")?
        .write_all(menu.as_bytes())
        .context("sending the menu to the picker")?;

    let output = child.wait_with_output().context("waiting for the picker")?;

    // A non-zero exit is how every one of these reports "dismissed".
    if !output.status.success() {
        return Ok(None);
    }

    let chosen = String::from_utf8_lossy(&output.stdout);
    let line = chosen.trim();
    if line.is_empty() {
        return Ok(None);
    }

    parse_id(line).map(Some).with_context(|| {
        format!("could not tell which entry was chosen from the picker's output: {line:?}")
    })
}

/// The label shown for an entry, without the id.
fn label(item: &HistoryItem) -> String {
    format!(
        "{}{} {}",
        item.kind.marker(),
        if item.pinned { "*" } else { "" },
        item.preview
    )
}

/// Whether this picker is a GTK dialog (which wants cell-per-line input).
fn is_gtk_dialog(program: &str) -> bool {
    let binary = program.split_whitespace().next().unwrap_or(program);
    let binary = binary.rsplit('/').next().unwrap_or(binary);
    GTK_PICKERS.contains(&binary)
}

/// Build the picker invocation, including the flags that make it behave like a
/// menu rather than an application launcher.
fn build_command(program: &str) -> Command {
    // The program name may carry arguments when given via `--with`.
    let mut parts = program.split_whitespace();
    let binary = parts.next().unwrap_or(program);
    let extra: Vec<&str> = parts.collect();

    let mut cmd = Command::new(binary);
    if extra.is_empty() {
        // Default flags per picker: read from stdin, act as a chooser, and
        // label the prompt.
        match binary {
            "fuzzel" => cmd.args(["--dmenu", "--prompt", "clipboard: "]),
            "rofi" => cmd.args(["-dmenu", "-i", "-p", "clipboard"]),
            "wofi" => cmd.args(["--dmenu", "--prompt", "clipboard"]),
            "tofi" => cmd.args(["--prompt-text", "clipboard: "]),
            "dmenu" => cmd.args(["-i", "-p", "clipboard"]),
            // GTK dialogs read a flat stream of cell values, one per line, and
            // print back only the id column — so the chosen line parses the
            // same way a dmenu-style pick does.
            "yad" => cmd.args([
                "--list",
                "--title=Clipboard history",
                "--width=700",
                "--height=500",
                "--column=id",
                "--column=item",
                "--print-column=1",
                "--search-column=2",
                "--no-headers",
                "--separator=",
            ]),
            "zenity" => cmd.args([
                "--list",
                "--title=Clipboard history",
                "--width=700",
                "--height=500",
                "--column=id",
                "--column=item",
                "--print-column=1",
                "--hide-column=1",
            ]),
            _ => &mut cmd,
        };
    } else {
        cmd.args(extra);
    }
    cmd
}

/// The best installed picker for this session.
///
/// Order depends on the compositor: a dmenu-style picker is the better
/// experience, but on GNOME the Wayland ones silently fail to open (see module
/// docs), so GTK dialogs are tried first there.
fn detect() -> Option<String> {
    let (first, second) = if uses_layer_shell() {
        (DMENU_PICKERS, GTK_PICKERS)
    } else {
        (GTK_PICKERS, DMENU_PICKERS)
    };
    first
        .iter()
        .chain(second.iter())
        .find(|program| which(program))
        .map(|program| program.to_string())
}

/// Whether this session's compositor is likely to support `wlr-layer-shell`.
///
/// Decided from the desktop rather than by probing, since the failure only
/// shows up when the picker is already running and exits with no window. GNOME
/// and KDE do not implement it; wlroots compositors (Sway, Hyprland, river…)
/// do. An X11 session is fine either way — layer-shell is not involved.
fn uses_layer_shell() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return true; // X11: dmenu-style pickers work normally
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    !(desktop.contains("gnome") || desktop.contains("kde") || desktop.contains("plasma"))
}

/// Whether `program` is on `PATH`.
fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Recover the item id from the chosen line.
///
/// Some pickers echo the line verbatim; others (notably `rofi` in certain
/// modes) may return only what was displayed. Taking everything before the tab
/// handles the first case, and a leading run of digits handles the second.
fn parse_id(line: &str) -> Result<i64> {
    let head = line.split('\t').next().unwrap_or(line).trim();
    if let Ok(id) = head.parse::<i64>() {
        return Ok(id);
    }
    let digits: String = head.chars().take_while(char::is_ascii_digit).collect();
    digits
        .parse::<i64>()
        .with_context(|| format!("no item id at the start of {line:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ItemKind;

    fn item(id: i64, preview: &str) -> HistoryItem {
        HistoryItem {
            id,
            copied_at: 0,
            preview: preview.to_string(),
            kind: ItemKind::Text,
            size: 0,
            pinned: false,
            thumbnail: None,
        }
    }

    #[test]
    fn id_is_parsed_from_a_full_line() {
        assert_eq!(parse_id("42\tT some text").unwrap(), 42);
    }

    #[test]
    fn id_is_parsed_when_the_tab_is_lost() {
        // Some pickers return only the rendered text.
        assert_eq!(parse_id("42 T some text").unwrap(), 42);
    }

    #[test]
    fn a_line_without_an_id_is_an_error() {
        assert!(parse_id("no id here").is_err());
    }

    #[test]
    fn preview_containing_digits_does_not_confuse_the_id() {
        // The id must come from the leading field, not from digits in the text.
        assert_eq!(parse_id("7\tT 2024 budget 99").unwrap(), 7);
    }

    #[test]
    fn menu_lines_carry_the_id_and_marker() {
        let items = vec![item(3, "hello"), {
            let mut pinned = item(4, "kept");
            pinned.pinned = true;
            pinned
        }];
        let menu: Vec<String> = items
            .iter()
            .map(|i| {
                format!(
                    "{}\t{}{} {}",
                    i.id,
                    i.kind.marker(),
                    if i.pinned { "*" } else { "" },
                    i.preview
                )
            })
            .collect();

        assert_eq!(menu[0], "3\tT hello");
        assert_eq!(menu[1], "4\tT* kept");
        // Round-trip: what is rendered must parse back to the same id.
        assert_eq!(parse_id(&menu[1]).unwrap(), 4);
    }
}
