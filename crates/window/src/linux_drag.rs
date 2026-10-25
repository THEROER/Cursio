//! Watching for window drags on GNOME, via the Cursio Shell extension.
//!
//! The Linux half of the drag trigger. On Windows the daemon can ask the OS
//! directly whether a window is being moved; on Wayland it cannot, because a
//! client is not allowed to know that other windows exist. Every route was
//! checked on GNOME 49 and is closed: `Shell.Eval` is dev-mode only,
//! `Shell.Introspect.GetWindows` returns `AccessDenied`, and Mutter does not
//! implement `ext_foreign_toplevel_list` (wlroots compositors do — this
//! limitation is GNOME's, not Wayland's).
//!
//! So the information comes from code running *inside* the compositor: the
//! extension in `native/cursio-shell-extension/`, which re-publishes Mutter's
//! own grab-op signals on the session bus.
//!
//! **The extension is optional.** Its absence disables drag transfer but must
//! not break the rest of a session. That is why [`watch`] cannot
//! fail: it reports what happened through the log and simply yields no events.

use tokio::sync::mpsc;

use crate::drag::{DragEvent, DragWatcher, PointerPos};

/// Start listening for drags through the GNOME Shell extension. Without it
/// the watcher simply never yields.
pub fn watch() -> DragWatcher {
    DragWatcher::run(listen)
}

/// The desktop's total size, for deciding what counts as "at the edge".
///
/// Read from `org.gnome.Shell.Introspect`'s `ScreenSize` property, which —
/// unlike its `GetWindows` method — is readable without special permission.
/// Returns `None` off GNOME or when the property can't be read; the caller then
/// has no edge to test against and the drag trigger stays inert.
pub async fn screen_size() -> Option<crate::drag_trigger::ScreenBounds> {
    let conn = zbus::Connection::session().await.ok()?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.gnome.Shell.Introspect",
        "/org/gnome/Shell/Introspect",
        "org.gnome.Shell.Introspect",
    )
    .await
    .ok()?;

    let (width, height): (i32, i32) = proxy.get_property("ScreenSize").await.ok()?;
    // GNOME's logical screen always starts at the origin, however the monitors
    // are arranged, so unlike Windows and macOS there is nothing to read here.
    Some(crate::drag_trigger::ScreenBounds {
        x: 0,
        y: 0,
        width,
        height,
    })
}

/// The listener proper: connect, subscribe, and forward until the bus dies.
async fn listen(tx: mpsc::UnboundedSender<DragEvent>) {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "no session bus; window-drag detection off");
            return;
        }
    };

    // Subscribe before checking whether anyone owns the name, so an extension
    // enabled *after* the daemon started is still picked up: the match rule
    // outlives any particular owner.
    let mut stream = match zbus::MessageStream::for_match_rule(
        zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("dev.cursio.Shell")
            .expect("static interface name is valid")
            .path("/dev/cursio/Shell")
            .expect("static path is valid")
            .build(),
        &conn,
        None,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not watch for window drags");
            return;
        }
    };

    tracing::info!("watching for window drags through the Cursio GNOME extension");

    use futures_util::StreamExt;
    while let Some(Ok(msg)) = stream.next().await {
        let Some(member) = msg.header().member().map(|m| m.to_string()) else {
            continue;
        };

        // A malformed signal means a version mismatch between daemon and
        // extension. Skip it rather than dying: the rest of the session is
        // still fine, and the log says what to reconcile.
        let event = match member.as_str() {
            "WindowDragStarted" => {
                msg.body()
                    .deserialize::<(String, i32, i32)>()
                    .ok()
                    .map(|(title, x, y)| DragEvent::Started {
                        title,
                        at: PointerPos { x, y },
                    })
            }
            "WindowDragMoved" => {
                msg.body()
                    .deserialize::<(i32, i32)>()
                    .ok()
                    .map(|(x, y)| DragEvent::Moved {
                        at: PointerPos { x, y },
                    })
            }
            "WindowDragEnded" => {
                msg.body()
                    .deserialize::<(i32, i32)>()
                    .ok()
                    .map(|(x, y)| DragEvent::Ended {
                        at: PointerPos { x, y },
                    })
            }
            other => {
                tracing::debug!(signal = other, "unknown drag signal; ignoring");
                None
            }
        };

        match event {
            Some(e) => {
                if tx.send(e).is_err() {
                    return; // watcher dropped
                }
            }
            None => tracing::debug!(
                signal = %member,
                "drag signal did not match the expected shape; \
                 is the Cursio extension a different version?"
            ),
        }
    }
}
