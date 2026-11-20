//! Every setting a node has, described once.
//!
//! A setting used to exist in seven places: a field of [`Config`], a flag, a
//! line in `live::plan`, maybe one in `live::restart_needed`, a control
//! command, a TypeScript type and a hand-built control in the app. One
//! forgotten meant a setting that saved and did nothing, or that the app could
//! not reach at all — and every module gaining one paid for all seven.
//!
//! Here each setting is one entry in [`SETTINGS`]: its dotted key in
//! `config.toml`, the kind of value it takes, what changing it on a running
//! node takes ([`Apply`]), and where it exists at all. Everything else follows
//! from the entry:
//!
//! - **Values** are read and written through the key, on the TOML form of the
//!   config ([`get`], [`set`]), so no setting needs a getter of its own, and a
//!   value that does not fit is refused by the same deserializer that reads
//!   the file.
//! - **Applying** a change is derived from [`Apply`] by `live::plan` and
//!   `live::restart_needed`.
//! - **Readers** — the app, the CLI, an AI agent, another machine — list the
//!   entries with their values ([`view`]) and change one by key; none of them
//!   names a setting itself.
//!
//! ## Nothing left out
//!
//! The tests build a config in which every field is set — by struct literals
//! without `..`, so a field added to [`Config`] does not compile until it is
//! set there too — and require every value in it to lie under some entry. A
//! new field therefore cannot ship as a setting nobody can see or that
//! silently does nothing on a running node.

use cursio_protocol::settings::{SettingApply, SettingKind, SettingView};
use cursio_protocol::ModuleId;

use crate::config::{Config, ConfigError, Overrides};

/// The group a setting is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Modules,
    Input,
    Audio,
    Calls,
    Files,
    Network,
    Bluetooth,
    Updates,
    AirPlay,
    Desk,
    Apps,
}

impl Section {
    pub fn name(self) -> &'static str {
        match self {
            Section::Modules => "modules",
            Section::Input => "input",
            Section::Audio => "audio",
            Section::Calls => "calls",
            Section::Files => "files",
            Section::Network => "network",
            Section::Bluetooth => "bluetooth",
            Section::Updates => "updates",
            Section::AirPlay => "airplay",
            Section::Desk => "desk",
            Section::Apps => "apps",
        }
    }
}

/// What kind of value a setting takes.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Toggle,
    Choice(&'static [&'static str]),
    /// One of the words, or a whole number from `min` to `max`, stored as text.
    Level {
        words: &'static [&'static str],
        min: i32,
        max: i32,
        unit: &'static str,
    },
    Number {
        min: i64,
        max: i64,
    },
    Text(&'static [&'static str]),
    /// A sound device by name, `Output` or `Input`; the node knows which are
    /// present now.
    Device(DeviceRole),
    Address,
    Structured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRole {
    Output,
    Input,
}

/// What a changed value takes to reach a running node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// Read where it is used, at every use: nothing to do.
    Read,
    /// Switches a module on or off on every link.
    Switch(ModuleId),
    /// Carried to every link by `Reconfigure`: where this machine stands.
    Relink,
    /// Handed to the piece that uses it, which takes it at once.
    Handed,
    /// Read by a module when it is built: handed over, then that module
    /// restarts on every link.
    Module(ModuleId),
    /// Held by something made once per process; waits for a restart.
    Process,
}

impl Apply {
    fn view(self) -> SettingApply {
        match self {
            Apply::Read | Apply::Handed => SettingApply::Live,
            Apply::Switch(_) | Apply::Relink => SettingApply::Relink,
            Apply::Module(_) => SettingApply::RestartModule,
            Apply::Process => SettingApply::Restart,
        }
    }
}

/// Platforms a setting exists on, as a bit set.
pub mod on {
    pub const LINUX: u8 = 1;
    pub const WINDOWS: u8 = 2;
    pub const MACOS: u8 = 4;
    pub const ANDROID: u8 = 8;
    pub const DESKTOP: u8 = LINUX | WINDOWS | MACOS;
    pub const ALL: u8 = DESKTOP | ANDROID;

    /// This build's platform.
    pub const HERE: u8 = if cfg!(target_os = "android") {
        ANDROID
    } else if cfg!(target_os = "windows") {
        WINDOWS
    } else if cfg!(target_os = "macos") {
        MACOS
    } else {
        LINUX
    };
}

/// One setting.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    /// Its dotted path in `config.toml`; a table's key covers what is in it.
    pub key: &'static str,
    pub section: Section,
    pub title: &'static str,
    pub hint: &'static str,
    pub kind: Kind,
    pub apply: Apply,
    /// Where it exists, from [`on`].
    pub on: u8,
    /// The module it belongs to, when it means nothing without it.
    pub module: Option<ModuleId>,
}

