//! Binding the picker to a key, under GNOME.
//!
//! Wayland gives no way for an application to register a global shortcut for
//! itself — that is the compositor's business, by design. GNOME's answer is
//! "custom keybindings" in its settings: a name, a command, and a binding,
//! stored in dconf. So `clipd` writes such an entry via `gsettings` instead of
//! trying to grab a key.
//!
//! ## Why not Super+V
//!
//! Super+V is what Windows uses, so it is the obvious default — but on GNOME it
//! is already bound to `toggle-message-tray` (verified on this project's target
//! system). A custom binding that collides with a built-in one silently loses:
//! GNOME keeps its own action, and the user sees a shortcut that does nothing.
//!
//! Rather than fail confusingly, the default here is `<Super><Shift>v`, which
//! is free. Passing `--force` takes `<Super>v` by first clearing GNOME's claim
//! on it — an explicit, reversible choice the user opts into.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Where GNOME keeps custom keybindings.
const CUSTOM_PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/";

/// The dconf slot `clipd` claims. Fixed, so re-running updates the same entry
/// instead of adding another one every time.
const SLOT: &str = "clipd";

/// GNOME's own bindings that a custom shortcut would silently lose to.
///
/// Only the ones plausibly wanted for a clipboard picker are listed; each is
/// (schema, key) as `gsettings` addresses it.
const BUILTIN_BINDINGS: &[(&str, &str)] = &[
    ("org.gnome.shell.keybindings", "toggle-message-tray"),
    ("org.gnome.shell.keybindings", "toggle-overview"),
    ("org.gnome.desktop.wm.keybindings", "switch-input-source"),
];

/// Install (or update) the shortcut that runs `clipd pick`.
pub fn install(binding: &str, force: bool) -> Result<()> {
    if !has_gsettings() {
        bail!("`gsettings` not found; this command supports GNOME only");
    }

    if let Some((schema, key)) = conflicting_builtin(binding)? {
        if !force {
            bail!(
                "GNOME already uses {binding} for {schema} {key}, and a custom shortcut \
                 would silently lose to it.\n\
                 Either pick a free combination (the default `<Super><Shift>v` is free), \
                 or re-run with --force to take {binding} away from GNOME."
            );
        }
        // Remove only the colliding binding, keeping the others. GNOME actions
        // commonly have several (toggle-message-tray is bound to both Super+V
        // and Super+M), and clearing the whole list would silently take away a
        // shortcut the user never asked to lose.
        let remaining = remove_binding(&gsettings_output(&["get", schema, key])?, binding);
        tracing::warn!(schema, key, binding, "releasing GNOME's binding");
        gsettings(&["set", schema, key, &render_list(&remaining)])?;
    }

    let exe = std::env::current_exe()
        .context("could not determine this executable's path")?
        .display()
        .to_string();
    let command = format!("{exe} pick");

    let slot_path = format!("{CUSTOM_PATH}{SLOT}/");
    let schema = format!("org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:{slot_path}");

    gsettings(&["set", &schema, "name", "Clipboard history"])?;
    gsettings(&["set", &schema, "command", &command])?;
    gsettings(&["set", &schema, "binding", binding])?;

    register_slot(&slot_path)?;

    println!("bound {binding} to `{command}`");
    println!("Press it to open the clipboard picker.");
    Ok(())
}

/// Add our slot to the list GNOME reads, preserving any others already there.
///
/// The list is a single dconf value, so it has to be read, extended, and
/// written back — appending blindly would drop the user's other shortcuts.
fn register_slot(slot_path: &str) -> Result<()> {
    let existing = gsettings_output(&[
        "get",
        "org.gnome.settings-daemon.plugins.media-keys",
        "custom-keybindings",
    ])?;

    let mut paths = parse_list(&existing);
    if paths.iter().any(|p| p == slot_path) {
        return Ok(()); // already registered; nothing to change
    }
    paths.push(slot_path.to_string());

    let rendered = render_list(&paths);
    gsettings(&[
        "set",
        "org.gnome.settings-daemon.plugins.media-keys",
        "custom-keybindings",
        &rendered,
    ])
}

/// Which built-in binding, if any, already claims `binding`.
fn conflicting_builtin(binding: &str) -> Result<Option<(&'static str, &'static str)>> {
    for (schema, key) in BUILTIN_BINDINGS {
        let value = match gsettings_output(&["get", schema, key]) {
            Ok(value) => value,
            // A schema missing on this GNOME version is not a conflict.
            Err(_) => continue,
        };
        if parse_list(&value).iter().any(|b| bindings_equal(b, binding)) {
            return Ok(Some((schema, key)));
        }
    }
    Ok(None)
}

