//! An application's window on this computer, read and driven for a peer —
//! `cursio window`, an AI agent's `window_screenshot` and `window_click`.
//!
//! A phone's screen already reaches a computer as a window it can be driven
//! from (`phone_screen`); a desktop application had nothing like it, so a
//! fix to what a button does was checked by a person pressing it. This is
//! the fallback beside an application's own declared actions
//! (`apps::actions`): it works with any registered application, and it checks
//! what the person would see.
//!
//! **Only registered applications.** A request names an application, a
//! process of it and at most a title to choose among its windows; the node
//! finds them through the process ids that application registered with
//! (`apps::Registry`), never through a handle or a path a peer chose.
//!
//! **The actions consent covers it.** Clicking an application's window does
//! at least what calling its actions does, so `[apps.<id>] actions` decides
//! both, asked once with buttons on this machine.
//!
//! Built for Windows (`cursio_window::windows_app`) and for GNOME, on Wayland
//! and X11 alike (`cursio_window::linux_app`, through Cursio's Shell
//! extension: a Wayland client cannot find another's window, and the
//! ScreenCast portal streams only one the person picks, with no process to
//! match it to). Elsewhere it says it is not built.

use cursio_app_protocol::{AppId, ProcessName};
use cursio_protocol::diag::{DesktopInput, DiagResponse, WindowTarget, MAX_TYPED, WINDOW_SPAN};

/// The process ids of `target`'s running application, after this machine's
/// consent for `peer`.
fn pids(peer: &str, target: &WindowTarget) -> Result<(AppId, Vec<u32>), String> {
    let id = AppId::parse(&target.app).map_err(|e| e.to_string())?;
    let process = target
        .process
        .as_deref()
        .map(ProcessName::parse)
        .transpose()
        .map_err(|e| e.to_string())?;
    let pids: Vec<u32> = {
        let registry = crate::apps::registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        registry
            .processes(&id)
            .into_iter()
            .filter(|(key, _)| process.is_none() || key.process == process)
            .filter(|(_, r)| r.running())
            .filter_map(|(_, r)| r.pid)
            .collect()
    };
    if pids.is_empty() {
        return Err(format!("{id} is not running on this machine"));
    }
    crate::apps::actions::allowed(peer, &id)?;
    Ok((id, pids))
}

/// The evdev code of a named key.
pub fn named_key(name: &str) -> Result<u16, String> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "escape" | "esc" => 1,
        "backspace" => 14,
        "tab" => 15,
        "enter" | "return" => 28,
        "home" => 102,
        "up" => 103,
        "pageup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" => 109,
        "delete" => 111,
        "f11" => 87,
        "f12" => 88,
        f if f.starts_with('f') => match f[1..].parse::<u16>() {
            Ok(n @ 1..=10) => 58 + n,
            _ => return Err(format!("no key called {name}")),
        },
        _ => return Err(format!("no key called {name}")),
    })
}

/// Check a request's input before anything is found or done.
pub fn check(input: &DesktopInput) -> Result<(), String> {
    match input {
        DesktopInput::Click { x, y, .. } if *x > WINDOW_SPAN || *y > WINDOW_SPAN => {
            Err("a click is outside the window".into())
        }
        DesktopInput::Type { text } if text.chars().count() > MAX_TYPED => {
            Err(format!("one request types at most {MAX_TYPED} characters"))
        }
        DesktopInput::Key { key } => named_key(key).map(drop),
        _ => Ok(()),
    }
}

/// `image` no longer than `max_side` on its long side, as the answer to a
/// still.
#[cfg(all(any(windows, target_os = "linux"), feature = "phone-control"))]
fn answer_still(
    mut image: image::RgbaImage,
    max_side: Option<u32>,
    title: String,
) -> Result<DiagResponse, String> {
    let (w, h) = image.dimensions();
    if let Some(max) = max_side.filter(|m| *m > 0) {
        let long = w.max(h);
        if long > max {
            let scale = max as f32 / long as f32;
            let (sw, sh) = (
                ((w as f32 * scale).round() as u32).max(1),
                ((h as f32 * scale).round() as u32).max(1),
            );
            image = image::imageops::resize(&image, sw, sh, image::imageops::FilterType::Triangle);
        }
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| format!("cannot encode the picture: {e}"))?;
    Ok(DiagResponse::WindowShot {
        png: png.into_inner(),
        width: image.width(),
        height: image.height(),
        title,
    })
}

