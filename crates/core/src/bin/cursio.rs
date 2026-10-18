//! `cursio` CLI — discovery, pairing, and sessions between machines on a LAN.
//!
//! **There is no server.** Every machine runs the same command, `mesh`, and
//! every node both accepts and dials: if one goes away the others keep talking
//! to each other directly and reconnect when it returns. Nothing is a hub.
//!
//! Subcommands:
//! - `mesh` — run a node: listen, dial every trusted peer, run the modules
//!   `config.toml` names (flags override it for one run).
//! - `config` — print the config `mesh` would run with.
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
use cursio_core::input_hub::{InputHub, MachineInput, Placement};
use cursio_core::live::Shared;
use cursio_core::node_status::NodeStatusMonitor;
use cursio_core::pairing_window::PairingWindow;
use cursio_core::session::{self};
use cursio_core::NodeIdentity;
use cursio_protocol::{Fitness, ModuleId, PeerId, Quality, ServiceId, ServiceOffer};
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

    /// Read the node's config from this file instead of the platform default
    /// (`~/.config/cursio/config.toml`, `%APPDATA%\cursio\cursio\config\config.toml`).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

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
    ///
    /// What runs comes from `config.toml` (see `cursio config`); every flag
    /// below overrides the file for this run only.
    Mesh(MeshArgs),
    /// Print the config `cursio mesh` would run with, and where it is read from.
    Config,
    /// Install, switch and roll back versions of Cursio on this machine.
    Update {
        #[command(subcommand)]
        action: cursio_core::update::cli::UpdateAction,
    },
    /// Ask the running node what it is doing: peers, modules, latency, problems.
    Status {
        /// Print the raw control reply instead of a summary.
        #[arg(long)]
        json: bool,
    },
    /// Pair with a new machine using a one-time code, through the running node.
    ///
    /// Without a code, this machine shows one; run `cursio pair <CODE>` (or
    /// enter it in the app) on the other machine within five minutes.
    Pair {
        /// The code shown on the other machine, e.g. `ABCD-EFGH`.
        code: Option<String>,
    },
    /// Remove a paired machine from this mesh: forget its key, close its link,
    /// stop accepting its builds. It has to be paired again to come back.
    ///
    /// Through the running node when there is one, so the link closes at once;
    /// otherwise only the files are changed, which is all there is to change.
    Forget {
        /// The machine's name or id, as `cursio status` shows it.
        machine: String,
    },
    /// Send a link, text or a file to a connected machine.
    ///
    /// A link opens in that machine's browser and a file in its default
    /// program; `--copy` puts it on that machine's clipboard instead, and
    /// `--save` keeps a file in its Downloads. Goes through the running node.
    Send {
        /// A web address, a file on this machine, or text.
        target: String,
        /// The machine's name or id, as `cursio status` shows it.
        #[arg(long)]
        to: String,
        #[arg(long, conflicts_with = "save")]
        copy: bool,
        #[arg(long)]
        save: bool,
    },
    /// Make a connected phone ring loudly, even on silent, until it is found.
    ///
    /// It stops when the phone is unlocked or its notification answered, or
    /// by itself after a while. Goes through the running node.
    Ring {
        /// The phone's name or id, as `cursio status` shows it.
        phone: String,
    },
    /// Show a connected phone's screen in a window here, and use it with the
    /// mouse: a click is a tap, a drag a swipe, the right button Back. The
    /// phone asks its user to allow it first. Goes through the running node.
    Screen {
        /// The phone's name, as `cursio status` shows it.
        phone: String,
        /// Stop showing it instead.
        #[arg(long)]
        stop: bool,
    },
    /// Answer, decline, end or place a call on a connected phone. Goes through
    /// the running node.
    Call {
        #[command(subcommand)]
        action: CallCommand,
    },
    /// Make this computer the headset and speaker of paired phones, taking
    /// those roles from PipeWire (Linux).
    Bluetooth {
        #[command(subcommand)]
        action: BluetoothAction,
    },
    /// Read and send a connected phone's text messages, through the running
    /// node.
    Sms {
        #[command(subcommand)]
        action: SmsAction,
    },
    /// List peers discovered on the LAN, then exit.
    Peers {
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
    },
    /// Cursio as this user's ScreenCast portal backend (Linux), which is how
    /// peers' screens reach Discord's and OBS's "share your screen" dialogs.
    Portal {
        #[command(subcommand)]
        action: PortalAction,
    },
    /// Hold a peer's drives for an elevated node, in the session Explorer runs
    /// in (Windows). Started by the node itself.
    #[command(name = "mount-helper", hide = true)]
    MountHelper {
        /// The node's process id; the helper exits with it.
        #[arg(long)]
        parent: u32,
    },
}

/// `cursio mesh`'s flags. Each is an override of `config.toml`: absent means
/// "whatever the file says", so every module switch comes as an on/off pair —
/// a lone `--audio` that is false when omitted could never leave the file's
/// answer standing.
#[derive(clap::Args)]
struct MeshArgs {
    /// TCP port to listen on. A **stable default** (not 0/OS-assigned) is
    /// deliberate: the peer caches our last address to reconnect when mDNS is
    /// slow or blocked, and a port that changed every restart made that cache
    /// point at a dead port — so a bounced node was never redialled. Pass 0
    /// explicitly for an ephemeral port.
    #[arg(long)]
    port: Option<u16>,
    /// Capture this machine's mouse/keyboard and share it with peers.
    #[arg(long, overrides_with = "no_input")]
    send_input: bool,
    /// Do not capture or share this machine's mouse/keyboard.
    #[arg(long, overrides_with = "send_input")]
    no_input: bool,
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
            allow_hyphen_values = true,
            value_parser = parse_position
        )]
    at: Option<cursio_protocol::Position>,
    /// Share audio with peers.
    #[arg(long, overrides_with = "no_audio")]
    audio: bool,
    #[arg(long, overrides_with = "audio")]
    no_audio: bool,
    /// Render peer audio into the output device whose name contains this
    /// (Windows only; ignored elsewhere, with a line in the log saying so).
    #[arg(long)]
    audio_render_device: Option<String>,
    /// What a peer recording from this machine's microphones hears: `auto`
    /// (follow the mic's noise floor), `off`, or a level in dBFS such as -55.
    #[arg(long, allow_hyphen_values = true)]
    mic_gate: Option<String>,
    /// Share the clipboard with peers (text, images, and other formats).
    #[arg(long, overrides_with = "no_clipboard")]
    clipboard: bool,
    #[arg(long, overrides_with = "clipboard")]
    no_clipboard: bool,
    /// Share disks both ways: this machine's volumes appear on the peer,
    /// and the peer's appear here under `~/Cursio/<peer>/`.
    ///
    /// Unless `--files-share` or `[files.share]` narrows it, every disk is
    /// offered read-write, so anything either machine can reach, the other
    /// can change. Linux mounts with FUSE and Windows with WinFsp; macOS
    /// serves its own volumes but cannot yet mount the peer's.
    #[arg(long, overrides_with = "no_files")]
    files: bool,
    #[arg(long, overrides_with = "files")]
    no_files: bool,
    /// Offer only this root to peers instead of every disk; repeat for
    /// several. A volume's root (`/`, `C:\`) offers that disk, anything
    /// else offers that folder alone. Append `:ro` to refuse every change
    /// the peer asks for there: `C:\:ro`, `/home/me/Projects:ro`. Replaces
    /// the config's `[files.share]` for this run.
    #[arg(long = "files-share", value_name = "PATH[:ro]")]
    files_share: Vec<String>,
    /// Share this machine's USB devices with the mesh, and use the devices
    /// its peers are sharing. A device stays usable here while it is
    /// shared.
    ///
    /// Needs the exporting half in place: `sudo modprobe usbip-host` on
    /// Linux, `usbipd-win` installed on Windows. Without it the module is
    /// not advertised and the log says why, rather than offering devices
    /// that would fail on claim.
    #[arg(long, overrides_with = "no_usb")]
    usb: bool,
    #[arg(long, overrides_with = "usb")]
    no_usb: bool,
    /// When this machine is the selected AirPlay host, offer the mirrored
    /// screen to every peer instead of only peers using --airplay-window.
    ///
    /// The peer sees it as an ordinary transferred window: the H.264 from
    /// AirPlay is forwarded untouched, so nothing is re-encoded on the way.
    #[arg(long, overrides_with = "no_share_airplay")]
    share_airplay: bool,
    #[arg(long, overrides_with = "share_airplay")]
    no_share_airplay: bool,
    /// Offer every peer's windows and monitors to screen-sharing software
    /// here (Discord, OBS, a browser call), without drawing them on this
    /// desktop.
    ///
    /// Linux: they appear in the app's own "share your screen" dialog,
    /// through Cursio's ScreenCast portal backend (install it once with
    /// `cursio portal install`). Windows: an unseen window per peer,
    /// "Cursio — <peer>", appears in the app's window list; choose what it
    /// shows from its icon in the notification area.
    #[arg(long, overrides_with = "no_remote_screen")]
    remote_screen: bool,
    #[arg(long, overrides_with = "remote_screen")]
    no_remote_screen: bool,
    /// Refuse peers' --remote-screen views of this machine.
    #[arg(long, overrides_with = "screen_host")]
    no_screen_host: bool,
    /// Serve peers' --remote-screen views of this machine.
    #[arg(long, overrides_with = "no_screen_host")]
    screen_host: bool,
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
    #[arg(long, value_name = "MODE")]
    airplay: Option<String>,
    /// Show the mirrored iPhone screen on this machine.
    ///
    /// Separate from `--airplay` because they answer different questions:
    /// that one is about the picture the mesh receives, this one about
    /// which desk it appears on. A machine can hold the receiver and show
    /// nothing — passing the stream to peers with `--share-airplay` — or
    /// show a picture another machine is receiving.
    #[arg(long, overrides_with = "no_airplay_window")]
    airplay_window: bool,
    #[arg(long, overrides_with = "airplay_window")]
    no_airplay_window: bool,
    /// AirPlay control port.
    #[arg(long)]
    airplay_port: Option<u16>,
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
    #[arg(long, value_name = "SPEC")]
    mac_modifiers: Option<String>,
}

