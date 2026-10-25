//! Building the window module for a link: which capture and renderer this
//! platform has, the remote-screen sources it offers, and the drag and AirPlay
//! triggers that start a transfer.

use crate::module::SessionModule;
use crate::remote_screen::ScreenHost;
use crate::screen_hub::ScreenHub;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use crate::window_session::TransferRequest;
use crate::window_session::WindowEngine;
use crate::window_session::VERSION;
use cursio_protocol::ModuleId;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use cursio_window::{WindowCapture, WindowRenderer};

/// Builds [`WindowEngine`] when window transfer is enabled *and* this platform
/// has a capture/render backend.
///
/// This is where window transfer's `#[cfg]` lives. `main` used to carry three
/// platform blocks to construct the same module three ways; now the platform
/// question is answered once, here, and a machine with no backend simply
/// advertises nothing.
pub struct WindowFactory {
    /// When set, this node shares whatever an iPhone is mirroring to it instead
    /// of one of its own windows. The two are alternatives rather than both at
    /// once: an engine hosts a single window per session.
    ///
    /// No option starts a *transfer*. A window of this machine's own transfers
    /// by being dragged across the peer-facing edge — the drag is the request,
    /// and a flag that meant the same thing was a second way to say it that had
    /// to agree with the first. The remote-screen options below are not that:
    /// a view is a different thing (see `remote_screen`), not a second way to
    /// start a transfer.
    #[cfg(feature = "airplay")]
    pub airplay: Option<AirPlayShare>,
    /// Offer every peer's windows and screens to this machine's capture apps
    /// (`--remote-screen`). Off by default: it asks each peer for its source
    /// list, and on Windows keeps an unseen view window per peer.
    pub remote_screen: bool,
    /// Where capture apps ask for a peer's source — set on Linux, where the
    /// view is a PipeWire node opened on request through Cursio's ScreenCast
    /// portal backend instead of a window kept open per peer.
    pub screen_hub: Option<ScreenHub>,
    /// Serve peers' remote-screen views of this machine. On by default and
    /// turned off by `--no-screen-host`; either way the module still
    /// negotiates, so a peer asking gets a refusal with a reason rather than
    /// silence. Shared so the app can change it; the node restarts the module
    /// on every link for the change to reach the engines already built.
    pub screen_host: crate::live::Shared<bool>,
}

/// Hosting views, viewing none, sharing no iPhone: what a phone runs. A
/// constructor rather than a literal because which fields exist depends on
/// the features this crate was built with, which a dependent cannot see.
impl Default for WindowFactory {
    fn default() -> Self {
        Self {
            #[cfg(feature = "airplay")]
            airplay: None,
            remote_screen: false,
            screen_hub: None,
            screen_host: crate::live::Shared::new(true),
        }
    }
}

impl WindowFactory {
    /// Give an engine this node's remote-screen configuration.
    ///
    /// Applied to every engine this factory builds, including an AirPlay one:
    /// hosting and viewing a screen are independent of which window, if any,
    /// this node is transferring.
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    fn with_screen<C: WindowCapture, R: WindowRenderer>(
        &self,
        engine: WindowEngine<C, R>,
        ctx: &crate::module::SessionCtx,
    ) -> WindowEngine<C, R> {
        let host = if self.screen_host.get() {
            screen_sources(&ctx.local.display_name)
        } else {
            ScreenHost::refusing(format!(
                "{} does not let peers view its screen",
                ctx.local.display_name
            ))
        };
        let mut engine = engine.with_screen_host(host);
        // A phone's screen is asked for from here (the menu, the app, `cursio
        // screen`), which reaches this link through the registry.
        if ctx.peer.device_kind == cursio_protocol::DeviceKind::Phone {
            engine = engine.with_screen_asks(crate::phone_screen::register_phone(ctx.peer_name()));
        }
        match (self.remote_screen, self.screen_hub.as_ref()) {
            (false, _) => engine,
            (true, Some(hub)) => {
                engine.with_screen_view_on_demand(hub.clone(), ctx.peer.peer_id.to_string())
            }
            (true, None) => engine.with_screen_view(),
        }
    }
}

/// This platform's remote-screen source list, or a refusal naming why not.
#[cfg(target_os = "linux")]
pub(super) fn screen_sources(machine: &str) -> ScreenHost {
    ScreenHost::serving(std::sync::Arc::new(cursio_window::PortalScreenSources {
        machine: machine.to_string(),
    }))
}

#[cfg(target_os = "windows")]
pub(super) fn screen_sources(_machine: &str) -> ScreenHost {
    ScreenHost::serving(std::sync::Arc::new(cursio_window::WgcScreenSources))
}

