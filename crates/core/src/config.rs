//! What this node runs, written down once instead of retyped as flags.
//!
//! The daily command line had grown to eight flags per machine, and half of
//! the problems reported were a flag forgotten on one side — a module that
//! silently did not negotiate because it was on here and off there. A file
//! makes the set something the machine *has* rather than something the user
//! has to remember, and gives the app one place to change it.
//!
//! ## Precedence: defaults, then the file, then flags
//!
//! [`Config::default`] is the everyday set — input, clipboard, audio and
//! files on, AirPlay taking part, the machine at `0,0`. The file overrides
//! that, field by field (a file naming only `position` keeps every default).
//! Flags override the file, through [`Overrides`], whose every field is an
//! `Option`: a flag the user did not type must not reset the file's answer,
//! which is why the booleans come in `--audio`/`--no-audio` pairs rather than
//! as one switch that is false when absent.
//!
//! A flag stays pinned for the life of the process, so the status API reports
//! which fields are pinned: a toggle in the app that changes nothing is
//! otherwise a bug report.
//!
//! ## Applied live, not by restarting
//!
//! A changed file is applied to the running node ([`crate::live`]): a module
//! switched off stops on every link and nothing else does. This used to
//! restart the process, on the grounds that a fresh start is the one reload
//! guaranteed correct — but a restart drops every link, unmounts the peer's
//! disks and, on Windows, asks for elevation again, all for one toggle. The
//! few settings that really are read once (the port, AirPlay, the
//! remote-screen portal) are named in the status as waiting for a restart
//! the user asks for.
//!
//! ## The files share policy
//!
//! `[files.share]` is carried as an opaque table. Its shape is whatever the
//! files module's `SharePolicy` deserializes from; this crate only stores and
//! returns it, so the policy can evolve without a config migration here.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cursio_protocol::{Edge, Position};
use serde::{Deserialize, Serialize};

/// Why a config could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot resolve the config directory; set HOME (or APPDATA on Windows)")]
    NoConfigDir,
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a valid cursio config: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot serialize the config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("{0}")]
    Invalid(String),
}

/// The default listening port. Stable rather than ephemeral: peers cache the
/// last address that worked, and a port that moved every restart made that
/// cache point at nothing.
pub const DEFAULT_PORT: u16 = 46073;

/// Everything `cursio mesh` decides at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// TCP port the node listens on.
    pub port: u16,
    /// Where this machine stands, in grid cells: `{ x = 1, y = 0 }` is one
    /// machine to the right of the origin.
    pub position: Position,
    pub modules: Modules,
    pub audio: AudioConfig,
    pub airplay: AirPlayConfig,
    pub input: InputConfig,
    pub files: FilesConfig,
    pub update: UpdateConfig,
    pub calls: CallsConfig,
    pub bluetooth: BluetoothConfig,
    pub layout: LayoutConfig,
}

/// This machine's part of the shared desk (`docs/input-design.md`, "Screens
/// on one desk"). Its screens are read from the desktop as it runs, never
/// stored: a monitor unplugged is gone from the desk at once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutConfig {
    /// Where its screens' top-left corner stands, in millimetres from the
    /// desk's origin. Absent until someone arranges this machine; the grid
    /// (`position`) decides its edges until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<cursio_protocol::Place>,
    /// Seams the person closed between one of this machine's screens and a
    /// peer's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub barriers: Vec<cursio_protocol::Barrier>,
}

/// The mesh's Bluetooth path: a phone's to its computers, and a computer's to
/// the computers it is paired with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BluetoothConfig {
    /// Whether this machine carries the mesh over Bluetooth at all. Read on
    /// every turn of the dialler, so a change needs no restart.
    pub mesh: BluetoothMesh,
    /// Keep a Bluetooth path open beside the network one, so a session moves
    /// to it within seconds when Wi-Fi drops instead of waiting for the
    /// network to come back. Off by default: an open Bluetooth link costs
    /// battery, and a busy one costs Wi-Fi on the shared 2.4 GHz antenna.
    /// Without it Bluetooth still carries the session once the network has
    /// been gone a while.
    pub standby: bool,
}

