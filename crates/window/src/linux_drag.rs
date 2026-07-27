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
//! **The extension is optional.** It is an accelerator for a feature that also
//! works from the command line (`--share-window-title`), so every failure here
//! — not installed, disabled, D-Bus unavailable, a version mismatch — must
//! degrade to "no drag detection" and never to a broken session. That is why
//! [`DragWatcher::spawn`] cannot fail: it reports what happened through the log
//! and simply yields no events.

use tokio::sync::mpsc;

use crate::drag::{DragEvent, PointerPos};

/// Receives [`DragEvent`]s for as long as it is held.
///
/// Dropping it stops the background listener.
pub struct DragWatcher {
    events: mpsc::UnboundedReceiver<DragEvent>,
    /// Dropped with the watcher, which ends the listener task.
    _cancel: tokio::sync::oneshot::Sender<()>,
}

impl DragWatcher {
    /// Start listening for drags, if the extension is there.
    ///
    /// Never fails: without the extension this returns a watcher that simply
    /// never yields, which is exactly how a machine with no drag detection
    /// should behave.
    pub fn spawn() -> Self {
        let (tx, events) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            tokio::select! {
                _ = listen(tx) => {}
                // The watcher was dropped; stop listening.
                _ = cancel_rx => {}
            }
        });

        Self {
            events,
            _cancel: cancel_tx,
        }
    }

    /// The next drag event, or `None` once the listener has stopped.
    pub async fn next(&mut self) -> Option<DragEvent> {
        self.events.recv().await
    }
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
    Some(crate::drag_trigger::ScreenBounds { width, height })
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

    tracing::info!(
        "watching for window drags (needs the Cursio GNOME extension; \
         without it, share a window with --share-window-title)"
    );

    use futures_util::StreamExt;
    while let Some(Ok(msg)) = stream.next().await {
        let Some(member) = msg.header().member().map(|m| m.to_string()) else {
            continue;
        };

        // A malformed signal means a version mismatch between daemon and
        // extension. Skip it rather than dying: the rest of the session is
        // still fine, and the log says what to reconcile.
        let event = match member.as_str() {
            "WindowDragStarted" => msg
                .body()
                .deserialize::<(String, i32, i32)>()
                .ok()
                .map(|(title, x, y)| DragEvent::Started {
                    title,
                    at: PointerPos { x, y },
                }),
            "WindowDragMoved" => msg
                .body()
                .deserialize::<(i32, i32)>()
                .ok()
                .map(|(x, y)| DragEvent::Moved {
                    at: PointerPos { x, y },
                }),
            "WindowDragEnded" => msg
                .body()
                .deserialize::<(i32, i32)>()
                .ok()
                .map(|(x, y)| DragEvent::Ended {
                    at: PointerPos { x, y },
                }),
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
