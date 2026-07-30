//! `cursio` CLI — discovery, pairing, and sessions between machines on a LAN.
//!
//! **There is no server.** Every machine runs the same command, `mesh`, and
//! every node both accepts and dials: if one goes away the others keep talking
//! to each other directly and reconnect when it returns. Nothing is a hub.
//!
//! Subcommands:
//! - `mesh` — run a node: listen, dial every trusted peer, share input/audio/clipboard.
//! - `join` — one-time pairing with a new machine (the only place a password is used), then exit.
//! - `peers` — list peers discovered on the LAN and exit.
//!
//! Typical use: `cursio join` once per new machine, then `cursio mesh` on every
//! machine, every time. `join` reads the shared password from the terminal
//! without echoing it or exposing it in the process list.
//!
//! (`serve`/`connect` are gone: they encoded a client/server split that the mesh
//! model doesn't have, and keeping all three meant the same session logic lived
//! in three places — where a fix landed in one and not the others.)

use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use cursio_airplay::crypto::AppleCryptoProvider;
use cursio_airplay::{Receiver, ReceiverConfig, Resolution, VideoProfile};
use cursio_core::session::{self};
use cursio_core::NodeIdentity;
use cursio_transport::discovery;
use cursio_transport::discovery::DiscoveryConfig;
use cursio_transport::pairing::{self, Role, TrustStore};
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use cursio_transport::Transport;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(name = "cursio", about = "Cursio LAN session driver")]
struct Cli {
    /// Increase console logging detail (-v for debug, -vv for trace).
    ///
    /// May be placed before or after the subcommand. Explicit RUST_LOG and
    /// CURSIO_LOG_FILE values take precedence.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pair with a new machine, once, then exit.
    ///
    /// The password is only ever needed here: after this both machines hold
    /// each other's key and `mesh` connects them automatically. There is no
    /// long-running "client" mode — `mesh` is how you run Cursio.
    Join {
        /// Explicit peer address (skips discovery), e.g. 192.168.1.20:9000.
        #[arg(long)]
        addr: Option<SocketAddr>,
        /// How long to browse for a peer before giving up.
        #[arg(long, default_value_t = 10)]
        discover_timeout_secs: u64,
    },
    /// Run as a mesh node: listen *and* dial every trusted peer at once.
    ///
    /// No server. Each node keeps its own list of known machines (from the
    /// trust store), so one dying doesn't make the others forget it — they mark
    /// it offline, carry on with each other, and reconnect when it returns.
    ///
    /// New peers still join explicitly via `join`; the mesh only
    /// auto-dials machines already paired with.
    Mesh {
        /// TCP port to listen on. A **stable default** (not 0/OS-assigned) is
        /// deliberate: the peer caches our last address to reconnect when mDNS is
        /// slow or blocked, and a port that changed every restart made that cache
        /// point at a dead port — so a bounced node was never redialled. Pass 0
        /// explicitly for an ephemeral port.
        #[arg(long, default_value_t = 46073)]
        port: u16,
        /// Capture this machine's mouse/keyboard and share it with peers.
        #[arg(long, default_value_t = false)]
        send_input: bool,
        /// Which of this machine's screen edges borders the peer.
        #[arg(long, value_enum)]
        peer_edge: Option<EdgeArg>,
        /// Share audio with peers.
        #[arg(long, default_value_t = false)]
        audio: bool,
        /// Windows: render peer audio into the output device whose name
        /// contains this.
        #[arg(long)]
        audio_render_device: Option<String>,
        /// Share the clipboard with peers (text, images, and other formats).
        #[arg(long, default_value_t = false)]
        clipboard: bool,
        /// Share disks both ways: this machine's volumes appear on the peer,
        /// and the peer's appear here under `~/Cursio/<peer>/`.
        ///
        /// Mounted read-write, so anything either machine can reach, the other
        /// can change. Linux mounts with FUSE; Windows shares its drives but
        /// cannot yet mount the peer's.
        #[arg(long, default_value_t = false)]
        files: bool,
        /// Let windows move to the peer: drag one past the `--peer-edge` and it
        /// appears on the peer's desktop, while the app keeps running here.
        ///
        /// Dragging needs the Cursio GNOME extension on Linux (a Wayland client
        /// cannot see other windows being dragged); Windows needs nothing extra.
        /// Without it, `--share-window-title` still shares a window directly.
        #[arg(long, default_value_t = false)]
        share_window: bool,
        /// Which window to share, matched against its title (case-insensitive
        /// substring). Without this the foreground window is taken — which,
        /// when you start the daemon from a terminal, is that terminal. Ignored
        /// unless `--share-window` is given.
        #[arg(long, default_value = "")]
        share_window_title: String,
        /// Keep the shared window visible on this machine instead of hiding it.
        ///
        /// Hiding is what makes a transfer feel like the window *moved*, but it
        /// is also the most likely reason a capture produces no frames — so this
        /// separates "capture is broken" from "hiding broke the capture".
        #[arg(long, default_value_t = false)]
        keep_window_visible: bool,
        /// Accept a first-time pairing from another machine for the next ten
        /// minutes, using a password read from the terminal.
        ///
        /// Needed on the machine that is *waiting*: `cursio join` on the other
        /// one dials this node, and without an open window an unknown machine
        /// is refused before pairing can run.
        #[arg(long, default_value_t = false)]
        pair: bool,
        /// Decode iPhone mirroring video and show it in a native window.
        ///
        /// Without this flag the AirPlay RTSP control receiver still runs, but
        /// it does not create a video decoder or screen window.
        #[arg(long, visible_alias = "airplay", default_value_t = false)]
        airplay_screen: bool,
        /// Disable the AirPlay RTSP receiver completely.
        #[arg(long, default_value_t = false, conflicts_with = "airplay_screen")]
        no_airplay: bool,
        /// Override the name shown in the iPhone Screen Mirroring picker.
        #[arg(long)]
        airplay_name: Option<String>,
        /// AirPlay control port.
        #[arg(long, default_value_t = 7000)]
        airplay_port: u16,
        /// Pin AirPlay mDNS to the LAN interface with this address.
        #[arg(long)]
        airplay_interface: Option<IpAddr>,
        /// Target AirPlay output resolution.
        #[arg(long, value_enum, default_value_t = AirPlayResolutionArg::FullHd)]
        res: AirPlayResolutionArg,
        /// Target AirPlay output frame rate.
        #[arg(long, default_value_t = 60)]
        fps: u16,
    },
    /// List peers discovered on the LAN, then exit.
    Peers {
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
    },
}