/// `Some(true)` for the on flag, `Some(false)` for the off one, `None` for
/// neither. `overrides_with` already leaves at most one of them set.
fn switch(on: bool, off: bool) -> Option<bool> {
    match (on, off) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

impl MeshArgs {
    fn overrides(&self) -> cursio_core::config::Overrides {
        cursio_core::config::Overrides {
            port: self.port,
            position: self.at,
            input: switch(self.send_input, self.no_input),
            clipboard: switch(self.clipboard, self.no_clipboard),
            audio: switch(self.audio, self.no_audio),
            files: switch(self.files, self.no_files),
            usb: switch(self.usb, self.no_usb),
            remote_screen: switch(self.remote_screen, self.no_remote_screen),
            screen_host: switch(self.screen_host, self.no_screen_host),
            audio_render_device: self.audio_render_device.clone(),
            audio_mic_gate: self.mic_gate.clone(),
            airplay_mode: self.airplay.clone(),
            airplay_window: switch(self.airplay_window, self.no_airplay_window),
            airplay_share: switch(self.share_airplay, self.no_share_airplay),
            airplay_port: self.airplay_port,
            airplay_interface: self.airplay_interface,
            mac_modifiers: self.mac_modifiers.clone(),
        }
    }
}

#[derive(Subcommand)]
enum CallCommand {
    /// List the calls of every connected phone.
    List,
    /// Answer the call ringing on PHONE.
    Answer { phone: String },
    /// Refuse the call ringing on PHONE.
    Decline { phone: String },
    /// End the call that is up on PHONE.
    Hangup { phone: String },
    /// Call NUMBER from PHONE.
    Dial { phone: String, number: String },
    /// Mute (or with --off, unmute) the microphone on the call.
    Mute {
        phone: String,
        #[arg(long)]
        off: bool,
    },
    /// Move the call's voice to this computer, or with --phone back to it.
    Audio {
        phone: String,
        #[arg(long)]
        phone_side: bool,
    },
}

#[derive(Subcommand)]
enum BluetoothAction {
    /// Take the hands-free and media sink roles from PipeWire (restarts
    /// WirePlumber; Bluetooth audio drops for a second).
    Install,
    /// Give those roles back to PipeWire.
    Uninstall,
}

#[derive(Subcommand)]
enum PortalAction {
    /// Route ScreenCast through Cursio (per user; restart the portal after).
    Install,
    /// Give ScreenCast back to GNOME alone.
    Uninstall,
    /// Forward every screen-cast request to GNOME; what D-Bus starts when no
    /// `cursio mesh` holds the backend's name.
    Relay,
}

#[derive(Subcommand)]
enum SmsAction {
    /// The phone's conversations, newest first.
    List {
        /// The phone's name or id, as `cursio status` shows it.
        phone: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// One conversation, oldest of the page first.
    Read {
        phone: String,
        /// The conversation's id, as `cursio sms list` shows it.
        thread: i64,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Send an SMS from the phone.
    Send {
        phone: String,
        number: String,
        text: String,
    },
}

/// What this machine offers over `--files`: the flag's roots when any were
/// given, else the config's `[files.share]`, else every disk.
///
/// The flag replaces the table rather than adding to it, so a one-off
/// `--files-share /tmp:ro` narrows this run instead of widening the file.
fn share_policy(
    flag: &[String],
    table: Option<toml::Table>,
) -> anyhow::Result<cursio_files::SharePolicy> {
    if !flag.is_empty() {
        return Ok(cursio_files::SharePolicy::from_args(flag)?);
    }
    match table {
        Some(table) => table
            .try_into()
            .map_err(|e| anyhow::anyhow!("config [files.share]: {e}")),
        None => Ok(cursio_files::SharePolicy::everything()),
    }
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

/// Keep glibc to a few malloc arenas, before any thread exists.
///
/// glibc gives a thread that finds every arena busy a new one, up to eight per
/// core, and the first large block in a new arena is carved from that arena's
/// fresh heap and then cleared with `memset` by `calloc` — outside the main
/// arena it does not trust fresh memory to be zero. `fuser` allocates a zeroed
/// 16 MiB request buffer on every mount thread, and a mount runs dozens of
/// threads (`cursio_files::bridge::mount_threads`), so each buffer landed in
/// an arena of its own and every page of it became resident: 1.5 GB of zeros
/// on a node with four peer volumes mounted, measured page by page.
///
/// With the arenas capped, a new thread shares an existing one, its buffer
/// does not fit that arena's top and is served by a fresh `mmap`, which
/// `calloc` knows is zero and leaves untouched — resident only where a request
/// actually lands. Measured with sixteen threads each holding one such buffer:
/// 258 MB by default, 49 MB with four arenas. Four rather than one because the
/// input, audio and video threads allocate on their hot paths, and one lock
/// for all of them would trade memory for contention.
///
/// The cap alone held only until the first large block was freed. glibc's mmap
/// threshold is dynamic: freeing an mmapped block raises it to that block's
/// size, up to 32 MiB, and from then on a 16 MiB buffer is carved from an
/// arena heap — memory earlier mounts already dirtied, which `calloc` must
/// clear. Every peer reconnect rebuilds the mounts, so an idle node climbed to
/// 2.6 GB with 32 arena heaps each holding exactly three such buffers.
/// Pinning the threshold keeps it fixed: blocks of 16 MiB and up always come
/// from `mmap`, lazily zeroed and returned on free, while everything smaller —
/// video frames included — stays in the arenas as before. Measured over three
/// rebuilds of 48 mount threads: 261 MB resident unpinned, 2 MB pinned.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
    // SAFETY: mallopt only adjusts allocator parameters; called before any
    // other thread is started, so nothing is allocating concurrently.
    if unsafe { libc::mallopt(libc::M_ARENA_MAX, 4) } == 0 {
        eprintln!("warning: glibc refused M_ARENA_MAX; mount threads may hold 16 MiB each");
    }
    // SAFETY: as above.
    if unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, 16 * 1024 * 1024) } == 0 {
        eprintln!("warning: glibc refused M_MMAP_THRESHOLD; mount buffers may stay resident after a reconnect");
    }
}

/// Let SIGINT stop this process however it was started.
///
/// A program started in the background by a non-interactive shell inherits
/// SIGINT as ignored, and every node an update helper or `nohup … &` brings up
/// is such a program. The helper stops the old node with SIGINT, so a node
/// started that way could never be replaced: on one machine the switch timed
/// out, retried every 15 s, and the machine stayed on an old protocol for good.
#[cfg(unix)]
fn accept_interrupt() {
    // SAFETY: restoring a signal's default disposition before any thread or
    // handler exists has no memory effects.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
}

/// Lift the open-descriptor limit from the soft default to the hard one.
///
/// A node's descriptors scale with what it serves, not with what it does: each
/// peer volume mounted here runs up to 64 FUSE threads, each with its own
/// `/dev/fuse` clone and the eventfd/epoll pair of its loop, so two peers'
/// volumes alone take ~700 of the 1024 a desktop session hands out. Every file
/// a peer holds open on *our* disks costs one more, and a program walking a
/// shared volume used the remaining ~300 in minutes: from then on every file
/// request failed with EMFILE and the peer's mounts went away. The hard limit
/// (524288 under systemd) is what the kernel will actually allow; the soft
/// 1024 exists for programs still using `select`, which nothing here does.
#[cfg(unix)]
fn raise_descriptor_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit read and write a struct we own.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            eprintln!("warning: cannot read the open-file limit; serving shared disks may run out of descriptors");
            return;
        }
        if limit.rlim_cur >= limit.rlim_max {
            return;
        }
        // macOS reports an unlimited hard limit it then refuses above
        // OPEN_MAX (10240, `sys/syslimits.h`; the libc crate does not carry it).
        #[cfg(target_os = "macos")]
        let target = limit.rlim_max.min(10240);
        #[cfg(not(target_os = "macos"))]
        let target = limit.rlim_max;
        let raised = libc::rlimit {
            rlim_cur: target,
            rlim_max: limit.rlim_max,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) != 0 {
            eprintln!(
                "warning: cannot raise the open-file limit above {}; serving shared disks may run out of descriptors",
                limit.rlim_cur
            );
        }
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    limit_malloc_arenas();
    #[cfg(unix)]
    raise_descriptor_limit();
    #[cfg(unix)]
    accept_interrupt();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("cannot start the async runtime: {e}"))?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // The portal backend's screen chooser is this executable in a child
    // process (see `linux_chooser`); it takes no flags a user would type.
    #[cfg(target_os = "linux")]
    if std::env::args().nth(1).as_deref() == Some(cursio_window::linux_chooser::CHOOSER_ARG) {
        std::process::exit(cursio_window::linux_chooser::run_chooser_process());
    }
    // Started as the install's stable copy: run the current version instead.
    if let Some(target) = cursio_core::update::layout::handover("cursio") {
        cursio_core::update::layout::hand_over(&target)?;
        return Ok(());
    }
    let cli = Cli::parse();

    // Its own log file: the node is writing `cursio`'s at the same moment.
    if let Command::MountHelper { parent } = cli.command {
        let _log = cursio_core::logging::init_with_verbosity("cursio-mount-helper", cli.verbose);
        return cursio_core::mount_helper::run_helper(parent).await;
    }

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
    if matches!(cli.command, Command::Mesh(_))
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

    let config_path = match cli.config {
        Some(path) => path,
        None => cursio_core::config::default_path()?,
    };

    match cli.command {
        Command::Update { action } => cursio_core::update::cli::run(action, &config_path).await,
        Command::Config => {
            let config = cursio_core::config::Config::load(&config_path)?;
            let origin = match config_path.exists() {
                true => "read from",
                false => "defaults; no file at",
            };
            println!("# {origin} {}", config_path.display());
            print!("{}", config.to_toml()?);
            Ok(())
        }
        Command::Status { json } => print_status(json).await,
        Command::Pair { code } => pair_with_code(code).await,
        Command::Mesh(args) => {
            let overrides = args.overrides();
            let config =
                cursio_core::config::Config::load(&config_path)?.with_overrides(&overrides);
            tracing::info!(
                path = %config_path.display(),
                pinned_by_flags = ?overrides.pinned(),
                "config"
            );
            // Before the registry: the update module advertises only once
            // there is an agent to hand builds to.
            cursio_core::update::agent::start(config_path.clone());
            let MeshArgs {
                pair,
                pair_addr,
                files_share: files_share_flag,
                ..
            } = args;
            let pairing = PairingWindow::closed();
            // What a change in the app reaches without a restart: the
            // switches and settings every link follows, and the options the
            // factories read when they build a module.
            let live = cursio_core::live::LiveConfig::new(&config);
            let screen_host = Shared::new(config.modules.screen_host);
            let files_share =
                Shared::new(share_policy(&files_share_flag, config.files.share.clone())?);
            let render_device = Shared::new(config.audio.render_device.clone());
            let mic_gate = Shared::new(cursio_core::audio_gate::MicGate::from_config(
                config.audio.mic_gate.as_deref(),
            )?);
            let applier: cursio_core::control::Applier = {
                let live = live.clone();
                let screen_host = screen_host.clone();
                let files_share = files_share.clone();
                let render_device = render_device.clone();
                let mic_gate = mic_gate.clone();
                std::sync::Arc::new(move |running, next| {
                    let mut plan = cursio_core::live::plan(running, next);
                    if let Some(on) = plan.screen_host {
                        screen_host.set(on);
                    }
                    if plan.files_share_changed {
                        // A `--files-share` flag replaces the file's policy for
                        // this run, so the file changing changes nothing.
                        if files_share_flag.is_empty() {
                            match share_policy(&files_share_flag, next.files.share.clone()) {
                                Ok(policy) => files_share.set(policy),
                                Err(error) => {
                                    tracing::warn!(%error, "keeping the running share policy");
                                    plan.restart.retain(|id| *id != ModuleId::FILES);
                                }
                            }
                        } else {
                            plan.restart.retain(|id| *id != ModuleId::FILES);
                        }
                    }
                    if let Some(device) = &plan.render_device {
                        render_device.set(device.clone());
                    }
                    if let Some(gate) = &plan.mic_gate {
                        match cursio_core::audio_gate::MicGate::from_config(gate.as_deref()) {
                            Ok(gate) => mic_gate.set(gate),
                            Err(error) => tracing::warn!(%error, "keeping the running mic gate"),
                        }
                    }
                    if let Some(spec) = &plan.mac_modifiers {
                        if let Err(error) = apply_modifier_map(spec) {
                            tracing::warn!(%error, "keeping the running modifier map");
                        }
                    }
                    tracing::info!(
                        switches = ?plan.switches,
                        position = ?plan.position,
                        input = ?plan.input,
                        restart = ?plan.restart,
                        "config changed; applying it to the running node"
                    );
                    live.apply(&plan);
                })
            };
            let control = cursio_core::control::NodeState::new(
                &identity,
                config_path.clone(),
                overrides.clone(),
                config.clone(),
                pairing.clone(),
                applier,
            );
            {
                let live = live.clone();
                control.set_on_forget(std::sync::Arc::new(move |peer| live.forget(peer)));
            }
            start_control(control.clone());
            let cursio_core::config::Config {
                port,
                modules,
                airplay: airplay_config,
                input: input_config,
                ..
            } = config;
            let send_input = modules.input;
            let remote_screen = modules.remote_screen;
            let share_airplay = airplay_config.share;
            let airplay = airplay_config.mode;
            let airplay_window = airplay_config.window;
            let airplay_port = airplay_config
                .port
                .unwrap_or(cursio_airplay::DEFAULT_CONTROL_PORT);
            let airplay_interface = airplay_config.interface;
            let mac_modifiers = input_config.mac_modifiers;
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
            if pair {
                let password = read_pairing_password()?;
                tracing::info!(
                    minutes = cursio_core::pairing_window::PASSWORD_LIFETIME.as_secs() / 60,
                    "pairing window open; run `cursio mesh --pair` on the other machine too"
                );
                pairing.open_password(password, pair_addr);
            }
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
                render_device,
                mic_gate,
                control.mixer(),
                files_share,
                live.switches(),
                WindowOpts {
                    airplay: airplay_tap.clone().map(|tap| {
                        cursio_core::window_session::AirPlayShare {
                            tap,
                            state: service_state.clone(),
                            local: identity.peer_id,
                            force: share_airplay,
                        }
                    }),
                    remote_screen,
                    screen_host,
                    screen_hub: start_screen_portal(remote_screen),
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
            // Every desktop has an input backend; the rest follow the build.
            control.set_available(cursio_core::control::ModuleSwitch::available(
                &modules.available(),
                true,
            ));
            let mesh = run_mesh(
                identity.clone(),
                port,
                send_input,
                live,
                modules,
                pairing,
                offer,
                control.clone(),
            );
            let node = async move {
                if !airplay.participates() {
                    // Nothing bound and nothing announced, and the service module
                    // is refusing on this machine's behalf, so a peer that asks
                    // learns to look elsewhere. With every machine here, AirPlay
                    // does not run at all — which is what asking for it means.
                    tracing::info!("AirPlay is off on this machine");
                    return mesh.await;
                }
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
            };
            // The file changing under the node — a text editor, a script — is
            // applied the way a change from the app is. The app's own changes
            // are applied before its reply and come back through here as no
            // change at all.
            let watcher = control.clone();
            tokio::spawn(async move {
                loop {
                    let running = watcher.running();
                    let next = cursio_core::config::wait_for_change(
                        watcher.config_path(),
                        watcher.overrides(),
                        &running,
                    )
                    .await;
                    let waiting = watcher.apply(next);
                    if !waiting.is_empty() {
                        tracing::warn!(
                            settings = ?waiting,
                            "these settings changed and apply only after a restart"
                        );
                    }
                }
            });
            tokio::select! {
                result = node => return result,
                _ = control.restart_requested() => {
                    tracing::info!("restart requested over the control socket");
                }
            };
            restart_process("asked to").await
        }
        Command::Forget { machine } => forget_machine(&machine, &config_path).await,
        Command::Send {
            target,
            to,
            copy,
            save,
        } => send_share(&target, &to, copy, save).await,
        Command::Ring { phone } => ring_phone(&phone).await,
        Command::Screen { phone, stop } => phone_screen(&phone, !stop).await,
        Command::Peers { timeout_secs } => list_peers(identity, timeout_secs).await,
        Command::Call { action } => call(action).await,
        Command::Bluetooth { action } => bluetooth(action),
        Command::Sms { action } => sms(action).await,
        Command::Portal { action } => portal(action).await,
        Command::MountHelper { .. } => unreachable!("handled before the node starts"),
    }
}

