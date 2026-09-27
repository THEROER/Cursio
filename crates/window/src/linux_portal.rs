//! Cursio's ScreenCast portal backend: a capture app's own "share your screen"
//! lists the peers' windows and screens beside this machine's.
//!
//! Discord, OBS and browsers on GNOME get screen pixels in exactly one way —
//! `org.freedesktop.portal.ScreenCast`, which `xdg-desktop-portal` forwards to
//! whichever *backend* implements `org.freedesktop.impl.portal.ScreenCast`.
//! Selected per interface in `portals.conf` (`xdg-desktop-portal` ≥ 1.18
//! reads user-level definitions from `~/.local/share/xdg-desktop-portal`),
//! this backend becomes that one. A peer's source picked here is answered with
//! a PipeWire node of our own ([`PipeWireScreenSource`](crate::PipeWireScreenSource)),
//! which the portal hands the app exactly like one of Mutter's — no window is
//! ever drawn on this desktop.
//!
//! ## GNOME stays in charge of this machine
//!
//! Every request is also forwarded to `xdg-desktop-portal-gnome`, unchanged:
//! `CreateSession` and `SelectSources` always (so a GNOME session exists at
//! the same object path whatever the user picks), `Start` whenever the user
//! picks "this computer", whenever no peer offers anything (no extra dialog
//! then), and whenever the request carries GNOME's own restore data (an app
//! re-sharing what it shared last time).
//!
//! **RemoteDesktop sessions ride through here too**, and this is what makes
//! redirecting the interface safe for Cursio itself: `xdg-desktop-portal`
//! sends `ScreenCast.SelectSources` for a *RemoteDesktop* session to the
//! ScreenCast backend (`screen-cast.c`, 1.20.3: one `impl` proxy for both),
//! while `RemoteDesktop.Start` goes to GNOME's backend, which then starts
//! whatever sources *it* was told about. A session this backend did not
//! create is therefore forwarded verbatim — window transfer's combined
//! RemoteDesktop+ScreenCast session keeps working. `InputCapture` is a
//! different interface and never comes here.
//!
//! ## When Cursio is not running
//!
//! The daemon owns the bus name while it runs. When it does not, D-Bus starts
//! `cursio portal-relay`, the same backend with no peers, which forwards
//! everything to GNOME — screen sharing on the machine must not depend on
//! Cursio being up. The daemon takes the name over (`ReplaceExisting`) and
//! the relay, which allowed that, exits when it loses it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cursio_protocol::{ScreenSource, SourceId, SourceKind};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::linux_chooser::{self, Choice};

/// The bus name `xdg-desktop-portal` reaches this backend at.
pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.cursio";
/// GNOME's backend, which every local request is forwarded to.
const GNOME_BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.gnome";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_IFACE: &str = "org.freedesktop.impl.portal.ScreenCast";
const SESSION_IFACE: &str = "org.freedesktop.impl.portal.Session";

/// Portal response codes.
const RESPONSE_OK: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

/// ScreenCast source-type bits.
const SOURCE_MONITOR: u32 = 1;
const SOURCE_WINDOW: u32 = 2;

type Results = HashMap<String, OwnedValue>;

/// One connected peer's shareable sources, as the chooser lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerScreens {
    /// Stable key for the peer, handed back in [`RemoteScreens::show`].
    pub peer: String,
    pub name: String,
    pub sources: Vec<ScreenSource>,
}

/// A peer source now published as a local node.
pub struct ShownNode {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub kind: SourceKind,
    /// Identifies this showing, so a `stop` for a showing already replaced
    /// by a newer one on the same peer does not end the newer one.
    pub token: u64,
    /// Fires when the node goes away on its own (the peer left, the source
    /// closed), so the capture app is told its session ended.
    pub ended: tokio::sync::oneshot::Receiver<()>,
}

/// What the mesh offers this backend: the policy half lives in `cursio-core`.
pub trait RemoteScreens: Send + Sync + 'static {
    fn peers(&self) -> Vec<PeerScreens>;
    /// Show `source` of `peer` as a node; resolves once frames can flow.
    fn show(
        &self,
        peer: &str,
        source: SourceId,
    ) -> tokio::sync::oneshot::Receiver<Result<ShownNode, String>>;
    /// The capture app is done with showing `token` of `peer`.
    fn stop(&self, peer: &str, token: u64);
}