impl Setting {
    /// Whether it exists on this build's platform.
    pub fn here(&self) -> bool {
        self.on & on::HERE != 0
    }
}

const fn switch(
    key: &'static str,
    title: &'static str,
    hint: &'static str,
    id: ModuleId,
    platforms: u8,
) -> Setting {
    Setting {
        key,
        section: Section::Modules,
        title,
        hint,
        kind: Kind::Toggle,
        apply: Apply::Switch(id),
        on: platforms,
        module: Some(id),
    }
}

/// Every setting, in the order a reader shows them.
pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "modules.input",
        section: Section::Modules,
        title: "Share keyboard and mouse",
        hint: "Capture this machine's input and move it to the peer the cursor crosses to.",
        kind: Kind::Toggle,
        apply: Apply::Handed,
        on: on::DESKTOP,
        module: None,
    },
    switch("modules.clipboard", "Shared clipboard", "Copy on one machine, paste on another.", ModuleId::CLIPBOARD, on::ALL),
    switch("modules.audio", "Audio", "Peers' applications and microphones as devices here, and this machine's for them.", ModuleId::AUDIO, on::DESKTOP),
    switch("modules.files", "Shared disks", "Peers' disks as folders here, and this machine's for them.", ModuleId::FILES, on::DESKTOP),
    switch("modules.usb", "USB devices", "Use a peer's USB devices as if plugged in here.", ModuleId::USB, on::DESKTOP),
    switch("modules.window", "Windows and screens", "Windows dragged across, peers' screens and a phone's screen.", ModuleId::WINDOW, on::ALL),
    Setting {
        key: "modules.screen_host",
        section: Section::Modules,
        title: "Let peers view this screen",
        hint: "Peers may show this machine's windows and screens.",
        kind: Kind::Toggle,
        apply: Apply::Module(ModuleId::WINDOW),
        on: on::DESKTOP,
        module: Some(ModuleId::WINDOW),
    },
    Setting {
        key: "modules.remote_screen",
        section: Section::Modules,
        title: "Peers' screens in Discord and OBS",
        hint: "Offer every peer's windows and screens to screen-sharing apps here.",
        kind: Kind::Toggle,
        apply: Apply::Process,
        on: on::LINUX | on::WINDOWS,
        module: Some(ModuleId::WINDOW),
    },
    switch("modules.notify", "Notifications", "A phone's notifications shown here, and this machine's sent to it.", ModuleId::NOTIFY, on::ALL),
    switch("modules.media", "Media controls", "What plays on a peer, and its controls.", ModuleId::MEDIA, on::ALL),
    switch("modules.calls", "Calls", "A phone's calls, rung and answered here.", ModuleId::CALLS, on::ALL),
    switch("modules.messages", "Text messages", "A phone's SMS, read and sent from here.", ModuleId::MESSAGES, on::ALL),
    switch("modules.gallery", "Gallery", "A machine's photos, browsed from another.", ModuleId::GALLERY, on::ALL),
    switch("modules.share", "Send to a peer", "Links, text and files sent to a peer.", ModuleId::SHARE, on::ALL),
    switch("modules.direct", "Direct Wi-Fi", "Reach a nearby machine over a Wi-Fi link of its own beside the router.", ModuleId::DIRECT, on::DESKTOP),
    Setting {
        key: "position",
        section: Section::Desk,
        title: "Position on the grid",
        hint: "Where this machine stands among the others, in grid cells.",
        kind: Kind::Structured,
        apply: Apply::Relink,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "layout",
        section: Section::Desk,
        title: "Place on the desk",
        hint: "Where this machine's screens stand on the shared desk, and the seams closed.",
        kind: Kind::Structured,
        apply: Apply::Relink,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "input.crossing",
        section: Section::Input,
        title: "Crossing edges",
        hint: "Which part of each edge sends the cursor on.",
        kind: Kind::Structured,
        apply: Apply::Handed,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "input.mac_modifiers",
        section: Section::Input,
        title: "Modifier keys on this Mac",
        hint: "Where a peer's modifiers land: positional, swapped, or from=to pairs.",
        kind: Kind::Text(&["positional", "swapped"]),
        apply: Apply::Handed,
        on: on::MACOS,
        module: None,
    },
    Setting {
        key: "audio.mic_gate",
        section: Section::Audio,
        title: "Microphone gate",
        hint: "What a peer recording this machine's microphones hears below this level: silence.",
        kind: Kind::Level {
            words: &["auto", "off"],
            min: -80,
            max: -20,
            unit: "dB",
        },
        apply: Apply::Handed,
        on: on::ALL,
        module: Some(ModuleId::AUDIO),
    },
    Setting {
        key: "audio.render_device",
        section: Section::Audio,
        title: "Output for peers' sound",
        hint: "Play peers' audio on the output whose name contains this; the default output when unset.",
        kind: Kind::Device(DeviceRole::Output),
        apply: Apply::Module(ModuleId::AUDIO),
        on: on::WINDOWS,
        module: Some(ModuleId::AUDIO),
    },
    Setting {
        key: "calls.answer_moves_voice",
        section: Section::Calls,
        title: "Answering here brings the voice here",
        hint: "Answering or dialling from this computer moves the call's sound to it.",
        kind: Kind::Toggle,
        apply: Apply::Handed,
        on: on::DESKTOP,
        module: Some(ModuleId::CALLS),
    },
    Setting {
        key: "calls.voice",
        section: Section::Calls,
        title: "Where the voice plays",
        hint: "follow: on the machine the user is at; here: always on this computer.",
        kind: Kind::Choice(&["follow", "here"]),
        apply: Apply::Handed,
        on: on::DESKTOP,
        module: Some(ModuleId::CALLS),
    },
    Setting {
        key: "calls.output",
        section: Section::Calls,
        title: "Call output",
        hint: "The output a call plays on here; the default when unset.",
        kind: Kind::Device(DeviceRole::Output),
        apply: Apply::Handed,
        on: on::DESKTOP,
        module: Some(ModuleId::CALLS),
    },
    Setting {
        key: "calls.input",
        section: Section::Calls,
        title: "Call microphone",
        hint: "The microphone a call hears here; the default when unset.",
        kind: Kind::Device(DeviceRole::Input),
        apply: Apply::Handed,
        on: on::DESKTOP,
        module: Some(ModuleId::CALLS),
    },
    Setting {
        key: "files.share",
        section: Section::Files,
        title: "What this machine shares",
        hint: "The disks and folders peers may see, and how.",
        kind: Kind::Structured,
        apply: Apply::Module(ModuleId::FILES),
        on: on::ALL,
        module: Some(ModuleId::FILES),
    },
    Setting {
        key: "files.cache_gb",
        section: Section::Files,
        title: "Read cache, GiB",
        hint: "How much of peers' files a mount here keeps on this disk across runs; 0 turns it off.",
        kind: Kind::Number { min: 0, max: 1024 },
        apply: Apply::Process,
        on: on::DESKTOP,
        module: Some(ModuleId::FILES),
    },
    Setting {
        key: "port",
        section: Section::Network,
        title: "Port",
        hint: "The TCP port peers reach this node on.",
        kind: Kind::Number { min: 1024, max: 65535 },
        apply: Apply::Process,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "bluetooth.mesh",
        section: Section::Bluetooth,
        title: "Mesh over Bluetooth",
        hint: "auto stays off on a Wi-Fi card known to lose the network while Bluetooth is busy.",
        kind: Kind::Choice(&["auto", "on", "off"]),
        apply: Apply::Read,
        on: on::ALL,
        module: None,
    },
    Setting {
        key: "bluetooth.standby",
        section: Section::Bluetooth,
        title: "Keep a Bluetooth path ready",
        hint: "Move to Bluetooth within seconds when Wi-Fi drops, at some battery and Wi-Fi cost.",
        kind: Kind::Toggle,
        apply: Apply::Read,
        on: on::ALL,
        module: None,
    },
    Setting {
        key: "update.policy",
        section: Section::Updates,
        title: "New builds",
        hint: "auto switches at a quiet moment, ask waits for you, never takes none.",
        kind: Kind::Choice(&["auto", "ask", "never"]),
        apply: Apply::Read,
        on: on::ALL,
        module: None,
    },
    Setting {
        key: "update.channel",
        section: Section::Updates,
        title: "Channel",
        hint: "stable takes release builds only; dev also those of trusted builders.",
        kind: Kind::Choice(&["stable", "dev"]),
        apply: Apply::Read,
        on: on::ALL,
        module: None,
    },
    Setting {
        key: "update.trusted_builders",
        section: Section::Updates,
        title: "Trusted builders",
        hint: "Machines whose dev builds this one accepts, each pinned to its key.",
        kind: Kind::Structured,
        apply: Apply::Read,
        on: on::ALL,
        module: None,
    },
    Setting {
        key: "airplay.mode",
        section: Section::AirPlay,
        title: "AirPlay",
        hint: "on, off, or a quality such as 1080p60 this machine asks the mesh for.",
        kind: Kind::Text(&["on", "off", "1080p60", "1440p:30fps"]),
        apply: Apply::Process,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "airplay.window",
        section: Section::AirPlay,
        title: "Show the mirrored picture here",
        hint: "Open the iPhone's picture on this machine.",
        kind: Kind::Toggle,
        apply: Apply::Process,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "airplay.share",
        section: Section::AirPlay,
        title: "Offer it to every peer",
        hint: "Stream the mirrored screen to every connected machine.",
        kind: Kind::Toggle,
        apply: Apply::Process,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "airplay.port",
        section: Section::AirPlay,
        title: "AirPlay port",
        hint: "The receiver's control port; its own default when unset.",
        kind: Kind::Number { min: 1024, max: 65535 },
        apply: Apply::Process,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "airplay.interface",
        section: Section::AirPlay,
        title: "AirPlay network",
        hint: "Announce AirPlay only on the interface with this address.",
        kind: Kind::Address,
        apply: Apply::Process,
        on: on::DESKTOP,
        module: None,
    },
    Setting {
        key: "apps",
        section: Section::Apps,
        title: "Applications",
        hint: "Other applications' builds this machine takes, and what they may do.",
        kind: Kind::Structured,
        apply: Apply::Read,
        on: on::ALL,
        module: None,
    },
    Setting {
        key: "package_access",
        section: Section::Apps,
        title: "Apps allowed to reach the node",
        hint: "Which of the phone's apps may talk to the node, and as which application.",
        kind: Kind::Structured,
        apply: Apply::Read,
        on: on::ANDROID,
        module: None,
    },
    Setting {
        key: "ext",
        section: Section::Modules,
        title: "Modules built outside Cursio",
        hint: "Each such module's own settings, in its own table; they are listed one by one once the module declares them.",
        kind: Kind::Structured,
        apply: Apply::Read,
        on: on::ALL,
        module: None,
    },
];

