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
//! Built for Windows (`cursio_window::windows_app`). On Linux, reading another
//! application's window means the ScreenCast portal and its per-session
//! consent, and input means libei — not built yet, and said so.

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

#[cfg(all(windows, feature = "phone-control"))]
mod imp {
    use super::*;
    use cursio_window::windows_app::{self, AppWindow};

    fn window(target: &WindowTarget, pids: &[u32], id: &AppId) -> Result<AppWindow, String> {
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
        let mut image =
            image::RgbaImage::from_raw(w, h, rgba).ok_or("the picture has the wrong size")?;
        if let Some(max) = max_side.filter(|m| *m > 0) {
            let long = w.max(h);
            if long > max {
                let scale = max as f32 / long as f32;
                let (sw, sh) = (
                    ((w as f32 * scale).round() as u32).max(1),
                    ((h as f32 * scale).round() as u32).max(1),
                );
                image =
                    image::imageops::resize(&image, sw, sh, image::imageops::FilterType::Triangle);
            }
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(|e| format!("cannot encode the picture: {e}"))?;
        tracing::info!(%peer, app = %id, title = %found.title, "a peer took a still of an application's window");
        Ok(DiagResponse::WindowShot {
            png: png.into_inner(),
            width: image.width(),
            height: image.height(),
            title: found.title,
        })
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
                let (fx, fy) = (
                    f32::from(*x) / f32::from(WINDOW_SPAN),
                    f32::from(*y) / f32::from(WINDOW_SPAN),
                );
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

#[cfg(not(all(windows, feature = "phone-control")))]
mod imp {
    use super::*;

    fn not_built() -> String {
        format!(
            "reading or driving an application's window is built for Windows only; on {} it \
             needs the ScreenCast portal and libei, not built yet",
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

#[cfg(test)]
mod tests {
    use super::*;

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