#[cfg(target_os = "linux")]
async fn portal(action: PortalAction) -> anyhow::Result<()> {
    match action {
        PortalAction::Install => cursio_core::portal_setup::install(),
        PortalAction::Uninstall => cursio_core::portal_setup::uninstall(),
        PortalAction::Relay => Ok(cursio_window::linux_portal::serve(None).await?),
    }
}

#[cfg(target_os = "linux")]
fn bluetooth(action: BluetoothAction) -> anyhow::Result<()> {
    match action {
        BluetoothAction::Install => cursio_core::bluetooth_setup::install(),
        BluetoothAction::Uninstall => cursio_core::bluetooth_setup::uninstall(),
    }
}

#[cfg(not(target_os = "linux"))]
fn bluetooth(_action: BluetoothAction) -> anyhow::Result<()> {
    anyhow::bail!(
        "a phone's calls reach this computer's audio over bluetooth only on Linux so far; calls are still answered and placed from here over the mesh"
    )
}

async fn call(command: CallCommand) -> anyhow::Result<()> {
    use cursio_core::calls_hub::CallAction;
    use cursio_core::control::{Command, Reply};
    let client = control_client()
        .await
        .map_err(|e| anyhow::anyhow!("no running node ({e}); start `cursio mesh` first"))?;
    let (phone, action) = match command {
        CallCommand::List => {
            let Reply::Status(status) = client.request(Command::Status).await? else {
                anyhow::bail!("the node did not answer with its status");
            };
            if status.calls.is_empty() {
                println!("no phone that reports calls is connected");
            }
            for phone in &status.calls {
                let route = if phone.hands_free {
                    "bluetooth"
                } else {
                    "mesh"
                };
                println!("{} ({route})", phone.name);
                if let Some(why) = &phone.unavailable {
                    println!("  calls off: {why}");
                }
                for c in &phone.calls {
                    println!("  #{} {:?} {:?} {}", c.id, c.direction, c.state, c.who());
                }
            }
            return Ok(());
        }
        CallCommand::Answer { phone } => (phone, CallAction::Answer),
        CallCommand::Decline { phone } => (phone, CallAction::Decline),
        CallCommand::Hangup { phone } => (phone, CallAction::HangUp),
        CallCommand::Dial { phone, number } => (phone, CallAction::Dial { number }),
        CallCommand::Mute { phone, off } => (phone, CallAction::Mute { muted: !off }),
        CallCommand::Audio { phone, phone_side } => {
            (phone, CallAction::AudioHere { here: !phone_side })
        }
    };
    match client.request(Command::Call { phone, action }).await? {
        Reply::Done => Ok(()),
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected reply from the node: {other:?}"),
    }
}

#[cfg(not(target_os = "linux"))]
async fn portal(_action: PortalAction) -> anyhow::Result<()> {
    anyhow::bail!(
        "the ScreenCast portal exists only on Linux; on Windows `mesh --remote-screen` needs no setup"
    )
}