/// One setting as the node works with it: a built-in one from [`SETTINGS`]
/// that exists on this platform, or one a module or an application declared
/// while the node runs ([`register`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    pub section: String,
    pub title: String,
    pub hint: String,
    pub kind: SettingKind,
    pub apply: Apply,
    /// The module it means nothing without.
    pub module: Option<ModuleId>,
    /// Its value when the file says nothing; `None` makes it optional.
    pub default: Option<toml::Value>,
}

impl Setting {
    fn entry(&self) -> Entry {
        Entry {
            key: self.key.to_owned(),
            section: self.section.name().to_owned(),
            title: self.title.to_owned(),
            hint: self.hint.to_owned(),
            kind: kind_view(self.kind),
            apply: self.apply,
            module: self.module,
            default: get(&Config::default(), self.key),
        }
    }
}

/// Where a declared setting's value may live: under a module's own table or
/// an application's, so one can never shadow a built-in setting or another's.
pub const DECLARED_PREFIXES: [&str; 2] = ["ext.", "apps."];

fn declared() -> &'static std::sync::RwLock<Vec<Entry>> {
    static DECLARED: std::sync::OnceLock<std::sync::RwLock<Vec<Entry>>> =
        std::sync::OnceLock::new();
    DECLARED.get_or_init(Default::default)
}