/// Compare two GNOME bindings, ignoring case and modifier order.
///
/// `<Super><Shift>v` and `<shift><super>V` are the same shortcut, and a naive
/// string comparison would miss the collision it is meant to catch.
fn bindings_equal(a: &str, b: &str) -> bool {
    fn parts(s: &str) -> (Vec<String>, String) {
        let lower = s.to_ascii_lowercase();
        let mut modifiers = Vec::new();
        let mut rest = lower.as_str();
        while let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
            modifiers.push(rest[1..=end].to_string());
            rest = &rest[end + 2..];
        }
        modifiers.sort();
        (modifiers, rest.to_string())
    }
    parts(a) == parts(b)
}

/// A binding list with `binding` taken out, comparing semantically.
fn remove_binding(value: &str, binding: &str) -> Vec<String> {
    parse_list(value)
        .into_iter()
        .filter(|existing| !bindings_equal(existing, binding))
        .collect()
}

/// Render a list of strings the way dconf expects it.
fn render_list(items: &[String]) -> String {
    if items.is_empty() {
        // The typed empty list; a bare `[]` is ambiguous to gsettings.
        return "@as []".to_string();
    }
    format!(
        "[{}]",
        items
            .iter()
            .map(|item| format!("'{item}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Parse a dconf string list — `['a', 'b']` or `@as []` — into its elements.
fn parse_list(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    // The empty list has two spellings.
    if trimmed.is_empty() || trimmed == "@as []" || trimmed == "[]" {
        return Vec::new();
    }
    let inner = trimmed
        .trim_start_matches("@as ")
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');

    inner
        .split(',')
        .map(|part| part.trim().trim_matches('\'').trim_matches('"').to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn has_gsettings() -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join("gsettings").is_file()))
        .unwrap_or(false)
}

fn gsettings(args: &[&str]) -> Result<()> {
    let status = Command::new("gsettings")
        .args(args)
        .status()
        .context("running gsettings")?;
    if !status.success() {
        bail!("gsettings {} failed", args.join(" "));
    }
    Ok(())
}

fn gsettings_output(args: &[&str]) -> Result<String> {
    let output = Command::new("gsettings")
        .args(args)
        .output()
        .context("running gsettings")?;
    if !output.status.success() {
        bail!("gsettings {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_populated_list() {
        assert_eq!(
            parse_list("['/one/', '/two/']"),
            vec!["/one/".to_string(), "/two/".to_string()]
        );
    }

    #[test]
    fn parses_both_spellings_of_the_empty_list() {
        assert!(parse_list("@as []").is_empty());
        assert!(parse_list("[]").is_empty());
        assert!(parse_list("").is_empty());
    }

    #[test]
    fn bindings_match_regardless_of_case_and_order() {
        // The collision check depends on this: GNOME stores <Super>v, and a
        // user may well type <super>V.
        assert!(bindings_equal("<Super>v", "<super>V"));
        assert!(bindings_equal("<Super><Shift>v", "<Shift><Super>v"));
    }

    #[test]
    fn different_bindings_do_not_match() {
        assert!(!bindings_equal("<Super>v", "<Super><Shift>v"));
        assert!(!bindings_equal("<Super>v", "<Super>c"));
        assert!(!bindings_equal("<Ctrl>v", "<Super>v"));
    }

    #[test]
    fn releasing_a_binding_keeps_the_others() {
        // toggle-message-tray holds both Super+V and Super+M on the target
        // system. Taking Super+V must not cost the user Super+M as well.
        let remaining = remove_binding("['<Super>v', '<Super>m']", "<Super>v");
        assert_eq!(remaining, vec!["<Super>m".to_string()]);
        assert_eq!(render_list(&remaining), "['<Super>m']");
    }

    #[test]
    fn releasing_the_only_binding_yields_a_typed_empty_list() {
        let remaining = remove_binding("['<Super>v']", "<Super>v");
        assert!(remaining.is_empty());
        assert_eq!(render_list(&remaining), "@as []");
    }

    #[test]
    fn releasing_matches_regardless_of_spelling() {
        // GNOME stores <Super>v; the user may have typed <super>V.
        let remaining = remove_binding("['<Super>v', '<Super>m']", "<super>V");
        assert_eq!(remaining, vec!["<Super>m".to_string()]);
    }

    #[test]
    fn the_gnome_default_conflict_is_detected() {
        // Exactly what GNOME reports for toggle-message-tray on the target
        // system — the reason Super+V is not the default here.
        let gnome_value = "['<Super>v', '<Super>m']";
        assert!(parse_list(gnome_value)
            .iter()
            .any(|b| bindings_equal(b, "<Super>v")));
        // ...and the shipped default stays clear of it.
        assert!(!parse_list(gnome_value)
            .iter()
            .any(|b| bindings_equal(b, "<Super><Shift>v")));
    }
}
