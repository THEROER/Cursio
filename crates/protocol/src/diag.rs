//! Looking at another machine of the mesh from this one: its log, and which
//! path carries the session between the two.
//!
//! A fault in a mesh is seen from two ends at once — a path one side calls
//! silent is one the other side wrote into, a session one side ends is one
//! the other was about to join — and reading only one end's log guesses at
//! the other's. Fetching the peer's log over the link it is about is what
//! makes the two read as one story, and it reaches a machine that has no
//! shell to log into: a phone, or a laptop on a hotspot.
//!
//! ## Asked for, never pushed
//!
//! A day of log is tens of megabytes, so a request names how much: the last
//! lines, newer than a mark or within a window of the answering machine's own
//! clock, and holding a piece of text. "Newer than a mark" is how a follower
//! keeps up: it hands back the timestamp of the last line it showed. The
//! answer is bounded by [`MAX_LINES`] and [`MAX_LOG_BYTES`] whatever was
//! asked, and says when it left older lines out.
//!
//! ## Choosing the path
//!
//! [`PathChoice`] is a preference for testing and for diagnosis, not a
//! setting: it lasts as long as the session, and a choice of path that does
//! not exist leaves the session on the one that does. Each end chooses for
//! what it sends, so the asking machine applies it to its own end and asks the
//! peer to do the same to the other.
//!
//! The trusted mesh permits both: a paired machine can already drive this
//! one's input and read its disks.

use serde::{Deserialize, Serialize};

use crate::Priority;

/// Most lines one answer carries.
pub const MAX_LINES: u32 = 5000;

/// Most text one answer carries, so it stays within one control-socket line
/// on the asking machine (1 MiB) once escaped as JSON.
pub const MAX_LOG_BYTES: usize = 512 * 1024;

/// Longest piece of text a request may filter by.
pub const MAX_FILTER_BYTES: usize = 256;

/// Whether `name` may name a log a peer reads: one of Cursio's own, a bare
/// name with no path in it. A name off the wire is untrusted, and this is
/// what keeps it from reaching any file but a log beside the node's.
pub fn valid_log_name(name: &str) -> bool {
    name.starts_with("cursio")
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Which of a session's paths should carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PathChoice {
    /// The network whenever it is there; Bluetooth when it is not.
    #[default]
    Auto,
    /// The network even while it is failing: no move to Bluetooth while a
    /// network path exists, which is how the network alone is tested.
    Network,
    /// Bluetooth whenever there is a Bluetooth path, the network standing by.
    Bluetooth,
}

/// What one machine asks another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagRequest {
    /// The newest `max_lines` lines of the node's log that are newer than
    /// `after` (a line's leading timestamp, compared as text), no older than
    /// `within_secs` by the answering machine's clock, and contain
    /// `contains`. `log` names another of Cursio's logs in the same
    /// directory instead — `cursio-mount-helper`, `cursio-app` — since a
    /// helper process writes its own; see [`valid_log_name`] — or
    /// `app:<id>`, the log an application on that machine registered with
    /// its node (`docs/app-sdk-design.md`).
    Logs {
        after: Option<String>,
        within_secs: Option<u64>,
        contains: Option<String>,
        max_lines: u32,
        log: Option<String>,
    },
    /// Report the session's paths, first choosing `choice` for this end of
    /// it when given.
    Paths { choice: Option<PathChoice> },
    /// How the node has fared over the last `since_secs` by its own clock,
    /// counted from its log by the node itself; see [`NodeReport`].
    Report { since_secs: u64 },
    /// Restart the node, or switch it to the build from the mesh that is
    /// installed and waiting; see [`Maintenance`].
    Maintain(Maintenance),
    /// Start the current build of application `app` — only one installed
    /// through the `apps` module, never a path — once `version` is it when
    /// given; `restart` ends the copies already running from its install
    /// first. Added last, so the variants before it keep their numbers.
    Launch {
        app: String,
        version: Option<String>,
        /// Which of the build's files to run, by component name; the one
        /// that is plainly the application when absent.
        component: Option<String>,
        restart: bool,
        /// `false` only answers whether `version` is current, starting
        /// nothing: how a ship that installs only waits for it.
        start: bool,
        /// Arguments for the started file. Only arguments cross: what runs
        /// is still only the installed build's own file.
        args: Vec<String>,
    },
    /// The actions application `app`'s running processes declared.
    AppActions { app: String },
    /// Ask application `app` (its `process`, or its main one) to do the
    /// declared `action` with the JSON `args_json`. JSON travels as text:
    /// this vocabulary is bincode, which cannot carry a free-form value.
    CallAction {
        app: String,
        process: Option<String>,
        action: String,
        args_json: String,
    },
    /// Can this phone carry out touch from a computer now? A tap sent to a
    /// phone whose accessibility service is off did nothing and said nothing.
    PhoneTouch,
    /// Type `text` into the field that has focus on this phone, any script:
    /// set by the accessibility service, not keyed as a US keyboard.
    PhoneType { text: String },
    /// A still of one window of an application registered here, its long
    /// side at most `max_side`.
    WindowShot {
        target: WindowTarget,
        max_side: Option<u32>,
    },
    /// Input to one window of an application registered here.
    WindowInput {
        target: WindowTarget,
        input: DesktopInput,
    },
    /// The interface of one window of an application registered here, as
    /// the tree of elements the platform publishes to assistive technology:
    /// at most `depth` levels and `max_nodes` elements.
    WindowTree {
        target: WindowTarget,
        depth: u16,
        max_nodes: u32,
    },
    /// Do `act` to the element `pick` names in one window of an application
    /// registered here.
    WindowElement {
        target: WindowTarget,
        pick: ElementPick,
        act: ElementAct,
    },
    /// A still of this phone's whole screen, taken by its own shell
    /// (`screencap`), answered as [`DiagResponse::WindowShot`]. No stream,
    /// no consent and no window on the computer, and the picture is of now,
    /// not of a stream seconds behind. A phone whose shell is not paired
    /// answers why; an older phone does not answer, and the computer takes
    /// the picture from the stream as before.
    PhoneShot,
}

