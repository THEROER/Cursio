//! One application's window on this GNOME machine, read and driven for a
//! program on another machine — the Linux half of `cursio window`, beside
//! [`crate::windows_app`].
//!
//! On Wayland a client cannot find, read or drive another client's window:
//! the ScreenCast portal streams only a window the person picks in its
//! dialog, and nothing in its stream says which process the window belongs
//! to, so "the window of the application registered as pid 1234" has no
//! portal answer. The GNOME Shell extension (`native/cursio-shell-extension`,
//! `appWindows.js`) runs inside the compositor, where it is one call. So this
//! is a client of that extension over the session bus; it works the same in an
//! X11 GNOME session. Another desktop answers that it needs GNOME with the
//! extension, rather than pretending.
//!
//! Every call runs on a thread of its own: zbus's blocking calls run their own
//! executor, and called from a thread inside the node's Tokio runtime they
//! panic ("cannot start a runtime from within a runtime").

/// One window of an application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppWindow {
    /// Mutter's stable sequence number for the window.
    pub id: u32,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

const NAME: &str = "dev.cursio.Shell";
const PATH: &str = "/dev/cursio/Shell";

/// What a failed call to the extension means to the person asking.
fn explain(error: zbus::Error) -> String {
    match &error {
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
                || name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            "reading another application's window on Linux needs GNOME with Cursio's Shell \
             extension enabled (`gnome-extensions enable cursio@cursio.dev`)"
                .into()
        }
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
        {
            "Cursio's Shell extension on this machine is older than this node; install the \
             current one (`cargo xtask install`) and reload it"
                .into()
        }
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        _ => format!("cannot reach Cursio's Shell extension: {error}"),
    }
}

/// Call `method` on the extension with `body` from a thread outside any
/// runtime, decoding the reply as `R`.
fn call<B, R>(method: &'static str, body: B) -> Result<R, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType + Send + 'static,
    R: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type + Send + 'static,
{
    std::thread::Builder::new()
        .name("cursio-app-window".into())
        .spawn(move || {
            let bus = zbus::blocking::Connection::session()
                .map_err(|e| format!("no session bus: {e}"))?;
            let reply = bus
                .call_method(Some(NAME), PATH, Some(NAME), method, &body)
                .map_err(explain)?;
            reply
                .body()
                .deserialize::<R>()
                .map_err(|e| format!("the Shell extension answered {method} unreadably: {e}"))
        })
        .map_err(|e| format!("cannot start a thread to ask the Shell: {e}"))?
        .join()
        .map_err(|_| "asking the Shell extension panicked".to_string())?
}

/// The windows of the processes `pids`, as the Shell sees them.
pub fn windows_of(pids: &[u32]) -> Result<Vec<AppWindow>, String> {
    let found: Vec<(u32, String, u32, u32)> = call("AppWindows", (pids.to_vec(),))?;
    Ok(found
        .into_iter()
        .map(|(id, title, width, height)| AppWindow {
            id,
            title,
            width,
            height,
        })
        .collect())
}

/// The window raised to the top, as the PNG the Shell took of it.
pub fn still(id: u32) -> Result<Vec<u8>, String> {
    call("ShotAppWindow", (id,))
}

/// Click at `fx`, `fy` — fractions of the window's frame.
pub fn click(id: u32, fx: f32, fy: f32, right: bool) -> Result<(), String> {
    call("ClickAppWindow", (id, f64::from(fx), f64::from(fy), right))
}

/// Type `text` into the window's focused field; any script.
pub fn type_text(id: u32, text: &str) -> Result<(), String> {
    call("TypeAppWindow", (id, text.to_owned()))
}

/// Press and release the key with evdev code `evdev`.
pub fn key(id: u32, evdev: u16) -> Result<(), String> {
    call("KeyAppWindow", (id, u32::from(evdev)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desktop_without_the_extension_is_told_what_to_enable() {
        let missing = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("org.freedesktop.DBus.Error.ServiceUnknown")
                .unwrap(),
            Some("The name is not activatable".into()),
            zbus::message::Message::method_call("/", "Ping")
                .unwrap()
                .build(&())
                .unwrap(),
        );
        assert!(explain(missing).contains("gnome-extensions enable cursio@cursio.dev"));
    }

    #[test]
    fn the_extension_refusing_passes_its_own_words_on() {
        let refused = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("dev.cursio.Shell.Error.Refused").unwrap(),
            Some("that window is gone".into()),
            zbus::message::Message::method_call("/", "Ping")
                .unwrap()
                .build(&())
                .unwrap(),
        );
        assert_eq!(explain(refused), "that window is gone");
    }
}
