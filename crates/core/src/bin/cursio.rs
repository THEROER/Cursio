//! `cursio` CLI — discovery, pairing, and sessions between machines on a LAN.
//!
//! **There is no server.** Every machine runs the same command, `mesh`, and
//! every node both accepts and dials: if one goes away the others keep talking
//! to each other directly and reconnect when it returns. Nothing is a hub.
//!
//! Subcommands:
//! - `mesh` — run a node: listen, dial every trusted peer, share input/audio/clipboard.
//! - `peers` — list peers discovered on the LAN and exit.
//!
//! Typical use: `cursio mesh --pair` on both machines once, with the same
//! password, then `cursio mesh` on every machine, every time. The password is
//! read from the terminal without echoing it or exposing it in the process
//! list.
//!
//! (`serve`/`connect` are gone: they encoded a client/server split that the mesh
//! model doesn't have. `join` was the last of that split to go — it was the
//! *dialling* half of pairing while `mesh --pair` was the listening half, two
//! commands for one handshake that had to be run on the right two machines in
//! the right order. The mesh already listens and dials at once, so pairing does
//! too, and `should_dial` settles which side places the call.)

use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use cursio_airplay::crypto::AppleCryptoProvider;
use cursio_airplay::{Receiver, ReceiverConfig, Resolution, VideoProfile};
use cursio_core::session::{self};
use cursio_core::NodeIdentity;
use cursio_protocol::{Fitness, PeerId, Quality, ServiceId, ServiceOffer};
use cursio_transport::discovery;
use cursio_transport::discovery::DiscoveryConfig;
use cursio_transport::pairing::{Role, TrustStore};
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
    /// Run as a mesh node: listen *and* dial every trusted peer at once.
    ///
    /// No server. Each node keeps its own list of known machines (from the
    /// trust store), so one dying doesn't make the others forget it — they mark
    /// it offline, carry on with each other, and reconnect when it returns.
    ///
    /// New machines are met explicitly, with `--pair` on both; the mesh
    /// only auto-dials machines already paired with.
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
        /// Where this machine stands, as a cell on a grid: `--at 1,0` is one
        /// machine to the right of `0,0`.
        ///
        /// Cells, not pixels — nothing needs to know how wide anybody's screen
        /// is. Four machines in a row are `0,0 1,0 2,0 3,0`, and each works out
        /// for itself which of its edges faces which peer, and which peer is
        /// nearest in that direction. Left unset every machine sits at `0,0`,
        /// where nothing borders anything and Ctrl+Alt+G is the only handoff.
        ///
        /// `allow_hyphen_values` because half the grid is negative: a machine
        /// to the left of the origin is `-1,0`, and clap would otherwise read
        /// the leading minus as the start of another flag and refuse it.
        #[arg(
            long,
            value_name = "X,Y",
            default_value = "0,0",
            allow_hyphen_values = true,
            value_parser = parse_position
        )]
        at: cursio_protocol::Position,
        /// Share audio with peers.
        #[arg(long, default_value_t = false)]
        audio: bool,
        /// Render peer audio into the output device whose name contains this
        /// (Windows only; ignored elsewhere, with a line in the log saying so).
        #[arg(long)]
        audio_render_device: Option<String>,
        /// Share the clipboard with peers (text, images, and other formats).
        #[arg(long, default_value_t = false)]
        clipboard: bool,
        /// Share disks both ways: this machine's volumes appear on the peer,
        /// and the peer's appear here under `~/Cursio/<peer>/`.
        ///
        /// Mounted read-write, so anything either machine can reach, the other
        /// can change. Linux mounts with FUSE and Windows with WinFsp; macOS
        /// serves its own volumes but cannot yet mount the peer's.
        #[arg(long, default_value_t = false)]
        files: bool,
        /// When this machine is the selected AirPlay host, offer the mirrored
        /// screen to every peer instead of only peers using --airplay-window.
        ///
        /// The peer sees it as an ordinary transferred window: the H.264 from
        /// AirPlay is forwarded untouched, so nothing is re-encoded on the way.
        #[arg(long, default_value_t = false)]
        share_airplay: bool,
        /// Pair with a machine that is not trusted yet: run this on **both**,
        /// with the same password, for the next ten minutes.
        ///
        /// Not two commands. The mesh already listens and dials at once; the
        /// only thing it will not do on its own is call a machine it has never
        /// met, because joining has to stay a deliberate, password-gated act.
        /// This opens that window at both ends and lets the two sort out which
        /// one places the call, the same way two trusted nodes do.
        #[arg(long, default_value_t = false)]
        pair: bool,
        /// Where to find the machine to pair with, when mDNS will not say.
        ///
        /// Only consulted while `--pair` is open. Naming an address also means
        /// this side places the call rather than waiting to be called.
        #[arg(long, value_name = "HOST:PORT", requires = "pair")]
        pair_addr: Option<SocketAddr>,
        /// What this machine asks of the mesh's AirPlay receiver. On by
        /// default.
        ///
        /// `on` — the default — takes part: this machine will hold the receiver
        /// if the mesh picks it, with no request about quality. A profile in
        /// its place asks the *mesh* for that quality — `1440p:30fps`,
        /// `1080p60`, `2k` — and the receiver moves to whichever machine can deliver it,
        /// which need not be this one. `off` steps out entirely: nothing is
        /// bound, and a peer asking this machine to host is refused, so the
        /// mesh picks another. Every machine `off` means AirPlay does not run.
        ///
        /// Where the picture *appears* is `--airplay-window`, not this.
        #[arg(long, default_value = "on", value_name = "MODE")]
        airplay: String,
        /// Show the mirrored iPhone screen on this machine.
        ///
        /// Separate from `--airplay` because they answer different questions:
        /// that one is about the picture the mesh receives, this one about
        /// which desk it appears on. A machine can hold the receiver and show
        /// nothing — passing the stream to peers with `--share-airplay` — or
        /// show a picture another machine is receiving.
        #[arg(long, default_value_t = false)]
        airplay_window: bool,
        /// AirPlay control port.
        #[arg(long, default_value_t = cursio_airplay::DEFAULT_CONTROL_PORT)]
        airplay_port: u16,
        /// Pin AirPlay mDNS to the LAN interface with this address.
        #[arg(long)]
        airplay_interface: Option<IpAddr>,
        /// Where a peer's modifier keys land on this machine (macOS only).
        ///
        /// `positional` — the default — puts every key where the hardware says
        /// it is: Alt is Option, Ctrl is Control, the Windows key is Command.
        /// `swapped` trades Control and the Windows key instead, so a PC
        /// keyboard's Ctrl+C copies.
        ///
        /// Anything else can be written out as `from=to` pairs, e.g.
        /// `ctrl=cmd,win=ctrl` (the swap spelled by hand) or
        /// `rightalt=globe` (the right Alt changes input source, which is what
        /// a modern MacBook's Globe key does and no PC keyboard has). Names:
        /// ctrl, alt/option, win/cmd, shift, caps, globe/fn — each optionally
        /// prefixed `left` or `right`.
        ///
        /// This also decides where the Ctrl+Alt+G hotkey lands on a Mac's own
        /// keyboard; the log says which keys, at startup.
        #[arg(long, default_value = "positional", value_name = "SPEC")]
        mac_modifiers: String,
    },
    /// List peers discovered on the LAN, then exit.
    Peers {
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
    },
}

