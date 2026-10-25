//! Which modules this node offers, built once from the settings and the
//! platform: the registry every link negotiates from.

use cursio_core::live::Shared;
use cursio_protocol::ServiceId;
use std::sync::Arc;

/// Serve the ScreenCast portal backend with the mesh's peers behind it, for
/// `--remote-screen` on Linux; `None` elsewhere, where views are windows.
#[cfg(target_os = "linux")]
pub(crate) fn start_screen_portal(
    remote_screen: bool,
) -> Option<cursio_core::screen_hub::ScreenHub> {
    if !remote_screen {
        return None;
    }
    let hub = cursio_core::screen_hub::ScreenHub::default();
    let screens: std::sync::Arc<dyn cursio_window::linux_portal::RemoteScreens> =
        std::sync::Arc::new(hub.clone());
    tokio::spawn(async move {
        if let Err(e) = cursio_window::linux_portal::serve(Some(screens)).await {
            tracing::warn!(
                error = %e,
                "peers' screens will not appear in screen-sharing dialogs; \
                 is the portal backend installed (`cursio portal install`)?"
            );
        }
    });
    Some(hub)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn start_screen_portal(
    _remote_screen: bool,
) -> Option<cursio_core::screen_hub::ScreenHub> {
    None
}

/// Window-transfer options, grouped for the same reason [`AudioOpts`] is: the
/// registry builder should gain a struct field per subsystem, not a positional
/// argument.
pub(crate) struct WindowOpts {
    /// Set when this node shares whatever an iPhone mirrors to it. A window of
    /// this machine's own moves by being dragged across the peer-facing edge,
    /// which needs no flag — the drag is the request.
    pub(crate) airplay: Option<cursio_core::window_session::AirPlayShare>,
    /// `--remote-screen`: offer each peer's screen to capture apps here.
    pub(crate) remote_screen: bool,
    /// `!--no-screen-host`: let peers view this machine's screen. Shared, so
    /// the app can change it on a running node.
    pub(crate) screen_host: Shared<bool>,
    /// Where the portal backend's requests reach the links (Linux).
    pub(crate) screen_hub: Option<cursio_core::screen_hub::ScreenHub>,
}

/// What the service module needs, gathered so `module_registry` takes one
/// parameter rather than three.
pub(crate) struct ServiceSetup {
    pub(crate) state: cursio_core::service_session::ServiceState,
    pub(crate) reasons: Arc<std::sync::Mutex<std::collections::HashMap<ServiceId, String>>>,
    pub(crate) make_host:
        Box<dyn Fn() -> Box<dyn cursio_core::service_session::ServiceHost> + Send + Sync>,
}

/// How this desktop shows a phone's notifications, where it can.
#[cfg(target_os = "linux")]
pub(crate) fn notify_display() -> Option<std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>>
{
    cursio_core::notify_display::FdoDisplay::start()
        .map(|d| d as std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>)
}

#[cfg(target_os = "windows")]
pub(crate) fn notify_display() -> Option<std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>>
{
    cursio_core::notify_toast::ToastDisplay::start()
        .map(|d| d as std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(crate) fn notify_display() -> Option<std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>>
{
    tracing::info!("a phone's notifications are not shown on this platform yet");
    None
}

/// Everything this node can run, assembled once at startup.
///
/// This is the only place that names the concrete modules. Adding one means
/// adding a factory here — not an `if` in the session path, a `#[cfg]` block, or
/// another parameter threaded through `run_pairing_and_session`.
///
/// Every factory is registered as able to run; whether the user wants it is
/// `switches`, which the app flips on a running node. A module switched on
/// later needs its factory to be here already.
///
/// Input is deliberately absent: it is not a `SessionModule` at all, but stays
/// hard-wired in `run_session` because its ownership/grab/edge logic is
/// intertwined with the loop state. It is still *advertised* (see
/// `advertise_with_input`), because the peer needs to know we have it.
pub(crate) fn module_registry(
    // Windows: render the peer's audio into this output device (by name).
    render_device: Shared<Option<String>>,
    mic_gate: Shared<cursio_core::audio_gate::MicGate>,
    mixer: cursio_core::mixer::LocalMixer,
    files_share: Shared<cursio_files::SharePolicy>,
    switches: cursio_core::module::ModuleSwitches,
    window: WindowOpts,
    services: ServiceSetup,
) -> cursio_core::module::ModuleRegistry {
    // Said once at startup rather than left to a peer that agreed to run audio
    // and then hears nothing: a flag that does nothing has to say so.
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    if switches.is_on(ModuleId::AUDIO) {
        tracing::warn!("no audio backend on this platform; --audio is not advertised to peers");
    }

    // One display for a phone's notifications and its calls: they are the
    // same notification server, and a call's buttons answer through it.
    let display = notify_display();
    if let Some(display) = &display {
        cursio_core::calls_display::start(display.clone());
    }
    #[cfg(feature = "calls")]
    cursio_core::call_audio::start();
    cursio_core::network_watch::start();
    // Players are read and shown over MPRIS. Windows' system media controls
    // are read but cannot be published to, so there the module only offers
    // this machine's players; elsewhere it is not advertised rather than
    // advertised and silent.
    #[cfg(target_os = "linux")]
    cursio_core::media_mpris::start();
    #[cfg(target_os = "linux")]
    let (media_plays, media_shows) = (true, true);
    #[cfg(target_os = "windows")]
    let (media_plays, media_shows) = (cursio_core::media_smtc::start(), false);
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let (media_plays, media_shows) = {
        tracing::warn!("sharing what plays is not implemented on this platform yet");
        (false, false)
    };

    cursio_core::module::ModuleRegistry::new()
        .with_switches(switches)
        .with(Box::new(cursio_core::audio_session::AudioFactory {
            enabled: true,
            render_device,
            mic_gate,
            mixer,
        }))
        .with(Box::new(cursio_core::clipboard_session::ClipboardFactory {
            enabled: true,
        }))
        .with(Box::new(cursio_core::share_session::ShareFactory {
            sink: cursio_core::share_sink::DesktopSink::shared(),
        }))
        .with(Box::new(cursio_core::notify_session::NotifyFactory {
            display: display.clone(),
            actor: None,
            rings: true,
        }))
        .with(Box::new(cursio_core::calls_session::CallsFactory {
            actor: None,
            computer: true,
        }))
        .with(Box::new(cursio_core::messages_session::MessagesFactory {
            store: None,
            view: true,
        }))
        .with(Box::new(cursio_core::media_session::MediaFactory {
            plays: media_plays,
            shows: media_shows,
            shows_only_computers: false,
        }))
        .with(Box::new(cursio_core::gallery_session::GalleryFactory {
            store: cursio_core::gallery_folder::FolderGallery::for_user()
                .map(|g| std::sync::Arc::new(g) as _),
            view: true,
        }))
        .with(Box::new(cursio_core::service_session::ServiceFactory {
            state: services.state,
            reasons: services.reasons,
            make_host: services.make_host,
        }))
        // Service goes before window so its per-peer offer is in shared state
        // before the AirPlay window trigger makes a placement decision.
        .with(Box::new(cursio_core::window_session::WindowFactory {
            airplay: window.airplay,
            remote_screen: window.remote_screen,
            screen_host: window.screen_host,
            screen_hub: window.screen_hub,
        }))
        // Registered everywhere, including the platforms with no mount backend.
        // Sharing is two independent halves and only one of them needs FUSE or
        // WinFsp: a Mac that cannot show the peer's disks can still serve its
        // own, and skipping the module entirely meant a Linux peer could not
        // reach them either — with `--files` accepted and silently doing
        // nothing.
        .with(Box::new(cursio_core::files_session::FilesFactory {
            enabled: true,
            share: files_share,
            role: cursio_protocol::ModuleRole::Both,
        }))
        // Like files, a machine both offers its own ports and attaches a
        // peer's. The factory carries the node-level state every link shares,
        // which is why it is constructed rather than built from fields.
        .with(Box::new(cursio_core::usb_session::UsbFactory::new(true)))
        .with(Box::new(cursio_core::update::lane::UpdateFactory))
}