/// Whether the mesh may use this machine's Bluetooth radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BluetoothMesh {
    /// On, unless the Wi-Fi card is one known to lose the network while
    /// Bluetooth is busy on the antenna they share; then off, with a note
    /// saying so and offering to turn it on regardless.
    #[default]
    Auto,
    On,
    Off,
}

/// One change to the config a problem offers to make, so a person can act on
/// the problem where they read it rather than in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEdit {
    BluetoothMesh(BluetoothMesh),
    /// Narrow one edge to the part that crosses; `None` gives it back whole.
    Crossing {
        edge: Edge,
        span: Option<EdgeSpan>,
    },
}

impl ConfigEdit {
    pub fn apply(self, config: &mut Config) {
        match self {
            ConfigEdit::BluetoothMesh(mode) => config.bluetooth.mesh = mode,
            ConfigEdit::Crossing { edge, span } => {
                let span = span.filter(|s| *s != EdgeSpan::WHOLE);
                let crossing = &mut config.input.crossing;
                match edge {
                    Edge::Left => crossing.left = span,
                    Edge::Right => crossing.right = span,
                    Edge::Top => crossing.top = span,
                    Edge::Bottom => crossing.bottom = span,
                }
            }
        }
    }
}

/// Where a phone's call voice goes once this computer carries it. Read at
/// every call, so a change needs no restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CallsConfig {
    /// Answering or dialling from this computer brings the voice here, as on
    /// Phone Link. Off, the voice stays wherever the phone puts it.
    pub answer_moves_voice: bool,
    /// Where the voice plays while this computer carries it.
    pub voice: CallVoice,
    /// The output the voice plays on here, by name; the default output when
    /// unset.
    pub output: Option<String>,
    /// The microphone the call hears here, by name; the default when unset.
    pub input: Option<String>,
}