/// Parse `--at X,Y` into a grid cell.
///
/// Spelled out rather than derived so the error names both the input and the
/// shape expected: a mistyped layout is otherwise discovered as "the cursor
/// does not cross", which is a long way from the comma that was missing.
fn parse_position(value: &str) -> Result<cursio_protocol::Position, String> {
    let (x, y) = value
        .split_once(',')
        .ok_or_else(|| format!("expected X,Y (for example 1,0), got {value:?}"))?;
    let parse = |part: &str, axis: &str| {
        part.trim()
            .parse::<i32>()
            .map_err(|_| format!("{axis} of {value:?} is not a whole number"))
    };
    Ok(cursio_protocol::Position::new(
        parse(x, "the x")?,
        parse(y, "the y")?,
    ))
}

/// What `--airplay` was set to.
///
/// One value rather than the three booleans this used to be (`--airplay-screen`,
/// its `--airplay` alias, and `--no-airplay`, two of which had to be declared as
/// conflicting). There are three states and they are mutually exclusive, which
/// is exactly what one value expresses and three flags do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AirPlayMode {
    /// Do not take part: nothing is bound, nothing announced, and a peer's
    /// request to host is refused so it keeps looking.
    Off,
    /// Take part, asking the mesh for `wanted` if anything.
    ///
    /// The quality is a request made *of the mesh*, not a local setting: the
    /// receiver runs at the best any node asked for, on whichever machine can
    /// deliver it. A user asking for 1440p at their desk is asking for the
    /// picture to be that, and does not care which box decodes it.
    On { wanted: Option<Quality> },
}

/// The rate a profile means when it names only a size.
const DEFAULT_AIRPLAY_FPS: u16 = 60;

/// What the receiver runs at when nobody asked for anything in particular.
const DEFAULT_AIRPLAY_QUALITY: Quality = Quality::new(1920, 1080, DEFAULT_AIRPLAY_FPS);

/// How well this machine could hold the mesh's AirPlay receiver.
///
/// Two honest signals and no third. Whether the user let it take part, and
/// whether a hardware video decoder is installed — which the video module
/// answers by asking GStreamer for the elements it would actually use, rather
/// than by reading a GPU's name and guessing. A machine without one still
/// makes a fine control receiver; it is simply not where a picture should be
/// decoded, since this codebase refuses to fall back to software.
fn local_airplay_fitness(mode: AirPlayMode, port: u16) -> Fitness {
    if !mode.participates() {
        return Fitness::No;
    }
    // Port availability is part of capability, not a failure discovered after
    // election. Advertising a great decoder that cannot bind the receiver
    // would make every node consistently choose a host that can never start.
    if let Err(error) = std::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))) {
        tracing::info!(
            port,
            %error,
            "AirPlay port is unavailable here; this node will only request/view the mesh receiver"
        );
        return Fitness::No;
    }
    match cursio_airplay::video::dependency_warnings(cursio_airplay::video::VideoCodec::H264)
        .is_empty()
    {
        true => Fitness::Full,
        false => Fitness::ControlOnly,
    }
}

/// Map the protocol's numeric quality back to one of the receiver profiles.
fn resolution_for(quality: Quality) -> Resolution {
    if quality.height > 1080 {
        Resolution::Qhd
    } else {
        Resolution::FullHd
    }
}

impl AirPlayMode {
    /// Read `off`, `on`, or a profile like `1080p30`.
    fn parse(spec: &str) -> Result<Self, String> {
        let cleaned: String = spec
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | ' '))
            .collect();
        match cleaned.as_str() {
            "off" | "no" | "none" => return Ok(Self::Off),
            "on" | "control" => return Ok(Self::On { wanted: None }),
            // Named its own thing until the window became a flag of its own.
            // Refused rather than quietly re-read as `on`, which would leave a
            // user waiting for a picture that now needs asking for.
            "screen" => {
                return Err(
                    "`screen` no longer decides where the picture appears — pass \
                     --airplay-window on the machine it should appear on, and use --airplay \
                     on (or a profile like 1080p60) here"
                        .into(),
                )
            }
            _ => {}
        }
        parse_video_profile(&cleaned).map(|wanted| Self::On {
            wanted: Some(wanted),
        })
    }

    /// The quality this machine is asking the mesh for.
    fn wanted(self) -> Option<Quality> {
        match self {
            Self::On { wanted } => wanted,
            Self::Off => None,
        }
    }

    /// Whether this machine takes part in AirPlay at all.
    fn participates(self) -> bool {
        self != Self::Off
    }
}

/// Parse `1080p30`, `1440p`, `2k60` into a quality.
///
/// The rate may be separated by `:` or `@` or by nothing at all, because all
/// three are how people write it and none is ambiguous. Omitted, it is
/// [`DEFAULT_AIRPLAY_FPS`].
fn parse_video_profile(spec: &str) -> Result<Quality, String> {
    // Longest name first, so `1080p60` is not read as `1080` followed by the
    // nonsense `p60`.
    const NAMES: &[(&str, u16, u16)] = &[
        ("1080p", 1920, 1080),
        ("fullhd", 1920, 1080),
        ("1080", 1920, 1080),
        ("1440p", 2560, 1440),
        ("1440", 2560, 1440),
        ("qhd", 2560, 1440),
        ("2k", 2560, 1440),
    ];
    for &(name, width, height) in NAMES {
        let Some(rest) = spec.strip_prefix(name) else {
            continue;
        };
        let rest = rest.trim_start_matches([':', '@']);
        let rest = rest.strip_suffix("fps").unwrap_or(rest);
        // Anything but digits after the size means this was not the name we
        // took it for — `1080i` is a resolution nobody here supports, not a
        // 1080p at a frame rate called `i`. Fall through and let the error at
        // the end list what is actually accepted.
        if !rest.is_empty() && !rest.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let fps = match rest.is_empty() {
            true => DEFAULT_AIRPLAY_FPS,
            false => rest
                .parse::<u16>()
                .map_err(|_| format!("`{rest}` is not a frame rate"))?,
        };
        // The same bound the receiver's own profile enforces, checked here so
        // a typo is refused by the flag rather than by a bind three steps
        // later.
        if !(1..=120).contains(&fps) {
            return Err(format!("frame rate must be in 1..=120, got {fps}"));
        }
        return Ok(Quality::new(width, height, fps));
    }
    Err(format!(
        "`{spec}` is not one of off, control, screen, or a profile such as 1080p60, 1080p30, 2k30"
    ))
}