/// How a request names an element of a window's interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementPick {
    /// The id a tree printed after `#`.
    Id(String),
    /// The `nth` (from 0) element of `role` named `name`.
    Match {
        role: String,
        name: String,
        nth: u32,
    },
}

/// What to do to an element of a window's interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementAct {
    Invoke,
    Toggle,
    Select,
    Expand,
    Collapse,
    Focus,
    SetValue(String),
}

/// Most a tree request may ask for.
pub const MAX_TREE_DEPTH: u16 = 64;
pub const MAX_TREE_NODES: u32 = 10_000;
/// What a tree request asks for unless told otherwise: deep enough for a web
/// view's nesting, a whole launcher page, a few hundred kilobytes at most.
pub const DEFAULT_TREE_DEPTH: u16 = 40;
pub const DEFAULT_TREE_NODES: u32 = 3000;
/// Longest role, name or id an element is picked by.
pub const MAX_PICK: usize = 256;

/// Which window of an application a request means. Only an application that
/// registered here can be named: the node finds its windows by the process
/// ids it registered with, never by a handle or a path a peer chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowTarget {
    pub app: String,
    pub process: Option<String>,
    /// The window whose title contains this, when the application has
    /// several; the largest otherwise.
    pub title: Option<String>,
}

/// Something done to a desktop window. Points are ten-thousandths of the
/// window (0 to [`WINDOW_SPAN`]), so the asker need not know its size and
/// the request stays comparable, as floats would not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopInput {
    Click {
        x: u16,
        y: u16,
        right: bool,
    },
    Type {
        text: String,
    },
    /// A named key: `enter`, `escape`, `tab`, `backspace`, `delete`, `up`,
    /// `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`,
    /// `f1`…`f12`.
    Key {
        key: String,
    },
}

/// Most text one [`DiagRequest::PhoneType`] or [`DesktopInput::Type`] carries.
pub const MAX_TYPED: usize = 4000;

/// A whole window's width or height in a [`DesktopInput::Click`].
pub const WINDOW_SPAN: u16 = 10_000;

/// A fraction of a window, 0 to 1, as a [`DesktopInput::Click`] carries it.
pub fn window_fraction(fraction: f32) -> Result<u16, String> {
    if !(0.0..=1.0).contains(&fraction) {
        return Err(format!(
            "{fraction} is outside the window; points are fractions from 0 to 1"
        ));
    }
    Ok((fraction * f32::from(WINDOW_SPAN)).round() as u16)
}

/// Most arguments a launch may carry, and the longest one.
pub const MAX_LAUNCH_ARGS: usize = 32;
pub const MAX_LAUNCH_ARG: usize = 1024;

/// Longest arguments a call to an application's action carries, as JSON —
/// under the line the application's socket takes.
pub const MAX_CALL_ARGS: usize = 32 * 1024;

/// One action an application declared, as a peer sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppAction {
    /// Which process declared it; `None` is the main one.
    pub process: Option<String>,
    pub name: String,
    pub description: String,
    /// JSON Schema of its arguments, as text.
    pub schema_json: String,
    /// Whether that process runs now; a call reaches only one that does.
    pub running: bool,
}

/// Something a person at another machine asks a node to do to itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Maintenance {
    /// Stop and come back from the top, re-reading everything.
    Restart,
    /// Switch to the waiting build now rather than at a quiet moment.
    ApplyUpdate,
}

