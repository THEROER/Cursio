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
//! A flag stays pinned for the life of the process, including across the
//! restart a changed file triggers, so the status API reports which fields
//! are pinned: a toggle in the app that changes nothing is otherwise a bug
//! report.
//!
//! ## Why a restart and not a live reload
//!
//! Most of these are read once where they are expensive to change: capture
//! holds a portal permission that must not be re-requested per change, module
//! factories are negotiated into every open link, the listener owns its port.
//! Re-running the node from the top is the one reload that is exactly as
//! correct as a fresh start, and a mesh reconnects in seconds by design.
//!
//! ## The files share policy
//!
//! `[files.share]` is carried as an opaque table. Its shape is whatever the
//! files module's `SharePolicy` deserializes from; this crate only stores and
//! returns it, so the policy can evolve without a config migration here.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cursio_protocol::Position;
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
        let fields: [(&'static str, bool); 16] = [
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
/// the same content, or changing a field a flag pins, restarts nothing. A
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