/// Read `--mac-modifiers` and hand it to the keymap, or refuse to start.
///
/// Refusing is deliberate. A mistyped map would otherwise be silently ignored
/// and the user would spend the session wondering why Command is still in the
/// wrong place — and the parse error already names what it could not read.
///
/// The map is stored in the keymap rather than threaded through the session
/// layers because it is not a property of any session: there is one keyboard
/// per machine, settled before the first peer connects, exactly like the
/// capture backend.
fn apply_modifier_map(spec: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let map = cursio_input::ModifierMap::parse(spec)
            .map_err(|e| anyhow::anyhow!("--mac-modifiers: {e}"))?;
        cursio_input::mac_keymap::configure(map);
        tracing::info!(
            modifiers = %map.describe(),
            hotkey = %map.hotkey_on_this_mac(),
            "keyboard mapping"
        );
    }
    // Elsewhere the flag is accepted and does nothing: the mapping happens
    // entirely on the Mac, in both directions. Saying so beats a user setting
    // it on the wrong machine and watching nothing change.
    #[cfg(not(target_os = "macos"))]
    if spec != "positional" {
        tracing::warn!(
            spec,
            "--mac-modifiers only applies on a Mac; set it there, on the machine being driven"
        );
    }
    Ok(())
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
    // Task Manager) unless we match them. `peers` never injects, so
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
        Command::Mesh {
            port,
            send_input,
            at,
            audio,
            audio_render_device,
            clipboard,
            files,
            share_airplay,
            pair,
            pair_addr,
            airplay,
            airplay_window,
            airplay_port,
            airplay_interface,
            mac_modifiers,
        } => {
            apply_modifier_map(&mac_modifiers)?;
            let airplay =
                AirPlayMode::parse(&airplay).map_err(|e| anyhow::anyhow!("--airplay: {e}"))?;
            // Checked here rather than with clap's `requires`, which cannot see
            // inside a value: a machine that took itself out of AirPlay has
            // nothing to share, and starting anyway would leave peers waiting
            // on a stream that never comes.
            if (share_airplay || airplay_window) && !airplay.participates() {
                anyhow::bail!(
                    "--airplay off leaves nothing to show or share; drop it, or drop \
                     --airplay-window/--share-airplay"
                );
            }
            // What this machine brings to the mesh's choice of host, and what
            // it wants from it. Both travel in the greeting, so placement is
            // settled the moment two machines meet.
            let offer = ServiceOffer {
                fitness: local_airplay_fitness(airplay, airplay_port),
                wanted: airplay.wanted(),
                show_window: airplay_window,
            };
            tracing::info!(
                fitness = ?offer.fitness,
                wanted = ?offer.wanted.map(|q| q.to_string()),
                window = airplay_window,
                "AirPlay: what this machine offers the mesh"
            );
            // Read before anything binds a port, so a mistyped password costs
            // nothing and the prompt is not buried under startup logging.
            let pairing = if pair {
                let password = read_pairing_password()?;
                tracing::info!(
                    minutes = PAIRING_WINDOW.as_secs() / 60,
                    "pairing window open; run `cursio mesh --pair` on the other machine too"
                );
                PairingWindow::open(password, PAIRING_WINDOW, pair_addr)
            } else {
                PairingWindow::closed()
            };
            // Everything the service module needs, built before the registry
            // so the same state is read by the AirPlay bring-up below.
            let service_state = cursio_core::service_session::ServiceState::new();
            let service_reasons: Arc<
                std::sync::Mutex<std::collections::HashMap<ServiceId, String>>,
            > = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            service_state.note_offer(identity.peer_id, offer);

            // Every participating receiver has a tap. It is also the sink that
            // lets a selected host accept mirroring without opening a local
            // window; the window module subscribes only for peers that asked
            // for one (or when --share-airplay explicitly asks for all peers).
            let airplay_tap = airplay.participates().then(cursio_airplay::MirrorTap::new);

            // The CLI's only job for modules is to build the registry; from here
            // on nothing names a specific module.
            let modules = module_registry(
                &AudioOpts {
                    audio,
                    audio_render_device,
                },
                clipboard,
                files,
                WindowOpts {
                    airplay: airplay_tap.clone().map(|tap| {
                        cursio_core::window_session::AirPlayShare {
                            tap,
                            state: service_state.clone(),
                            local: identity.peer_id,
                            force: share_airplay,
                        }
                    }),
                },
                ServiceSetup {
                    state: service_state.clone(),
                    reasons: service_reasons.clone(),
                    make_host: {
                        let state = service_state.clone();
                        // `off` refuses rather than declining to answer. A peer
                        // that asked and heard nothing waits forever for a
                        // receiver; one that hears "no" asks the next machine.
                        match airplay.participates() {
                            false => Box::new(|| {
                                Box::new(cursio_core::service_session::RefuseAll)
                                    as Box<dyn cursio_core::service_session::ServiceHost>
                            }),
                            true => Box::new(move || {
                                Box::new(AirPlayHost {
                                    state: state.clone(),
                                })
                                    as Box<dyn cursio_core::service_session::ServiceHost>
                            }),
                        }
                    },
                },
            );
            let mesh = run_mesh(
                identity.clone(),
                port,
                send_input,
                at,
                modules,
                pairing,
                offer,
            );
            if !airplay.participates() {
                // Nothing bound and nothing announced, and the service module
                // is refusing on this machine's behalf, so a peer that asks
                // learns to look elsewhere. With every machine here, AirPlay
                // does not run at all — which is what asking for it means.
                tracing::info!("AirPlay is off on this machine");
                mesh.await
            } else {
                // Is another cursio on *this* machine already announcing? No
                // peer can answer that — both processes are the same peer — so
                // a lock file settles it locally, before anything binds.
                let airplay_lock = claim_local_airplay();
                run_airplay(
                    AirPlayHosting {
                        identity: identity.clone(),
                        port: airplay_port,
                        interface: airplay_interface,
                        show_window: airplay_window,
                        tap: airplay_tap,
                        may_announce: airplay_lock.is_some(),
                        state: service_state.clone(),
                        reasons: service_reasons.clone(),
                    },
                    mesh,
                )
                .await
            }
        }
        Command::Peers { timeout_secs } => list_peers(identity, timeout_secs).await,
    }
}

/// Window-transfer options, grouped for the same reason [`AudioOpts`] is: the
/// registry builder should gain a struct field per subsystem, not a positional
/// argument.
struct WindowOpts {
    /// Set when this node shares whatever an iPhone mirrors to it. A window of
    /// this machine's own moves by being dragged across the peer-facing edge,
    /// which needs no flag — the drag is the request.
    airplay: Option<cursio_core::window_session::AirPlayShare>,
}

/// What the service module needs, gathered so `module_registry` takes one
/// parameter rather than three.
struct ServiceSetup {
    state: cursio_core::service_session::ServiceState,
    reasons: Arc<std::sync::Mutex<std::collections::HashMap<ServiceId, String>>>,
    make_host: Box<dyn Fn() -> Box<dyn cursio_core::service_session::ServiceHost> + Send + Sync>,
}

/// Answers a peer's request only when a hidden receiver is already bound.
struct AirPlayHost {
    state: cursio_core::service_session::ServiceState,
}

impl cursio_core::service_session::ServiceHost for AirPlayHost {
    fn accept(&mut self, service: ServiceId, _reason: &str) -> Result<(), String> {
        if service != ServiceId::AIRPLAY {
            return Err(format!("this machine does not host {service}"));
        }
        if self.state.is_hosted_here(service) {
            // `answer` records that this peer asked us; the placement watcher
            // then announces the receiver that is already serving.
            return Ok(());
        }
        Err("this machine has no bound AirPlay receiver to take over with".into())
    }
}