/// Which of *this* machine's screen edges borders the peer, for edge-crossing
/// handoff. A clap-parseable mirror of [`cursio_protocol::Edge`]. When set,
/// shoving the shared cursor off this edge hands control to the peer (in
/// addition to the Ctrl+Alt+G hotkey). Only the Wayland capture backend detects
/// crossings today.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum EdgeArg {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<EdgeArg> for cursio_protocol::Edge {
    fn from(e: EdgeArg) -> Self {
        match e {
            EdgeArg::Left => cursio_protocol::Edge::Left,
            EdgeArg::Right => cursio_protocol::Edge::Right,
            EdgeArg::Top => cursio_protocol::Edge::Top,
            EdgeArg::Bottom => cursio_protocol::Edge::Bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AirPlayResolutionArg {
    #[value(name = "1080p")]
    FullHd,
    #[value(name = "2k")]
    Qhd,
}

impl From<AirPlayResolutionArg> for Resolution {
    fn from(resolution: AirPlayResolutionArg) -> Self {
        match resolution {
            AirPlayResolutionArg::FullHd => Self::FullHd,
            AirPlayResolutionArg::Qhd => Self::Qhd,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Console (info) + rotating file (debug) logging. `_log` must live for the
    // whole run — dropping it flushes and stops the file writer. See
    // `cursio_core::logging` for the filters and log-file location.
    let _log = cursio_core::logging::init_with_verbosity("cursio", cli.verbose);

    // Ask for admin rights before doing anything else, but only for `mesh` —
    // it's the mode that injects input, and Windows' UIPI silently discards
    // injected events aimed at higher-integrity windows (an elevated console,
    // Task Manager) unless we match them. `join` and `peers` never inject, so
    // prompting there would be a UAC dialog for nothing.
    //
    // Declining is fine and expected: `ensure_elevated` logs what will and won't
    // work and we carry on. Only `Relaunched` ends this process — an elevated
    // copy is taking over, and two nodes must not share the port and portal.
    if matches!(cli.command, Command::Mesh { .. })
        && cursio_core::elevate::ensure_elevated() == cursio_core::elevate::Elevation::Relaunched
    {
        return Ok(());
    }

    let identity = NodeIdentity::load_or_create()?;
    tracing::info!(
        peer_id = %identity.peer_id,
        name = %identity.display_name,
        "this node"
    );

    match cli.command {
        Command::Join {
            addr,
            discover_timeout_secs,
        } => {
            let password = read_pairing_password()?;
            join(identity, addr, &password, discover_timeout_secs).await
        }
        Command::Mesh {
            port,
            send_input,
            peer_edge,
            audio,
            audio_render_device,
            clipboard,
            files,
            share_window,
            share_window_title,
            keep_window_visible,
            pair,
            airplay_screen,
            no_airplay,
            airplay_name,
            airplay_port,
            airplay_interface,
            res,
            fps,
        } => {
            // Read before anything binds a port, so a mistyped password costs
            // nothing and the prompt is not buried under startup logging.
            let pairing = if pair {
                let password = read_pairing_password()?;
                tracing::info!(
                    minutes = PAIRING_WINDOW.as_secs() / 60,
                    "pairing window open; run `cursio join` on the other machine"
                );
                PairingWindow::open(password, PAIRING_WINDOW)
            } else {
                PairingWindow::closed()
            };
            // The CLI's only job for modules is to build the registry; from here
            // on nothing names a specific module.
            let modules = module_registry(
                &AudioOpts {
                    audio,
                    audio_render_device,
                },
                clipboard,
                files,
                share_window,
                share_window_title,
                keep_window_visible,
            );
            let mesh = run_mesh(
                identity.clone(),
                port,
                send_input,
                peer_edge.map(Into::into),
                modules,
                pairing,
            );
            if no_airplay {
                mesh.await
            } else {
                match bind_airplay(
                    &identity,
                    AirPlayOpts {
                        name: airplay_name,
                        port: airplay_port,
                        interface: airplay_interface,
                        resolution: res,
                        fps,
                        screen_enabled: airplay_screen,
                    },
                )
                .await
                {
                    Ok(receiver) => {
                        tokio::select! {
                            result = mesh => result,
                            result = receiver.serve() => result.map_err(Into::into),
                        }
                    }
                    // A busy port (e.g. macOS' own AirPlay receiver) must not
                    // take the whole mesh down with it.
                    Err(error) => {
                        tracing::warn!(%error, "AirPlay receiver unavailable, continuing without it");
                        mesh.await
                    }
                }
            }
        }
        Command::Peers { timeout_secs } => list_peers(identity, timeout_secs).await,
    }
}

struct AirPlayOpts {
    name: Option<String>,
    port: u16,
    interface: Option<IpAddr>,
    resolution: AirPlayResolutionArg,
    fps: u16,
    screen_enabled: bool,
}

async fn bind_airplay(identity: &NodeIdentity, options: AirPlayOpts) -> anyhow::Result<Receiver> {
    let profile = VideoProfile::new(options.resolution.into(), options.fps)?;
    if options.screen_enabled {
        let hardware = cursio_airplay::hardware::HardwareInfo::detect();
        for gpu in &hardware.gpus {
            let vram = gpu
                .dedicated_vram_bytes
                .map(|bytes| format!("{:.1} GiB", bytes as f64 / 1024_f64.powi(3)))
                .unwrap_or_else(|| "shared/unknown VRAM".to_string());
            tracing::info!(gpu = %gpu.name, %vram, "AirPlay GPU");
        }
        for warning in hardware.profile_warnings(profile) {
            tracing::warn!("{warning}");
        }
        for warning in
            cursio_airplay::video::dependency_warnings(cursio_airplay::video::VideoCodec::H264)
        {
            tracing::warn!("{warning}");
        }
    }
    let crypto = Arc::new(AppleCryptoProvider::new());
    let config = ReceiverConfig::new(
        options
            .name
            .unwrap_or_else(|| airplay_receiver_name(&identity.display_name)),
        identity.peer_id,
        SocketAddr::from(([0, 0, 0, 0], options.port)),
        profile,
    )?
    .with_pairing_public_key(crypto.public_key());
    let discovery = options
        .interface
        .map_or_else(DiscoveryConfig::default, DiscoveryConfig::pinned_to);
    let receiver = Receiver::bind_with_crypto(config, discovery, crypto)
        .await?
        .with_screen_enabled(options.screen_enabled);
    tracing::info!(
        address = %receiver.local_addr()?,
        screen = options.screen_enabled,
        "mesh AirPlay RTSP receiver is available"
    );
    Ok(receiver)
}

fn airplay_receiver_name(display_name: &str) -> String {
    let hostname = display_name
        .trim()
        .trim_end_matches('.')
        .trim_end_matches(".local");
    format!("Cursio - {}", sanitize_instance(hostname))
}

fn read_pairing_password() -> anyhow::Result<Zeroizing<String>> {
    let password = Zeroizing::new(rpassword::prompt_password("Pairing password: ")?);
    if password.is_empty() {
        anyhow::bail!("pairing password cannot be empty");
    }
    Ok(password)
}

/// Audio-share options for a session. When `audio` is set, audio runs as a
/// session module multiplexed onto the same encrypted channel as input: it no
/// longer replaces input sharing. Symmetric, like everything else in the mesh —
/// each node captures its own output and plays what it receives.
#[derive(Clone)]
struct AudioOpts {
    audio: bool,
    /// Windows: render the peer's audio into this output device (by name).
    audio_render_device: Option<String>,
}

/// Everything this node can run, assembled once from the CLI flags.
///
/// This is the only place that names the concrete modules. Adding one means
/// adding a factory here — not an `if` in the session path, a `#[cfg]` block, or
/// another parameter threaded through `run_pairing_and_session`.
///
/// Input is deliberately absent: it is not a `SessionModule` at all, but stays
/// hard-wired in `run_session` because its ownership/grab/edge logic is
/// intertwined with the loop state. It is still *advertised* (see
/// `advertise_with_input`), because the peer needs to know we have it.
fn module_registry(
    audio: &AudioOpts,
    clipboard: bool,
    files: bool,
    share_window: bool,
    share_window_title: String,
    keep_window_visible: bool,
) -> cursio_core::module::ModuleRegistry {
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut registry = cursio_core::module::ModuleRegistry::new()
        .with(Box::new(cursio_core::audio_session::AudioFactory {
            enabled: audio.audio,
            render_device: audio.audio_render_device.clone(),
        }))
        .with(Box::new(cursio_core::clipboard_session::ClipboardFactory {
            enabled: clipboard,
        }))
        .with(Box::new(cursio_core::window_session::WindowFactory {
            enabled: share_window,
            title_hint: share_window_title,
            keep_visible: keep_window_visible,
        }));

    // File sharing needs a mount implementation: FUSE on Linux, WinFsp on
    // Windows. A MinGW build has neither — `winfsp-sys` refuses that target —
    // so there the flag is accepted and does nothing, rather than failing a
    // session over a capability this binary genuinely cannot honour.
    #[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
    {
        registry = registry.with(Box::new(cursio_core::files_session::FilesFactory {
            enabled: files,
        }));
    }
    #[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
    let _ = files;

    registry
}

/// Run as a mesh node: listen and dial trusted peers simultaneously, forever.
///
/// This is *the* way to run Cursio — every node accepts and every node dials,
/// which is what "no server" means in practice. Peers come from the trust store,
/// so the set survives any of them being off; discovery only supplies their
/// current addresses.
///
/// Sessions are spawned on a [`tokio::task::LocalSet`], not `tokio::spawn`: the
/// input path shares its injector/cursor between closures via `Rc` and so isn't
/// `Send`. Keeping sessions single-threaded lets that stay exactly as it is.
async fn run_mesh(
    identity: NodeIdentity,
    port: u16,
    send_input: bool,
    peer_edge: Option<cursio_protocol::Edge>,
    modules: cursio_core::module::ModuleRegistry,
    pairing: PairingWindow,
) -> anyhow::Result<()> {
    let NodeBase {
        listener,
        _announcement,
        capture,
    } = start_node(&identity, port, send_input, peer_edge).await?;

    // One registry for the whole process, shared by every session. Sessions are
    // single-threaded by design, so `Rc` is the right sharing primitive here.
    let modules = std::rc::Rc::new(modules);
    for advert in modules.advertise() {
        tracing::info!(module = %advert.id, version = advert.version, "module available");
    }

    let registry = cursio_core::mesh::registry_from_trust(identity.peer_id)?;
    for peer in registry.borrow().all() {
        tracing::info!(peer = %peer.display_name, id = %peer.peer_id, "known peer");
    }

    // Discovery runs on its own thread (the mdns callback is blocking) and
    // reports sightings over a channel. A sighting only ever *refreshes* a
    // trusted peer's address — an unknown machine announcing itself is ignored,
    // so joining the mesh stays an explicit, password-gated act.
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let self_id = identity.peer_id;
    std::thread::Builder::new()
        .name("cursio-mesh-discovery".into())
        .spawn(move || {
            let config = cursio_core::link::discovery_config();
            if let Err(e) = discovery::browse_with(self_id, &config, move |peer| {
                let _ = seen_tx.send(peer);
            }) {
                tracing::warn!(error = %e, "discovery ended");
            }
        })?;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mut sweep = tokio::time::interval(cursio_core::mesh::sweep_interval());
            loop {
                tokio::select! {
                    // A peer dialled us. We don't know who until the handshake,
                    // so the session itself reports the identity back.
                    accepted = listener.accept() => {
                        let transport = match accepted {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(error = %e, "accept failed");
                                continue;
                            }
                        };
                        tracing::info!(peer = %transport.peer_addr(), "peer dialled in");
                        spawn_peer_session(
                            transport,
                            identity.clone(),
                            Role::Responder,
                            None,
                            registry.clone(),
                            pairing.clone(),
                            capture.clone(),
                            peer_edge,
                            modules.clone(),
                        );
                    }

                    // Discovery saw someone. Refresh their address if trusted.
                    Some(peer) = seen_rx.recv() => {
                        let known = registry.borrow_mut().note_seen(
                            peer.peer_id,
                            &peer.display_name,
                            peer.addr,
                        );
                        if known {
                            tracing::debug!(peer = %peer.display_name, addr = %peer.addr, "trusted peer seen");
                        } else {
                            tracing::trace!(peer = %peer.display_name, "ignoring untrusted peer");
                        }
                    }

                    // Time to dial whoever is known, offline, and ours to call.
                    _ = sweep.tick() => {
                        for target in cursio_core::mesh::next_dial_targets(self_id, &registry) {
                            let transport = match TcpTransport::connect(target.addr).await {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::debug!(
                                        peer = %target.display_name,
                                        error = %e,
                                        "dial failed; will retry"
                                    );
                                    // Back to Offline so the next sweep retries.
                                    registry.borrow_mut().mark_offline(target.peer_id);
                                    continue;
                                }
                            };
                            tracing::info!(peer = %target.display_name, addr = %target.addr, "dialled peer");
                            // Cache the address that just worked so we can redial
                            // this peer after a restart even if mDNS never
                            // announces it. Best-effort: a failed write must not
                            // stop the session.
                            if let Ok(mut store) = TrustStore::load() {
                                if let Err(e) = store.remember_addr(&target.peer_id, target.addr) {
                                    tracing::debug!(error = %e, "could not cache peer address");
                                }
                            }
                            spawn_peer_session(
                                transport,
                                identity.clone(),
                                Role::Initiator,
                                Some(target.peer_id),
                                registry.clone(),
                                pairing.clone(),
                                capture.clone(),
                                peer_edge,
                                modules.clone(),
                            );
                        }
                    }
                }
            }
        })
        .await
}