/// Serve the ScreenCast portal backend with the mesh's peers behind it, for
/// `--remote-screen` on Linux; `None` elsewhere, where views are windows.
#[cfg(target_os = "linux")]
fn start_screen_portal(remote_screen: bool) -> Option<cursio_core::screen_hub::ScreenHub> {
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
fn start_screen_portal(_remote_screen: bool) -> Option<cursio_core::screen_hub::ScreenHub> {
    None
}

/// Window-transfer options, grouped for the same reason [`AudioOpts`] is: the
/// registry builder should gain a struct field per subsystem, not a positional
/// argument.
struct WindowOpts {
    /// Set when this node shares whatever an iPhone mirrors to it. A window of
    /// this machine's own moves by being dragged across the peer-facing edge,
    /// which needs no flag — the drag is the request.
    airplay: Option<cursio_core::window_session::AirPlayShare>,
    /// `--remote-screen`: offer each peer's screen to capture apps here.
    remote_screen: bool,
    /// `!--no-screen-host`: let peers view this machine's screen. Shared, so
    /// the app can change it on a running node.
    screen_host: Shared<bool>,
    /// Where the portal backend's requests reach the links (Linux).
    screen_hub: Option<cursio_core::screen_hub::ScreenHub>,
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
    let password = prompt_password("Pairing password: ")?;
    if password.is_empty() {
        anyhow::bail!("pairing password cannot be empty");
    }
    Ok(password)
}

/// Read a line from the terminal with echo off and nothing else changed.
///
/// Not rpassword: since 7.5 it puts the terminal in raw mode, ISIG included, and
/// answers Ctrl-C by raising SIGINT at itself before its guard restores the
/// terminal. The process dies with the terminal still raw, `cargo xtask run`
/// exits normally on seeing that, and the shell adopts the raw settings as its
/// own — so Ctrl-C stops interrupting anything in that terminal until `stty
/// sane`. With only ECHO cleared, Ctrl-C is the terminal's own SIGINT to the
/// whole foreground group, every parent dies by signal too, and the shell puts
/// back the settings it saved.
#[cfg(unix)]
fn prompt_password(prompt: &str) -> anyhow::Result<Zeroizing<String>> {
    use std::io::{BufRead, Write};
    use std::os::fd::AsRawFd;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| anyhow::anyhow!("--pair needs a terminal to read the password from: {e}"))?;
    let fd = tty.as_raw_fd();

    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `fd` is an open terminal for the life of `tty`, and tcgetattr
    // fully initialises the struct when it returns 0.
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
        anyhow::bail!(
            "cannot read the terminal settings: {}",
            std::io::Error::last_os_error()
        );
    }
    let original = unsafe { original.assume_init() };
    let mut silent = original;
    silent.c_lflag &= !libc::ECHO;
    silent.c_lflag |= libc::ECHONL;

    struct Restore(i32, libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the fd outlives this guard (declared after `tty`).
            unsafe { libc::tcsetattr(self.0, libc::TCSAFLUSH, &self.1) };
        }
    }

    (&tty).write_all(prompt.as_bytes())?;
    (&tty).flush()?;
    // SAFETY: as above; `silent` is a copy of settings the kernel just gave us.
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &silent) } != 0 {
        anyhow::bail!(
            "cannot turn off terminal echo: {}",
            std::io::Error::last_os_error()
        );
    }
    let _restore = Restore(fd, original);

    let mut line = Zeroizing::new(String::new());
    std::io::BufReader::new(&tty).read_line(&mut line)?;
    let trimmed = line.trim_end_matches(['\n', '\r']).len();
    line.truncate(trimmed);
    Ok(line)
}

#[cfg(not(unix))]
fn prompt_password(prompt: &str) -> anyhow::Result<Zeroizing<String>> {
    Ok(Zeroizing::new(rpassword::prompt_password(prompt)?))
}

/// How this desktop shows a phone's notifications, where it can.
#[cfg(target_os = "linux")]
fn notify_display() -> Option<std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>> {
    cursio_core::notify_display::FdoDisplay::start()
        .map(|d| d as std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>)
}