impl Default for CallsConfig {
    fn default() -> Self {
        Self {
            answer_moves_voice: true,
            voice: CallVoice::Follow,
            output: None,
            input: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallVoice {
    /// Always on this computer's own output and microphone.
    Here,
    /// On the machine the user is at: this one, or the peer this machine's
    /// mouse is driving.
    #[default]
    Follow,
}

/// How this machine takes new builds that reach it over the mesh. Read again
/// at every decision, so a change here needs no restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    pub policy: UpdatePolicy,
    pub channel: UpdateChannel,
    /// Machines whose builds this one accepts on the `dev` channel, each
    /// pinned to the signing key it had when it was trusted.
    pub trusted_builders: Vec<TrustedBuilder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdatePolicy {
    /// Install, then switch at the first quiet moment.
    Auto,
    /// Install, then wait for the user to say when.
    Ask,
    /// Neither fetch nor install; still hand builds on to peers.
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    /// Only builds signed with a release key.
    Stable,
    /// Release builds, plus builds from a trusted builder.
    Dev,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedBuilder {
    pub peer: cursio_protocol::PeerId,
    /// What the machine was called when it was trusted; for the reader only.
    pub name: String,
    /// Its Ed25519 public key, hex.
    pub key: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            policy: UpdatePolicy::Ask,
            channel: UpdateChannel::Stable,
            trusted_builders: Vec::new(),
        }
    }
}

/// Which modules this node offers. A module is still negotiated per link; off
/// here means it is never advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Modules {
    /// Capture this machine's keyboard and mouse and share them.
    pub input: bool,
    pub clipboard: bool,
    pub audio: bool,
    pub files: bool,
    pub usb: bool,
    /// Offer peers' screens to screen-sharing apps here.
    pub remote_screen: bool,
    /// Let peers view this machine's screen.
    pub screen_host: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Windows: render peers' audio into the output whose name contains this.
    pub render_device: Option<String>,
    /// What a peer recording from this machine's microphones hears: `auto`
    /// (follow the mic's noise floor, the default), `off`, or a level in dBFS
    /// such as `-55`. See `audio_gate::MicGate`.
    pub mic_gate: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AirPlayConfig {
    /// `on`, `off`, or a quality profile such as `1080p60`; the CLI parses it.
    pub mode: String,
    /// Show the mirrored picture on this machine.
    pub window: bool,
    /// Offer the mirrored screen to every peer.
    pub share: bool,
    /// Control port; `None` is the receiver's own default.
    pub port: Option<u16>,
    /// Pin AirPlay mDNS to the interface with this address.
    pub interface: Option<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    /// Where a peer's modifiers land on a Mac: `positional`, `swapped`, or
    /// `from=to` pairs. Ignored elsewhere.
    pub mac_modifiers: String,
    /// Which part of each edge sends the cursor on.
    pub crossing: CrossingConfig,
}

/// Which part of each of this machine's edges hands the cursor to the peer
/// beyond it. An edge left out crosses along its whole length.
///
/// The rest of the edge stays an ordinary wall. That is what an edge shared
/// with something of the desktop's own needs: GNOME reveals its dock when the
/// pointer presses the bottom edge, and on Wayland a barrier there takes the
/// pointer at first touch, so with a peer below the dock never showed. A span
/// of `[67, 100]` keeps the edge under the dock to the desktop and crosses only
/// in its right third.
///
/// No pressure threshold instead, deliberately: the portal grabs the pointer
/// the moment it meets the barrier, so the desktop never sees the press
/// however long crossing waits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrossingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<EdgeSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<EdgeSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top: Option<EdgeSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottom: Option<EdgeSpan>,
}

impl CrossingConfig {
    /// The span of `edge` that crosses, in percent along it.
    pub fn span(&self, edge: Edge) -> EdgeSpan {
        let span = match edge {
            Edge::Left => self.left,
            Edge::Right => self.right,
            Edge::Top => self.top,
            Edge::Bottom => self.bottom,
        };
        span.unwrap_or(EdgeSpan::WHOLE)
    }
}

/// `[from, to]` in percent along an edge: left to right along the top or
/// bottom one, top to bottom along a side. Whole percents are enough for a
/// hand to aim at, and keep the file readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "[u8; 2]", into = "[u8; 2]")]
pub struct EdgeSpan {
    pub from: u8,
    pub to: u8,
}

impl EdgeSpan {
    pub const WHOLE: Self = Self { from: 0, to: 100 };
}

impl TryFrom<[u8; 2]> for EdgeSpan {
    type Error = String;

    fn try_from([from, to]: [u8; 2]) -> Result<Self, Self::Error> {
        if from < to && to <= 100 {
            Ok(Self { from, to })
        } else {
            Err(format!(
                "a crossing span is [from, to] in percent along the edge, \
                 from below to and to at most 100; got [{from}, {to}]"
            ))
        }
    }
}

impl From<EdgeSpan> for [u8; 2] {
    fn from(span: EdgeSpan) -> Self {
        [span.from, span.to]
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesConfig {
    /// What this machine shares, in the files module's `SharePolicy` shape.
    /// `None` keeps that module's own default.
    pub share: Option<toml::Table>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            position: Position::new(0, 0),
            modules: Modules::default(),
            audio: AudioConfig::default(),
            airplay: AirPlayConfig::default(),
            input: InputConfig::default(),
            files: FilesConfig::default(),
            update: UpdateConfig::default(),
            calls: CallsConfig::default(),
            bluetooth: BluetoothConfig::default(),
            layout: LayoutConfig::default(),
        }
    }
}

impl Default for Modules {
    /// The set the machines are actually run with every day.
    fn default() -> Self {
        Self {
            input: true,
            clipboard: true,
            audio: true,
            files: true,
            usb: false,
            remote_screen: false,
            screen_host: true,
        }
    }
}

impl Default for AirPlayConfig {
    fn default() -> Self {
        Self {
            mode: "on".into(),
            window: false,
            share: false,
            port: None,
            interface: None,
        }
    }
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            mac_modifiers: "positional".into(),
            crossing: CrossingConfig::default(),
        }
    }
}