/// Run one peer session on the local task set, keeping the registry's view of
/// that peer up to date for its whole life.
///
/// `expected` is the peer's id when we dialled it; `None` for an inbound
/// connection, where the identity is only known after the handshake. Either way
/// the peer ends up `Offline` when the session finishes, so the next sweep can
/// reconnect it.
// Still over clippy's threshold at 8: what remains is genuinely per-session
// plumbing (transport, identity, role, registry, capture, edge), not module
// configuration. The five module flags that used to be here are gone.
#[allow(clippy::too_many_arguments)]
/// How long `--pair` stays willing to meet a machine it has never seen.
const PAIRING_WINDOW: Duration = Duration::from_secs(10 * 60);

/// A time-boxed willingness to pair with an unknown machine.
///
/// A mesh node refuses an inbound stranger outright: the trust store is the
/// only way in, which is what keeps joining the mesh an explicit act. That left
/// no way to *become* trusted, because the other machine's `cursio join` needs
/// this side to run its half of SPAKE2 — and a node with no password bails
/// before that. `--pair` opens this window so a first meeting can happen, and
/// the deadline closes it again so a node left running overnight is not
/// standing open to the LAN.
#[derive(Clone)]
struct PairingWindow(std::rc::Rc<std::cell::RefCell<Option<OpenPairingWindow>>>);