#[cfg(target_os = "macos")]
pub(super) fn screen_sources(machine: &str) -> ScreenHost {
    ScreenHost::refusing(format!(
        "{machine} is a Mac, and remote-screen sources are not implemented on macOS yet"
    ))
}

/// The receiver stream this node can offer through the window module.
#[cfg(feature = "airplay")]
#[derive(Clone)]
pub struct AirPlayShare {
    pub tap: cursio_airplay::MirrorTap,
    pub state: crate::service_session::ServiceState,
    pub local: cursio_protocol::PeerId,
    /// Preserve `--share-airplay`: offer to every peer, not only peers that
    /// explicitly asked for a local AirPlay window.
    pub force: bool,
}

/// Whether this build has a window-transfer backend at all.
///
/// macOS counts even though it can only host: a Mac that shares its screen with
/// a Windows peer is the whole point of the backend, and negotiation would skip
/// the module entirely if this said otherwise. Its renderer refuses politely,
/// so a peer offering *it* a window gets a clear error rather than silence.
/// A phone counts for the same reason: it hosts its screen and shows nothing.
pub(super) const SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "android"
));

/// What a window backend found, as this machine's problem list says it.
pub(super) fn report_notice(notice: &cursio_window::notice::Notice) {
    use cursio_window::notice::Notice;
    match notice {
        Notice::ShellExtensionOutdated { missing } => {
            crate::health::shell_extension_outdated(missing)
        }
        Notice::ShellExtensionMissing { detail } => crate::health::shell_extension_missing(detail),
        Notice::ShellExtensionCurrent => crate::health::shell_extension_current(),
        Notice::ScreenCastRefused { detail } => {
            crate::health::portal_refused("screen sharing", detail)
        }
        Notice::ScreenCastGranted => crate::health::portal_granted("screen sharing"),
    }
}

impl crate::module::ModuleFactory for WindowFactory {
    fn id(&self) -> ModuleId {
        ModuleId::WINDOW
    }

    /// Advertised by any machine that *could* show a window, not only by one
    /// that wants to send one.
    ///
    /// Negotiation is symmetric: a module runs only when both sides advertise
    /// it. Receiving is passive and costs an idle engine; offering stays the
    /// deliberate act of dragging a window across an edge.
    fn advert(&self) -> Option<cursio_protocol::ModuleAdvert> {
        // A phone only hosts (its own screen), so two phones have nothing to
        // show each other and do not run the module.
        let role = if cfg!(target_os = "android") {
            cursio_protocol::ModuleRole::Source
        } else {
            cursio_protocol::ModuleRole::Both
        };
        SUPPORTED.then(|| cursio_protocol::ModuleAdvert::new(ModuleId::WINDOW, VERSION, role))
    }