#[cfg(target_os = "windows")]
fn notify_display() -> Option<std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>> {
    cursio_core::notify_toast::ToastDisplay::start()
        .map(|d| d as std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn notify_display() -> Option<std::sync::Arc<dyn cursio_core::notify_hub::NotifyDisplay>> {
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
fn module_registry(
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
    // Players are read and shown over MPRIS; Windows' media session API is
    // not wired yet, and the module is not advertised there rather than
    // advertised and silent.
    #[cfg(target_os = "linux")]
    cursio_core::media_mpris::start();
    #[cfg(not(target_os = "linux"))]
    tracing::warn!("sharing what plays is not implemented on this platform yet");
    let media = cfg!(target_os = "linux");

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
            plays: media,
            shows: media,
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
#[allow(clippy::too_many_arguments)]
async fn run_mesh(
    identity: NodeIdentity,
    port: u16,
    send_input: bool,
    live: cursio_core::live::LiveConfig,
    modules: cursio_core::module::ModuleRegistry,
    pairing: PairingWindow,
    offer: ServiceOffer,
    control: cursio_core::control::NodeState,
) -> anyhow::Result<()> {
    let NodeBase {
        listener,
        _announcement,
        capture,
        status,
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
            let mut bluetooth = BluetoothListener::start();
            loop {
                tokio::select! {
                    // A paired phone off the network opened the mesh's
                    // Bluetooth channel. Same handshake, same session: only
                    // the path differs.
                    Some((device, transport)) = bluetooth.accept() => {
                        tracing::info!(%device, "peer dialled in over bluetooth");
                        spawn_peer_session(
                            transport,
                            identity.clone(),
                            Role::Responder,
                            None,
                            registry.clone(),
                            pairing.clone(),
                            control.clone(),
                            capture.clone(),
                            status.clone(),
                            live.clone(),
                            modules.clone(),
                            offer,
                        );
                    }

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
                            control.clone(),
                            capture.clone(),
                            status.clone(),
                            live.clone(),
                            modules.clone(),
                            offer,
                        );
                    }

                    // Discovery saw someone. Refresh their address if trusted.
                    Some(peer) = seen_rx.recv() => {
                        control.saw(peer.peer_id, &peer.display_name, peer.addr);
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
                        // Before the dial below and the publish, so a machine
                        // the user just removed is neither called again nor
                        // shown for one more sweep.
                        for peer in live.current().forgotten {
                            if let Some(gone) = registry.borrow_mut().forget(&peer) {
                                tracing::info!(peer = %gone.display_name, "no longer dialling a removed machine");
                            }
                        }
                        publish_mesh(&control, &registry, &status);
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
                                    control.clone(),
                                    capture.clone(),
                                    status.clone(),
                                    live.clone(),
                                    modules.clone(),
                                    offer,
                                );
                            }
                        }
                        let targets = cursio_core::mesh::next_dial_targets(self_id, &registry);
                        for (target, dialled) in cursio_core::mesh::dial(targets).await {
                            let transport = match dialled {
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
                                control.clone(),
                                capture.clone(),
                                status.clone(),
                                live.clone(),
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
/// Hand the control socket what the mesh loop knows. Once per sweep: the
/// registry is `!Send` and this is the loop that owns it.
fn publish_mesh(
    control: &cursio_core::control::NodeState,
    registry: &cursio_core::mesh::SharedRegistry,
    status: &SharedNodeStatus,
) {
    use cursio_core::control::{KnownPeerView, Presence};
    use cursio_core::registry::PeerState;
    control.set_local_status(status.borrow().current());
    control.set_known(
        registry
            .borrow()
            .all()
            .map(|peer| KnownPeerView {
                id: peer.peer_id,
                name: peer.display_name.clone(),
                presence: match peer.state {
                    PeerState::Online => Presence::Online,
                    PeerState::Connecting => Presence::Connecting,
                    PeerState::Offline { .. } => Presence::Offline,
                },
            })
            .collect(),
    );
}

/// Set in the environment of a node started by [`restart_process`], so it
/// waits out the port its predecessor is still releasing.
const RESTARTED_ENV: &str = "CURSIO_RESTARTED";

/// Start this node again from the top, with the same command line.
///
/// By the time this runs the node future has been dropped, which drops every
/// session and with them the modules' mounts, streams and capture handles.
/// The pause lets the module threads notice their channels closing and
/// unmount before the image is replaced; a mount that still outlives it is
/// cleared by the next start, as after a crash.
///
/// Unix replaces the process image: same PID, so a supervisor (`cargo xtask
/// run`, systemd) sees one continuous process. Every descriptor is made
/// close-on-exec first — the listener's, so the port is free at once, and the
/// ones libraries opened without it, which would otherwise pile up across
/// restarts (see `update::apply::close_inherited_on_exec`).
///
/// Windows has no exec: a child is started with the same arguments and this
/// process exits; the child retries its bind while the port is released.
async fn restart_process(reason: &str) -> anyhow::Result<()> {
    tokio::time::sleep(Duration::from_secs(1)).await;
    let exe = match cursio_core::update::layout::restart_target("cursio") {
        Some(current) => current,
        None => std::env::current_exe()?,
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    tracing::info!(reason, exe = %exe.display(), "restarting");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cursio_core::update::apply::close_inherited_on_exec();
        let error = std::process::Command::new(&exe)
            .args(&args)
            .env(RESTARTED_ENV, "1")
            .exec();
        Err(anyhow::anyhow!(
            "could not restart {}: {error}",
            exe.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let mut command = std::process::Command::new(&exe);
        command.args(&args).env(RESTARTED_ENV, "1");
        #[cfg(windows)]
        if !cursio_core::elevate::has_console() {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
            .spawn()
            .map_err(|e| anyhow::anyhow!("could not restart {}: {e}", exe.display()))?;
        std::process::exit(0)
    }
}

/// Serve the control socket for this node.
///
/// A second node for the same user is not refused here: two processes on one
/// machine are an existing, deliberate setup (a test node on another port),
/// and the first one keeps answering for the machine. The second says so.
fn start_control(state: cursio_core::control::NodeState) {
    let endpoint = match cursio_core::control::transport::endpoint() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::warn!(%error, "no control socket; the app and `cursio status` cannot reach this node");
            return;
        }
    };
    tokio::spawn(async move {
        if let Err(error) = cursio_core::control::transport::serve(endpoint, state).await {
            tracing::warn!(%error, "control socket stopped; the app and `cursio status` cannot reach this node");
        }
    });
}

/// The mesh's Bluetooth channel where this build has one, and on Windows a
/// listener that never accepts, so the mesh loop reads the same either way.
struct BluetoothListener {
    #[cfg(unix)]
    inner: cursio_core::bluetooth_link::Listener,
}

impl BluetoothListener {
    fn start() -> Self {
        #[cfg(unix)]
        {
            BluetoothListener {
                inner: cursio_core::bluetooth_link::listen(),
            }
        }
        #[cfg(not(unix))]
        {
            tracing::warn!("the mesh's bluetooth channel is not implemented on this platform; phones reach it over the network only");
            BluetoothListener {}
        }
    }

    #[cfg(unix)]
    async fn accept(
        &mut self,
    ) -> Option<(
        String,
        cursio_transport::stream::StreamTransport<tokio::net::UnixStream>,
    )> {
        self.inner.accept().await
    }

    #[cfg(not(unix))]
    async fn accept(&mut self) -> Option<(String, TcpTransport)> {
        std::future::pending().await
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_peer_session<T: cursio_transport::Transport + 'static>(
    transport: T,
    identity: NodeIdentity,
    role: Role,
    expected: Option<cursio_protocol::PeerId>,
    registry: cursio_core::mesh::SharedRegistry,
    // Only consulted for an inbound connection: a peer we dialled is one we
    // already trust, so it never needs a password.
    pairing: PairingWindow,
    control: cursio_core::control::NodeState,
    // Shared with every other mesh session: capture is one machine-wide
    // resource (one portal permission, one grab), so sessions all attach to
    // it rather than each building their own. See [`PersistentCapture`].
    capture: SharedCapture,
    // Machine-wide too, and for the same reason: every link greets with the
    // same reading, and every link should announce the same change.
    status: SharedNodeStatus,
    // What the user has this node set to, followed by the link while it runs:
    // where the machine stands, which modules and whether input is on.
    live: cursio_core::live::LiveConfig,
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
        let live_for_cb = live.clone();
        let mut on_identified = move |id: cursio_protocol::PeerId, name: &str| {
            *seen.borrow_mut() = Some(id);
            let mut reg = reg_for_cb.borrow_mut();
            // An inbound peer we already trust is simply now online. One we
            // don't know is refused by the key lookup right after this, so it
            // never becomes a mesh member here.
            if reg.is_known(&id) {
                reg.set_state(id, cursio_core::registry::PeerState::Online);
                tracing::info!(peer = %name, "peer online");
                cursio_core::health::link_restored(id);
            } else if pairing_for_cb.is_open() {
                tracing::info!(peer = %name, %id, "unknown machine dialled in; pairing it");
            } else if live_for_cb.is_forgotten(&id) {
                // It still trusts us and calls every sweep; one line says why
                // it gets nowhere, the rest would only bury the log.
                if cursio_core::forget::note_refused_call(id) {
                    tracing::info!(peer = %name, "a machine removed from this mesh keeps calling; refusing it until it is paired again");
                }
            } else {
                tracing::warn!(peer = %name, %id, "untrusted peer dialled in; not admitted");
            }
        };

        // Whichever way the call went, the password only matters on a first
        // meeting: `link::establish` uses the trust store when it has an entry
        // and reaches for this when it does not. Both roles get it because
        // either can be the one that dialled a stranger — the mesh calls a
        // machine it has never met while `--pair` is open, and used to leave
        // this empty on the assumption that a dialled peer is always trusted.
        let result = run_pairing_and_session(
            transport,
            identity,
            &pairing,
            &control,
            role,
            false,
            capture.as_ref().as_ref(),
            &status,
            &live,
            &modules,
            offer,
            Some(&mut on_identified),
        )
        .await;

        let who = *identified.borrow();
        match &result {
            Ok(()) => tracing::info!("peer session ended"),
            Err(e) if who.is_some_and(|id| live.is_forgotten(&id)) => {
                tracing::debug!(error = %e, "refused a machine removed from this mesh");
            }
            Err(e) => {
                tracing::warn!(error = %e, "peer session ended with error");
                // Only a peer that was greeted: a stranger refused at the
                // door, or a socket that closed before `Hello`, is not a link
                // the user has to hear dropped.
                let known = who.and_then(|id| {
                    let reg = registry.borrow();
                    reg.get(&id).map(|peer| (id, peer.display_name.clone()))
                });
                if let Some((id, name)) = known {
                    cursio_core::health::link_dropped(id, &name, &e.to_string());
                }
            }
        }

        // Offline, never removed: the machine still exists, it's just not here
        // right now. This is the whole point of the registry.
        if let Some(id) = who {
            registry.borrow_mut().mark_offline(id);
            control.link_down(&id);
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
async fn run_pairing_and_session<T: cursio_transport::Transport + 'static>(
    transport: T,
    identity: NodeIdentity,
    // Consulted only for a stranger, and told how the meeting went: a code
    // is spent by one success, and every window shuts after a few failures.
    pairing: &PairingWindow,
    // Told what this link agreed once it is up, and forgets it when it ends.
    control: &cursio_core::control::NodeState,
    role: Role,
    test_input: bool,
    // Capture is created ONCE by the caller and shared by every peer session —
    // see [`PersistentCapture`] for why it must not be rebuilt per connection.
    capture: Option<&PersistentCapture>,
    // Machine-wide power/network state: greeted with, then polled by the
    // session loop so a change reaches every peer.
    status: &SharedNodeStatus,
    // Where this machine stands, which modules the user has on, and whether
    // input is shared — as of the greeting, and followed while the link runs.
    // The edge to *this* peer is derived from the two positions; both the
    // cursor handoff and the window-drag trigger read that answer back.
    live: &cursio_core::live::LiveConfig,
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
    let password = pairing.password();
    // Subscribed before reading, so a change made while the link comes up is
    // still seen by the session and sent on.
    let settings = live.subscribe();
    let greeted = settings.borrow().clone();
    let can_send_input = capture.is_some();
    let sends_input = can_send_input && greeted.input;
    let plan = cursio_core::link::Handshake {
        identity: &identity,
        password: &password,
        role,
        registry: modules,
        position: greeted.position,
        service: offer,
        status: status.borrow().current(),
        sends_input,
    };
    let path = cursio_core::control::LinkPath::of(transport.kind());
    let identified = cursio_core::link::identify(transport, &plan).await?;
    // Report who this turned out to be before the slower half runs: for an
    // inbound connection this is the first moment we know at all, and the mesh
    // wants such a peer marked online without waiting out pairing.
    if let Some(cb) = on_identified {
        cb(identified.peer.peer_id, &identified.peer.display_name);
    }
    let stranger = !password.is_empty()
        && TrustStore::load().is_ok_and(|store| store.get(&identified.peer.peer_id).is_none());
    let peer_name = identified.peer.display_name.clone();
    let peer_id = identified.peer.peer_id;
    let negotiated = identified.negotiated().clone();
    let link = match cursio_core::link::secure(identified, &plan).await {
        Ok(link) => link,
        Err(error) => {
            // Only a wrong secret counts against the window. A dropped socket
            // mid-handshake is the network, and closing the window for it
            // would punish the user for Wi-Fi.
            if stranger
                && matches!(
                    error.downcast_ref::<cursio_transport::pairing::PairingError>(),
                    Some(
                        cursio_transport::pairing::PairingError::ConfirmationFailed
                            | cursio_transport::pairing::PairingError::Rejected(_)
                    )
                )
            {
                pairing.note_failure();
            }
            return Err(error);
        }
    };
    if stranger {
        pairing.note_paired(&peer_name);
        // Paired again after being removed: its links may stay up now.
        live.unforget(peer_id);
    }
    let peer = link.peer;

    // Now that the peer has said where it stands, capture can watch the edge
    // between us, alongside every other peer's. This is what replaced
    // `--peer-edge`: both machines already know, and the flag only existed
    // because the barrier had to be armed before anyone had spoken. From here
    // this session hears only the input and handoffs that concern its peer.
    let alone = capture.is_none_or(|c| !c.hub.machine().linked);
    let hub_link = capture.map(|capture| {
        let placement = Placement {
            edge: link.peer_edge,
            distance: greeted.position.distance_to(peer.position),
            takes_cursor: peer.device_kind.takes_cursor(),
        };
        let (ports, machine) = capture.hub.attach(peer.peer_id, placement);
        capture.apply(machine);
        HubLink {
            capture,
            token: ports.token,
            events: ports.events,
            commands: ports.commands,
        }
    });
    let token = hub_link.as_ref().map(|l| l.token);
    let hub = capture.map(|c| c.hub.clone());
    let (skipped, inapplicable) = cursio_core::control::link_skips(&negotiated, peer.device_kind);
    control.link_up(
        peer.peer_id,
        &peer.display_name,
        cursio_core::control::LinkView {
            position: peer.position,
            device_kind: peer.device_kind,
            status: peer.status,
            modules: negotiated.agreed.iter().map(|id| id.to_string()).collect(),
            skipped,
            inapplicable,
            since: std::time::SystemTime::now(),
            path,
        },
    );
    let mut secure = link.transport;
    let built = link.modules;
    let live_link = cursio_core::reconfigure::LinkLive {
        registry: modules,
        local: &identity,
        peer: peer.clone(),
        advertised: cursio_core::link::advertise(modules, sends_input),
        position: greeted.position,
        settings,
        can_send_input,
        on_change: {
            let control = control.clone();
            let (id, name) = (peer.peer_id, peer.display_name.clone());
            let takes_cursor = peer.device_kind.takes_cursor();
            let peer_kind = peer.device_kind;
            Box::new(move |state| {
                if state.edge_changed {
                    match state.peer_edge {
                        Some(edge) => {
                            tracing::info!(peer = %name, ?edge, "this peer now borders another edge")
                        }
                        None => {
                            tracing::info!(peer = %name, "this peer no longer borders any edge")
                        }
                    }
                }
                // Distance can change with the edge staying put, and it decides
                // which of two peers beyond one edge the cursor reaches.
                if let (Some(hub), Some(token)) = (hub.as_ref(), token) {
                    let placement = Placement {
                        edge: state.peer_edge,
                        distance: state.position.distance_to(state.peer_position),
                        takes_cursor,
                    };
                    cursio_input::set_watched_edges(hub.place(token, placement).edges);
                }
                control.link_reconfigured(
                    &id,
                    state.peer_position,
                    state
                        .negotiated
                        .agreed
                        .iter()
                        .map(|id| id.to_string())
                        .collect(),
                    cursio_core::control::link_skips(&state.negotiated, peer_kind),
                );
            })
        },
    };

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

    // This machine's real mouse/keyboard, as the hub routes them to this link.
    // The hotkey filter and edge-crossing forwarder are already running inside
    // the persistent capture.
    let mut hub_link = hub_link;
    let (captured, commands) = match hub_link.as_mut() {
        Some(l) => (Some(&mut l.events), Some(&mut l.commands)),
        None => (None, None),
    };

    // Who starts with the shared cursor was settled while the link came up —
    // see `CursorSetup`, which weighs what sort of machine the peer is before
    // falling back to comparing ids.
    //
    // That comparison knows only this link. With another peer already linked,
    // losing it would start this machine driving the newcomer: the cursor would
    // leave the screen the user is working on because a machine switched on.
    // So only a first link may start that way; a later one starts with the
    // cursor held here, which the peer — which either agrees or also holds its
    // own — cannot turn into both sides streaming to each other.
    let mut cursor_setup = link.cursor;
    if !alone && cursor_setup.peer_takes_cursor {
        cursor_setup.start_active = true;
    }

    tracing::info!(
        start_active = cursor_setup.start_active,
        peer_kind = ?peer.device_kind,
        peer_takes_cursor = cursor_setup.peer_takes_cursor,
        "session up — sharing input; press Ctrl+Alt+G to toggle control (Ctrl-C to stop)"
    );
    let inject_ref = injector.clone();
    let release_ref = injector.clone();
    let relay = capture.map(|c| c.hub.clone());
    // Level with the reading the greeting carried, so this link announces only
    // what changes from here — and shares the monitor with every other session,
    // so a change reaches all of them.
    let status_ref = status.clone();
    let mut status_cursor = status_ref.borrow().cursor();
    let result = session::run_session_live(
        secure,
        cursor_setup,
        captured,
        move |event| {
            // A peer moving the cursor held here — a phone used as a touchpad —
            // is one more mouse on this machine, so it follows the cursor the
            // way the local one does: across an edge to the machine beyond.
            // Once this machine drives another, injecting here would move a
            // pointer that is parked against the edge and hidden.
            let relayed = relay
                .as_ref()
                .is_some_and(|hub| hub.route_event(session::CapturedInput::from(event)));
            if !relayed {
                inject(inject_ref.borrow_mut().as_mut(), event);
            }
        },
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
                if let (Some(c), Some(token)) = (capture, token) {
                    c.hub.note_driving(token, false);
                    // `Idle` deliberately still observes edges on macOS so the
                    // initially non-owning machine can start a handoff. Panic is
                    // different: it promises that barriers are DOWN. Every edge
                    // stays down until a link changes, otherwise a mouse still
                    // moving toward the edge immediately hands control away
                    // again (the repeated PANIC -> crossing loop in the logs).
                    cursio_input::set_watched_edges(c.hub.disarm().edges);
                    c.grab.set_mode(cursio_input::InputMode::Idle);
                    if c.hidden.replace(false) {
                        set_cursor_hidden(c.cursor.borrow_mut().as_mut(), false);
                    }
                }
                set_injecting(release_ref.borrow_mut().as_mut(), true);
                tracing::warn!("panic recovery: input returned to this machine, barriers down");
                return;
            }
            // Two states, because there is one shared cursor and every machine
            // can move it. Holding it is Active; not holding it is Dormant —
            // local input is grabbed and streamed to whichever machine does.
            //
            // A third state sorted by `driving` was tried and is what made a
            // mouse stop working: a machine that had handed the cursor back
            // landed in Idle, where its pointer acts locally and moves nothing
            // shared. The user then had one mouse that drove both screens and
            // one that drove neither. Which side the user is sitting at is not
            // knowable here and does not need to be — an unheld cursor is
            // driven from wherever the hand is.
            //
            // Which of the two this *machine* is in is the hub's answer, not
            // this link's: another link may be driving its peer while this one
            // holds nothing, and each writing its own view over the grab left
            // it saying whatever the last report said.
            //
            // Cursor visibility before the injector, and both before the log
            // line. On macOS showing the cursor also warps it back from the
            // corner it was parked in, and the injector re-seeds its shadow
            // position from the real pointer the moment it goes active — in the
            // other order it seeds from the parking corner, and the peer's next
            // movement teleports the pointer there.
            if let (Some(c), Some(token)) = (capture, token) {
                c.apply(c.hub.note_driving(token, ownership.driving));
                // The cursor just left across the peer-facing edge, so this
                // machine's own pointer is pinned against it — a `pos_norm` of
                // exactly 0.0 or 0.9989 in the log is that. Left there, the next
                // small movement is outward and reads as another crossing, and
                // two handoffs with no return between them leave this machine
                // believing it drives a peer the user is not sitting at. Windows
                // already does this from its hook; macOS had no equivalent.
                if !ownership.active {
                    if let Some(cursor) = c.cursor.borrow_mut().as_mut() {
                        cursor.nudge_off_peer_edge();
                    }
                }
            }
            set_injecting(release_ref.borrow_mut().as_mut(), ownership.active);
            tracing::info!(active = ownership.active, "ownership changed");
        },
        built,
        move || status_ref.borrow_mut().poll(&mut status_cursor),
        Some(live_link),
        Some(peer_id),
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
    // What it rests in is decided by the peers still linked: with none, `Idle`
    // — no grab, no barriers, backend alive and ready for the next peer; with
    // others, whatever they need, minus this peer's edge.
    drop(hub_link);

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

/// Spawn a task that hands captured events to the hub, but watches for the
/// ownership hotkey (**Ctrl+Alt+G**) and emits an [`OwnershipCmd`] instead of
/// forwarding the trigger key. The hub decides which session each one reaches.
///
/// Combo (not a double-tap) so it can't fire by accident and doesn't depend on
/// tap timing. Modifier keys are still forwarded to the peer (they're needed in
/// normal use); only the `G` press that completes the combo is swallowed, so a
/// stray "g" doesn't reach the peer. Toggling alternates Take/Release; the
/// session ignores a redundant one, so we don't track remote state here.
fn spawn_hotkey_filter(
    mut upstream: tokio::sync::mpsc::UnboundedReceiver<cursio_protocol::InputEvent>,
    hub: InputHub,
    grab: cursio_input::GrabControl,
) {
    use cursio_protocol::InputEvent;

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
        // Every change of mode forgets what is held. A hotkey that takes the
        // cursor back ends capture with Ctrl and Alt still down, and their
        // releases go to the local desktop — capture never sees them. Kept,
        // `ctrl` stayed true and the swallow never ended: from then on every
        // Alt press and release was eaten, so Alt+Tab on the peer arrived as
        // Tab, until the user happened to press Ctrl again.
        let mut last_mode = grab.mode();
        while let Some(event) = upstream.recv().await {
            let mode = grab.mode();
            if mode != last_mode {
                last_mode = mode;
                ctrl = false;
                alt = false;
                swallow_until_release = false;
            }
            if !mode.forwards_events() {
                continue;
            }
            if let InputEvent::Key { key, pressed } = event {
                match key {
                    KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                        ctrl = pressed;
                        // Tail of a fired combo: drop the release, and end the
                        // swallow once both modifiers are up. A press is never
                        // swallowed — it starts a new combo, and ends the old
                        // one's tail whatever was missed of it.
                        if swallow_until_release {
                            if pressed {
                                swallow_until_release = false;
                            } else {
                                if !alt {
                                    swallow_until_release = false;
                                }
                                continue;
                            }
                        }
                    }
                    KEY_LEFTALT | KEY_RIGHTALT => {
                        alt = pressed;
                        if swallow_until_release {
                            if pressed {
                                swallow_until_release = false;
                            } else {
                                if !ctrl {
                                    swallow_until_release = false;
                                }
                                continue;
                            }
                        }
                    }
                    KEY_G if pressed && ctrl && alt => {
                        // Combo complete: request a handoff and swallow this G.
                        // The session decides direction (only the active side
                        // hands off), so we just send a Toggle.
                        tracing::info!("ownership hotkey (Ctrl+Alt+G)");
                        hub.route_command(session::OwnershipCmd::Toggle);
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
                        // is wedged. Also ask every session to flip ownership +
                        // restore the cursor, which works if they're still alive.
                        tracing::warn!("PANIC hotkey (Ctrl+Alt+Esc): releasing locally");
                        grab.set_mode(cursio_input::InputMode::Idle);
                        cursio_input::set_watched_edges(hub.panic().edges);
                        swallow_until_release = true;
                        continue; // don't forward Esc to the peer
                    }
                    _ => {}
                }
            }
            hub.route_event(session::CapturedInput::from(event));
        }
    });
}

/// Spawn a task that turns capture-reported edge crossings into ownership
/// handoffs, each to the session of the peer beyond the crossed edge. A
/// crossing with no peer beyond it is dropped by the hub, so the cursor just
/// stops there as usual — though capture arms only edges that border a peer,
/// so that is a peer that left between the crossing and now.
///
/// This is where the screen topology lives; the session stays topology-agnostic
/// (it only sees "hand off to the peer").
fn spawn_edge_handoff(
    mut edges: tokio::sync::mpsc::UnboundedReceiver<cursio_input::EdgeCrossing>,
    hub: InputHub,
) {
    tokio::spawn(async move {
        while let Some(crossing) = edges.recv().await {
            tracing::info!(edge = ?crossing.edge, pos = crossing.pos_norm, "edge crossing toward peer");
            hub.route_command(session::OwnershipCmd::HandoffToPeer {
                edge: crossing.edge,
                pos_norm: crossing.pos_norm,
            });
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
/// live here, outliving individual sessions. They feed the [`InputHub`], and
/// every session attaches to it for as long as it runs — all of them at once,
/// each reached only by what concerns its own peer.
struct PersistentCapture {
    /// Which session each captured event and ownership request reaches.
    hub: InputHub,
    /// Freeze/release local input.
    grab: cursio_input::GrabControl,
    /// Hides this machine's pointer while it drives a peer. One per machine,
    /// like the pointer: two sessions each holding one wrote over each other,
    /// and macOS counts hides, so two hides needed two shows.
    cursor: std::cell::RefCell<Option<Box<dyn cursio_input::CursorControl>>>,
    /// Whether the pointer is hidden now, so a session reporting a change that
    /// changes nothing machine-wide does not hide or show it again.
    hidden: std::cell::Cell<bool>,
}

/// One session's attachment to the hub, and its two ports.
///
/// Dropping it detaches — on every way out of a session, an error included —
/// and puts grab, barriers and pointer back in line with the links that
/// remain. A session that left without detaching would keep its edge armed and
/// could keep receiving this machine's input.
struct HubLink<'c> {
    capture: &'c PersistentCapture,
    token: cursio_core::input_hub::LinkToken,
    events: tokio::sync::mpsc::UnboundedReceiver<session::CapturedInput>,
    commands: tokio::sync::mpsc::UnboundedReceiver<session::OwnershipCmd>,
}

impl Drop for HubLink<'_> {
    fn drop(&mut self) {
        self.capture.apply(self.capture.hub.detach(self.token));
    }
}

impl PersistentCapture {
    /// Bring grab, barriers and pointer in line with what the hub says this
    /// machine's input is doing.
    ///
    /// Each is written only when it changes. Every session reports its own
    /// ownership, and most reports change nothing machine-wide; setting the
    /// grab mode again is not free — on Wayland re-entering Active releases
    /// the portal grab and nudges the pointer.
    fn apply(&self, machine: MachineInput) {
        cursio_input::set_watched_edges(machine.edges);
        let mode = if machine.driving {
            cursio_input::InputMode::Dormant
        } else if machine.linked {
            cursio_input::InputMode::Active
        } else {
            cursio_input::InputMode::Idle
        };
        if self.grab.mode() != mode {
            self.grab.set_mode(mode);
        }
        if self.hidden.get() != machine.driving {
            self.hidden.set(machine.driving);
            set_cursor_hidden(self.cursor.borrow_mut().as_mut(), machine.driving);
        }
    }
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
    status: SharedNodeStatus,
}

/// This machine's power/network state, shared between concurrent sessions.
///
/// One monitor per machine, not per link: the readings are machine-wide, and a
/// node bringing up four links would otherwise probe sysfs four times to learn
/// the same number — and could report four different answers if a poll landed
/// mid-change. `Rc<RefCell<_>>` for the same reason as [`SharedCapture`]:
/// sessions are `!Send` and never leave this thread.
type SharedNodeStatus = std::rc::Rc<std::cell::RefCell<NodeStatusMonitor>>;

/// Bind the mesh port, waiting a few seconds for it after a restart.
///
/// Only after a restart: then the previous process is known to be letting go
/// of this port. Otherwise a busy port is another node, and saying so at once
/// beats making the user wait to hear it.
async fn bind_mesh_port(port: u16) -> anyhow::Result<TcpListenerHandle> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let mut attempts = match std::env::var_os(RESTARTED_ENV) {
        Some(_) => 20,
        None => 1,
    };
    loop {
        attempts -= 1;
        match TcpListenerHandle::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if attempts > 0 => {
                tracing::debug!(error = %e, "mesh port still held by the previous run; retrying");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Bind, announce on the LAN, and bring up input capture.
async fn start_node(
    identity: &NodeIdentity,
    port: u16,
    send_input: bool,
) -> anyhow::Result<NodeBase> {
    let listener = bind_mesh_port(port).await?;
    let bound = listener.local_addr();
    tracing::info!(%bound, "listening for peers");

    // Being reachable is not the same as being bound. On Windows the firewall
    // decides, and its own prompt opens the wrong protocol here — see
    // `cursio_core::firewall`. Done with the port that was actually bound, so a
    // node started on a different one opens that.
    cursio_core::firewall::allow_mesh_port(bound.port());

    // A machine short of memory looks, from inside Cursio, like a network
    // fault in every module at once; this says which it is.
    cursio_core::pressure::watch();

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
    // It is started without edges on purpose. Which edge borders a peer is a
    // per-link answer derived from the two `--at` positions, and no peer has
    // said hello yet; the hub arms them as links come up, through
    // [`cursio_input::set_watched_edges`].
    let capture: SharedCapture = std::rc::Rc::new(make_persistent_capture(send_input));

    // Read once here rather than per link, for the same reason capture is
    // built once: the answer is machine-wide. The first reading rides every
    // greeting; changes are announced from the session loop.
    let status: SharedNodeStatus =
        std::rc::Rc::new(std::cell::RefCell::new(NodeStatusMonitor::new()));
    tracing::info!(status = %status.borrow().current(), "local node status");

    Ok(NodeBase {
        listener,
        _announcement: announcement,
        capture,
        status,
    })
}

/// Capture shared between concurrent mesh sessions.
///
/// `Rc` rather than a lock because mesh runs single-threaded on a `LocalSet`.
/// `None` inside means capture is unavailable/disabled. Every session uses it
/// at once: which of them a keystroke or a crossing reaches is the hub's
/// decision, not a question of who borrowed capture first. Lending it whole to
/// the first session left every other peer without local input.
type SharedCapture = std::rc::Rc<Option<PersistentCapture>>;

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

    // Both forwarders are permanent too: they own the raw receivers and hand
    // what they read to the hub, which passes each to the session it concerns.
    let hub = InputHub::new();
    spawn_hotkey_filter(handle.events, hub.clone(), grab.clone());
    spawn_edge_handoff(handle.edges, hub.clone());

    Some(PersistentCapture {
        hub,
        grab,
        cursor: std::cell::RefCell::new(make_cursor()),
        hidden: std::cell::Cell::new(false),
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
            cursio_core::health::input_capture_started();
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start input capture");
            cursio_core::health::input_capture_unavailable(&e.to_string());
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

async fn control_client() -> anyhow::Result<cursio_core::control::transport::Client> {
    let endpoint = cursio_core::control::transport::endpoint()?;
    let (client, _pushes) = cursio_core::control::transport::connect(&endpoint).await?;
    Ok(client)
}

async fn print_status(json: bool) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Presence, Reply};
    let reply = control_client().await?.request(Command::Status).await?;
    let status = match reply {
        Reply::Status(status) => status,
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected reply {other:?}"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    let at = |p: cursio_protocol::Position| format!("{},{}", p.x, p.y);
    let online = status
        .peers
        .iter()
        .filter(|p| p.presence == Presence::Online)
        .count();
    println!(
        "{} at {} — {online} of {} peers online",
        status.node.name,
        at(status.node.position),
        status.peers.len()
    );
    for peer in &status.peers {
        let presence = match (peer.presence, peer.link.as_ref().map(|l| l.path)) {
            (Presence::Online, Some(cursio_core::control::LinkPath::Bluetooth)) => "bluetooth",
            (Presence::Online, _) => "online",
            (Presence::Connecting, _) => "connecting",
            (Presence::Offline, _) => "offline",
        };
        match &peer.link {
            Some(link) => {
                // A phone's cell borders nothing; its kind is what to know.
                let place = if link.device_kind.takes_cursor() {
                    format!("at {}", at(link.position))
                } else {
                    format!("{:?}", link.device_kind).to_lowercase()
                };
                println!(
                    "  {:<16} {presence:<10} {place:<9} {}  modules: {}",
                    peer.name,
                    link.status,
                    link.modules.join(", ")
                )
            }
            None => println!("  {:<16} {presence}", peer.name),
        }
    }
    for r in &status.latency {
        println!(
            "input latency {}: p50 {:.1} ms, p99 {:.1} ms over {} events",
            r.path,
            r.p50_us as f64 / 1000.0,
            r.p99_us as f64 / 1000.0,
            r.events
        );
    }
    if status.problems.is_empty() {
        println!("no problems");
    }
    for problem in &status.problems {
        println!("{:?}: {}", problem.severity, problem.summary);
        if let Some(hint) = &problem.hint {
            println!("  {hint}");
        }
    }
    if !status.restart_needed.is_empty() {
        println!(
            "waiting for a restart: {}",
            status.restart_needed.join(", ")
        );
    }
    if !status.pinned.is_empty() {
        println!("pinned by flags: {}", status.pinned.join(", "));
    }
    Ok(())
}

/// Hand a share to the running node, which holds the links.
/// `cursio sms`: one request to the phone through the running node.
async fn sms(action: SmsAction) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!(
            "no running node to reach the phone through ({e}); start `cursio mesh` first"
        )
    })?;
    let command = match &action {
        SmsAction::List { phone, limit } => Command::ListConversations {
            peer: phone.clone(),
            limit: *limit,
            before_ms: None,
        },
        SmsAction::Read {
            phone,
            thread,
            limit,
        } => Command::ListMessages {
            peer: phone.clone(),
            thread_id: *thread,
            limit: *limit,
            before_ms: None,
        },
        SmsAction::Send {
            phone,
            number,
            text,
        } => Command::SendSms {
            peer: phone.clone(),
            addresses: vec![number.clone()],
            body: text.clone(),
        },
    };
    match client.request(command).await? {
        Reply::Conversations { conversations } => {
            for c in conversations {
                let who = if c.names.is_empty() {
                    c.addresses.join(", ")
                } else {
                    c.names.join(", ")
                };
                println!(
                    "{:>6}  {}{}  {}",
                    c.thread_id,
                    if c.unread { "* " } else { "" },
                    who,
                    c.snippet.replace('\n', " ")
                );
            }
            Ok(())
        }
        Reply::Messages { messages } => {
            for m in messages.iter().rev() {
                let arrow = match m.kind {
                    cursio_protocol::messages::MessageKind::Inbox => "<",
                    cursio_protocol::messages::MessageKind::Failed => "!",
                    _ => ">",
                };
                let attachment = if m.attachment {
                    " [attachment not shown]"
                } else {
                    ""
                };
                println!("{arrow} {}: {}{attachment}", m.address, m.body);
            }
            Ok(())
        }
        Reply::Done => {
            if let SmsAction::Send { number, .. } = action {
                println!("handed to the phone for {number}");
            }
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know a phone's messages ({other:?}); restart it with this version of Cursio"
        ),
    }
}

async fn send_share(target: &str, to: &str, copy: bool, save: bool) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    use cursio_protocol::share::ShareAction;
    let action = match (copy, save) {
        (true, _) => ShareAction::Copy,
        (_, true) => ShareAction::Save,
        _ => ShareAction::Open,
    };
    // The node's working directory is not this shell's, so a relative path
    // would name some other file there, or none.
    let target = match std::fs::canonicalize(target) {
        Ok(path) if path.is_file() => path.to_string_lossy().into_owned(),
        _ => target.to_string(),
    };
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!("no running node to send through ({e}); start `cursio mesh` first")
    })?;
    match client
        .request(Command::Share {
            peer: to.to_string(),
            target,
            action,
        })
        .await?
    {
        Reply::Done => {
            println!("sent to {to}");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to share ({other:?}); restart it with this version of Cursio"
        ),
    }
}

/// Ask the running node, which holds the links, to ring a phone.
async fn ring_phone(phone: &str) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!("no running node to ring through ({e}); start `cursio mesh` first")
    })?;
    match client
        .request(Command::Ring {
            peer: phone.to_string(),
        })
        .await?
    {
        Reply::Done => {
            println!("ringing {phone}");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to ring ({other:?}); restart it with this version of Cursio"
        ),
    }
}

async fn phone_screen(phone: &str, show: bool) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!(
            "no running node to show the phone through ({e}); start `cursio mesh` first"
        )
    })?;
    match client
        .request(Command::PhoneScreen {
            peer: phone.to_string(),
            show,
        })
        .await?
    {
        Reply::Done if show => {
            println!("asked {phone}; its screen opens here once it is allowed on the phone");
            Ok(())
        }
        Reply::Done => {
            println!("{phone}'s screen is no longer shown here");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to show a phone's screen ({other:?}); restart it with this version of Cursio"
        ),
    }
}

