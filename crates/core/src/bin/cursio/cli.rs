//! What the command line says: the subcommands, `mesh`'s flags, and how a
//! flag lays over the config file.

use clap::{Parser, Subcommand};
use std::net::{IpAddr, SocketAddr};

#[derive(Parser)]
#[command(name = "cursio", about = "Cursio LAN session driver")]
pub(crate) struct Cli {
    /// Increase console logging detail (-v for debug, -vv for trace).
    ///
    /// May be placed before or after the subcommand. Explicit RUST_LOG and
    /// CURSIO_LOG_FILE values take precedence.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub(crate) verbose: u8,

    /// Read the node's config from this file instead of the platform default
    /// (`~/.config/cursio/config.toml`, `%APPDATA%\cursio\cursio\config\config.toml`).
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
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
    /// Wake a paired machine that is asleep, with a Wake-on-LAN packet.
    ///
    /// Needs no running node: the machine's hardware address was learned the
    /// last time it connected over the network, and its own firmware must
    /// have wake-on-LAN on.
    Wake {
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
pub(crate) struct MeshArgs {
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
    pub(crate) files_share: Vec<String>,
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
    pub(crate) pair: bool,
    /// Where to find the machine to pair with, when mDNS will not say.
    ///
    /// Only consulted while `--pair` is open. Naming an address also means
    /// this side places the call rather than waiting to be called.
    #[arg(long, value_name = "HOST:PORT", requires = "pair")]
    pub(crate) pair_addr: Option<SocketAddr>,
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
pub(crate) fn switch(on: bool, off: bool) -> Option<bool> {
    match (on, off) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

impl MeshArgs {
    pub(crate) fn overrides(&self) -> cursio_core::config::Overrides {
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
pub(crate) enum CallCommand {
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
pub(crate) enum BluetoothAction {
    /// Take the hands-free and media sink roles from PipeWire (restarts
    /// WirePlumber; Bluetooth audio drops for a second).
    Install,
    /// Give those roles back to PipeWire.
    Uninstall,
}

#[derive(Subcommand)]
pub(crate) enum PortalAction {
    /// Route ScreenCast through Cursio (per user; restart the portal after).
    Install,
    /// Give ScreenCast back to GNOME alone.
    Uninstall,
    /// Forward every screen-cast request to GNOME; what D-Bus starts when no
    /// `cursio mesh` holds the backend's name.
    Relay,
}

#[derive(Subcommand)]
pub(crate) enum SmsAction {
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
pub(crate) fn share_policy(
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
pub(crate) fn parse_position(value: &str) -> Result<cursio_protocol::Position, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AirPlayMode;

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
}