/// What the dialog lists, in order: this machine first, then each peer's
/// screens and windows of the kinds the app asked for. The second value maps
/// each line after the first to its peer source.
pub fn choices_for(peers: &[PeerScreens], types: u32) -> (Vec<Choice>, Vec<(String, SourceId)>) {
    let mut choices = vec![Choice {
        section: "This computer".into(),
        label: "Choose a screen or a window…".into(),
    }];
    let mut targets = Vec::new();
    for peer in peers {
        for kind in [SourceKind::Monitor, SourceKind::Window] {
            let wanted = match kind {
                SourceKind::Monitor => types & SOURCE_MONITOR != 0,
                SourceKind::Window => types & SOURCE_WINDOW != 0,
            };
            if !wanted {
                continue;
            }
            for source in peer.sources.iter().filter(|s| s.kind == kind) {
                let label = if source.picked_on_host {
                    format!("{} (chosen on {})", source.title, peer.name)
                } else {
                    source.title.clone()
                };
                choices.push(Choice {
                    section: peer.name.clone(),
                    label,
                });
                targets.push((peer.peer.clone(), source.id));
            }
        }
    }
    (choices, targets)
}

/// What a session of ours remembers between calls.
#[derive(Default)]
struct SessionState {
    /// `types` from `SelectSources`; monitors only when unset, as the spec says.
    types: u32,
    /// GNOME restore data came with `SelectSources`: the app is re-sharing
    /// something local, and GNOME alone can honour that.
    restoring_local: bool,
    /// The peer showing this session started, if it picked one.
    showing: Option<(String, u64)>,
}

struct Shared {
    screens: Option<Arc<dyn RemoteScreens>>,
    sessions: Mutex<HashMap<OwnedObjectPath, SessionState>>,
    gnome: zbus::Proxy<'static>,
    connection: zbus::Connection,
}

impl Shared {
    async fn forward(
        &self,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> (u32, Results) {
        match self.gnome.call::<_, _, (u32, Results)>(method, body).await {
            Ok(answer) => answer,
            Err(e) => {
                tracing::warn!(error = %e, method, "GNOME's ScreenCast backend refused a forwarded call");
                (RESPONSE_FAILED, Results::new())
            }
        }
    }

    fn ours(&self, session: &OwnedObjectPath) -> bool {
        self.sessions
            .lock()
            .map(|s| s.contains_key(session))
            .unwrap_or(false)
    }

    /// End a session of ours: GNOME's twin, the peer showing, the object.
    async fn close_session(&self, path: &OwnedObjectPath) {
        let state = self.sessions.lock().ok().and_then(|mut s| s.remove(path));
        if let (Some(screens), Some((peer, token))) =
            (self.screens.as_ref(), state.and_then(|s| s.showing))
        {
            screens.stop(&peer, token);
        }
        if let Ok(gnome_session) = zbus::Proxy::new(
            &self.connection,
            GNOME_BUS_NAME,
            path.as_str().to_owned(),
            SESSION_IFACE,
        )
        .await
        {
            let _ = gnome_session.call_method("Close", &()).await;
        }
        let _ = self
            .connection
            .object_server()
            .remove::<PortalSession, _>(path.as_str())
            .await;
    }
}

/// `org.freedesktop.impl.portal.ScreenCast` at the portal's object path.
struct ScreenCast {
    shared: Arc<Shared>,
    source_types: u32,
    cursor_modes: u32,
    version: u32,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let answer = self
            .shared
            .forward(
                "CreateSession",
                &(&handle, &session_handle, &app_id, &options),
            )
            .await;
        // Created even if GNOME's twin was not: a peer source needs nothing
        // from GNOME, and "this computer" then fails in GNOME's own words.
        let session = PortalSession {
            shared: self.shared.clone(),
            path: session_handle.clone(),
        };
        let exported = self
            .shared
            .connection
            .object_server()
            .at(session_handle.as_str(), session)
            .await;
        if let Err(e) = exported {
            tracing::warn!(error = %e, "could not export a screen-cast session");
            return (RESPONSE_FAILED, Results::new());
        }
        if let Ok(mut sessions) = self.shared.sessions.lock() {
            sessions.insert(session_handle, SessionState::default());
        }
        (RESPONSE_OK, answer.1)
    }