    #[allow(unused_variables)]
    fn build(&self, ctx: &crate::module::SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        cursio_window::notice::set_reporter(report_notice);
        #[cfg(feature = "airplay")]
        if let Some(share) = &self.airplay {
            share.state.note_offer(ctx.peer.peer_id, ctx.peer.service);
        }
        #[cfg(target_os = "android")]
        {
            let computer = ctx.peer_name().to_string();
            let phone = ctx.local.display_name.clone();
            let requests = crate::phone_screen::spawn_trigger(computer.clone(), phone.clone());
            let asked_by = computer.clone();
            let engine = WindowEngine::new(
                crate::phone_screen::PhoneScreenCapture::new(computer.clone(), phone.clone()),
                cursio_window::NoViewRenderer,
                computer,
            )
            .with_requests(requests)
            .on_screen_request(move |show| crate::phone_screen::requested(&asked_by, show))
            .with_screen_host(ScreenHost::serving(std::sync::Arc::new(
                crate::phone_screen::PhoneScreenSources { phone },
            )));
            Ok(Box::new(engine))
        }
        #[cfg(target_os = "linux")]
        {
            #[cfg(feature = "airplay")]
            if let Some(airplay) = self
                .airplay
                .clone()
                .filter(|share| share.force || ctx.peer.service.show_window)
            {
                return Ok(Box::new(self.with_screen(
                    airplay_engine(airplay, cursio_window::GtkWindowRenderer, ctx),
                    ctx,
                )));
            }
            // On Linux the portal runs its own window picker, so there is no
            // title to match and no window of ours to hide — both options are
            // Windows-only and deliberately ignored here.
            let (requests, ended) = spawn_drag_trigger(ctx.peer_edge);
            let engine = WindowEngine::new(
                cursio_window::PortalWindowCapture,
                cursio_window::GtkWindowRenderer,
                ctx.peer_name().to_string(),
            )
            .with_requests(requests)
            .with_transfer_ended(ended);
            Ok(Box::new(self.with_screen(engine, ctx)))
        }
        #[cfg(target_os = "windows")]
        {
            #[cfg(feature = "airplay")]
            if let Some(airplay) = self
                .airplay
                .clone()
                .filter(|share| share.force || ctx.peer.service.show_window)
            {
                return Ok(Box::new(self.with_screen(
                    airplay_engine(airplay, cursio_window::Win32WindowRenderer, ctx),
                    ctx,
                )));
            }
            // Hiding the source is what makes a transfer feel like the window
            // moved rather than been copied; the drag that started it is the
            // user saying they want it over there.
            let (requests, ended) = spawn_drag_trigger(ctx.peer_edge);
            let engine = WindowEngine::new(
                cursio_window::WgcWindowCapture {
                    title_hint: String::new(),
                    hide_source: true,
                },
                cursio_window::Win32WindowRenderer,
                ctx.peer_name().to_string(),
            )
            .with_requests(requests)
            .with_transfer_ended(ended);
            Ok(Box::new(self.with_screen(engine, ctx)))
        }
        #[cfg(target_os = "macos")]
        {
            #[cfg(feature = "airplay")]
            if let Some(airplay) = self
                .airplay
                .clone()
                .filter(|share| share.force || ctx.peer.service.show_window)
            {
                return Ok(Box::new(self.with_screen(
                    airplay_engine(airplay, cursio_window::MacWindowRenderer, ctx),
                    ctx,
                )));
            }
            let (requests, ended) = spawn_drag_trigger(ctx.peer_edge);
            let engine = WindowEngine::new(
                cursio_window::MacWindowCapture::default(),
                cursio_window::MacWindowRenderer,
                ctx.peer_name().to_string(),
            )
            .with_requests(requests)
            .with_transfer_ended(ended);
            Ok(Box::new(self.with_screen(engine, ctx)))
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "windows",
            target_os = "macos",
            target_os = "android"
        )))]
        {
            // Unreachable in practice: `advert` returns `None` here, so
            // negotiation never agrees on this module.
            anyhow::bail!("window transfer has no backend on this platform")
        }
    }
}

/// Watch for windows dragged toward the peer's edge and turn that into
/// [`TransferRequest`]s.
///
/// Lives here rather than in the binary because it is part of what window
/// transfer *is* on each platform — the same reason the capture backends do.
/// Returns an empty-forever channel if the platform can't detect drags, which
/// the engine handles as "no drag trigger attached".
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub(super) fn spawn_drag_trigger(
    peer_edge: Option<cursio_protocol::Edge>,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<TransferRequest>,
    tokio::sync::mpsc::UnboundedSender<()>,
) {
    use cursio_window::drag_trigger::{DragTrigger, TriggerAction};

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (ended_tx, mut ended_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let (mut watcher, screen) = (
            cursio_window::linux_drag::DragWatcher::spawn(),
            cursio_window::linux_drag::screen_size().await,
        );
        #[cfg(target_os = "windows")]
        let (mut watcher, screen) = (
            cursio_window::windows_drag::DragWatcher::spawn(),
            cursio_window::windows_drag::screen_size(),
        );
        #[cfg(target_os = "macos")]
        let (mut watcher, screen) = (
            cursio_window::macos_drag::DragWatcher::spawn(),
            cursio_window::macos_drag::screen_size(),
        );

        // Without a screen size there is no edge to compare against. Bail
        // rather than guess: a wrong bound would fire mid-screen.
        let Some(screen) = screen else {
            tracing::warn!("could not determine screen size; window drag detection is off");
            return;
        };
        if peer_edge.is_none() {
            tracing::info!(
                "this peer borders no edge of this screen, so dragging a window off one does \
                 nothing; configure distinct --at positions to enable window transfer"
            );
        }
        tracing::info!(
            ?peer_edge,
            width = screen.width,
            height = screen.height,
            "watching for windows dragged toward the peer"
        );

        let mut trigger = DragTrigger::new(peer_edge, screen);
        loop {
            let event = tokio::select! {
                // Biased so a transfer that just ended is recorded before the
                // drag that may already be under way is judged against it.
                biased;
                ended = ended_rx.recv() => {
                    if ended.is_none() {
                        break; // the session ended
                    }
                    trigger.transfer_ended();
                    continue;
                }
                event = watcher.next() => match event {
                    Some(event) => event,
                    None => break,
                },
            };

            // Re-read the geometry as each drag begins. A monitor unplugged or
            // a resolution changed mid-session moves the edge, and this is the
            // one moment where asking costs nothing anybody can feel.
            if matches!(event, cursio_window::drag::DragEvent::Started { .. }) {
                #[cfg(target_os = "linux")]
                let now = cursio_window::linux_drag::screen_size().await;
                #[cfg(target_os = "windows")]
                let now = cursio_window::windows_drag::screen_size();
                #[cfg(target_os = "macos")]
                let now = cursio_window::macos_drag::screen_size();
                if let Some(now) = now {
                    trigger.set_screen(now);
                }
            }

            let Some(action) = trigger.on_drag(&event) else {
                continue;
            };
            let request = match action {
                TriggerAction::SendToPeer { title } => TransferRequest::Send { title },
                TriggerAction::BringBack => TransferRequest::BringBack,
            };
            if tx.send(request).is_err() {
                break; // the session ended
            }
        }
    });

    (rx, ended_tx)
}