/// Where the config lives when no path is given.
pub fn default_path() -> Result<PathBuf, ConfigError> {
    if let Some(dir) = cursio_protocol::paths::config_dir() {
        return Ok(dir.join("config.toml"));
    }
    let dirs = directories::ProjectDirs::from("dev", "cursio", "cursio")
        .ok_or(ConfigError::NoConfigDir)?;
    Ok(dirs.config_dir().join("config.toml"))
}

impl Config {
    /// Read `path`, or the defaults when it does not exist.
    ///
    /// A missing file is the ordinary first run; an unreadable or malformed
    /// one is an error, because starting on defaults would quietly turn off
    /// whatever the user wrote there.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source: Box::new(source),
        })
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Write the config so a reader never sees half a file: the daemon polls
    /// it, and a torn write would parse as an error or, worse, as defaults.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let write_err = |source| ConfigError::Write {
            path: path.to_owned(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(write_err)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_toml()?).map_err(write_err)?;
        std::fs::rename(&tmp, path).map_err(write_err)
    }

    /// Lay flags over this config.
    pub fn with_overrides(mut self, o: &Overrides) -> Self {
        fn set<T: Clone>(field: &mut T, value: &Option<T>) {
            if let Some(v) = value {
                *field = v.clone();
            }
        }
        set(&mut self.port, &o.port);
        set(&mut self.position, &o.position);
        set(&mut self.modules.input, &o.input);
        set(&mut self.modules.clipboard, &o.clipboard);
        set(&mut self.modules.audio, &o.audio);
        set(&mut self.modules.files, &o.files);
        set(&mut self.modules.usb, &o.usb);
        set(&mut self.modules.remote_screen, &o.remote_screen);
        set(&mut self.modules.screen_host, &o.screen_host);
        if o.audio_render_device.is_some() {
            self.audio.render_device = o.audio_render_device.clone();
        }
        if o.audio_mic_gate.is_some() {
            self.audio.mic_gate = o.audio_mic_gate.clone();
        }
        set(&mut self.airplay.mode, &o.airplay_mode);
        set(&mut self.airplay.window, &o.airplay_window);
        set(&mut self.airplay.share, &o.airplay_share);
        if o.airplay_port.is_some() {
            self.airplay.port = o.airplay_port;
        }
        if o.airplay_interface.is_some() {
            self.airplay.interface = o.airplay_interface;
        }
        set(&mut self.input.mac_modifiers, &o.mac_modifiers);
        self
    }
}

/// Values given on the command line. `None` means "not given", so the file's
/// answer stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub port: Option<u16>,
    pub position: Option<Position>,
    pub input: Option<bool>,
    pub clipboard: Option<bool>,
    pub audio: Option<bool>,
    pub files: Option<bool>,
    pub usb: Option<bool>,
    pub remote_screen: Option<bool>,
    pub screen_host: Option<bool>,
    pub audio_render_device: Option<String>,
    pub audio_mic_gate: Option<String>,
    pub airplay_mode: Option<String>,
    pub airplay_window: Option<bool>,
    pub airplay_share: Option<bool>,
    pub airplay_port: Option<u16>,
    pub airplay_interface: Option<IpAddr>,
    pub mac_modifiers: Option<String>,
}

impl Overrides {
    /// The config fields a flag holds fixed, named as they are in the file.
    pub fn pinned(&self) -> Vec<&'static str> {
        let fields: [(&'static str, bool); 17] = [
            ("port", self.port.is_some()),
            ("position", self.position.is_some()),
            ("modules.input", self.input.is_some()),
            ("modules.clipboard", self.clipboard.is_some()),
            ("modules.audio", self.audio.is_some()),
            ("modules.files", self.files.is_some()),
            ("modules.usb", self.usb.is_some()),
            ("modules.remote_screen", self.remote_screen.is_some()),
            ("modules.screen_host", self.screen_host.is_some()),
            ("audio.render_device", self.audio_render_device.is_some()),
            ("audio.mic_gate", self.audio_mic_gate.is_some()),
            ("airplay.mode", self.airplay_mode.is_some()),
            ("airplay.window", self.airplay_window.is_some()),
            ("airplay.share", self.airplay_share.is_some()),
            ("airplay.port", self.airplay_port.is_some()),
            ("airplay.interface", self.airplay_interface.is_some()),
            ("input.mac_modifiers", self.mac_modifiers.is_some()),
        ];
        fields
            .into_iter()
            .filter(|(_, set)| *set)
            .map(|(name, _)| name)
            .collect()
    }
}