    async fn select_sources(
        &self,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let answer = self
            .shared
            .forward(
                "SelectSources",
                &(&handle, &session_handle, &app_id, &options),
            )
            .await;
        if !self.shared.ours(&session_handle) {
            // A RemoteDesktop session GNOME created: its sources are GNOME's
            // business entirely.
            return answer;
        }
        let types = options
            .get("types")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(SOURCE_MONITOR);
        let restoring_local = options.contains_key("restore_data");
        if let Ok(mut sessions) = self.shared.sessions.lock() {
            if let Some(state) = sessions.get_mut(&session_handle) {
                state.types = types;
                state.restoring_local = restoring_local;
            }
        }
        (RESPONSE_OK, Results::new())
    }

    async fn start(
        &self,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let forward = || async {
            self.shared
                .forward(
                    "Start",
                    &(&handle, &session_handle, &app_id, &parent_window, &options),
                )
                .await
        };
        let known = self.shared.sessions.lock().ok().and_then(|s| {
            s.get(&session_handle)
                .map(|st| (st.types, st.restoring_local))
        });
        let Some((types, restoring_local)) = known else {
            return forward().await;
        };
        let Some(screens) = self.shared.screens.clone() else {
            return forward().await;
        };
        let peers = screens.peers();
        let (choices, targets) = choices_for(&peers, types);
        if targets.is_empty() || restoring_local {
            return forward().await;
        }
        let picked = match linux_chooser::choose(&app_id, choices).await {
            Ok(Some(index)) => index,
            Ok(None) => return (RESPONSE_CANCELLED, Results::new()),
            Err(e) => {
                tracing::warn!(error = %e, "the screen chooser failed; offering this computer only");
                return forward().await;
            }
        };
        let Some((peer, source)) = picked.checked_sub(1).and_then(|i| targets.get(i)).cloned()
        else {
            return forward().await;
        };
        let shown = match screens.show(&peer, source).await {
            Ok(Ok(shown)) => shown,
            Ok(Err(reason)) => {
                tracing::warn!(%reason, "the peer could not show the chosen source");
                return (RESPONSE_FAILED, Results::new());
            }
            Err(_) => return (RESPONSE_FAILED, Results::new()),
        };
        if let Ok(mut sessions) = self.shared.sessions.lock() {
            if let Some(state) = sessions.get_mut(&session_handle) {
                state.showing = Some((peer, shown.token));
            }
        }
        let results = stream_results(&shown);
        let shared = self.shared.clone();
        let path = session_handle.clone();
        let ended = shown.ended;
        tokio::spawn(async move {
            if ended.await.is_ok() && shared.ours(&path) {
                if let Ok(emitter) = SignalEmitter::new(&shared.connection, path.as_str()) {
                    let _ = PortalSession::closed(&emitter).await;
                }
                shared.close_session(&path).await;
            }
        });
        (RESPONSE_OK, results)
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        self.source_types
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        self.cursor_modes
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        self.version
    }
}

/// `Start`'s answer for a peer node: one stream, no restore data (a peer
/// source is chosen afresh each time; restoring it would need the peer).
fn stream_results(shown: &ShownNode) -> Results {
    let source_type = match shown.kind {
        SourceKind::Monitor => SOURCE_MONITOR,
        SourceKind::Window => SOURCE_WINDOW,
    };
    let mut props: HashMap<&str, Value> = HashMap::new();
    props.insert("source_type", Value::U32(source_type));
    props.insert(
        "size",
        Value::from((shown.width as i32, shown.height as i32)),
    );
    let streams = vec![(shown.node_id, props)];
    let mut results = Results::new();
    if let Ok(value) = OwnedValue::try_from(Value::from(streams)) {
        results.insert("streams".into(), value);
    }
    results
}