/// Turn a click's fixed-point position into fractions of the window.
#[cfg(all(any(windows, target_os = "linux"), feature = "phone-control"))]
fn fractions(x: u16, y: u16) -> (f32, f32) {
    (
        f32::from(x) / f32::from(WINDOW_SPAN),
        f32::from(y) / f32::from(WINDOW_SPAN),
    )
}

#[cfg(all(windows, feature = "phone-control"))]
mod imp {
    use super::*;
    use cursio_window::windows_app::{self, AppWindow};

    pub(super) fn window(
        target: &WindowTarget,
        pids: &[u32],
        id: &AppId,
    ) -> Result<AppWindow, String> {
        let windows = windows_app::windows_of(pids);
        let wanted = target.title.as_deref().map(str::to_lowercase);
        windows
            .into_iter()
            .find(|w| {
                wanted
                    .as_deref()
                    .is_none_or(|t| w.title.to_lowercase().contains(t))
            })
            .ok_or_else(|| match &target.title {
                Some(title) => format!("{id} has no visible window titled like {title:?}"),
                None => format!("{id} has no visible window"),
            })
    }

    pub fn shot(
        peer: &str,
        target: &WindowTarget,
        max_side: Option<u32>,
    ) -> Result<DiagResponse, String> {
        let (id, pids) = pids(peer, target)?;
        let found = window(target, &pids, &id)?;
        let (w, h, rgba) = windows_app::still(found.hwnd)?;
        let image =
            image::RgbaImage::from_raw(w, h, rgba).ok_or("the picture has the wrong size")?;
        tracing::info!(%peer, app = %id, title = %found.title, "a peer took a still of an application's window");
        answer_still(image, max_side, found.title)
    }

    pub fn input(
        peer: &str,
        target: &WindowTarget,
        input: &DesktopInput,
    ) -> Result<DiagResponse, String> {
        check(input)?;
        let (id, pids) = pids(peer, target)?;
        let found = window(target, &pids, &id)?;
        let what = match input {
            DesktopInput::Click { x, y, right } => {
                let (fx, fy) = fractions(*x, *y);
                windows_app::click(found.hwnd, fx, fy, *right)?;
                format!("clicked {:?} at {fx:.3}, {fy:.3}", found.title)
            }
            DesktopInput::Type { text } => {
                windows_app::type_text(found.hwnd, text)?;
                format!(
                    "typed {} characters into {:?}",
                    text.chars().count(),
                    found.title
                )
            }
            DesktopInput::Key { key } => {
                windows_app::key(found.hwnd, named_key(key)?)?;
                format!("pressed {key} in {:?}", found.title)
            }
        };
        tracing::info!(%peer, app = %id, %what, "a peer drove an application's window");
        Ok(DiagResponse::WindowDone { what })
    }
}

#[cfg(all(target_os = "linux", feature = "phone-control"))]
mod imp {
    use super::*;
    use cursio_window::linux_app::{self, AppWindow};

    fn window(target: &WindowTarget, pids: &[u32], id: &AppId) -> Result<AppWindow, String> {
        let windows = linux_app::windows_of(pids)?;
        let wanted = target.title.as_deref().map(str::to_lowercase);
        windows
            .into_iter()
            .find(|w| {
                wanted
                    .as_deref()
                    .is_none_or(|t| w.title.to_lowercase().contains(t))
            })
            .ok_or_else(|| match &target.title {
                Some(title) => format!("{id} has no window titled like {title:?}"),
                None => format!("{id} has no window"),
            })
    }

    pub fn shot(
        peer: &str,
        target: &WindowTarget,
        max_side: Option<u32>,
    ) -> Result<DiagResponse, String> {
        let (id, pids) = pids(peer, target)?;
        let found = window(target, &pids, &id)?;
        let png = linux_app::still(found.id)?;
        let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .map_err(|e| format!("the Shell's picture is unreadable: {e}"))?
            .to_rgba8();
        tracing::info!(%peer, app = %id, title = %found.title, "a peer took a still of an application's window");
        answer_still(image, max_side, found.title)
    }