/// Everything hosting the mesh's AirPlay receiver needs from the command line.
struct AirPlayHosting {
    identity: NodeIdentity,
    port: u16,
    interface: Option<IpAddr>,
    /// Whether the mirrored picture appears on this machine.
    show_window: bool,
    /// Set when peers may watch the phone's screen through this node.
    tap: Option<cursio_airplay::MirrorTap>,
    /// False when another cursio on this machine holds the local claim — a
    /// question no peer can answer, since both processes are the same peer.
    may_announce: bool,
    state: cursio_core::service_session::ServiceState,
    reasons: Arc<std::sync::Mutex<std::collections::HashMap<ServiceId, String>>>,
}

/// Hold the receiver for as long as the mesh wants it here, rebuilding it when
/// the agreed quality changes, until the mesh itself ends.
///
/// A loop rather than a single bind, because the quality is the *mesh's* and
/// can change under this machine: a peer joining and asking for 1440p is a
/// request this node has to honour if it is the one holding the receiver. The
/// mesh future is pinned across iterations — it is the thing that must not be
/// restarted, since dropping it would take every peer session with it.
async fn run_airplay(
    opts: AirPlayHosting,
    mesh: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    tokio::pin!(mesh);
    let mut running = local_airplay_placement(&opts.state, opts.identity.peer_id);
    loop {
        // Asked before every bind, not once: a peer that joined during the
        // last receiver's life may have raised what the mesh wants.
        let announce = opts.may_announce && running.selected;
        let receiver = match bind_airplay(
            &opts.identity,
            AirPlayOpts {
                announce,
                port: opts.port,
                interface: opts.interface,
                quality: running.quality,
                show_window: opts.show_window && running.selected,
                tap: opts.tap.clone(),
            },
        )
        .await
        {
            Ok(receiver) => receiver,
            // A busy port — macOS' own receiver holds 7000 — must not take the
            // mesh down, and must not end in silence either. The machine says
            // what it could not do, and every session asks its peer to host it
            // instead; the phone then mirrors to that peer, which streams the
            // picture back here through the ordinary window path. Which machine
            // holds the socket stops being something the user has to know.
            Err(error) => {
                tracing::warn!(
                    %error,
                    "cannot host AirPlay here; asking a peer to host it for the mesh"
                );
                opts.reasons
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(ServiceId::AIRPLAY, error.to_string());
                opts.state.needs_host(ServiceId::AIRPLAY);
                return mesh.await;
            }
        };

        // Published before serving, so a peer's query is answered from what is
        // actually bound rather than from what was asked for on the command
        // line.
        if let Ok(addr) = receiver.local_addr() {
            let reachable = SocketAddr::new(
                discovery::guess_lan_ipv4().unwrap_or_else(|| IpAddr::from([127, 0, 0, 1])),
                addr.port(),
            );
            opts.state
                .now_hosting(ServiceId::AIRPLAY, reachable, running.quality);
        }

        let follower = follow_airplay_answers(
            receiver.announcer(),
            opts.state.clone(),
            opts.identity.peer_id,
            opts.may_announce,
        );
        let wanted = tokio::select! {
            result = &mut mesh => return result,
            result = receiver.serve() => return result.map_err(Into::into),
            wanted = follower => wanted,
        };

        // Only a change gets here, and rebuilding costs the viewer a second of
        // black screen — so say why in the log, where a user watching it happen
        // will look.
        tracing::info!(
            from = ?running.quality.map(|q| q.to_string()),
            to = ?wanted.quality.map(|q| q.to_string()),
            was_host = running.selected,
            is_host = wanted.selected,
            "the mesh changed AirPlay placement; restarting the receiver"
        );
        opts.state.no_longer_hosting(ServiceId::AIRPLAY);
        running = wanted;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalAirPlayPlacement {
    selected: bool,
    quality: Option<Quality>,
}

/// What this machine should run from the mesh's deterministic placement.
fn local_airplay_placement(
    state: &cursio_core::service_session::ServiceState,
    local: cursio_protocol::PeerId,
) -> LocalAirPlayPlacement {
    if state.was_asked_to_host(ServiceId::AIRPLAY) && state.is_hosted_here(ServiceId::AIRPLAY) {
        return LocalAirPlayPlacement {
            selected: true,
            quality: Some(
                state
                    .placement()
                    .and_then(|placement| placement.quality)
                    .unwrap_or(DEFAULT_AIRPLAY_QUALITY),
            ),
        };
    }
    match state.placement() {
        Some(placement) if placement.host == local => LocalAirPlayPlacement {
            selected: true,
            quality: Some(placement.quality.unwrap_or(DEFAULT_AIRPLAY_QUALITY)),
        },
        _ => LocalAirPlayPlacement {
            selected: false,
            quality: None,
        },
    }
}

/// Keep the mDNS record in step with what peers say they are hosting.
///
/// Polled rather than pushed: the answer arrives on whichever peer session
/// happens to be up, and there may be several coming and going, so the state
/// they all write is the one thing that outlives them. A second is far below
/// what a user notices between plugging a machine in and finding one Cursio in
/// the list, and costs one atomic read.
async fn follow_airplay_answers(
    announcer: cursio_airplay::Announcer,
    state: cursio_core::service_session::ServiceState,
    local: cursio_protocol::PeerId,
    // Whether this process may announce at all. False when another cursio on
    // this machine holds the claim — a question no peer can answer, since both
    // processes are the same peer, so it has to be carried in rather than
    // rediscovered here.
    may_announce: bool,
) -> LocalAirPlayPlacement {
    const POLL: Duration = Duration::from_secs(1);
    let running = local_airplay_placement(&state, local);
    // How long a machine has to look like the host before it acts on it.
    //
    // A peer's offer is forgotten the instant its link drops, so a reconnect —
    // which the mesh attempts every two seconds — briefly looks exactly like
    // that peer being gone for good. Acting on it immediately meant every blink
    // of the link cost two receiver restarts: this node took the duty, the peer
    // came back a second later and took it again.
    //
    // Only *taking* the duty waits. Standing down stays immediate, because the
    // failure it prevents is worse: two `Cursio` in an iPhone's picker, which
    // is what the user sees and has to choose between.
    const SETTLE: u32 = 5;
    // How many consecutive polls have agreed on `settled`. Counted against the
    // reading itself rather than against what is running, so an announcement
    // that drifted out of step with the placement is still corrected — a node
    // that bound silently because a peer looked reachable, and then turned out
    // to be the host after all, would otherwise stay silent forever.
    let mut settled = local_airplay_placement(&state, local);
    let mut steady = 0_u32;
    loop {
        tokio::time::sleep(POLL).await;
        let wanted = local_airplay_placement(&state, local);
        steady = match wanted == settled {
            true => steady.saturating_add(1),
            false => {
                settled = wanted;
                1
            }
        };

        let should_announce = may_announce && wanted.selected;
        if !should_announce && announcer.is_announced() {
            tracing::info!(
                addr = ?state.host_address(ServiceId::AIRPLAY),
                "the mesh selected another AirPlay host; standing down"
            );
            announcer.withdraw();
        } else if should_announce && !announcer.is_announced() && steady >= SETTLE {
            if let Err(error) = announcer.announce() {
                tracing::warn!(%error, "could not announce the selected AirPlay host");
            }
        }

        // A host change also rebuilds when the quality is the same: local
        // window/tap behaviour belongs to the selected receiver, not merely to
        // a numeric profile.
        if wanted == running {
            continue;
        }
        // Losing the duty is acted on at once; gaining it waits out a peer that
        // is merely reconnecting.
        if !wanted.selected || steady >= SETTLE {
            return wanted;
        }
        if steady == 1 {
            tracing::debug!(
                seconds = SETTLE,
                "this machine would take AirPlay; waiting in case the peer is reconnecting"
            );
        }
    }
}

struct AirPlayOpts {
    /// Whether to put this receiver in an iPhone's picker. False on every node
    /// but the one that won the election; see [`claims_airplay`].
    announce: bool,
    port: u16,
    interface: Option<IpAddr>,
    /// What the mesh agreed the picture should be, or `None` for a
    /// control-only receiver that answers an iPhone and decodes nothing.
    quality: Option<Quality>,
    /// Whether the picture appears on this machine.
    show_window: bool,
    /// Set when peers should be able to watch the mirrored screen too.
    tap: Option<cursio_airplay::MirrorTap>,
}

async fn bind_airplay(identity: &NodeIdentity, options: AirPlayOpts) -> anyhow::Result<Receiver> {
    // A control-only receiver still needs a profile to describe itself with in
    // `/info`; it simply never decodes anything.
    let quality = options.quality.unwrap_or(DEFAULT_AIRPLAY_QUALITY);
    let profile = VideoProfile::new(resolution_for(quality), quality.fps)?;
    if options.quality.is_some() {
        for warning in
            cursio_airplay::video::dependency_warnings(cursio_airplay::video::VideoCodec::H264)
        {
            tracing::warn!("{warning}");
        }
    }
    let crypto = Arc::new(AppleCryptoProvider::new());
    let config = ReceiverConfig::new(
        AIRPLAY_RECEIVER_NAME,
        identity.peer_id,
        SocketAddr::from(([0, 0, 0, 0], options.port)),
        profile,
    )?
    .with_pairing_public_key(crypto.public_key());
    let discovery = options
        .interface
        .map_or_else(DiscoveryConfig::default, DiscoveryConfig::pinned_to);
    let mut receiver = Receiver::bind(config, discovery, crypto, options.announce)
        .await?
        .with_screen_enabled(options.show_window);
    if let Some(tap) = options.tap {
        receiver = receiver.with_mirror_tap(tap);
    }
    tracing::info!(
        address = %receiver.local_addr()?,
        screen = options.show_window,
        quality = %quality,
        announced = options.announce,
        "mesh AirPlay RTSP receiver is available"
    );
    Ok(receiver)
}

/// What an iPhone shows in its Screen Mirroring list.
///
/// One name for the whole mesh, not one per machine. Which machine actually
/// decodes the stream is Cursio's business — the phone is picking "my
/// desktop", and offering it `Cursio - macbook` beside `Cursio - desktop`
/// makes the user choose between two answers to a question they did not ask.
/// Exactly one node announces it; see [`claims_airplay`].
const AIRPLAY_RECEIVER_NAME: &str = "Cursio";

/// Hold the machine-wide claim on announcing AirPlay, if it is free.
///
/// Two `cursio` processes on one machine put two `Cursio` entries in an
/// iPhone's list, and no amount of talking to *peers* fixes that: both are the
/// same peer, with the same id, so the mesh election ranks them identically
/// and both conclude they won. The question "is one already running here" has
/// a local answer, and a lock file is it.
///
/// The returned file must be held for as long as the process announces —
/// dropping it releases the lock, and closing it on exit is what lets the next
/// run take over even after a crash, since the kernel releases the lock with
/// the descriptor rather than leaving a stale file behind.
fn claim_local_airplay() -> Option<std::fs::File> {
    take_lock(&cursio_log::log_dir().join("airplay.lock"))
}

/// Take an exclusive lock on `path`, or report that somebody else holds it.
fn take_lock(path: &std::path::Path) -> Option<std::fs::File> {
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => {
            // Not fatal: without the lock the worst case is the duplicate entry
            // this is meant to prevent, which beats refusing to run at all.
            tracing::warn!(%error, path = %path.display(), "cannot take the AirPlay lock");
            return None;
        }
    };
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(_) => {
            tracing::info!(
                "another cursio on this machine is already announcing AirPlay; not adding a second"
            );
            None
        }
    }
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
    window: WindowOpts,
    services: ServiceSetup,
) -> cursio_core::module::ModuleRegistry {
    // Said once at startup rather than left to a peer that agreed to run audio
    // and then hears nothing: a flag that does nothing has to say so.
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    if audio.audio {
        tracing::warn!("no audio backend on this platform; --audio is not advertised to peers");
    }

    cursio_core::module::ModuleRegistry::new()
        .with(Box::new(cursio_core::audio_session::AudioFactory {
            enabled: audio.audio,
            render_device: audio.audio_render_device.clone(),
        }))
        .with(Box::new(cursio_core::clipboard_session::ClipboardFactory {
            enabled: clipboard,
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
        }))
        // Registered everywhere, including the platforms with no mount backend.
        // Sharing is two independent halves and only one of them needs FUSE or
        // WinFsp: a Mac that cannot show the peer's disks can still serve its
        // own, and skipping the module entirely meant a Linux peer could not
        // reach them either — with `--files` accepted and silently doing
        // nothing.
        .with(Box::new(cursio_core::files_session::FilesFactory {
            enabled: files,
        }))
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
    position: cursio_protocol::Position,
    modules: cursio_core::module::ModuleRegistry,
    pairing: PairingWindow,
    offer: ServiceOffer,
) -> anyhow::Result<()> {
    let NodeBase {
        listener,
        _announcement,
        capture,
    } = start_node(&identity, port, send_input).await?;

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

    // Machines seen announcing themselves that this node does not trust yet.
    // Only ever filled while `--pair` holds a window open; emptied by the sweep
    // that dials them, so a machine that refuses is retried on the next
    // sighting rather than hammered every two seconds.
    let mut strangers: std::collections::HashMap<PeerId, (String, SocketAddr)> =
        std::collections::HashMap::new();
    if let Some(addr) = pairing.addr() {
        // An address given on the command line stands in for a sighting, for
        // the LAN where mDNS is blocked. The id is not known until the
        // handshake, so a nil one is used purely to key the map — `should_dial`
        // would otherwise decide the call, and a machine the user named
        // explicitly is one they want called.
        strangers.insert(PeerId::nil(), ("the machine you named".into(), addr));
    }

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
                            position,
                            modules.clone(),
                            offer,
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
                        } else if pairing.is_open() {
                            // The one window in which a stranger is interesting.
                            // Outside it an unknown machine announcing itself is
                            // ignored, which is what keeps joining the mesh an
                            // explicit, password-gated act.
                            if strangers.insert(peer.peer_id, (peer.display_name.clone(), peer.addr)).is_none() {
                                tracing::info!(
                                    peer = %peer.display_name,
                                    addr = %peer.addr,
                                    "unpaired machine found; will offer it the pairing password"
                                );
                            }
                        } else {
                            tracing::trace!(peer = %peer.display_name, "ignoring untrusted peer");
                        }
                    }

                    // Time to dial whoever is known, offline, and ours to call.
                    _ = sweep.tick() => {
                        // A machine that is not trusted yet, while the window is
                        // open. This is what `join` used to be: the mesh only
                        // ever dials the trust store, so somebody has to make
                        // the first call to a stranger. Both sides run the same
                        // command and `should_dial` settles which one calls —
                        // the same rule that stops two trusted nodes dialling
                        // each other at once.
                        if pairing.is_open() {
                            for (peer_id, (name, addr)) in std::mem::take(&mut strangers) {
                                // A nil id is the `--addr` placeholder: the user
                                // named this machine, so this side calls it
                                // rather than waiting to be called.
                                if peer_id != PeerId::nil()
                                    && !cursio_core::registry::should_dial(self_id, peer_id)
                                {
                                    tracing::debug!(peer = %name, "letting the other machine call us");
                                    continue;
                                }
                                let transport = match TcpTransport::connect(addr).await {
                                    Ok(t) => t,
                                    Err(e) => {
                                        tracing::warn!(peer = %name, %addr, error = %e, "could not reach the machine to pair with");
                                        continue;
                                    }
                                };
                                tracing::info!(peer = %name, %addr, "dialled an unpaired machine to pair with it");
                                spawn_peer_session(
                                    transport,
                                    identity.clone(),
                                    Role::Initiator,
                                    None,
                                    registry.clone(),
                                    pairing.clone(),
                                    capture.clone(),
                                    position,
                                    modules.clone(),
                                    offer,
                                );
                            }
                        }
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
                                position,
                                modules.clone(),
                                offer,
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
/// How long `--pair` stays willing to meet a machine it has never seen.
const PAIRING_WINDOW: Duration = Duration::from_secs(10 * 60);

/// A time-boxed willingness to pair with an unknown machine.
///
/// A mesh node refuses an inbound stranger outright: the trust store is the
/// only way in, which is what keeps joining the mesh an explicit act. That left
/// no way to *become* trusted: SPAKE2 needs both halves, and a node with no
/// password bails before running its own. `--pair` opens this window at both
/// ends so a first meeting can happen — it is what lets the mesh dial a machine
/// it has never met, as well as accept one — and the deadline closes it again
/// so a node left running overnight is not standing open to the LAN.
#[derive(Clone)]
struct PairingWindow(std::rc::Rc<std::cell::RefCell<Option<OpenPairingWindow>>>);

struct OpenPairingWindow {
    password: Zeroizing<String>,
    closes_at: std::time::Instant,
    /// Where to find the machine to pair with, when mDNS will not say. Part of
    /// the window rather than carried beside it: both are the same fact —
    /// this node is trying to meet a stranger, and here is what it knows.
    addr: Option<SocketAddr>,
}

impl PairingWindow {
    fn closed() -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(None)))
    }

    fn open(password: Zeroizing<String>, for_: Duration, addr: Option<SocketAddr>) -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(Some(
            OpenPairingWindow {
                password,
                addr,
                closes_at: std::time::Instant::now() + for_,
            },
        ))))
    }

    /// The address the user named, while the window is open.
    fn addr(&self) -> Option<SocketAddr> {
        self.0
            .borrow()
            .as_ref()
            .filter(|open| open.closes_at > std::time::Instant::now())
            .and_then(|open| open.addr)
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
    // Where this machine stands, from which the edge to each peer is derived.
    position: cursio_protocol::Position,
    // Shared rather than cloned: the registry is built once per process and read
    // by every session. `Rc` because sessions are deliberately single-threaded
    // (see the mesh module's threading note).
    modules: std::rc::Rc<cursio_core::module::ModuleRegistry>,
    offer: ServiceOffer,
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

        // Whichever way the call went, the password only matters on a first
        // meeting: `link::establish` uses the trust store when it has an entry
        // and reaches for this when it does not. Both roles get it because
        // either can be the one that dialled a stranger — the mesh calls a
        // machine it has never met while `--pair` is open, and used to leave
        // this empty on the assumption that a dialled peer is always trusted.
        let password = pairing.password();
        let result = run_pairing_and_session(
            transport,
            identity,
            &password,
            role,
            false,
            capture_for_session,
            position,
            &modules,
            offer,
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

/// Called once a session learns who the peer is (from the Hello handshake).
/// The mesh uses it to mark an inbound peer online — until the handshake, a
/// peer that dialled *us* is just an anonymous socket. Not `Send`: it reaches
/// the mesh's own `Rc` state, which never leaves this thread.
type IdentifiedCallback<'a> = dyn FnMut(cursio_protocol::PeerId, &str) + 'a;

/// Shared tail of `serve`/`connect`: Hello handshake, pair on first meeting,
/// bring up a Noise-encrypted channel, then run heartbeats over it.
///
/// The Hello handshake and pairing run over the plaintext transport — that's
/// safe, since SPAKE2 never sends the password. Once we hold the shared key
/// (freshly paired, or recovered from the trust store), we wrap the transport
/// in [`EncryptedTransport`] so all subsequent traffic (heartbeats today; input
/// and video later) is confidential and integrity-protected.
// Down from 13 arguments to 10, and the rest are session plumbing rather than
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
    // Where this machine stands. The edge to *this* peer is derived from it and
    // the peer's own position during the handshake — both the cursor handoff
    // and the window-drag trigger read that answer back off the link.
    position: cursio_protocol::Position,
    // Everything this node can run. Built once by the caller; a session takes
    // what negotiation agreed on. Replaces the five per-module flags that used
    // to be threaded through here.
    modules: &cursio_core::module::ModuleRegistry,
    offer: ServiceOffer,
    on_identified: Option<&mut IdentifiedCallback<'_>>,
) -> anyhow::Result<()> {
    // Handshake, pairing, Noise and module negotiation are the same five steps
    // for every node, so they live in `link` — the app and a phone run this
    // exact sequence rather than a second copy of it.
    let plan = cursio_core::link::Handshake {
        identity: &identity,
        password,
        role,
        registry: modules,
        position,
        service: offer,
        sends_input: capture.is_some(),
    };
    let identified = cursio_core::link::identify(transport, &plan).await?;
    // Report who this turned out to be before the slower half runs: for an
    // inbound connection this is the first moment we know at all, and the mesh
    // wants such a peer marked online without waiting out pairing.
    if let Some(cb) = on_identified {
        cb(identified.peer.peer_id, &identified.peer.display_name);
    }
    let link = cursio_core::link::secure(identified, &plan).await?;
    // Now that the peer has said where it stands, capture can watch the edge
    // between us. This is what replaced `--peer-edge`: both machines already
    // know, and the flag only existed because the barrier had to be armed
    // before anyone had spoken.
    cursio_input::set_peer_edge(link.peer_edge);


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
                // `Idle` deliberately still observes edges on macOS so the
                // initially non-owning machine can start a handoff. Panic is
                // different: it promises that barriers are DOWN. Forget the
                // edge until the next handshake restores it, otherwise a mouse
                // still moving toward the edge immediately hands control away
                // again (the repeated PANIC -> crossing loop in the logs).
                cursio_input::set_peer_edge(None);
                set_cursor_hidden(cursor_ref.borrow_mut().as_mut(), false);
                set_injecting(release_ref.borrow_mut().as_mut(), true);
                tracing::warn!("panic recovery: input returned to this machine, barriers down");
                return;
            }
            // Three states, not two. Dormant grabs local input and streams it,
            // and that is only true while we are actually driving the peer.
            // Holding the cursor is Active; neither is Idle, where this
            // machine's own keyboard and mouse simply act on it.
            let dormant = ownership.driving;
            if let Some(g) = grab_ref.as_ref() {
                g.set_mode(match (ownership.active, ownership.driving) {
                    (true, _) => cursio_input::InputMode::Active,
                    (false, true) => cursio_input::InputMode::Dormant,
                    (false, false) => cursio_input::InputMode::Idle,
                });
            }
            // Cursor visibility before the injector, and both before the log
            // line. On macOS showing the cursor also warps it back from the
            // corner it was parked in, and the injector re-seeds its shadow
            // position from the real pointer the moment it goes active — in the
            // other order it seeds from the parking corner, and the peer's next
            // movement teleports the pointer there.
            set_cursor_hidden(cursor_ref.borrow_mut().as_mut(), dormant);
            // The cursor just left across the peer-facing edge, so this
            // machine's own pointer is pinned against it — a `pos_norm` of
            // exactly 0.0 or 0.9989 in the log is that. Left there, the next
            // small movement is outward and reads as another crossing, and two
            // handoffs with no return between them leave this machine believing
            // it drives a peer the user is not sitting at. Windows already does
            // this from its hook; macOS had no equivalent.
            if !ownership.active {
                if let Some(c) = cursor_ref.borrow_mut().as_mut() {
                    c.nudge_off_peer_edge();
                }
            }
            set_injecting(release_ref.borrow_mut().as_mut(), ownership.active);
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
        // broken. That was the "pressed a key and everything froze" report.
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
/// (it only sees "hand off to the peer"). Until a peer has greeted us there is
/// no edge to match against, so crossings are ignored entirely (the receiver
/// still drains so the channel doesn't back up). Shares `cmd_tx` with the
/// hotkey filter: both are just ways to ask for the same handoff.
fn spawn_edge_handoff(
    mut edges: tokio::sync::mpsc::UnboundedReceiver<cursio_input::EdgeCrossing>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<session::OwnershipCmd>,
) {
    tokio::spawn(async move {
        while let Some(crossing) = edges.recv().await {
            // Read now, not captured when this task started. The edge is
            // learned from the peer's position in its greeting, which is after
            // this forwarder exists — comparing against the command-line flag
            // meant every crossing was discarded the moment `--peer-edge`
            // stopped being required.
            match cursio_input::peer_edge() {
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
/// Linux uses a uinput virtual pointer, Windows `SendInput`, macOS a `CGEvent`
/// posted at the HID tap. On any other platform there's no backend yet, so
/// injection falls back to logging (see [`inject`]).
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
    #[cfg(target_os = "macos")]
    {
        use cursio_input::InputInjector as _;
        match cursio_input::MacInjector::new() {
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
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        tracing::warn!("no input backend on this platform yet; peer input will be logged only");
        None
    }
}

/// Create the platform cursor controller (hide/show our own cursor), or `None`
/// if unsupported. Linux is currently a no-op backend (Wayland can't hide; X11
/// XFixes not wired yet); Windows really hides the cursor, and macOS hides it
/// for as long as Cursio is the frontmost application.
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
    #[cfg(target_os = "macos")]
    {
        use cursio_input::CursorControl as _;
        match cursio_input::MacCursor::new() {
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
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Tell the injector whether peer events should act on this machine.
///
/// Only true while this machine holds the shared cursor — which is also the
/// only time a peer sends any. Setting it matters beyond dropping stray events:
/// a backend uses the transition to false→true to re-read state that went stale
/// while it was idle, and the macOS one in particular re-seeds the shadow
/// cursor it has to keep because `CGEvent` has no relative motion. Left
/// permanently active, that shadow drifts from the real pointer every time the
/// user moves this machine's own mouse, and the peer's first movement afterwards
/// teleports the cursor to wherever the shadow had got to.
fn set_injecting(injector: Option<&mut Box<dyn cursio_input::InputInjector>>, active: bool) {
    if let Some(inj) = injector {
        inj.set_active(active);
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
) -> anyhow::Result<NodeBase> {
    let listener = TcpListenerHandle::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    let bound = listener.local_addr();
    tracing::info!(%bound, "listening for peers");

    // Announce ourselves so other nodes can find us. Pin mDNS to the real LAN
    // interface so VPN/virtual interfaces don't break it.
    let instance = discovery::mdns_label(&identity.display_name, "cursio-node");
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
    //
    // It is started without an edge on purpose. Which edge borders a peer is a
    // per-link answer derived from the two `--at` positions, and no peer has
    // said hello yet; each backend is pointed at one later through
    // [`cursio_input::set_peer_edge`].
    let capture: SharedCapture =
        std::rc::Rc::new(std::cell::RefCell::new(make_persistent_capture(send_input)));

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
fn make_persistent_capture(enabled: bool) -> Option<PersistentCapture> {
    if !enabled {
        return None;
    }

    let handle = make_capture()?;
    let grab = handle.grab.clone();

    // Both forwarders are permanent too: they own the raw receivers and feed the
    // two channels a session consumes. One shared command channel, because the
    // hotkey and an edge crossing are just two ways to ask for the same handoff.
    let (cmd_tx, commands) = tokio::sync::mpsc::unbounded_channel();
    let events = spawn_hotkey_filter(handle.events, cmd_tx.clone(), grab.clone());
    spawn_edge_handoff(handle.edges, cmd_tx);

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
/// access) or the Wayland portal; on Windows it installs RawInput hooks; on
/// macOS it opens a `CGEventTap` (needs the Accessibility permission).
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn make_capture() -> Option<cursio_input::CaptureHandle> {
    match cursio_input::spawn_capture() {
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

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn make_capture() -> Option<cursio_input::CaptureHandle> {
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
    /// Unplaced machines must border nothing: a default that implied adjacency
    /// would hand the cursor away on a shove the user never meant.
    fn an_unplaced_node_sits_at_the_origin() {
        let Command::Mesh { at, .. } = Cli::try_parse_from(["cursio", "mesh"]).unwrap().command
        else {
            panic!("expected mesh");
        };
        assert_eq!(at, cursio_protocol::Position::new(0, 0));
        assert_eq!(at.edge_toward(at), None);
    }

    #[test]
    /// The case named sides could not express: with three in a row, each knows
    /// which way its neighbour lies without anyone being a designated centre.
    fn a_row_of_machines_faces_the_right_way() {
        let Command::Mesh { at, .. } = Cli::try_parse_from(["cursio", "mesh", "--at", "1,0"])
            .unwrap()
            .command
        else {
            panic!("expected mesh");
        };
        assert_eq!(at, cursio_protocol::Position::new(1, 0));
        assert_eq!(
            at.edge_toward(cursio_protocol::Position::new(2, 0)),
            Some(cursio_protocol::Edge::Right)
        );
        assert_eq!(
            at.edge_toward(cursio_protocol::Position::new(0, 0)),
            Some(cursio_protocol::Edge::Left)
        );
    }

    /// Two processes on one machine are the same peer, with the same id, so
    /// the mesh election ranks them identically and both conclude they won.
    /// The lock is the only thing that tells them apart.
    #[test]
    fn only_one_holder_of_the_local_claim_at_a_time() {
        let path = std::env::temp_dir().join(format!("cursio-test-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let first = take_lock(&path).expect("the first caller takes it");
        assert!(
            take_lock(&path).is_none(),
            "a second caller must be refused"
        );

        // Released with the descriptor, so the next run takes over even if the
        // holder crashed without cleaning up.
        drop(first);
        assert!(take_lock(&path).is_some(), "releasing must hand it on");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    /// Half the grid is negative, and clap reads a leading minus as a flag
    /// unless told otherwise — so `--at -1,0` was refused outright.
    fn a_machine_left_of_the_origin_can_be_placed() {
        for argv in [
            vec!["cursio", "mesh", "--at", "-1,0"],
            vec!["cursio", "mesh", "--at=-1,0"],
            vec!["cursio", "mesh", "--at", "-2,-3"],
        ] {
            let expected = if argv.last().unwrap().contains("-2") {
                cursio_protocol::Position::new(-2, -3)
            } else {
                cursio_protocol::Position::new(-1, 0)
            };
            let Command::Mesh { at, .. } = Cli::try_parse_from(&argv).unwrap().command else {
                panic!("expected mesh");
            };
            assert_eq!(at, expected, "{argv:?}");
        }
    }

    #[test]
    /// A mistyped layout is otherwise discovered as "the cursor does not
    /// cross", which is a long way from the comma that was missing.
    fn a_malformed_position_is_refused_with_its_shape_named() {
        let Err(error) = Cli::try_parse_from(["cursio", "mesh", "--at", "1"]) else {
            panic!("a position without a comma must be refused");
        };
        let error = error.to_string();
        assert!(error.contains("1,0"), "should show the shape: {error}");
        assert!(Cli::try_parse_from(["cursio", "mesh", "--at", "a,0"]).is_err());
    }

    /// Pairing used to be two commands that had to be run on the right two
    /// machines in the right order. It is one now, run on both.
    #[test]
    fn there_is_one_command_and_pairing_is_a_flag_on_it() {
        assert!(
            Cli::try_parse_from(["cursio", "join"]).is_err(),
            "join is gone; `mesh --pair` is both halves"
        );
        let Command::Mesh { pair, .. } = Cli::try_parse_from(["cursio", "mesh", "--pair"])
            .unwrap()
            .command
        else {
            unreachable!();
        };
        assert!(pair);
    }

    /// An address is only meaningful while a window is open, so naming one
    /// without `--pair` is a mistake worth catching at the command line rather
    /// than by silently doing nothing.
    #[test]
    fn an_address_to_pair_with_needs_the_window_open() {
        assert!(
            Cli::try_parse_from(["cursio", "mesh", "--pair-addr", "192.168.1.5:46073"]).is_err()
        );
        let Command::Mesh { pair_addr, .. } = Cli::try_parse_from([
            "cursio",
            "mesh",
            "--pair",
            "--pair-addr",
            "192.168.1.5:46073",
        ])
        .unwrap()
        .command
        else {
            unreachable!();
        };
        assert_eq!(
            pair_addr,
            Some("192.168.1.5:46073".parse::<SocketAddr>().unwrap())
        );
    }

    /// Receiving is on by default: a user who wants an iPhone to find this
    /// machine should not have to know a flag exists.
    #[test]
    fn airplay_receives_without_being_asked_to() {
        let Command::Mesh { airplay, .. } =
            Cli::try_parse_from(["cursio", "mesh"]).unwrap().command
        else {
            unreachable!();
        };
        assert_eq!(
            AirPlayMode::parse(&airplay).unwrap(),
            AirPlayMode::On { wanted: None }
        );
    }

    #[test]
    fn the_mode_separates_participation_from_the_window_flag() {
        assert_eq!(AirPlayMode::parse("off").unwrap(), AirPlayMode::Off);
        assert!(!AirPlayMode::parse("off").unwrap().participates());
        assert_eq!(
            AirPlayMode::parse("control").unwrap(),
            AirPlayMode::On { wanted: None }
        );
        assert!(AirPlayMode::parse("screen").is_err());
    }

    /// A profile in place of `screen` says both things at once, which is the
    /// whole reason `--res` and `--fps` stopped being separate flags.
    #[test]
    fn a_profile_requests_mesh_quality_without_implying_a_window() {
        let cases = [
            ("1080p30", Resolution::FullHd, 30),
            ("1080p", Resolution::FullHd, 60),
            ("2k", Resolution::Qhd, 60),
            ("1440p24", Resolution::Qhd, 24),
        ];
        for (spec, resolution, fps) in cases {
            let quality = AirPlayMode::parse(spec)
                .unwrap_or_else(|e| panic!("{spec}: {e}"))
                .wanted()
                .unwrap_or_else(|| panic!("{spec} should request a quality"));
            assert_eq!(
                (resolution_for(quality), quality.fps),
                (resolution, fps),
                "{spec}"
            );
        }
    }

    /// Three ways of writing the same thing, because all three are how people
    /// write it and none of them is ambiguous.
    #[test]
    fn the_rate_may_be_separated_or_not() {
        for spec in ["1080p30", "1080p:30", "1080p@30", "1080P-30", "1080p:30fps"] {
            let quality = AirPlayMode::parse(spec).unwrap().wanted().unwrap();
            assert_eq!(quality.fps, 30, "{spec}");
        }
    }

    #[test]
    fn a_mode_that_is_not_a_mode_names_what_it_could_be() {
        let error = AirPlayMode::parse("1080i").unwrap_err();
        assert!(error.contains("1080p60"), "{error}");
        assert!(error.contains("off"), "{error}");
        // Out of range is the profile's own complaint, not this one.
        assert!(AirPlayMode::parse("1080p999").is_err());
    }

    /// The phone is picking "my desktop", not a machine. A name carrying the
    /// hostname is what produced two entries for one mesh.
    #[test]
    fn the_mesh_offers_one_name_whatever_the_hostname_is() {
        assert_eq!(AIRPLAY_RECEIVER_NAME, "Cursio");
        assert!(!AIRPLAY_RECEIVER_NAME.contains('-'));
    }
}