struct OpenPairingWindow {
    password: Zeroizing<String>,
    closes_at: std::time::Instant,
}

impl PairingWindow {
    fn closed() -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(None)))
    }

    fn open(password: Zeroizing<String>, for_: Duration) -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(Some(
            OpenPairingWindow {
                password,
                closes_at: std::time::Instant::now() + for_,
            },
        ))))
    }

    fn is_open(&self) -> bool {
        self.0
            .borrow()
            .as_ref()
            .is_some_and(|open| open.closes_at > std::time::Instant::now())
    }

    /// The password to offer an inbound peer, or empty once the window shut.
    ///
    /// An expired window drops the password rather than merely ignoring it, so
    /// it stops existing in the process at the moment it stops being usable.
    fn password(&self) -> Zeroizing<String> {
        let mut slot = self.0.borrow_mut();
        match slot.as_ref() {
            Some(open) if open.closes_at > std::time::Instant::now() => open.password.clone(),
            Some(_) => {
                *slot = None;
                tracing::info!("pairing window closed; unknown machines are refused again");
                Zeroizing::new(String::new())
            }
            None => Zeroizing::new(String::new()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_peer_session(
    transport: TcpTransport,
    identity: NodeIdentity,
    role: Role,
    expected: Option<cursio_protocol::PeerId>,
    registry: cursio_core::mesh::SharedRegistry,
    // Only consulted for an inbound connection: a peer we dialled is one we
    // already trust, so it never needs a password.
    pairing: PairingWindow,
    // Shared with every other mesh session: capture is one machine-wide
    // resource (one portal permission, one grab), so sessions take turns on it
    // rather than each building their own. See [`PersistentCapture`].
    capture: SharedCapture,
    // Ownership handoff doesn't need this — the topology lives in the shared
    // capture's edge forwarder, so sessions only see "hand off to the peer".
    // The *window drag* trigger does: it compares the pointer against the
    // peer-facing edge itself, independently of who owns the cursor.
    peer_edge: Option<cursio_protocol::Edge>,
    // Shared rather than cloned: the registry is built once per process and read
    // by every session. `Rc` because sessions are deliberately single-threaded
    // (see the mesh module's threading note).
    modules: std::rc::Rc<cursio_core::module::ModuleRegistry>,
) {
    tokio::task::spawn_local(async move {
        if let Some(id) = expected {
            registry
                .borrow_mut()
                .set_state(id, cursio_core::registry::PeerState::Online);
        }

        // Who we're actually talking to. For an inbound connection this stays
        // unknown until the handshake fires the callback below, so we track it
        // here and use it for the cleanup afterwards.
        let identified = std::rc::Rc::new(std::cell::RefCell::new(expected));
        let seen = identified.clone();
        let reg_for_cb = registry.clone();
        let pairing_for_cb = pairing.clone();
        let mut on_identified = move |id: cursio_protocol::PeerId, name: &str| {
            *seen.borrow_mut() = Some(id);
            let mut reg = reg_for_cb.borrow_mut();
            // An inbound peer we already trust is simply now online. One we
            // don't know is refused by the key lookup right after this, so it
            // never becomes a mesh member here.
            if reg.is_known(&id) {
                reg.set_state(id, cursio_core::registry::PeerState::Online);
                tracing::info!(peer = %name, "peer online");
            } else if pairing_for_cb.is_open() {
                tracing::info!(peer = %name, %id, "unknown machine dialled in; pairing it");
            } else {
                tracing::warn!(peer = %name, %id, "untrusted peer dialled in; not admitted");
            }
        };

        // Take the machine-wide capture for as long as this session runs. If
        // another peer session already holds it, `try_borrow_mut` fails and this
        // one runs without local input rather than panicking — there is only one
        // keyboard and mouse, so only one peer can be driven at a time.
        let mut borrowed = capture.try_borrow_mut().ok();
        let mut capture_for_session = borrowed.as_mut().and_then(|slot| slot.as_mut());
        if let Some(capture) = capture_for_session.as_mut() {
            while capture.events.try_recv().is_ok() {}
            while capture.commands.try_recv().is_ok() {}
        }

        // A dialled peer is already trusted, so its key comes from the store and
        // pairing is skipped. An inbound stranger is refused the same way —
        // unless `--pair` is holding a window open, which is the one path by
        // which a machine becomes trusted in the first place.
        let password = match role {
            Role::Responder => pairing.password(),
            Role::Initiator => Zeroizing::new(String::new()),
        };
        let result = run_pairing_and_session(
            transport,
            identity,
            &password,
            role,
            false,
            capture_for_session,
            peer_edge,
            &modules,
            Some(&mut on_identified),
        )
        .await;

        match &result {
            Ok(()) => tracing::info!("peer session ended"),
            Err(e) => tracing::warn!(error = %e, "peer session ended with error"),
        }

        // Offline, never removed: the machine still exists, it's just not here
        // right now. This is the whole point of the registry.
        let who = *identified.borrow();
        if let Some(id) = who {
            registry.borrow_mut().mark_offline(id);
        }
    });
}

/// Pair with a new machine, then exit.
///
/// This is a **setup** step, not a way to run Cursio: joining is the one moment
/// a password is needed, and it happens once per machine. Afterwards both sides
/// hold each other's key in the trust store, and `mesh` dials the peer on its
/// own — so there is no "client" mode to keep running here.
///
/// Deliberately one-shot: no reconnect loop, no capture, no session modules.
/// Running the pairing session for real would duplicate `mesh`, which is what
/// the old `serve`/`connect` split did.
async fn join(
    identity: NodeIdentity,
    addr: Option<SocketAddr>,
    password: &str,
    discover_timeout_secs: u64,
) -> anyhow::Result<()> {
    // Find the peer: an explicit address wins, otherwise browse the LAN.
    let target = match addr {
        Some(a) => a,
        None => {
            tracing::info!("browsing the LAN for a peer to pair with");
            match cursio_core::link::discover_one(
                identity.peer_id,
                Duration::from_secs(discover_timeout_secs),
            )
            .await
            {
                Some(peer) => {
                    tracing::info!(peer = %peer.display_name, addr = %peer.addr, "discovered peer");
                    peer.addr
                }
                None => {
                    anyhow::bail!(
                        "no peer found on the LAN; start `cursio mesh` on the other machine, \
                         or pass --addr"
                    );
                }
            }
        }
    };

    let mut transport = TcpTransport::connect(target).await?;
    tracing::info!(addr = %target, "connected to peer");

    // Hello first, so we know who we're pairing with before spending a password
    // on them. Pairing is a one-shot: it runs no modules, so it advertises none.
    let peer =
        session::handshake(&mut transport, &identity, session::local_device_kind(), &[]).await?;
    tracing::info!(peer = %peer.display_name, id = %peer.peer_id, "handshake ok");

    let mut store = TrustStore::load()?;
    if store.get(&peer.peer_id).is_some() {
        tracing::info!(
            peer = %peer.display_name,
            "already paired with this machine; nothing to do — run `cursio mesh` on both"
        );
        return Ok(());
    }

    // SPAKE2 over the plaintext transport: safe, because the password itself is
    // never sent. Both sides end up holding the same shared key.
    let paired = pairing::run_pairing(
        &mut transport,
        password,
        Role::Initiator,
        (peer.peer_id, peer.display_name.clone()),
    )
    .await?;
    store.add(&paired)?;
    tracing::info!(
        peer = %peer.display_name,
        "paired — run `cursio mesh` on both machines and they will find each other"
    );
    Ok(())
}

use cursio_core::link::IdentifiedCallback;

/// Shared tail of `serve`/`connect`: Hello handshake, pair on first meeting,
/// bring up a Noise-encrypted channel, then run heartbeats over it.
///
/// The Hello handshake and pairing run over the plaintext transport — that's
/// safe, since SPAKE2 never sends the password. Once we hold the shared key
/// (freshly paired, or recovered from the trust store), we wrap the transport
/// in [`EncryptedTransport`] so all subsequent traffic (heartbeats today; input
/// and video later) is confidential and integrity-protected.
// Down from 13 arguments to 9, and the rest are session plumbing rather than
// per-module knobs — a new module adds nothing here.
#[allow(clippy::too_many_arguments)]
async fn run_pairing_and_session(
    transport: TcpTransport,
    identity: NodeIdentity,
    password: &str,
    role: Role,
    test_input: bool,
    // Capture is created ONCE by the caller and lent to each peer session — see
    // [`PersistentCapture`] for why it must not be rebuilt per connection.
    capture: Option<&mut PersistentCapture>,
    // Cursor *ownership* handoff doesn't need this: that topology is baked into
    // the shared capture's edge forwarder, so a session only sees "hand off to
    // the peer". The window-drag trigger does need it — it compares the pointer
    // against the peer-facing edge itself, independently of who owns the cursor.
    peer_edge: Option<cursio_protocol::Edge>,
    // Everything this node can run. Built once by the caller; a session takes
    // what negotiation agreed on. Replaces the five per-module flags that used
    // to be threaded through here.
    modules: &cursio_core::module::ModuleRegistry,
    on_identified: Option<&mut IdentifiedCallback<'_>>,
) -> anyhow::Result<()> {
    // Handshake, pairing, Noise and module negotiation are the same five steps
    // for every node, so they live in `link` — the app and a phone run this
    // exact sequence rather than a second copy of it.
    let link = cursio_core::link::establish(
        transport,
        cursio_core::link::Handshake {
            identity: &identity,
            password,
            role,
            registry: modules,
            peer_edge,
            sends_input: capture.is_some(),
        },
        on_identified,
    )
    .await?;
    let peer = link.peer;
    let mut secure = link.transport;
    let modules = link.modules;

    // Test mode: stream a synthetic mouse pattern to the peer, then finish.
    // Lets us verify remote injection before the capture side exists.
    if test_input {
        tracing::info!("sending synthetic test input (a square) to the peer");
        send_test_pattern(secure.as_mut()).await?;
        let _ = secure.send(&cursio_protocol::Message::Bye).await;
        tracing::info!("test input sent; closing");
        return Ok(());
    }

    // Set up an input injector so the peer's input events replay on this
    // machine. On platforms without a backend yet, events are just logged.
    //
    // Shared (Rc/RefCell) because two closures below need it: one injects peer
    // events, the other releases whatever is still held when ownership changes.
    // Both run on this single-threaded async context, never concurrently.
    let injector = std::rc::Rc::new(std::cell::RefCell::new(make_injector()));

    // Optionally capture this machine's real mouse/keyboard. When capturing we
    // also get a grab control (freeze local input while we drive the peer) and
    // we watch the captured stream for the ownership hotkey.
    // The hotkey filter and edge-crossing forwarder are already running inside
    // the persistent capture, so a session just borrows their outputs.
    let (captured, grab, commands) = match capture {
        Some(cap) => (
            Some(&mut cap.events),
            Some(cap.grab.clone()),
            Some(&mut cap.commands),
        ),
        None => (None, None, None),
    };

    // Cursor control: hide our own cursor while we're dormant.
    // Shared for the same reason as the injector: the ownership closure needs it,
    // and so does the cleanup after the session ends.
    let cursor = std::rc::Rc::new(std::cell::RefCell::new(make_cursor()));

    // Who starts with the shared cursor was settled while the link came up —
    // see `CursorSetup`, which weighs what sort of machine the peer is before
    // falling back to comparing ids.
    let cursor_setup = link.cursor;

    tracing::info!(
        start_active = cursor_setup.start_active,
        peer_kind = ?peer.device_kind,
        peer_takes_cursor = cursor_setup.peer_takes_cursor,
        "session up — sharing input; press Ctrl+Alt+G to toggle control (Ctrl-C to stop)"
    );
    let inject_ref = injector.clone();
    let release_ref = injector.clone();
    let grab_ref = grab.clone();
    let cursor_ref = cursor.clone();
    let result = session::run_session(
        secure,
        cursor_setup,
        captured,
        move |event| inject(inject_ref.borrow_mut().as_mut(), event),
        commands,
        move |ownership| {
            // Whatever the peer was holding down stops being ours to finish the
            // moment control moves: the matching key-up/button-up will be
            // delivered to the *other* machine, so anything still pressed here
            // would stay pressed forever. A stuck right-button opens a context
            // menu that swallows input on both machines; a stuck left-button is
            // worse. Release before doing anything else with the new state.
            if let Some(inj) = release_ref.borrow_mut().as_mut() {
                if let Err(e) = inj.release_all() {
                    tracing::warn!(error = %e, "failed to release held input on ownership change");
                }
            }
            if ownership.panic {
                // Emergency: drop to a safe idle. Barriers fully down (not just
                // ungrabbed-but-armed) so nothing can re-capture, and show the
                // cursor. The hotkey path already set Idle directly; do it again
                // idempotently in case this arrives first.
                if let Some(g) = grab_ref.as_ref() {
                    g.set_mode(cursio_input::InputMode::Idle);
                }
                set_cursor_hidden(cursor_ref.borrow_mut().as_mut(), false);
                tracing::warn!("panic recovery: input returned to this machine, barriers down");
                return;
            }
            // Dormant => grab local input (freeze it here) and hide our cursor,
            // since our input now acts on the peer. Active => release + show.
            let dormant = !ownership.active;
            if let Some(g) = grab_ref.as_ref() {
                g.set_grabbed(dormant);
            }
            set_cursor_hidden(cursor_ref.borrow_mut().as_mut(), dormant);
            tracing::info!(active = ownership.active, "ownership changed");
        },
        modules,
    )
    .await;

    // TEMP hang-diagnosis (2026-07-21): if this line never prints after a link
    // drop, the hang is inside run_session (its teardown or dropping the modules,
    // e.g. an audio thread's blocking join). Remove once fixed.
    tracing::info!("run_session returned to supervisor; cleaning up session");

    // Session over (peer left, link dropped, or error): release any keys/buttons
    // still held via the injector, so a key that was down when the connection
    // died doesn't stick on this machine.
    if let Some(inj) = injector.borrow_mut().as_mut() {
        if let Err(e) = inj.release_all() {
            tracing::warn!(error = %e, "failed to release held input on session end");
        }
    }
    tracing::info!("released held input; parking capture");
    // Park the capture, but do NOT tear it down: it outlives this session now.
    //
    // This used to send `Shutdown`, because each session built its own capture
    // and leaving one running made the next a duplicate (two Wayland portal
    // sessions both reporting the same barrier hit — one crossing arrived four
    // times after four reconnects). With a single [`PersistentCapture`] owned by
    // the caller that reasoning inverts: killing the backend here would end the
    // portal session we deliberately keep, so the next peer would have to
    // re-acquire it — the permission re-prompt and reconnect hang this design
    // exists to avoid.
    //
    // `Idle` is the right resting state: no grab, no barriers, backend alive and
    // ready for the next peer.
    if let Some(g) = grab.as_ref() {
        g.set_mode(cursio_input::InputMode::Idle);
    }
    set_cursor_hidden(cursor.borrow_mut().as_mut(), false);

    match result {
        Ok(()) | Err(session::SessionError::PeerLeft) => {
            tracing::info!("session ended");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

// Ownership hotkey = Ctrl + Alt + G held together. evdev keycodes (the neutral
// namespace both platforms map to). We accept either left or right Ctrl/Alt so
// laptops without a right Ctrl still work.
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
/// The non-modifier "trigger" key of the combo. `G` for "grab".
const KEY_G: u16 = 34;
/// Panic / emergency-release trigger: **Ctrl+Alt+Esc**. Slams control back to
/// this machine no matter the session state (evdev `KEY_ESC` = 1).
const KEY_ESC: u16 = 1;

/// Spawn a task that relays captured events to a new channel, but watches for
/// the ownership hotkey (**Ctrl+Alt+G**) and emits an [`OwnershipCmd`] instead
/// of forwarding the trigger key. Returns the downstream event receiver the
/// session should consume.
///
/// Combo (not a double-tap) so it can't fire by accident and doesn't depend on
/// tap timing. Modifier keys are still forwarded to the peer (they're needed in
/// normal use); only the `G` press that completes the combo is swallowed, so a
/// stray "g" doesn't reach the peer. Toggling alternates Take/Release; the
/// session ignores a redundant one, so we don't track remote state here.
fn spawn_hotkey_filter(
    mut upstream: tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::InputEvent>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<session::OwnershipCmd>,
    grab: cursio_input::GrabControl,
) -> tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::InputEvent> {
    use cursio_protocol::InputEvent;

    let (down_tx, down_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        // Track whether each modifier side is currently held.
        let mut ctrl = false;
        let mut alt = false;
        // Set once the combo fires, so the modifier RELEASES that follow are
        // swallowed too.
        //
        // Without this the peer is left holding Ctrl+Alt forever: we forward
        // Ctrl↓ and Alt↓ (they look like ordinary keys until G arrives), then G
        // flips ownership to us — and from that moment we no longer forward
        // anything, so Ctrl↑/Alt↑ never reach the peer. The peer's injector
        // reports it as `releasing held keys ... keys=2` only at session end;
        // meanwhile every click there is a Ctrl+click and the mouse looks
        // broken. That was the "нажал и всё зависло" report.
        let mut swallow_until_release = false;
        while let Some(event) = upstream.recv().await {
            if !grab.mode().forwards_events() {
                ctrl = false;
                alt = false;
                swallow_until_release = false;
                continue;
            }
            if let InputEvent::Key { key, pressed } = event {
                match key {
                    KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                        ctrl = pressed;
                        // Tail of a fired combo: drop the release, and end the
                        // swallow once both modifiers are up.
                        if swallow_until_release {
                            if !pressed && !alt {
                                swallow_until_release = false;
                            }
                            continue;
                        }
                    }
                    KEY_LEFTALT | KEY_RIGHTALT => {
                        alt = pressed;
                        if swallow_until_release {
                            if !pressed && !ctrl {
                                swallow_until_release = false;
                            }
                            continue;
                        }
                    }
                    KEY_G if pressed && ctrl && alt => {
                        // Combo complete: request a handoff and swallow this G.
                        // The session decides direction (only the active side
                        // hands off), so we just send a Toggle.
                        tracing::info!("ownership hotkey (Ctrl+Alt+G)");
                        let _ = cmd_tx.send(session::OwnershipCmd::Toggle);
                        // Swallow the modifier releases too — see the flag's
                        // declaration for why the peer would otherwise be stuck
                        // holding Ctrl+Alt.
                        swallow_until_release = true;
                        continue; // don't forward the trigger key to the peer
                    }
                    KEY_ESC if pressed && ctrl && alt => {
                        // PANIC: recover control locally RIGHT NOW, without
                        // waiting on the session. Dropping to Idle here ungrabs
                        // local input and (on Wayland) tears the portal barriers
                        // down, so this machine is usable even if the peer/link
                        // is wedged. Also ask the session to flip ownership +
                        // restore the cursor, which works if it's still alive.
                        tracing::warn!("PANIC hotkey (Ctrl+Alt+Esc): releasing locally");
                        grab.set_mode(cursio_input::InputMode::Idle);
                        let _ = cmd_tx.send(session::OwnershipCmd::Panic);
                        swallow_until_release = true;
                        continue; // don't forward Esc to the peer
                    }
                    _ => {}
                }
            }
            if down_tx.send(event).is_err() {
                break; // session gone
            }
        }
    });
    down_rx
}

/// Spawn a task that turns capture-reported edge crossings into ownership
/// handoffs on the session's command channel — but **only** for crossings on
/// `peer_edge`, the edge that actually borders the peer. A crossing on any other
/// edge (a screen edge with no peer beyond it) is dropped, so the cursor just
/// stops there as usual.
///
/// This is where the screen topology lives; the session stays topology-agnostic
/// (it only sees "hand off to the peer"). With no `peer_edge` configured, edge
/// crossings are ignored entirely (the receiver still drains so the channel
/// doesn't back up). Shares `cmd_tx` with the hotkey filter: both are just ways
/// to ask for the same handoff.
fn spawn_edge_handoff(
    mut edges: tokio::sync::mpsc::UnboundedReceiver<cursio_input::EdgeCrossing>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<session::OwnershipCmd>,
    peer_edge: Option<cursio_protocol::Edge>,
) {
    tokio::spawn(async move {
        while let Some(crossing) = edges.recv().await {
            match peer_edge {
                Some(edge) if edge == crossing.edge => {
                    tracing::info!(?edge, pos = crossing.pos_norm, "edge crossing toward peer");
                    let cmd = session::OwnershipCmd::HandoffToPeer {
                        edge: crossing.edge,
                        pos_norm: crossing.pos_norm,
                    };
                    if cmd_tx.send(cmd).is_err() {
                        break; // session gone
                    }
                }
                _ => tracing::trace!(?crossing, "edge crossing away from peer; ignoring"),
            }
        }
    });
}

/// Stream a synthetic mouse pattern (a square, then a left click) to the peer
/// as `Message::Input` events, so remote injection can be tested before the
/// capture side exists. The peer's cursor should trace the square.
async fn send_test_pattern<T: Transport + ?Sized>(transport: &mut T) -> anyhow::Result<()> {
    use cursio_protocol::{InputEvent, Message, MouseButton};

    let step = 20;
    let reps = 5; // 5 * 20 = 100px per side
    let legs = [(step, 0), (0, step), (-step, 0), (0, -step)];
    for (dx, dy) in legs {
        for _ in 0..reps {
            transport
                .send(&Message::Input(InputEvent::PointerMotion { dx, dy }))
                .await?;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    // A left click to round it off.
    transport
        .send(&Message::Input(InputEvent::PointerButton {
            button: MouseButton::Left,
            pressed: true,
        }))
        .await?;
    transport
        .send(&Message::Input(InputEvent::PointerButton {
            button: MouseButton::Left,
            pressed: false,
        }))
        .await?;
    Ok(())
}

/// Create the platform input injector, or `None` if unavailable / unsupported.
///
/// Linux uses a uinput virtual pointer; Windows uses `SendInput`. On any other
/// platform there's no backend yet, so injection falls back to logging (see
/// [`inject`]).
fn make_injector() -> Option<Box<dyn cursio_input::InputInjector>> {
    #[cfg(target_os = "linux")]
    {
        use cursio_input::InputInjector as _;
        match cursio_input::UinputInjector::new() {
            Ok(inj) => {
                tracing::info!(backend = inj.backend(), "input injector ready");
                Some(Box::new(inj))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no input injector; peer input will be logged only");
                None
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use cursio_input::InputInjector as _;
        match cursio_input::WinInjector::new() {
            Ok(inj) => {
                tracing::info!(backend = inj.backend(), "input injector ready");
                Some(Box::new(inj))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no input injector; peer input will be logged only");
                None
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        tracing::warn!("no input backend on this platform yet; peer input will be logged only");
        None
    }
}

/// Create the platform cursor controller (hide/show our own cursor), or `None`
/// if unsupported. Linux is currently a no-op backend (Wayland can't hide; X11
/// XFixes not wired yet); Windows really hides the cursor.
fn make_cursor() -> Option<Box<dyn cursio_input::CursorControl>> {
    #[cfg(target_os = "linux")]
    {
        use cursio_input::CursorControl as _;
        match cursio_input::LinuxCursor::new() {
            Ok(c) => {
                tracing::info!(backend = c.backend(), "cursor control ready");
                Some(Box::new(c))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no cursor control");
                None
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use cursio_input::CursorControl as _;
        match cursio_input::WinCursor::new() {
            Ok(c) => {
                tracing::info!(backend = c.backend(), "cursor control ready");
                Some(Box::new(c))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no cursor control");
                None
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Hide (`true`) or show (`false`) the local cursor through the controller,
/// logging any backend error.
fn set_cursor_hidden(cursor: Option<&mut Box<dyn cursio_input::CursorControl>>, hidden: bool) {
    if let Some(c) = cursor {
        let res = if hidden { c.hide() } else { c.show() };
        if let Err(e) = res {
            tracing::warn!(error = %e, hidden, "cursor hide/show failed");
        }
    }
}

/// Local input capture, set up once and reused across peer sessions.
///
/// Capture is a property of this *machine*, not of any one peer link: on Wayland
/// starting it means asking the portal for an InputCapture session, which
/// prompts the user for permission. Rebuilding it per connection re-prompted on
/// every reconnect, and the new request raced the old session's teardown — the
/// portal still saw the previous session holding capture, so the request hung
/// and edge-crossing stopped working until the process was restarted.
///
/// So the capture task, the hotkey filter and the edge-crossing forwarder all
/// live here, outliving individual sessions. Each session borrows the two
/// receivers for as long as it runs.
struct PersistentCapture {
    /// Captured input events, hotkey presses already filtered out.
    events: tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::InputEvent>,
    /// Ownership requests, from either the hotkey or an edge crossing.
    commands: tokio::sync::mpsc::UnboundedReceiver<session::OwnershipCmd>,
    /// Freeze/release local input.
    grab: cursio_input::GrabControl,
}

/// Everything a node needs before it can talk to anyone: a bound listener, an
/// mDNS announcement, and local input capture.
///
/// Shared by every entry point so the sequence exists once. It used to be
/// copy-pasted between `serve` and `mesh`, which is exactly how the capture
/// lifetime bug got fixed in one place and not the other.
struct NodeBase {
    listener: TcpListenerHandle,
    /// Kept alive purely for its `Drop`: dropping it withdraws the mDNS record.
    _announcement: discovery::Announcement,
    capture: SharedCapture,
}

/// Bind, announce on the LAN, and bring up input capture.
async fn start_node(
    identity: &NodeIdentity,
    port: u16,
    send_input: bool,
    peer_edge: Option<cursio_protocol::Edge>,
) -> anyhow::Result<NodeBase> {
    let listener = TcpListenerHandle::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    let bound = listener.local_addr();
    tracing::info!(%bound, "listening for peers");

    // Announce ourselves so other nodes can find us. Pin mDNS to the real LAN
    // interface so VPN/virtual interfaces don't break it.
    let instance = sanitize_instance(&identity.display_name);
    let announcement = discovery::announce_with(
        identity.peer_id,
        &identity.display_name,
        &instance,
        bound.port(),
        &cursio_core::link::discovery_config(),
    )?;
    tracing::info!("announced on the LAN as {instance}");

    // Capture comes up ONCE here, before any session: the portal permission is
    // asked a single time and survives peers coming and going. See
    // [`PersistentCapture`].
    let capture: SharedCapture = std::rc::Rc::new(std::cell::RefCell::new(
        make_persistent_capture(send_input, peer_edge),
    ));

    Ok(NodeBase {
        listener,
        _announcement: announcement,
        capture,
    })
}

/// Capture shared between concurrent mesh sessions.
///
/// `Rc<RefCell<_>>` rather than a lock because mesh runs single-threaded on a
/// `LocalSet`. `None` inside means capture is unavailable/disabled. A session
/// borrows it only while it runs; since exactly one session can hold the borrow
/// at a time, a second concurrent peer simply runs without local input — which
/// is the right semantics anyway, as there is only one physical keyboard and
/// mouse to give away.
type SharedCapture = std::rc::Rc<std::cell::RefCell<Option<PersistentCapture>>>;

/// Bring up input capture for the whole process lifetime.
///
/// Returns `None` when capture is disabled or unavailable, in which case
/// sessions simply run without local input sharing.
fn make_persistent_capture(
    enabled: bool,
    peer_edge: Option<cursio_protocol::Edge>,
) -> Option<PersistentCapture> {
    if !enabled {
        return None;
    }

    let handle = make_capture(peer_edge)?;
    let grab = handle.grab.clone();

    // Both forwarders are permanent too: they own the raw receivers and feed the
    // two channels a session consumes. One shared command channel, because the
    // hotkey and an edge crossing are just two ways to ask for the same handoff.
    let (cmd_tx, commands) = tokio::sync::mpsc::unbounded_channel();
    let events = spawn_hotkey_filter(handle.events, cmd_tx.clone(), grab.clone());
    spawn_edge_handoff(handle.edges, cmd_tx, peer_edge);

    Some(PersistentCapture {
        events,
        commands,
        grab,
    })
}

/// Start capturing local input, returning the full [`CaptureHandle`] (event
/// channel + grab control), or `None` if unavailable / unsupported.
///
/// On Linux this reads `/dev/input/event*` via evdev (needs `input`-group
/// access). Other platforms have no capture backend yet.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn make_capture(peer_edge: Option<cursio_protocol::Edge>) -> Option<cursio_input::CaptureHandle> {
    match cursio_input::spawn_capture(peer_edge) {
        Ok(handle) => {
            tracing::info!(devices = ?handle.devices, "capturing local input");
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start input capture");
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn make_capture(_peer_edge: Option<cursio_protocol::Edge>) -> Option<cursio_input::CaptureHandle> {
    tracing::warn!("no input capture backend on this platform yet");
    None
}

/// Inject one event through the injector, or log it if there's no backend.
fn inject(
    injector: Option<&mut Box<dyn cursio_input::InputInjector>>,
    event: cursio_protocol::InputEvent,
) {
    match injector {
        Some(inj) => {
            if let Err(e) = inj.inject(event) {
                tracing::warn!(error = %e, "failed to inject input event");
            }
        }
        None => tracing::info!(?event, "peer input (no backend to inject)"),
    }
}

/// Browse the LAN and print discovered peers, then exit.
async fn list_peers(identity: NodeIdentity, timeout_secs: u64) -> anyhow::Result<()> {
    tracing::info!("browsing for {timeout_secs}s...");
    let self_id = identity.peer_id;
    let cfg = cursio_core::link::discovery_config();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = discovery::browse_with(self_id, &cfg, |peer| {
            let _ = tx.send(peer);
        });
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut found = 0;
    while std::time::Instant::now() < deadline {
        if let Ok(peer) = rx.recv_timeout(Duration::from_millis(200)) {
            found += 1;
            println!("  {} @ {} ({})", peer.display_name, peer.addr, peer.peer_id);
        }
    }
    tracing::info!("done; {found} peer(s) found");
    Ok(())
}

/// mDNS instance names must be simple; strip anything awkward from the hostname.
fn sanitize_instance(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "cursio-node".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_is_global_and_counted() {
        let before = Cli::try_parse_from(["cursio", "-v", "mesh"]).unwrap();
        assert_eq!(before.verbose, 1);

        let after = Cli::try_parse_from(["cursio", "mesh", "-vv"]).unwrap();
        assert_eq!(after.verbose, 2);
    }

    #[test]
    fn airplay_screen_is_enabled_only_by_its_flag() {
        let Command::Mesh { airplay_screen, .. } =
            Cli::try_parse_from(["cursio", "mesh"]).unwrap().command
        else {
            unreachable!();
        };
        assert!(!airplay_screen);

        let Command::Mesh { airplay_screen, .. } =
            Cli::try_parse_from(["cursio", "mesh", "--airplay-screen"])
                .unwrap()
                .command
        else {
            unreachable!();
        };
        assert!(airplay_screen);

        let Command::Mesh { airplay_screen, .. } =
            Cli::try_parse_from(["cursio", "mesh", "--airplay"])
                .unwrap()
                .command
        else {
            unreachable!();
        };
        assert!(airplay_screen);
    }

    #[test]
    fn airplay_name_removes_mdns_suffix_and_dots() {
        assert_eq!(
            airplay_receiver_name("gaming-pc.local"),
            "Cursio - gaming-pc"
        );
        assert_eq!(
            airplay_receiver_name("office.pc.local."),
            "Cursio - office-pc"
        );
    }
}