/// The answer to one [`DiagRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagResponse {
    /// Oldest first. `truncated` when older lines matched but were left out.
    Logs {
        lines: Vec<String>,
        truncated: bool,
    },
    Paths(PathReport),
    Report(Box<NodeReport>),
    /// The node took the request and is about to do it; the link drops as
    /// it does.
    Maintaining(Maintenance),
    /// The application was started; `what` says which build, as what.
    Launched {
        what: String,
    },
    /// The answer to [`DiagRequest::AppActions`].
    Actions(Vec<AppAction>),
    /// What the action answered, as JSON text.
    Called {
        result_json: String,
    },
    /// The answer to [`DiagRequest::PhoneTouch`]: `reason` says what the
    /// phone's person has to do when it cannot.
    Touch {
        ready: bool,
        reason: Option<String>,
    },
    /// The text was set in the focused field.
    Typed {
        chars: u32,
    },
    /// The answer to [`DiagRequest::WindowShot`]: a PNG of the window.
    WindowShot {
        png: Vec<u8>,
        width: u32,
        height: u32,
        title: String,
    },
    /// What [`DiagRequest::WindowInput`] did, in a sentence.
    WindowDone {
        what: String,
    },
    /// The answer to [`DiagRequest::WindowTree`]: one line per element.
    WindowTree {
        title: String,
        text: String,
        nodes: u32,
        truncated: bool,
    },
}

/// A node's own memory. `None` is "cannot say".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeMemory {
    /// What the node holds by the system's own limit: resident bytes on
    /// Linux, private bytes (its share of the commit charge) on Windows.
    pub held: Option<u64>,
    /// The most it has been resident at once.
    pub peak: Option<u64>,
    pub threads: Option<u32>,
}

/// One process on the answering machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessUse {
    pub name: String,
    pub pid: u32,
    /// Measured as [`NodeMemory::held`] is.
    pub bytes: u64,
}

/// Something counted, with how often.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counted {
    pub what: String,
    pub count: u32,
}

/// A module that kept its own traffic waiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlowModule {
    pub module: String,
    pub count: u32,
    pub max_ms: u64,
}

/// One peer's sessions as this node saw them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerReport {
    pub name: String,
    /// Sessions started: each is a full module set brought up.
    pub sessions: u32,
    /// Why sessions ended, most common first.
    pub sessions_ended: Vec<Counted>,
    /// Why paths ended, as "kind: reason", most common first.
    pub paths_ended: Vec<Counted>,
    /// Time with no path at all, summed and the longest stretch.
    pub pathless_secs: u64,
    pub longest_pathless_secs: u64,
}

/// How a node has fared, counted from its own log by the node. Asked of every
/// machine at once, it is the mesh's day in a screenful; see `cursio report`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReport {
    pub since_secs: u64,
    /// The oldest line read, which is where the counts begin: a node started
    /// later, or a log longer than one answer reads, begins later than asked.
    pub from: Option<String>,
    /// Older lines were left unread.
    pub truncated: bool,
    /// Times the node started.
    pub starts: u32,
    pub memory: NodeMemory,
    /// The machine's largest processes now, largest first.
    pub top: Vec<ProcessUse>,
    /// Problems the node is raising now.
    pub problems: Vec<String>,
    pub peers: Vec<PeerReport>,
    /// Warnings and errors by kind, the variable parts taken out, most common
    /// first.
    pub warnings: Vec<Counted>,
    pub slow_modules: Vec<SlowModule>,
}

/// One end's view of the session between two machines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathReport {
    pub network: bool,
    pub bluetooth: bool,
    /// What carries input from this end now: "network", "bluetooth", or
    /// `None` while the session waits for a path.
    pub carrying: Option<String>,
    pub choice: PathChoice,
}

/// The diagnostics module's messages.
pub type DiagMsg = crate::exchange::ExchangeMsg<DiagRequest, DiagResponse>;

impl DiagMsg {
    /// A log is bulk, behind input and sound; a path report and every request
    /// are small and wanted now.
    pub fn priority(&self) -> Priority {
        match self {
            DiagMsg::Response {
                response:
                    Ok(
                        DiagResponse::Logs { .. }
                        | DiagResponse::Report(_)
                        | DiagResponse::WindowShot { .. }
                        | DiagResponse::WindowTree { .. },
                    ),
                ..
            } => Priority::Bulk,
            _ => Priority::Metadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_answer_round_trips_and_rides_as_bulk() {
        let msg = DiagMsg::Response {
            id: 4,
            response: Ok(DiagResponse::Logs {
                lines: vec!["2026-10-31T16:11:12.191382Z  INFO cursio::mesh: up".into()],
                truncated: true,
            }),
        };
        let bytes = bincode::serialize(&msg).unwrap();
        assert_eq!(bincode::deserialize::<DiagMsg>(&bytes).unwrap(), msg);
        assert_eq!(msg.priority(), Priority::Bulk);
    }

    #[test]
    fn a_log_name_cannot_reach_outside_the_log_directory() {
        assert!(valid_log_name("cursio-mount-helper"));
        assert!(valid_log_name("cursio-app"));
        assert!(!valid_log_name("../cursio"));
        assert!(!valid_log_name("cursio/../../etc/passwd"));
        assert!(!valid_log_name("cursio.log"));
        assert!(!valid_log_name("trusted_peers"));
    }

    #[test]
    fn a_path_choice_is_wanted_at_once() {
        let msg = DiagMsg::Request {
            id: 1,
            request: DiagRequest::Paths {
                choice: Some(PathChoice::Bluetooth),
            },
        };
        assert_eq!(msg.priority(), Priority::Metadata);
    }
}