/// Add a setting a module (`ext.<module>.<name>`) or an application
/// (`apps.<id>.settings.<name>`) declares, replacing its earlier
/// declaration. Only [`Apply::Read`], [`Apply::Switch`] and
/// [`Apply::Module`] are open to them: the others hand a value to a piece of
/// the node by name, and a declared setting has no such piece.
pub fn register(entry: Entry) -> Result<(), String> {
    let in_ext = entry
        .key
        .strip_prefix("ext.")
        .is_some_and(|rest| rest.split('.').count() >= 2);
    let in_app = entry.key.strip_prefix("apps.").is_some_and(|rest| {
        let parts: Vec<&str> = rest.split('.').collect();
        parts.len() >= 3 && parts[1] == "settings"
    });
    if !in_ext && !in_app {
        return Err(format!(
            "{} is not a key a module or an application may declare: it goes under ext.<module>. or apps.<id>.settings.",
            entry.key
        ));
    }
    if entry.key.split('.').any(|part| {
        part.is_empty()
            || !part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }) {
        return Err(format!(
            "{} is not a key: letters, digits, _ and - between dots",
            entry.key
        ));
    }
    if !matches!(
        entry.apply,
        Apply::Read | Apply::Switch(_) | Apply::Module(_)
    ) {
        return Err(format!("{} cannot be applied {:?}", entry.key, entry.apply));
    }
    let mut all = declared().write().unwrap_or_else(|p| p.into_inner());
    all.retain(|e| e.key != entry.key);
    all.push(entry);
    Ok(())
}

/// Forget every declared setting whose key starts with `prefix`: an
/// application uninstalled, a module gone.
pub fn unregister(prefix: &str) {
    declared()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|e| !e.key.starts_with(prefix));
}