/// Remove a machine: through the running node when there is one, so the link
/// closes at once, otherwise only in the files.
async fn forget_machine(machine: &str, config_path: &std::path::Path) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let (peer, _) = cursio_core::forget::resolve(machine)?;
    let (name, was_builder) = match control_client().await {
        Ok(client) => match client.request(Command::Forget { peer }).await? {
            Reply::Forgotten { name, was_builder } => (name, was_builder),
            Reply::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!(
                "the running node does not know how to remove a machine ({other:?}); \
                 restart it with this version of Cursio"
            ),
        },
        // No node to tell; the durable half is all there is.
        Err(_) => {
            let forgotten = cursio_core::forget::forget_on_disk(peer, config_path)?;
            (forgotten.name, forgotten.was_builder)
        }
    };
    println!("{name} is no longer part of this mesh");
    if was_builder {
        println!("its builds are no longer accepted here");
    }
    println!("to bring it back, pair the two machines again");
    Ok(())
}

async fn pair_with_code(code: Option<String>) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    use cursio_core::pairing_window::PairingOutcome;
    let client = control_client().await?;
    let command = match code {
        Some(code) => Command::PairEnter { code },
        None => Command::PairShow,
    };
    let mut shown_nearby = match client.request(command).await? {
        Reply::Pairing {
            state: Some(state),
            nearby,
            ..
        } => {
            match state.code {
                Some(code) => println!(
                    "Pairing code: {code}\nOn the other machine run `cursio pair {code}`, or enter it in the Cursio app."
                ),
                None => println!("Code accepted; waiting for the other machine…"),
            }
            print_nearby(&nearby);
            nearby_names(&nearby)
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected reply {other:?}"),
    };
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Reply::Pairing {
            state,
            last,
            nearby,
        } = client.request(Command::Pairing).await?
        else {
            anyhow::bail!("unexpected reply while waiting for pairing");
        };
        let names = nearby_names(&nearby);
        if state.is_some() && names != shown_nearby {
            print_nearby(&nearby);
            shown_nearby = names;
        }
        if state.is_some() {
            continue;
        }
        return match last {
            Some(PairingOutcome::Paired { peer }) => {
                println!("Paired with {peer}.");
                Ok(())
            }
            Some(PairingOutcome::Expired) => anyhow::bail!("the code expired before it was used"),
            Some(PairingOutcome::TooManyAttempts) => {
                anyhow::bail!("too many wrong codes; the window is closed — start again")
            }
            Some(PairingOutcome::Cancelled) | None => anyhow::bail!("pairing was cancelled"),
        };
    }
}

