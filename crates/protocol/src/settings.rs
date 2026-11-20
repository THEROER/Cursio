//! A node's settings as another program reads them: the app, the CLI, an AI
//! agent, or another machine of the mesh.
//!
//! Each setting describes itself — its key in `config.toml`, what kind of
//! value it takes, what changing it costs — so whoever shows or changes it
//! needs no list of its own. The node is the only party that knows which
//! settings exist on this build and platform, and the reader that hard-codes
//! them falls behind the first time a module gains one.
//!
//! Values cross as JSON text rather than as a typed field: the shapes range
//! from a boolean to a table (`files.share`), and `bincode`, which carries this
//! between machines, cannot hold a self-describing value.

use serde::{Deserialize, Serialize};

/// What kind of value a setting takes, and so what a reader offers to edit it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingKind {
    /// `true` or `false`.
    Toggle,
    /// One of `options`.
    Choice { options: Vec<String> },
    /// One of `words`, or a whole number from `min` to `max` in `unit`,
    /// stored as text: the microphone gate's `auto`, `off` or `-55`.
    Level {
        words: Vec<String>,
        min: i32,
        max: i32,
        unit: String,
    },
    /// A whole number from `min` to `max`.
    Number { min: i64, max: i64 },
    /// Free text; `suggestions` are values worth offering.
    Text { suggestions: Vec<String> },
    /// A sound device by name, an `output` or an `input`; `options` are the
    /// ones present now, and a name not among them (a headset unplugged) is
    /// still a valid setting.
    Device { role: String, options: Vec<String> },
    /// An IP address.
    Address,
    /// A table or list with its own editor (the desk, the share policy,
    /// trusted builders) or none but the file. Shown, not edited generically.
    Structured,
}

/// What changing a setting takes on a running node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingApply {
    /// Takes effect at once; nothing visible restarts.
    Live,
    /// Every link renegotiates, starting or stopping a module on each.
    Relink,
    /// One module restarts on every link, both ends at once.
    RestartModule,
    /// Waits for the node to restart.
    Restart,
}

/// One setting as a node reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingView {
    /// Its dotted key in `config.toml`: `audio.mic_gate`.
    pub key: String,
    /// The group it is shown in: `modules`, `audio`, …
    pub section: String,
    /// A short name, in English; a reader with translations looks the key up
    /// first.
    pub title: String,
    pub hint: String,
    pub kind: SettingKind,
    /// The value the node runs with, as JSON; `null` for an optional setting
    /// left unset.
    pub value_json: String,
    /// The value with nothing in the file, as JSON.
    pub default_json: String,
    /// Whether `null` is a value it takes: unset, so the module decides.
    pub optional: bool,
    pub apply: SettingApply,
    /// Held by a command-line flag for the life of the process, so changing
    /// the file changes nothing until it restarts without the flag.
    pub pinned: bool,
    /// Changed since the node started, and waiting for a restart to apply.
    pub waits_for_restart: bool,
}

/// What changing a setting did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingSaved {
    /// The new value is in the file but a flag holds the running one.
    pub pinned: bool,
    /// Every setting, this one or earlier, that waits for a restart.
    pub restart_needed: Vec<String>,
}