/// Every setting that exists here: the built-in ones of this platform, then
/// those declared since the node started.
pub fn entries() -> Vec<Entry> {
    let mut all: Vec<Entry> = SETTINGS
        .iter()
        .filter(|s| s.here())
        .map(Setting::entry)
        .collect();
    all.extend(
        declared()
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned(),
    );
    all
}

/// The setting called `key`, if it exists here.
pub fn find(key: &str) -> Option<Entry> {
    if let Some(setting) = SETTINGS.iter().find(|s| s.key == key && s.here()) {
        return Some(setting.entry());
    }
    declared()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|e| e.key == key)
        .cloned()
}

/// What `key` is in `config`, or its default when the file says nothing.
pub fn value(config: &Config, key: &str) -> Option<toml::Value> {
    get(config, key).or_else(|| find(key).and_then(|e| e.default))
}

fn as_table(config: &Config) -> toml::Table {
    match toml::Value::try_from(config) {
        Ok(toml::Value::Table(table)) => table,
        // A config is a struct of tables; serializing it cannot give
        // anything else, and a failure is a bug in a field's serializer.
        other => panic!("the config does not serialize as a table: {other:?}"),
    }
}

fn lookup<'a>(table: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let mut parts = key.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.as_table()?.get(part)?;
    }
    Some(value)
}

/// The value at `key`, or `None` for an optional setting left unset.
pub fn get(config: &Config, key: &str) -> Option<toml::Value> {
    lookup(&as_table(config), key).cloned()
}

/// `config` with `key` set to `value`, `None` unsetting an optional one.
///
/// The value is checked against the setting's kind, then the whole config is
/// read back through the same deserializer as the file, so whatever the file
/// would refuse is refused here, with its reason.
pub fn set(config: &Config, key: &str, value: Option<toml::Value>) -> Result<Config, ConfigError> {
    let setting = find(key).ok_or_else(|| unknown(key))?;
    let value = match value {
        Some(value) => Some(check(&setting, value)?),
        None if setting.default.is_none() => None,
        None => {
            return Err(ConfigError::Invalid(format!(
                "{key} always has a value; reset it to go back to its default"
            )))
        }
    };
    set_raw(config, key, value)
}

/// `config` with `key` back at its default.
pub fn reset(config: &Config, key: &str) -> Result<Config, ConfigError> {
    find(key).ok_or_else(|| unknown(key))?;
    // Taken out of the file rather than written as the default, so a default
    // that changes in a later build is followed.
    set_raw(config, key, None)
}

fn set_raw(config: &Config, key: &str, value: Option<toml::Value>) -> Result<Config, ConfigError> {
    let mut table = as_table(config);
    let (parents, last) = key
        .rsplit_once('.')
        .map_or((None, key), |(p, l)| (Some(p), l));
    let mut at = &mut table;
    for part in parents.into_iter().flat_map(|p| p.split('.')) {
        at = at
            .entry(part)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| ConfigError::Invalid(format!("{part} in {key} is not a table")))?;
    }
    match value {
        Some(value) => {
            at.insert(last.to_owned(), value);
        }
        None => {
            at.remove(last);
            prune(&mut table, key);
        }
    }
    toml::Value::Table(table)
        .try_into()
        .map_err(|e: toml::de::Error| ConfigError::Invalid(format!("{key}: {}", e.message())))
}

/// Drop the tables on `key`'s path that unsetting it left empty, deepest
/// first: `[ext.tracking]` with nothing in it is noise in the file, and a
/// map of tables would keep it as an entry.
fn prune(table: &mut toml::Table, key: &str) {
    let parts: Vec<&str> = key.split('.').collect();
    for depth in (1..parts.len()).rev() {
        let mut at = &mut *table;
        for part in &parts[..depth - 1] {
            match at.get_mut(*part).and_then(toml::Value::as_table_mut) {
                Some(next) => at = next,
                None => return,
            }
        }
        let name = parts[depth - 1];
        if at
            .get(name)
            .and_then(toml::Value::as_table)
            .is_some_and(toml::Table::is_empty)
        {
            at.remove(name);
        } else {
            return;
        }
    }
}

fn unknown(key: &str) -> ConfigError {
    ConfigError::Invalid(format!(
        "no setting is called {key}; `cursio config list` names them all"
    ))
}