/// Build an engine that hosts the mirrored iPhone screen.
///
/// Sharing follows mesh placement rather than a drag: only the selected host
/// offers a surrogate, and a host or quality change closes and recreates it.
#[cfg(all(
    feature = "airplay",
    any(target_os = "linux", target_os = "windows", target_os = "macos")
))]
pub(super) fn airplay_engine<R: cursio_window::WindowRenderer>(
    share: AirPlayShare,
    renderer: R,
    ctx: &crate::module::SessionCtx,
) -> WindowEngine<crate::airplay_window::AirPlayWindowCapture, R> {
    let requests = spawn_airplay_trigger(share.state.clone(), share.local);
    WindowEngine::new(
        crate::airplay_window::AirPlayWindowCapture::for_mesh(share.tap, share.state),
        renderer,
        ctx.peer_name().to_string(),
    )
    .with_requests(requests)
}

#[cfg(all(
    feature = "airplay",
    any(target_os = "linux", target_os = "windows", target_os = "macos")
))]
pub(super) fn spawn_airplay_trigger(
    state: crate::service_session::ServiceState,
    local: cursio_protocol::PeerId,
) -> tokio::sync::mpsc::UnboundedReceiver<TransferRequest> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut last: Option<Option<cursio_protocol::Quality>> = None;
        let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            poll.tick().await;
            let placement = state.placement();
            let selected = placement.is_some_and(|placement| placement.host == local)
                || (state.was_asked_to_host(cursio_protocol::ServiceId::AIRPLAY)
                    && state.is_hosted_here(cursio_protocol::ServiceId::AIRPLAY));
            let wanted = selected.then(|| placement.and_then(|placement| placement.quality));
            if wanted == last {
                continue;
            }
            if last.is_some() && tx.send(TransferRequest::BringBack).is_err() {
                return;
            }
            if wanted.is_some()
                && tx
                    .send(TransferRequest::Send {
                        title: "Cursio (iPhone)".into(),
                    })
                    .is_err()
            {
                return;
            }
            last = wanted;
        }
    });
    rx
}

#[cfg(all(test, feature = "airplay"))]
mod tests {
    use super::*;
    use crate::window_session::TransferRequest;

    fn airplay_offer(
        fitness: cursio_protocol::Fitness,
        wanted: Option<cursio_protocol::Quality>,
    ) -> cursio_protocol::ServiceOffer {
        cursio_protocol::ServiceOffer {
            fitness,
            wanted,
            show_window: false,
        }
    }

    #[tokio::test]
    async fn airplay_window_transfer_follows_host_and_quality_changes() {
        let local = cursio_protocol::PeerId::from_u128(1);
        let peer = cursio_protocol::PeerId::from_u128(2);
        let state = crate::service_session::ServiceState::new();
        state.note_offer(local, airplay_offer(cursio_protocol::Fitness::Full, None));
        let mut requests = spawn_airplay_trigger(state.clone(), local);

        assert!(matches!(
            requests.recv().await,
            Some(TransferRequest::Send { .. })
        ));

        state.note_offer(
            peer,
            airplay_offer(
                cursio_protocol::Fitness::Full,
                Some(cursio_protocol::Quality::new(2560, 1440, 30)),
            ),
        );
        assert_eq!(requests.recv().await, Some(TransferRequest::BringBack));
        assert!(matches!(
            requests.recv().await,
            Some(TransferRequest::Send { .. })
        ));

        state.note_offer(local, airplay_offer(cursio_protocol::Fitness::No, None));
        assert_eq!(requests.recv().await, Some(TransferRequest::BringBack));
    }
}