    pub fn input(
        peer: &str,
        target: &WindowTarget,
        input: &DesktopInput,
    ) -> Result<DiagResponse, String> {
        check(input)?;
        let (id, pids) = pids(peer, target)?;
        let found = window(target, &pids, &id)?;
        let what = match input {
            DesktopInput::Click { x, y, right } => {
                let (fx, fy) = fractions(*x, *y);
                linux_app::click(found.id, fx, fy, *right)?;
                format!("clicked {:?} at {fx:.3}, {fy:.3}", found.title)
            }
            DesktopInput::Type { text } => {
                linux_app::type_text(found.id, text)?;
                format!(
                    "typed {} characters into {:?}",
                    text.chars().count(),
                    found.title
                )
            }
            DesktopInput::Key { key } => {
                linux_app::key(found.id, named_key(key)?)?;
                format!("pressed {key} in {:?}", found.title)
            }
        };
        tracing::info!(%peer, app = %id, %what, "a peer drove an application's window");
        Ok(DiagResponse::WindowDone { what })
    }
}

#[cfg(not(all(any(windows, target_os = "linux"), feature = "phone-control")))]
mod imp {
    use super::*;

    fn not_built() -> String {
        format!(
            "reading or driving an application's window is built for Windows and GNOME; not \
             for {}",
            std::env::consts::OS
        )
    }

    pub fn shot(
        peer: &str,
        target: &WindowTarget,
        _max_side: Option<u32>,
    ) -> Result<DiagResponse, String> {
        pids(peer, target)?;
        Err(not_built())
    }

    pub fn input(
        peer: &str,
        target: &WindowTarget,
        input: &DesktopInput,
    ) -> Result<DiagResponse, String> {
        check(input)?;
        pids(peer, target)?;
        Err(not_built())
    }
}

pub use imp::{input, shot};

/// Check a tree or element request before anything is found or done.
pub fn check_ui(depth: u16, max_nodes: u32) -> Result<(), String> {
    use cursio_protocol::diag::{MAX_TREE_DEPTH, MAX_TREE_NODES};
    if depth == 0 || depth > MAX_TREE_DEPTH {
        return Err(format!("a tree is 1 to {MAX_TREE_DEPTH} levels deep"));
    }
    if max_nodes == 0 || max_nodes > MAX_TREE_NODES {
        return Err(format!("a tree holds 1 to {MAX_TREE_NODES} elements"));
    }
    Ok(())
}

/// Check how an element is picked and what is done to it.
pub fn check_element(
    pick: &cursio_protocol::diag::ElementPick,
    act: &cursio_protocol::diag::ElementAct,
) -> Result<(), String> {
    use cursio_protocol::diag::{ElementAct, ElementPick, MAX_PICK};
    let long = match pick {
        ElementPick::Id(id) => id.len() > MAX_PICK,
        ElementPick::Match { role, name, .. } => role.len() > MAX_PICK || name.len() > MAX_PICK,
    };
    if long {
        return Err(format!(
            "a role, a name or an id is at most {MAX_PICK} bytes"
        ));
    }
    match act {
        ElementAct::SetValue(value) if value.chars().count() > MAX_TYPED => {
            Err(format!("one request sets at most {MAX_TYPED} characters"))
        }
        _ => Ok(()),
    }
}

/// The interface of `target`'s window, as one line per element.
pub fn tree(
    peer: &str,
    target: &WindowTarget,
    depth: u16,
    max_nodes: u32,
) -> Result<DiagResponse, String> {
    check_ui(depth, max_nodes)?;
    let (id, pids) = pids(peer, target)?;
    ui::tree(peer, target, &id, &pids, depth, max_nodes)
}

/// Do `act` to the element `pick` names in `target`'s window.
pub fn element(
    peer: &str,
    target: &WindowTarget,
    pick: &cursio_protocol::diag::ElementPick,
    act: &cursio_protocol::diag::ElementAct,
) -> Result<DiagResponse, String> {
    check_element(pick, act)?;
    let (id, pids) = pids(peer, target)?;
    ui::element(peer, target, &id, &pids, pick, act)
}

/// The UI tree is read through UI Automation on Windows; AT-SPI would be
/// Linux's, and is not built.
#[cfg(all(windows, feature = "phone-control"))]
mod ui {
    use super::*;
    use cursio_protocol::diag::{ElementAct, ElementPick};
    use cursio_window::ui_tree::{self, Limits};
    use cursio_window::windows_uia;