/// `value` as `setting` stores it, or why it does not fit.
fn check(setting: &Entry, value: toml::Value) -> Result<toml::Value, ConfigError> {
    let key = &setting.key;
    let wrong = |wanted: &str| {
        ConfigError::Invalid(format!(
            "{key} takes {wanted}, not {value}",
            value = value_text(&value)
        ))
    };
    match &setting.kind {
        SettingKind::Toggle => value
            .as_bool()
            .map(toml::Value::Boolean)
            .ok_or_else(|| wrong("true or false")),
        SettingKind::Choice { options } => match value.as_str() {
            Some(s) if options.iter().any(|o| o == s) => Ok(value),
            _ => Err(wrong(&format!("one of {}", options.join(", ")))),
        },
        SettingKind::Level {
            words,
            min,
            max,
            unit,
        } => {
            let wanted = || {
                wrong(&format!(
                    "{} or a level from {min} to {max} {unit}",
                    words.join(", ")
                ))
            };
            let level = match &value {
                toml::Value::String(s) if words.iter().any(|w| w == s.trim()) => {
                    return Ok(toml::Value::String(s.trim().to_owned()))
                }
                toml::Value::String(s) => s
                    .trim()
                    .trim_end_matches(unit.as_str())
                    .trim()
                    .parse::<i64>()
                    .ok(),
                toml::Value::Integer(n) => Some(*n),
                toml::Value::Float(f) if f.fract() == 0.0 => Some(*f as i64),
                _ => None,
            }
            .ok_or_else(wanted)?;
            if (i64::from(*min)..=i64::from(*max)).contains(&level) {
                Ok(toml::Value::String(level.to_string()))
            } else {
                Err(wanted())
            }
        }
        SettingKind::Number { min, max } => match value.as_integer() {
            Some(n) if (*min..=*max).contains(&n) => Ok(value),
            _ => Err(wrong(&format!("a whole number from {min} to {max}"))),
        },
        SettingKind::Text { .. } | SettingKind::Device { .. } => match value.as_str() {
            Some(s) if !s.trim().is_empty() => Ok(toml::Value::String(s.trim().to_owned())),
            _ => Err(wrong("some text")),
        },
        SettingKind::Address => match value.as_str().map(str::parse::<std::net::IpAddr>) {
            Some(Ok(addr)) => Ok(toml::Value::String(addr.to_string())),
            _ => Err(wrong("an IP address")),
        },
        SettingKind::Structured => Ok(value),
    }
}

fn value_text(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

/// `value`, as JSON, in TOML's form; `null` unsets.
pub fn from_json(value: serde_json::Value) -> Result<Option<toml::Value>, ConfigError> {
    if value.is_null() {
        return Ok(None);
    }
    toml::Value::try_from(value)
        .map(Some)
        .map_err(|e| ConfigError::Invalid(format!("not a value a setting can take: {e}")))
}

fn to_json(value: Option<toml::Value>) -> String {
    value
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or(serde_json::Value::Null)
        .to_string()
}

/// What a node knows about its own settings, to describe them.
pub struct Here<'a> {
    /// The config it runs.
    pub running: &'a Config,
    pub overrides: &'a Overrides,
    /// What [`crate::live::restart_needed`] says now.
    pub restart_needed: &'a [String],
    /// Whether a module can run here at all.
    pub has_module: &'a dyn Fn(ModuleId) -> bool,
    /// The sound devices present now.
    pub devices: &'a dyn Fn(DeviceRole) -> Vec<String>,
}

/// Every setting that exists here, with its value.
pub fn view(here: &Here<'_>) -> Vec<SettingView> {
    let pinned = here.overrides.pinned();
    entries()
        .into_iter()
        .filter(|e| e.module.is_none_or(|id| (here.has_module)(id)))
        .map(|mut e| {
            if let SettingKind::Device { role, options } = &mut e.kind {
                let role = if role == "input" {
                    DeviceRole::Input
                } else {
                    DeviceRole::Output
                };
                *options = (here.devices)(role);
            }
            SettingView {
                value_json: to_json(get(here.running, &e.key).or_else(|| e.default.clone())),
                default_json: to_json(e.default.clone()),
                optional: e.default.is_none(),
                apply: e.apply.view(),
                pinned: pinned.contains(&e.key.as_str()),
                waits_for_restart: here.restart_needed.contains(&e.key),
                key: e.key,
                section: e.section,
                title: e.title,
                hint: e.hint,
                kind: e.kind,
            }
        })
        .collect()
}