/// How often the file is re-read. Seconds, because a person saves a file or
/// clicks a toggle and then looks — and polling costs one `stat` per tick,
/// where a watcher would be a platform backend per OS for the same answer.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// Resolve when the file at `path`, laid under `overrides`, would produce a
/// config other than `running`.
///
/// Only a change in the *effective* config counts: rewriting the file with
/// the same content, or changing a field a flag pins, changes nothing. A
/// file that stops parsing is logged and waited out rather than acted on —
/// the user is most likely halfway through editing it.
pub async fn wait_for_change(path: &Path, overrides: &Overrides, running: &Config) -> Config {
    let mut last_seen = modified(path);
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let now = modified(path);
        if now == last_seen {
            continue;
        }
        last_seen = now;
        match Config::load(path) {
            Ok(file) => {
                let effective = file.with_overrides(overrides);
                if &effective != running {
                    return effective;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "config changed but does not parse; keeping the running one")
            }
        }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::parse(text, Path::new("test.toml")).unwrap()
    }

    #[test]
    fn a_machine_asks_before_updating_and_follows_stable_unless_told_otherwise() {
        let c = parse("");
        assert_eq!(c.update.policy, UpdatePolicy::Ask);
        assert_eq!(c.update.channel, UpdateChannel::Stable);
        let c = parse(
            "[update]\npolicy = \"auto\"\nchannel = \"dev\"\n\
             [[update.trusted_builders]]\npeer = \"00000000-0000-0000-0000-000000000000\"\n\
             name = \"nocturne\"\nkey = \"ab\"\n",
        );
        assert_eq!(c.update.policy, UpdatePolicy::Auto);
        assert_eq!(c.update.channel, UpdateChannel::Dev);
        assert_eq!(c.update.trusted_builders[0].name, "nocturne");
    }

    #[test]
    fn bluetooth_is_decided_by_the_machine_unless_the_file_says_otherwise() {
        assert_eq!(parse("").bluetooth.mesh, BluetoothMesh::Auto);
        let mut c = parse("[bluetooth]\nstandby = true\n");
        assert_eq!(c.bluetooth.mesh, BluetoothMesh::Auto);
        ConfigEdit::BluetoothMesh(BluetoothMesh::Off).apply(&mut c);
        assert_eq!(c.bluetooth.mesh, BluetoothMesh::Off);
        assert!(c.bluetooth.standby, "an edit touches only what it names");
        assert_eq!(
            parse("[bluetooth]\nmesh = \"on\"\n").bluetooth.mesh,
            BluetoothMesh::On
        );
    }

    #[test]
    fn defaults_are_the_everyday_set() {
        let c = Config::default();
        assert!(c.modules.input && c.modules.clipboard && c.modules.audio && c.modules.files);
        assert!(!c.modules.usb && !c.modules.remote_screen);
        assert_eq!(c.position, Position::new(0, 0));
        assert_eq!(c.airplay.mode, "on");
    }

    #[test]
    fn a_file_overrides_only_the_fields_it_names() {
        let c = parse("position = { x = 1, y = 0 }\n[modules]\naudio = false\n");
        assert_eq!(c.position, Position::new(1, 0));
        assert!(!c.modules.audio);
        assert!(c.modules.clipboard, "unnamed fields keep their default");
        assert_eq!(c.port, DEFAULT_PORT);
    }

    #[test]
    fn flags_override_the_file_and_absent_flags_do_not() {
        let file = parse("[modules]\naudio = false\nusb = true\n");
        let flags = Overrides {
            audio: Some(true),
            position: Some(Position::new(-1, 0)),
            ..Overrides::default()
        };
        let c = file.with_overrides(&flags);
        assert!(c.modules.audio, "a flag beats the file");
        assert!(c.modules.usb, "an absent flag leaves the file's answer");
        assert_eq!(c.position, Position::new(-1, 0));
        assert_eq!(flags.pinned(), vec!["position", "modules.audio"]);
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let err = Config::parse("[modules]\naudoi = true\n", Path::new("x.toml")).unwrap_err();
        assert!(err.to_string().contains("audoi"), "{err}");
    }

    #[test]
    fn an_edge_crosses_along_its_whole_length_unless_a_span_is_given() {
        let c = parse("[input.crossing]\nbottom = [66, 100]\n");
        assert_eq!(
            c.input.crossing.span(Edge::Bottom),
            EdgeSpan { from: 66, to: 100 }
        );
        assert_eq!(c.input.crossing.span(Edge::Left), EdgeSpan::WHOLE);
        assert_eq!(parse(&c.to_toml().unwrap()), c);
        assert_eq!(
            Config::default().input.crossing.span(Edge::Top),
            EdgeSpan::WHOLE
        );
    }

    #[test]
    fn the_app_narrows_an_edge_and_gives_it_back_whole() {
        let mut c = Config::default();
        let edit: ConfigEdit =
            serde_json::from_str(r#"{"crossing":{"edge":"Bottom","span":[66,100]}}"#).unwrap();
        edit.apply(&mut c);
        assert_eq!(
            c.input.crossing.bottom,
            Some(EdgeSpan { from: 66, to: 100 })
        );
        let whole: ConfigEdit =
            serde_json::from_str(r#"{"crossing":{"edge":"Bottom","span":[0,100]}}"#).unwrap();
        whole.apply(&mut c);
        assert_eq!(
            c.input.crossing,
            CrossingConfig::default(),
            "a whole edge is written as none"
        );
    }

    #[test]
    fn a_span_that_covers_nothing_is_refused_with_what_it_should_be() {
        for bad in ["[50, 50]", "[70, 20]", "[0, 120]"] {
            let err = Config::parse(
                &format!("[input.crossing]\ntop = {bad}\n"),
                Path::new("x.toml"),
            )
            .unwrap_err();
            assert!(err.to_string().contains("percent along the edge"), "{err}");
        }
    }

    #[test]
    fn the_share_policy_is_carried_through_untouched() {
        let c = parse("[files.share]\nmode = \"allow\"\npaths = [\"/home/me/Music\"]\n");
        let share = c.files.share.clone().unwrap();
        assert_eq!(share["mode"].as_str(), Some("allow"));
        let again = parse(&c.to_toml().unwrap());
        assert_eq!(again, c);
    }

    #[test]
    fn a_missing_file_reads_as_defaults_and_a_saved_one_round_trips() {
        let dir = std::env::temp_dir().join(format!("cursio-config-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        let mut c = Config {
            position: Position::new(2, -1),
            ..Config::default()
        };
        c.modules.usb = true;
        c.airplay.port = Some(7100);
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn only_a_change_to_the_effective_config_is_reported() {
        let dir = std::env::temp_dir().join(format!("cursio-watch-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let running = Config::default();
        running.save(&path).unwrap();
        let pinned = Overrides {
            usb: Some(false),
            ..Overrides::default()
        };

        let watcher = {
            let path = path.clone();
            let running = running.clone();
            tokio::spawn(async move { wait_for_change(&path, &pinned, &running).await })
        };
        // A pinned field changing is not a change; a free one is.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut edited = running.clone();
        edited.modules.usb = true;
        edited.save(&path).unwrap();
        tokio::time::sleep(WATCH_INTERVAL + Duration::from_millis(500)).await;
        assert!(!watcher.is_finished());
        edited.modules.audio = false;
        // Some filesystems keep whole-second mtimes; make this write distinct.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        edited.save(&path).unwrap();
        let got = tokio::time::timeout(WATCH_INTERVAL * 3, watcher)
            .await
            .expect("the change is noticed")
            .unwrap();
        assert!(!got.modules.audio);
        assert!(!got.modules.usb, "the flag still pins usb");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