/// Who a pairing code could reach, compared by name and pairing state so a
/// repeated sighting does not print the list again.
fn nearby_names(nearby: &[cursio_core::control::NearbyMachine]) -> Vec<(String, bool)> {
    nearby.iter().map(|m| (m.name.clone(), m.paired)).collect()
}

/// Say who this node can see while a code waits, and say plainly when that is
/// nobody: a code shown to an empty network is never going to be used, and
/// without this the only sign is the window expiring.
fn print_nearby(nearby: &[cursio_core::control::NearbyMachine]) {
    if nearby.is_empty() {
        println!("{}", NOBODY_VISIBLE);
        return;
    }
    println!("Visible on this network:");
    for machine in nearby {
        let what = if machine.paired {
            "already paired"
        } else {
            "can pair"
        };
        println!("  {} ({}) — {what}", machine.name, machine.addr);
    }
}

const NOBODY_VISIBLE: &str = "No other Cursio machine is visible on this network, so nothing can \
use this code yet. Check that the other machine runs Cursio and is on the same network (not a \
guest network or a VPN, and not a hotspot that isolates its clients), and that the firewall lets \
Cursio through. Where the network blocks discovery, pair from a terminal with \
`cursio mesh --pair --pair-addr <address>:<port>`.";

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

    fn mesh_args(argv: &[&str]) -> MeshArgs {
        let Command::Mesh(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected mesh");
        };
        args
    }

    /// What `argv` runs with when there is no config file.
    fn effective(argv: &[&str]) -> cursio_core::config::Config {
        cursio_core::config::Config::default().with_overrides(&mesh_args(argv).overrides())
    }

    #[test]
    /// Unplaced machines must border nothing: a default that implied adjacency
    /// would hand the cursor away on a shove the user never meant.
    fn an_unplaced_node_sits_at_the_origin() {
        let at = effective(&["cursio", "mesh"]).position;
        assert_eq!(at, cursio_protocol::Position::new(0, 0));
        assert_eq!(at.edge_toward(at), None);
    }

    /// The daily command lines must keep meaning what they meant, and the
    /// bare command must now mean the same thing.
    #[test]
    fn the_everyday_flags_and_no_flags_run_the_same_set() {
        let typed = effective(&[
            "cursio",
            "mesh",
            "--send-input",
            "--clipboard",
            "--audio",
            "--files",
        ]);
        assert_eq!(typed, effective(&["cursio", "mesh"]));
    }

    #[test]
    fn an_off_flag_overrides_the_default_and_the_last_of_a_pair_wins() {
        assert!(!effective(&["cursio", "mesh", "--no-audio"]).modules.audio);
        assert!(
            effective(&["cursio", "mesh", "--no-audio", "--audio"])
                .modules
                .audio
        );
        assert!(
            !effective(&["cursio", "mesh", "--audio", "--no-audio"])
                .modules
                .audio
        );
        assert!(mesh_args(&["cursio", "mesh"])
            .overrides()
            .pinned()
            .is_empty());
    }

    #[test]
    /// The case named sides could not express: with three in a row, each knows
    /// which way its neighbour lies without anyone being a designated centre.
    fn a_row_of_machines_faces_the_right_way() {
        let at = effective(&["cursio", "mesh", "--at", "1,0"]).position;
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
            assert_eq!(effective(&argv).position, expected, "{argv:?}");
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
        assert!(mesh_args(&["cursio", "mesh", "--pair"]).pair);
    }

    /// An address is only meaningful while a window is open, so naming one
    /// without `--pair` is a mistake worth catching at the command line rather
    /// than by silently doing nothing.
    #[test]
    fn an_address_to_pair_with_needs_the_window_open() {
        assert!(
            Cli::try_parse_from(["cursio", "mesh", "--pair-addr", "192.168.1.5:46073"]).is_err()
        );
        let args = mesh_args(&[
            "cursio",
            "mesh",
            "--pair",
            "--pair-addr",
            "192.168.1.5:46073",
        ]);
        assert_eq!(
            args.pair_addr,
            Some("192.168.1.5:46073".parse::<SocketAddr>().unwrap())
        );
    }

    /// Receiving is on by default: a user who wants an iPhone to find this
    /// machine should not have to know a flag exists.
    #[test]
    fn airplay_receives_without_being_asked_to() {
        let airplay = effective(&["cursio", "mesh"]).airplay.mode;
        assert_eq!(
            AirPlayMode::parse(&airplay).unwrap(),
            AirPlayMode::On { wanted: None }
        );
    }

    #[test]
    fn remote_screen_views_are_opt_in_and_hosting_them_is_opt_out() {
        let modules = effective(&["cursio", "mesh"]).modules;
        assert!(!modules.remote_screen, "no view window may appear unasked");
        assert!(modules.screen_host, "a peer's view is served by default");

        let modules = effective(&["cursio", "mesh", "--remote-screen", "--no-screen-host"]).modules;
        assert!(modules.remote_screen && !modules.screen_host);
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