/// `org.freedesktop.impl.portal.Session` for a session this backend created.
struct PortalSession {
    shared: Arc<Shared>,
    path: OwnedObjectPath,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl PortalSession {
    async fn close(&self) {
        let shared = self.shared.clone();
        let path = self.path.clone();
        // Removing this object from inside its own method call would wait on
        // itself; the removal runs once the call has returned.
        tokio::spawn(async move { shared.close_session(&path).await });
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// Serve the backend until the bus name is lost.
///
/// `screens` is `None` for the relay, which only forwards. The daemon takes
/// the name from a running relay; the relay gives it up and returns.
pub async fn serve(screens: Option<Arc<dyn RemoteScreens>>) -> Result<(), crate::WindowError> {
    use futures_util::StreamExt;
    use zbus::fdo::RequestNameFlags;

    let err = |what: &str, e: zbus::Error| crate::WindowError::Backend(format!("{what}: {e}"));
    let relay = screens.is_none();
    let connection = zbus::Connection::session()
        .await
        .map_err(|e| err("session bus", e))?;
    let gnome = zbus::Proxy::new(&connection, GNOME_BUS_NAME, PORTAL_PATH, SCREENCAST_IFACE)
        .await
        .map_err(|e| err("GNOME's ScreenCast backend", e))?;
    let source_types = gnome
        .get_property::<u32>("AvailableSourceTypes")
        .await
        .unwrap_or(SOURCE_MONITOR | SOURCE_WINDOW);
    let cursor_modes = gnome
        .get_property::<u32>("AvailableCursorModes")
        .await
        .unwrap_or(1 | 2);
    let version = gnome.get_property::<u32>("version").await.unwrap_or(5);
    let shared = Arc::new(Shared {
        screens,
        sessions: Mutex::new(HashMap::new()),
        gnome,
        connection: connection.clone(),
    });
    connection
        .object_server()
        .at(
            PORTAL_PATH,
            ScreenCast {
                shared,
                source_types,
                cursor_modes,
                version,
            },
        )
        .await
        .map_err(|e| err("export the ScreenCast backend", e))?;

    let dbus = zbus::fdo::DBusProxy::new(&connection)
        .await
        .map_err(|e| err("bus daemon", e))?;
    let mut lost = dbus
        .receive_name_lost()
        .await
        .map_err(|e| err("watch the bus name", e))?;
    let flags = if relay {
        RequestNameFlags::AllowReplacement | RequestNameFlags::DoNotQueue
    } else {
        RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue
    };
    connection
        .request_name_with_flags(BUS_NAME, flags)
        .await
        .map_err(|e| err("claim the portal backend's bus name", e))?;
    tracing::info!(relay, "ScreenCast portal backend serving");
    while let Some(signal) = lost.next().await {
        if signal.args().is_ok_and(|a| a.name() == BUS_NAME) {
            tracing::info!(relay, "the ScreenCast portal backend's bus name moved on");
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: SourceId, kind: SourceKind, title: &str) -> ScreenSource {
        ScreenSource {
            id,
            kind,
            title: title.into(),
            width: 1920,
            height: 1080,
            picked_on_host: false,
        }
    }

    fn lightmoon() -> PeerScreens {
        PeerScreens {
            peer: "lm".into(),
            name: "Lightmoon".into(),
            sources: vec![
                source(7, SourceKind::Window, "Firefox"),
                source(1, SourceKind::Monitor, "Screen 1"),
            ],
        }
    }

    #[test]
    fn this_computer_comes_first_then_each_peers_screens_before_windows() {
        let (choices, targets) = choices_for(&[lightmoon()], SOURCE_MONITOR | SOURCE_WINDOW);
        let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Choose a screen or a window…", "Screen 1", "Firefox"]
        );
        assert_eq!(targets, [("lm".to_string(), 1), ("lm".to_string(), 7)]);
        assert_eq!(choices[1].section, "Lightmoon");
    }

    #[test]
    fn an_app_asking_for_monitors_is_not_offered_windows() {
        let (_, targets) = choices_for(&[lightmoon()], SOURCE_MONITOR);
        assert_eq!(targets, [("lm".to_string(), 1)]);
    }

    #[test]
    fn with_no_peer_sources_only_this_computer_is_listed() {
        let (choices, targets) = choices_for(&[], SOURCE_MONITOR | SOURCE_WINDOW);
        assert_eq!(choices.len(), 1);
        assert!(targets.is_empty(), "no peer means GNOME's dialog, not ours");
    }
}