fn kind_view(kind: Kind) -> SettingKind {
    let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
    match kind {
        Kind::Toggle => SettingKind::Toggle,
        Kind::Choice(options) => SettingKind::Choice {
            options: owned(options),
        },
        Kind::Level {
            words,
            min,
            max,
            unit,
        } => SettingKind::Level {
            words: owned(words),
            min,
            max,
            unit: unit.to_owned(),
        },
        Kind::Number { min, max } => SettingKind::Number { min, max },
        Kind::Text(suggestions) => SettingKind::Text {
            suggestions: owned(suggestions),
        },
        Kind::Device(role) => SettingKind::Device {
            role: match role {
                DeviceRole::Output => "output",
                DeviceRole::Input => "input",
            }
            .to_owned(),
            options: Vec::new(),
        },
        Kind::Address => SettingKind::Address,
        Kind::Structured => SettingKind::Structured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use cursio_protocol::{Barrier, PeerId, Place, Position, ScreenOffset};

    /// A config in which every field holds something, and something other
    /// than its default. Struct literals without `..` on purpose: a field
    /// added to the config does not compile until it is set here, and then
    /// the tests below require a setting to cover it.
    fn everything() -> Config {
        let builder = TrustedBuilder {
            peer: PeerId::from_bytes([7; 16]),
            name: "builder".into(),
            key: "00ff".into(),
        };
        let mut apps = std::collections::BTreeMap::new();
        apps.insert(
            "leavepulse".to_owned(),
            AppConfig {
                policy: UpdatePolicy::Never,
                trusted_builders: vec![builder.clone()],
                admin: AdminConsent::Allow,
                actions: AdminConsent::Deny,
                devtools: DevtoolsConsent::Ask,
                devtools_grants: vec![DevtoolsGrant {
                    peer: "Lightmoon".into(),
                    until: 9,
                    pids: vec![1],
                }],
                settings: toml::toml! { quality = "high" },
            },
        );
        let mut ext = std::collections::BTreeMap::new();
        ext.insert("tracking".to_owned(), toml::toml! { predict = true });
        Config {
            port: 40000,
            position: Position::new(2, 1),
            modules: Modules {
                input: false,
                clipboard: false,
                audio: false,
                files: false,
                usb: true,
                remote_screen: true,
                screen_host: false,
                direct: false,
                window: false,
                notify: false,
                media: false,
                calls: false,
                messages: false,
                gallery: false,
                share: false,
            },
            audio: AudioConfig {
                render_device: Some("Speakers".into()),
                mic_gate: Some("-55".into()),
            },
            airplay: AirPlayConfig {
                mode: "off".into(),
                window: true,
                share: true,
                port: Some(7100),
                interface: Some("192.168.1.2".parse().unwrap()),
            },
            input: InputConfig {
                mac_modifiers: "swapped".into(),
                crossing: CrossingConfig {
                    left: Some(EdgeSpan { from: 0, to: 50 }),
                    right: Some(EdgeSpan { from: 10, to: 90 }),
                    top: Some(EdgeSpan { from: 20, to: 80 }),
                    bottom: Some(EdgeSpan { from: 67, to: 100 }),
                },
            },
            files: FilesConfig {
                share: Some(toml::toml! { home = true }),
                cache_gb: 3,
            },
            update: UpdateConfig {
                policy: UpdatePolicy::Auto,
                channel: UpdateChannel::Dev,
                trusted_builders: vec![builder],
            },
            calls: CallsConfig {
                answer_moves_voice: false,
                voice: CallVoice::Here,
                output: Some("Headset".into()),
                input: Some("Mic".into()),
            },
            bluetooth: BluetoothConfig {
                mesh: BluetoothMesh::Off,
                standby: true,
            },
            layout: LayoutConfig {
                place: Some(Place { x_mm: 1, y_mm: 2 }),
                barriers: vec![Barrier {
                    screen: "a".into(),
                    peer: PeerId::from_bytes([8; 16]),
                    peer_screen: "b".into(),
                }],
                offsets: vec![ScreenOffset {
                    screen: "a".into(),
                    dx_mm: 3,
                    dy_mm: 4,
                }],
            },
            apps,
            package_access: vec![PackageGrant {
                package: "com.example".into(),
                app: "leavepulse".into(),
                allowed: true,
            }],
            ext,
        }
    }

    fn leaves(prefix: &str, value: &toml::Value, out: &mut Vec<String>) {
        match value {
            toml::Value::Table(table) => {
                for (key, value) in table {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    leaves(&path, value, out);
                }
            }
            _ => out.push(prefix.to_owned()),
        }
    }

    fn covered(path: &str) -> bool {
        SETTINGS
            .iter()
            .any(|s| path == s.key || path.starts_with(&format!("{}.", s.key)))
    }

    #[test]
    fn every_value_of_the_config_lies_under_some_setting() {
        let mut all = Vec::new();
        leaves("", &toml::Value::try_from(everything()).unwrap(), &mut all);
        let loose: Vec<_> = all.iter().filter(|p| !covered(p)).collect();
        assert!(loose.is_empty(), "no setting covers {loose:?}");
    }

    #[test]
    fn every_setting_names_something_in_the_config() {
        let full = everything();
        for setting in SETTINGS {
            assert!(
                get(&full, setting.key).is_some(),
                "{} is in no config",
                setting.key
            );
        }
    }

    #[test]
    fn every_setting_differs_in_the_full_config_so_each_change_is_seen() {
        let (full, defaults) = (everything(), Config::default());
        for setting in SETTINGS {
            assert_ne!(
                get(&full, setting.key),
                get(&defaults, setting.key),
                "{}",
                setting.key
            );
        }
    }

    #[test]
    fn changing_any_setting_alone_reaches_the_running_node_as_its_kind_says() {
        let full = everything();
        let defaults = Config::default();
        for entry in entries() {
            let next = set_raw(&defaults, &entry.key, get(&full, &entry.key)).unwrap();
            let plan = crate::live::plan(&defaults, &next);
            let waits = crate::live::restart_needed(&defaults, &next);
            let key = &entry.key;
            match entry.apply {
                Apply::Read => {
                    assert_eq!(plan, crate::live::Plan::default(), "{key}");
                    assert!(waits.is_empty(), "{key}");
                }
                Apply::Process => assert_eq!(waits, vec![key.clone()], "{key}"),
                Apply::Switch(id) => {
                    let on = get(&full, key).and_then(|v| v.as_bool()).unwrap();
                    assert_eq!(plan.switches, vec![(id, on)], "{key}");
                }
                Apply::Relink | Apply::Handed => {
                    assert_ne!(plan, crate::live::Plan::default(), "{key} reaches nothing");
                    assert!(plan.restart.is_empty(), "{key}");
                }
                Apply::Module(id) => assert_eq!(plan.restart, vec![id], "{key}"),
            }
        }
    }

    #[test]
    fn a_value_is_checked_against_its_kind_and_read_back_like_the_file() {
        let defaults = Config::default();
        let set_json =
            |key: &str, json: serde_json::Value| set(&defaults, key, from_json(json).unwrap());
        assert!(
            !set_json("modules.files", serde_json::json!(false))
                .unwrap()
                .modules
                .files
        );
        assert!(set_json("modules.files", serde_json::json!("no")).is_err());
        assert!(set_json("bluetooth.mesh", serde_json::json!("sometimes")).is_err());
        assert_eq!(
            set_json("audio.mic_gate", serde_json::json!(-55))
                .unwrap()
                .audio
                .mic_gate
                .as_deref(),
            Some("-55")
        );
        assert_eq!(
            set_json("audio.mic_gate", serde_json::json!(" off "))
                .unwrap()
                .audio
                .mic_gate
                .as_deref(),
            Some("off")
        );
        assert!(set_json("audio.mic_gate", serde_json::json!(-5)).is_err());
        assert!(set_json("port", serde_json::json!(80)).is_err());
        assert!(set_json("airplay.interface", serde_json::json!("not an address")).is_err());
        assert!(set_json("input.crossing", serde_json::json!({ "left": [60, 10] })).is_err());
        assert!(set_json("no.such.thing", serde_json::json!(1)).is_err());
    }

    #[test]
    fn only_an_optional_setting_takes_null_and_reset_returns_to_the_default() {
        let full = everything();
        assert_eq!(
            set(&full, "audio.mic_gate", None).unwrap().audio.mic_gate,
            None
        );
        assert!(set(&full, "port", None).is_err());
        assert_eq!(reset(&full, "port").unwrap().port, DEFAULT_PORT);
    }

    #[test]
    fn a_declared_setting_lives_under_its_owner_and_has_its_own_default() {
        let entry = |key: &str, apply| Entry {
            key: key.into(),
            section: "tracking".into(),
            title: "Predict".into(),
            hint: String::new(),
            kind: SettingKind::Toggle,
            apply,
            module: None,
            default: Some(toml::Value::Boolean(true)),
        };
        assert!(register(entry("modules.files", Apply::Read)).is_err());
        assert!(register(entry("apps.leavepulse.policy", Apply::Read)).is_err());
        assert!(register(entry("ext.tracking.predict", Apply::Handed)).is_err());
        register(entry("ext.tracking.predict", Apply::Read)).unwrap();
        let config = Config::default();
        assert_eq!(
            value(&config, "ext.tracking.predict"),
            Some(toml::Value::Boolean(true))
        );
        let next = set(
            &config,
            "ext.tracking.predict",
            Some(toml::Value::Boolean(false)),
        )
        .unwrap();
        assert_eq!(next.ext["tracking"]["predict"], toml::Value::Boolean(false));
        assert_eq!(reset(&next, "ext.tracking.predict").unwrap(), config);
        unregister("ext.tracking.");
        assert!(find("ext.tracking.predict").is_none());
    }
}