    use super::imp::window;

    pub fn tree(
        peer: &str,
        target: &WindowTarget,
        id: &AppId,
        pids: &[u32],
        depth: u16,
        max_nodes: u32,
    ) -> Result<DiagResponse, String> {
        let found = window(target, pids, id)?;
        let snapshot = windows_uia::tree(
            found.hwnd,
            Limits {
                depth,
                nodes: max_nodes,
            },
        )?;
        tracing::info!(%peer, app = %id, nodes = snapshot.nodes, "a peer read an application's interface");
        Ok(DiagResponse::WindowTree {
            title: found.title,
            text: ui_tree::render(&snapshot.root),
            nodes: snapshot.nodes,
            truncated: snapshot.truncated,
        })
    }

    pub fn element(
        peer: &str,
        target: &WindowTarget,
        id: &AppId,
        pids: &[u32],
        pick: &ElementPick,
        act: &ElementAct,
    ) -> Result<DiagResponse, String> {
        let found = window(target, pids, id)?;
        // Acting re-walks the tree to find the element, with room for a
        // whole page.
        let what = windows_uia::act(found.hwnd, pick, act, Limits::most())?;
        tracing::info!(%peer, app = %id, %what, "a peer acted on an application's interface");
        Ok(DiagResponse::WindowDone { what })
    }
}

#[cfg(not(all(windows, feature = "phone-control")))]
mod ui {
    use super::*;
    use cursio_protocol::diag::{ElementAct, ElementPick};

    fn not_built() -> String {
        format!(
            "an application's interface tree is read through UI Automation on Windows; not \
             built for {} yet — use its window's screenshot and clicks there",
            std::env::consts::OS
        )
    }

    pub fn tree(
        _peer: &str,
        _target: &WindowTarget,
        _id: &AppId,
        _pids: &[u32],
        _depth: u16,
        _max_nodes: u32,
    ) -> Result<DiagResponse, String> {
        Err(not_built())
    }

    pub fn element(
        _peer: &str,
        _target: &WindowTarget,
        _id: &AppId,
        _pids: &[u32],
        _pick: &ElementPick,
        _act: &ElementAct,
    ) -> Result<DiagResponse, String> {
        Err(not_built())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_or_element_request_out_of_bounds_is_refused_before_anything_is_walked() {
        use cursio_protocol::diag::{ElementAct, ElementPick, MAX_PICK};
        assert!(check_ui(40, 3000).is_ok());
        assert!(check_ui(0, 3000).is_err());
        assert!(check_ui(65, 3000).is_err());
        assert!(check_ui(40, 10_001).is_err());
        let pick = ElementPick::Match {
            role: "button".into(),
            name: "x".repeat(MAX_PICK + 1),
            nth: 0,
        };
        assert!(check_element(&pick, &ElementAct::Invoke).is_err());
        let ok = ElementPick::Id("1-2".into());
        assert!(check_element(&ok, &ElementAct::SetValue("y".repeat(MAX_TYPED + 1))).is_err());
        assert!(check_element(&ok, &ElementAct::Invoke).is_ok());
    }

    #[test]
    fn a_tree_of_an_application_not_running_here_is_refused() {
        let target = WindowTarget {
            app: "not-registered-app".into(),
            process: None,
            title: None,
        };
        let error = tree("Nocturne", &target, 10, 100).unwrap_err();
        assert!(error.contains("not running"), "{error}");
    }

    #[test]
    fn named_keys_are_the_evdev_codes_and_others_are_refused() {
        assert_eq!(named_key("Enter").unwrap(), 28);
        assert_eq!(named_key("f1").unwrap(), 59);
        assert_eq!(named_key("f10").unwrap(), 68);
        assert_eq!(named_key("f12").unwrap(), 88);
        assert!(named_key("f13").is_err());
        assert!(named_key("rm -rf").is_err());
    }

    #[test]
    fn an_application_not_running_here_is_refused_before_any_window_is_looked_for() {
        let target = WindowTarget {
            app: "not-registered-app".into(),
            process: None,
            title: None,
        };
        let error = shot("Nocturne", &target, None).unwrap_err();
        assert!(error.contains("not running"), "{error}");
        assert!(check(&DesktopInput::Click {
            x: WINDOW_SPAN + 1,
            y: 0,
            right: false
        })
        .is_err());
    }
}
