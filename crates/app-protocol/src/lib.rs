//! What an application and the Cursio node on its machine say to each other.
//!
//! An application is never a member of the mesh: it does not pair, holds no
//! key and opens no session. It talks to the one node on its own machine over
//! a local socket, and the node carries what the application asked for across
//! the mesh. This crate is that local vocabulary and nothing else, so that a
//! program linking `cursio-sdk` links no Cursio internals — the node is GPL,
//! and the line between the two is a socket, not a library.
//!
//! ## Stable by construction
//!
//! Cursio updates itself across the mesh on its own schedule, and an
//! application built against an older SDK must keep working through that. So
//! the first message names [`PROTOCOL`], the node answers with the highest
//! version both know, messages are added and never changed, and the bytes of
//! every message are pinned by the tests at the bottom of this file. A node
//! that does not know a message answers [`NodeMsg::Unsupported`] rather than
//! closing, because silence would be indistinguishable from a node that hung.
//!
//! ## Framing
//!
//! One JSON object per line, each way, at most [`MAX_LINE`] bytes. JSON for
//! the same reason the node's control socket uses it: a person debugging an
//! application can read the conversation with `socat`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The version of this vocabulary an SDK built from this crate speaks.
pub const PROTOCOL: u32 = 1;

/// Longest line either side accepts: room for a [`MAX_PAYLOAD`] message in
/// base64 with its envelope. It was 64 KiB before messages; a node sends a
/// [`NodeMsg::Message`] only to a process that listens to its topic, which an
/// SDK with the older limit never does.
pub const MAX_LINE: usize = 1024 * 1024;

/// Most bytes one message between copies of an application carries. Larger
/// data — a build, a world — belongs to the builds the node carries, or to
/// files; a message says what changed.
pub const MAX_PAYLOAD: usize = 512 * 1024;

/// Most topics one process listens to.
pub const MAX_TOPICS: usize = 64;

/// The socket's file name inside the node's private runtime directory.
pub const SOCKET_NAME: &str = "apps.sock";

/// The node's abstract socket name on Android, where Cursio's own app hosts
/// the node and its private directory is out of every other app's reach. An
/// abstract socket has no file to protect, so the node checks who connected
/// (the peer's uid, its package) instead; see `docs/app-sdk-design.md`.
pub const ABSTRACT_NAME: &str = "cursio-apps";

/// Longest [`AppId`].
pub const MAX_APP_ID: usize = 32;

/// Longest detail a [`ClientMsg::Health`] carries, and the longest
/// description of an [`ActionSpec`].
pub const MAX_DETAIL: usize = 512;

/// Most bytes an action's argument schema takes, as JSON.
pub const MAX_SCHEMA: usize = 16 * 1024;

/// Most actions one process declares.
pub const MAX_ACTIONS: usize = 64;

/// Largest single blob a copy fetches from another: a mod, a world region,
/// an archive. It streams to disk; nothing holds it whole in memory.
pub const MAX_BLOB: u64 = 4 * 1024 * 1024 * 1024;

/// Most blobs one [`ClientMsg::Fetch`] names — a modpack's whole list.
pub const MAX_FETCH: usize = 10_000;

/// The hash function a blob is named by. Sha1 because Modrinth and
/// CurseForge manifests carry it, and an application should be able to ask
/// for a file by the hash its own manifest already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlobAlgo {
    Sha1,
    Sha256,
    Sha512,
}

impl BlobAlgo {
    /// Hex digits of a digest of this kind.
    pub fn hex_len(self) -> usize {
        match self {
            BlobAlgo::Sha1 => 40,
            BlobAlgo::Sha256 => 64,
            BlobAlgo::Sha512 => 128,
        }
    }

    fn name(self) -> &'static str {
        match self {
            BlobAlgo::Sha1 => "sha1",
            BlobAlgo::Sha256 => "sha256",
            BlobAlgo::Sha512 => "sha512",
        }
    }
}

/// A blob's content hash, written `sha1:<hex>`: what a copy asks its peers
/// for, and what the received bytes must hash to before they count.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BlobHash {
    algo: BlobAlgo,
    hex: String,
}

impl BlobHash {
    /// `hex` (any case) as a digest of `algo`.
    pub fn new(algo: BlobAlgo, hex: &str) -> Result<Self, Error> {
        let hex = hex.to_ascii_lowercase();
        let well_formed = hex.len() == algo.hex_len() && hex.bytes().all(|b| b.is_ascii_hexdigit());
        match well_formed {
            true => Ok(BlobHash { algo, hex }),
            false => Err(Error::BadBlobHash(format!("{}:{hex}", algo.name()))),
        }
    }

    /// `sha1:<hex>`, `sha256:<hex>` or `sha512:<hex>`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let bad = || Error::BadBlobHash(text.to_owned());
        let (algo, hex) = text.split_once(':').ok_or_else(bad)?;
        let algo = match algo {
            "sha1" => BlobAlgo::Sha1,
            "sha256" => BlobAlgo::Sha256,
            "sha512" => BlobAlgo::Sha512,
            _ => return Err(bad()),
        };
        BlobHash::new(algo, hex).map_err(|_| bad())
    }

    pub fn algo(&self) -> BlobAlgo {
        self.algo
    }

    /// The digest, lowercase hex.
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// The file a fetched blob is kept under in its destination directory.
    /// Its own name, not the caller's path: two entries with one content are
    /// one file, and a name off the wire never becomes a path.
    pub fn file_name(&self) -> String {
        format!("{}-{}", self.algo.name(), self.hex)
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo.name(), self.hex)
    }
}

impl TryFrom<String> for BlobHash {
    type Error = Error;
    fn try_from(text: String) -> Result<Self, Error> {
        BlobHash::parse(&text)
    }
}

impl From<BlobHash> for String {
    fn from(hash: BlobHash) -> String {
        hash.to_string()
    }
}

/// One file an application wants: its content hash and size, and the path
/// the application knows it by — carried back in the report for its own
/// bookkeeping, never used as a place to write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobEntry {
    pub path: String,
    pub hash: BlobHash,
    pub size: u64,
}

/// A blob that could not be fetched, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobFailure {
    pub hash: BlobHash,
    pub reason: String,
}

/// The shape every name here takes — id, process, action: lowercase ASCII
/// letters, digits and `-`, not starting with `-`, at most [`MAX_APP_ID`].
fn well_formed_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_APP_ID
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// An application's name on the mesh: what a machine agrees to take builds
/// of, and what `cursio logs --app` asks for. The same id on two machines is
/// the same application.
///
/// Lowercase ASCII letters, digits and `-`, at most [`MAX_APP_ID`]: it ends up
/// in directory names on every platform and in commands people type. `cursio`
/// is Cursio's own and cannot be taken.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AppId(String);

impl AppId {
    pub fn parse(id: &str) -> Result<Self, Error> {
        if !well_formed_name(id) {
            return Err(Error::BadAppId(id.to_owned()));
        }
        if id == "cursio" {
            return Err(Error::ReservedAppId);
        }
        Ok(AppId(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AppId {
    type Error = Error;
    fn try_from(id: String) -> Result<Self, Error> {
        AppId::parse(&id)
    }
}

impl From<AppId> for String {
    fn from(id: AppId) -> String {
        id.0
    }
}

/// One of an application's processes, when it runs several that each log —
/// a window and a daemon. Same characters as an [`AppId`]; absent is the
/// application's main process. Builds, `on_update` and starting it stay the
/// application's: one application, one build, whatever its processes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProcessName(String);

impl ProcessName {
    pub fn parse(name: &str) -> Result<Self, Error> {
        match well_formed_name(name) {
            true => Ok(ProcessName(name.to_owned())),
            false => Err(Error::BadProcessName(name.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProcessName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProcessName {
    type Error = Error;
    fn try_from(name: String) -> Result<Self, Error> {
        ProcessName::parse(&name)
    }
}

impl From<ProcessName> for String {
    fn from(name: ProcessName) -> String {
        name.0
    }
}

/// The name of something a running application lets another machine ask it
/// to do — `launch`, `stop`, `instances`. Same characters as an [`AppId`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionName(String);

impl ActionName {
    pub fn parse(name: &str) -> Result<Self, Error> {
        match well_formed_name(name) {
            true => Ok(ActionName(name.to_owned())),
            false => Err(Error::BadActionName(name.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ActionName {
    type Error = Error;
    fn try_from(name: String) -> Result<Self, Error> {
        ActionName::parse(&name)
    }
}

impl From<ActionName> for String {
    fn from(name: ActionName) -> String {
        name.0
    }
}

/// What a message between copies of an application is about — `instances`,
/// `sync` — and what a copy listens to. Same characters as an [`AppId`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TopicName(String);

impl TopicName {
    pub fn parse(name: &str) -> Result<Self, Error> {
        match well_formed_name(name) {
            true => Ok(TopicName(name.to_owned())),
            false => Err(Error::BadTopicName(name.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TopicName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for TopicName {
    type Error = Error;
    fn try_from(name: String) -> Result<Self, Error> {
        TopicName::parse(&name)
    }
}

impl From<TopicName> for String {
    fn from(name: TopicName) -> String {
        name.0
    }
}

/// A message's payload as a line carries it: base64, so any bytes cross a
/// JSON line.
pub fn encode_payload(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The bytes of a payload [`encode_payload`] wrote, at most [`MAX_PAYLOAD`].
pub fn decode_payload(text: &str) -> Result<Vec<u8>, Error> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|e| Error::Malformed(format!("the payload is not base64: {e}")))?;
    match bytes.len() > MAX_PAYLOAD {
        true => Err(Error::TooLarge(bytes.len())),
        false => Ok(bytes),
    }
}

/// Why a message between copies did not arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendFailure {
    /// The machine is not connected to this one now, or no machine has
    /// that name.
    Offline,
    /// The machine is there, but no copy of the application on it listens
    /// to the topic now.
    NotRunning,
    /// That machine's person does not let this one reach the application
    /// there, or has not answered yet.
    Refused,
    /// Anything else: no answer in time, a payload too large.
    Failed,
}

/// [`NodeMsg::Sent`]'s failure: which kind, and in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendError {
    pub kind: SendFailure,
    pub reason: String,
}

/// A machine where the application is registered, as [`NodeMsg::Peers`]
/// lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppPeer {
    pub machine: String,
    /// A copy is connected to that machine's node now.
    pub running: bool,
    pub version: String,
    /// Topics its running copies listen to.
    #[serde(default)]
    pub topics: Vec<TopicName>,
}

/// An action a process declares in its hello: what it is called, what it
/// does in a sentence, and a JSON Schema of its arguments — what an agent
/// reads to call it right the first time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSpec {
    pub name: ActionName,
    pub description: String,
    /// JSON Schema of the arguments; `null` for none.
    #[serde(default)]
    pub schema: serde_json::Value,
}

impl ActionSpec {
    /// Whether the node should take it: a description and a schema of a size
    /// that fits a line beside the rest of a hello.
    pub fn check(&self) -> Result<(), Error> {
        if self.description.len() > MAX_DETAIL {
            return Err(Error::BadAction(format!(
                "{}'s description is longer than {MAX_DETAIL} bytes",
                self.name
            )));
        }
        let schema = serde_json::to_vec(&self.schema).map_or(usize::MAX, |s| s.len());
        if schema > MAX_SCHEMA {
            return Err(Error::BadAction(format!(
                "{}'s schema is longer than {MAX_SCHEMA} bytes",
                self.name
            )));
        }
        Ok(())
    }
}

/// What a call to an action not in `declared` answers — the same words from
/// the SDK and from the node, naming what there is, so an agent that guessed
/// wrong learns the right name from the error itself.
pub fn no_such_action(app: &str, action: &str, declared: &[ActionName]) -> String {
    match declared {
        [] => format!("{app} declares no actions"),
        names => {
            // Sorted here, so the SDK and the node, holding them in
            // different orders, still say the same words.
            let mut names: Vec<&str> = names.iter().map(ActionName::as_str).collect();
            names.sort_unstable();
            format!("{app} has no action {action}; it has: {}", names.join(", "))
        }
    }
}

/// Where an application writes its log: the directory, and the file's base
/// name as its appender writes it (`leavepulse.log`). Daily-rotated siblings
/// (`leavepulse.log.2026-11-01`) are read with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSpec {
    pub dir: PathBuf,
    pub file: String,
}

impl LogSpec {
    /// An absolute directory and a bare file name. Only the application
    /// names these, over a socket only its user can open, and the node then
    /// reads only that file — but a relative directory would mean whatever
    /// the node's working directory is, and a file name with a separator in
    /// it would reach outside the directory the application named.
    pub fn check(&self) -> Result<(), Error> {
        if !self.dir.is_absolute() {
            return Err(Error::BadLogSpec(format!(
                "the log directory {} is not absolute",
                self.dir.display()
            )));
        }
        let bare = !self.file.is_empty()
            && self.file != "."
            && self.file != ".."
            && !self.file.contains(['/', '\\'])
            && Path::new(&self.file).file_name().is_some();
        if !bare {
            return Err(Error::BadLogSpec(format!(
                "{:?} is not a bare file name",
                self.file
            )));
        }
        Ok(())
    }
}

/// How the application says it is doing, in its own judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    #[default]
    Ok,
    /// Working, but something it relies on is not.
    Degraded,
    /// Not doing its job.
    Failing,
}

/// What an application says to the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ClientMsg {
    /// The first line on every connection.
    Hello {
        protocol: u32,
        app: AppId,
        version: String,
        pid: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        logs: Option<LogSpec>,
        /// Which of the application's processes this is; absent is its main
        /// one. Added after protocol 1 shipped: left out when absent, so an
        /// old node reads every hello it read before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process: Option<ProcessName>,
        /// What this process lets another machine ask it to do. Left out
        /// when empty, so a hello without actions has the bytes it had.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        actions: Vec<ActionSpec>,
        /// Topics of messages from its copies on other machines this
        /// process listens to. Left out when empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        topics: Vec<TopicName>,
        /// This process answers [`NodeMsg::BlobPath`]: its copies on other
        /// machines may fetch files from it. Left out when false.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        blobs: bool,
    },
    Health {
        state: HealthState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// The answer to [`NodeMsg::Call`] `id`: `error` when it failed, else
    /// `ok` (absent for an action that returns nothing).
    Answer {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Send `payload` (base64, at most [`MAX_PAYLOAD`] bytes) about `topic`
    /// to the copies of this application on machine `to`; answered by
    /// [`NodeMsg::Sent`] with the same `id`.
    Send {
        id: u64,
        to: String,
        topic: TopicName,
        payload: String,
    },
    /// Which machines this application is registered on; answered by
    /// [`NodeMsg::Peers`] with the same `id`.
    Peers { id: u64 },
    /// The answer to [`NodeMsg::BlobPath`] `id`: the file holding that blob,
    /// or nothing when this process does not have it.
    BlobPath {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    /// Fetch `entries` from this application's copies on other machines into
    /// the absolute directory `dest`, each under [`BlobHash::file_name`];
    /// progress comes as [`NodeMsg::Fetching`], the outcome as
    /// [`NodeMsg::Fetched`], both with the same `id`.
    Fetch {
        id: u64,
        entries: Vec<BlobEntry>,
        dest: PathBuf,
    },
}

impl ClientMsg {
    /// The answer to call `id` carrying `result`.
    pub fn answer(id: u64, result: Result<serde_json::Value, String>) -> Self {
        match result {
            Ok(value) => ClientMsg::Answer {
                id,
                ok: Some(value),
                error: None,
            },
            Err(error) => ClientMsg::Answer {
                id,
                ok: None,
                error: Some(error),
            },
        }
    }
}

/// What the node says to an application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum NodeMsg {
    /// The answer to `Hello`: the version both sides speak from now on, and
    /// the name of this machine on the mesh.
    Welcome { protocol: u32, machine: String },
    /// The node will not take this application; the connection closes.
    Refused { reason: String },
    /// The node does not know the message named `feature`; the connection
    /// stays open.
    Unsupported { feature: String },
    /// A new build of this application came over the mesh and is installed
    /// in `dir`, which its install root's `current` now names. What runs is
    /// the application's to restart; nothing has been stopped.
    BuildReady { version: String, dir: PathBuf },
    /// Do the declared `action` with `args`; answer with
    /// [`ClientMsg::Answer`] carrying the same `id`.
    Call {
        id: u64,
        action: ActionName,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// What became of [`ClientMsg::Send`] `id`: absent `error` is delivered.
    Sent {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<SendError>,
    },
    /// The answer to [`ClientMsg::Peers`] `id`.
    Peers { id: u64, peers: Vec<AppPeer> },
    /// A message from a copy of this application on machine `from` (its
    /// process `from_process`, absent for the main one) about `topic`, which
    /// this process listens to. `payload` is base64.
    Message {
        from: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_process: Option<ProcessName>,
        topic: TopicName,
        payload: String,
    },
    /// Which file holds blob `hash`, if this process has it; answer with
    /// [`ClientMsg::BlobPath`] carrying the same `id`. The node reads only a
    /// path the application named here.
    BlobPath { id: u64, hash: BlobHash },
    /// Fetch `id` has `received` of blob `hash`'s `size` bytes.
    Fetching {
        id: u64,
        hash: BlobHash,
        received: u64,
        size: u64,
    },
    /// What became of [`ClientMsg::Fetch`] `id`: blobs now in its
    /// destination, blobs no connected copy has, and blobs that failed.
    Fetched {
        id: u64,
        fetched: Vec<BlobHash>,
        not_found: Vec<BlobHash>,
        failed: Vec<BlobFailure>,
    },
}

/// Something wrong with a message or a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    BadAppId(String),
    ReservedAppId,
    BadProcessName(String),
    BadActionName(String),
    BadAction(String),
    BadTopicName(String),
    BadBlobHash(String),
    /// A message's payload of this many bytes is over [`MAX_PAYLOAD`].
    TooLarge(usize),
    BadLogSpec(String),
    TooLong,
    Malformed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadAppId(id) => write!(
                f,
                "{id:?} is not an application id; use up to {MAX_APP_ID} lowercase letters, digits and `-`"
            ),
            Error::ReservedAppId => f.write_str("`cursio` is Cursio's own id; pick another"),
            Error::BadProcessName(name) => write!(
                f,
                "{name:?} is not a process name; use up to {MAX_APP_ID} lowercase letters, digits and `-`"
            ),
            Error::BadActionName(name) => write!(
                f,
                "{name:?} is not an action name; use up to {MAX_APP_ID} lowercase letters, digits and `-`"
            ),
            Error::BadAction(why) => f.write_str(why),
            Error::BadTopicName(name) => write!(
                f,
                "{name:?} is not a topic; use up to {MAX_APP_ID} lowercase letters, digits and `-`"
            ),
            Error::BadBlobHash(text) => write!(
                f,
                "{text:?} is not a blob hash; write sha1:, sha256: or sha512: and the digest in hex"
            ),
            Error::TooLarge(bytes) => write!(
                f,
                "a message carries at most {MAX_PAYLOAD} bytes, not {bytes}; send a build or a file for more"
            ),
            Error::BadLogSpec(why) => f.write_str(why),
            Error::TooLong => write!(f, "a line is longer than {MAX_LINE} bytes"),
            Error::Malformed(why) => write!(f, "not a message: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// One message as a line, newline included.
pub fn encode_line<T: Serialize>(message: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(message).expect("protocol messages always serialize");
    line.push(b'\n');
    line
}

/// One line, without its newline, as a message.
pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Result<T, Error> {
    if line.len() > MAX_LINE {
        return Err(Error::TooLong);
    }
    serde_json::from_slice(line).map_err(|e| Error::Malformed(e.to_string()))
}

/// The `msg` a line names, when it is an object that names one: what a side
/// that cannot decode the line reports as [`NodeMsg::Unsupported`].
pub fn message_kind(line: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    value.get("msg")?.as_str().map(str::to_owned)
}

/// Where the node listens for applications of this user.
///
/// Unix: [`SOCKET_NAME`] in `$XDG_RUNTIME_DIR/cursio`, or in
/// `<temp>/cursio-<uid>` when that variable is unset — the directory the
/// node makes private to its user. Windows: `\\.\pipe\cursio-apps-<user>`.
/// Android: the abstract socket [`ABSTRACT_NAME`], written `@cursio-apps`.
pub fn endpoint() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        PathBuf::from(format!("@{ABSTRACT_NAME}"))
    }
    #[cfg(all(unix, not(target_os = "android")))]
    {
        runtime_dir().join(SOCKET_NAME)
    }
    #[cfg(not(unix))]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
        PathBuf::from(format!(r"\\.\pipe\cursio-apps-{}", pipe_safe(&user)))
    }
}

/// The abstract socket name an endpoint written `@name` stands for, or `None`
/// for a path. Abstract sockets exist on Linux and Android only.
pub fn abstract_name(endpoint: &std::path::Path) -> Option<&str> {
    endpoint
        .to_str()?
        .strip_prefix('@')
        .filter(|name| !name.is_empty())
}

/// The node's private runtime directory, as [`endpoint`] resolves it.
#[cfg(unix)]
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("cursio"),
        _ => {
            // SAFETY: getuid cannot fail and touches no memory.
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("cursio-{uid}"))
        }
    }
}

/// A user name as it may appear in a pipe name: anything but letters,
/// digits, `-` and `_` becomes `_`.
#[cfg_attr(unix, allow(dead_code))]
fn pipe_safe(user: &str) -> String {
    user.chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                true => c,
                false => '_',
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<T: Serialize>(message: &T) -> String {
        String::from_utf8(encode_line(message)).unwrap()
    }

    #[test]
    fn a_blob_is_named_by_its_algorithm_and_digest_and_nothing_else() {
        let sha1 = "a".repeat(40);
        let hash = BlobHash::parse(&format!("sha1:{}", sha1.to_uppercase())).unwrap();
        assert_eq!(hash.to_string(), format!("sha1:{sha1}"), "kept lowercase");
        assert_eq!(hash.file_name(), format!("sha1-{sha1}"));
        assert!(BlobHash::parse(&format!("sha1:{}", "a".repeat(39))).is_err());
        assert!(BlobHash::parse(&format!("md5:{}", "a".repeat(32))).is_err());
        assert!(BlobHash::parse(&format!("sha256:{}", "g".repeat(64))).is_err());
        assert!(BlobHash::parse("sha1:../../etc/passwd").is_err());
        assert!(BlobHash::new(BlobAlgo::Sha512, &"0".repeat(128)).is_ok());
    }

    #[test]
    fn blob_messages_have_the_bytes_both_sides_read() {
        let hash = BlobHash::new(BlobAlgo::Sha1, &"b".repeat(40)).unwrap();
        let fetch = ClientMsg::Fetch {
            id: 3,
            entries: vec![BlobEntry {
                path: "mods/a.jar".into(),
                hash: hash.clone(),
                size: 10,
            }],
            dest: PathBuf::from("/tmp/blobs"),
        };
        let text = line(&fetch);
        assert_eq!(
            text,
            format!(
                "{{\"msg\":\"fetch\",\"id\":3,\"entries\":[{{\"path\":\"mods/a.jar\",\"hash\":\"sha1:{}\",\"size\":10}}],\"dest\":\"/tmp/blobs\"}}\n",
                "b".repeat(40)
            )
        );
        assert_eq!(
            decode_line::<ClientMsg>(text.trim_end().as_bytes()).unwrap(),
            fetch
        );
        assert_eq!(
            line(&ClientMsg::BlobPath { id: 4, path: None }),
            "{\"msg\":\"blob_path\",\"id\":4}\n"
        );
        let fetched = NodeMsg::Fetched {
            id: 3,
            fetched: vec![hash.clone()],
            not_found: Vec::new(),
            failed: vec![BlobFailure {
                hash,
                reason: "corrupt".into(),
            }],
        };
        let text = line(&fetched);
        assert_eq!(
            decode_line::<NodeMsg>(text.trim_end().as_bytes()).unwrap(),
            fetched
        );
        assert!(decode_line::<ClientMsg>(
            br#"{"msg":"fetch","id":1,"entries":[{"path":"x","hash":"sha1:nope","size":1}],"dest":"/d"}"#
        )
        .is_err(), "a malformed hash is refused at the door");
    }

    #[test]
    fn hello_has_the_bytes_old_nodes_read() {
        let hello = ClientMsg::Hello {
            protocol: 1,
            app: AppId::parse("leavepulse").unwrap(),
            version: "0.6.0".into(),
            pid: 42,
            logs: Some(LogSpec {
                dir: "/home/u/.local/share/leavepulse/logs".into(),
                file: "leavepulse.log".into(),
            }),
            process: None,
            actions: Vec::new(),
            topics: Vec::new(),
            blobs: false,
        };
        let text = line(&hello);
        assert_eq!(
            text,
            "{\"msg\":\"hello\",\"protocol\":1,\"app\":\"leavepulse\",\"version\":\"0.6.0\",\"pid\":42,\"logs\":{\"dir\":\"/home/u/.local/share/leavepulse/logs\",\"file\":\"leavepulse.log\"}}\n"
        );
        assert_eq!(
            decode_line::<ClientMsg>(text.trim_end().as_bytes()).unwrap(),
            hello
        );
    }

    #[test]
    fn hello_without_logs_leaves_the_field_out() {
        let hello = ClientMsg::Hello {
            protocol: 1,
            app: AppId::parse("demo").unwrap(),
            version: "1".into(),
            pid: 7,
            logs: None,
            process: None,
            actions: Vec::new(),
            topics: Vec::new(),
            blobs: false,
        };
        assert_eq!(
            line(&hello),
            "{\"msg\":\"hello\",\"protocol\":1,\"app\":\"demo\",\"version\":\"1\",\"pid\":7}\n"
        );
    }

    #[test]
    fn a_second_process_of_an_application_names_itself_in_its_hello() {
        let hello = ClientMsg::Hello {
            protocol: 1,
            app: AppId::parse("leavepulse").unwrap(),
            version: "0.6.0".into(),
            pid: 9,
            logs: None,
            process: Some(ProcessName::parse("daemon").unwrap()),
            actions: Vec::new(),
            topics: Vec::new(),
            blobs: false,
        };
        let text = line(&hello);
        assert_eq!(
            text,
            "{\"msg\":\"hello\",\"protocol\":1,\"app\":\"leavepulse\",\"version\":\"0.6.0\",\"pid\":9,\"process\":\"daemon\"}\n"
        );
        assert_eq!(
            decode_line::<ClientMsg>(text.trim_end().as_bytes()).unwrap(),
            hello
        );
        assert!(ProcessName::parse("../x").is_err());
        assert!(decode_line::<ClientMsg>(
            br#"{"msg":"hello","protocol":1,"app":"a","version":"1","pid":1,"process":"Bad Name"}"#
        )
        .is_err());
    }

    #[test]
    fn messages_between_copies_have_pinned_bytes() {
        let hello = ClientMsg::Hello {
            protocol: 1,
            app: AppId::parse("leavepulse").unwrap(),
            version: "1".into(),
            pid: 4,
            logs: None,
            process: None,
            actions: Vec::new(),
            topics: vec![TopicName::parse("instances").unwrap()],
            blobs: false,
        };
        assert_eq!(
            line(&hello),
            "{\"msg\":\"hello\",\"protocol\":1,\"app\":\"leavepulse\",\"version\":\"1\",\"pid\":4,\"topics\":[\"instances\"]}\n"
        );
        let send = ClientMsg::Send {
            id: 2,
            to: "Lightmoon".into(),
            topic: TopicName::parse("instances").unwrap(),
            payload: encode_payload(b"hi"),
        };
        assert_eq!(
            line(&send),
            "{\"msg\":\"send\",\"id\":2,\"to\":\"Lightmoon\",\"topic\":\"instances\",\"payload\":\"aGk=\"}\n"
        );
        assert_eq!(
            line(&ClientMsg::Peers { id: 3 }),
            "{\"msg\":\"peers\",\"id\":3}\n"
        );
        assert_eq!(
            line(&NodeMsg::Sent { id: 2, error: None }),
            "{\"msg\":\"sent\",\"id\":2}\n"
        );
        assert_eq!(
            line(&NodeMsg::Sent {
                id: 2,
                error: Some(SendError {
                    kind: SendFailure::NotRunning,
                    reason: "no".into()
                })
            }),
            "{\"msg\":\"sent\",\"id\":2,\"error\":{\"kind\":\"not_running\",\"reason\":\"no\"}}\n"
        );
        assert_eq!(
            line(&NodeMsg::Message {
                from: "Nocturne".into(),
                from_process: None,
                topic: TopicName::parse("instances").unwrap(),
                payload: encode_payload(b"hi"),
            }),
            "{\"msg\":\"message\",\"from\":\"Nocturne\",\"topic\":\"instances\",\"payload\":\"aGk=\"}\n"
        );
        assert_eq!(decode_payload("aGk=").unwrap(), b"hi");
        assert!(matches!(
            decode_payload(&encode_payload(&vec![0; MAX_PAYLOAD + 1])),
            Err(Error::TooLarge(_))
        ));
        assert!(TopicName::parse("Bad Topic").is_err());
    }

    #[test]
    fn a_hello_declares_its_actions_and_a_call_and_its_answer_have_pinned_bytes() {
        let hello = ClientMsg::Hello {
            protocol: 1,
            app: AppId::parse("leavepulse").unwrap(),
            version: "0.6.0".into(),
            pid: 3,
            logs: None,
            process: None,
            actions: vec![ActionSpec {
                name: ActionName::parse("stop").unwrap(),
                description: "Stop an instance".into(),
                schema: serde_json::json!({"type": "object"}),
            }],
            topics: Vec::new(),
            blobs: false,
        };
        let text = line(&hello);
        assert_eq!(
            text,
            "{\"msg\":\"hello\",\"protocol\":1,\"app\":\"leavepulse\",\"version\":\"0.6.0\",\"pid\":3,\"actions\":[{\"name\":\"stop\",\"description\":\"Stop an instance\",\"schema\":{\"type\":\"object\"}}]}\n"
        );
        assert_eq!(
            decode_line::<ClientMsg>(text.trim_end().as_bytes()).unwrap(),
            hello
        );

        let call = NodeMsg::Call {
            id: 7,
            action: ActionName::parse("stop").unwrap(),
            args: serde_json::json!({"instance": "vanilla"}),
        };
        assert_eq!(
            line(&call),
            "{\"msg\":\"call\",\"id\":7,\"action\":\"stop\",\"args\":{\"instance\":\"vanilla\"}}\n"
        );
        assert_eq!(
            line(&ClientMsg::answer(7, Ok(serde_json::json!(true)))),
            "{\"msg\":\"answer\",\"id\":7,\"ok\":true}\n"
        );
        assert_eq!(
            line(&ClientMsg::answer(7, Err("no such instance".into()))),
            "{\"msg\":\"answer\",\"id\":7,\"error\":\"no such instance\"}\n"
        );
    }

    #[test]
    fn an_action_too_big_to_declare_is_refused_and_a_wrong_name_lists_the_right_ones() {
        let huge = ActionSpec {
            name: ActionName::parse("launch").unwrap(),
            description: "x".repeat(MAX_DETAIL + 1),
            schema: serde_json::Value::Null,
        };
        assert!(huge.check().is_err());
        assert!(ActionName::parse("Launch!").is_err());
        let declared = [
            ActionName::parse("instances").unwrap(),
            ActionName::parse("launch").unwrap(),
        ];
        assert_eq!(
            no_such_action("leavepulse", "foo", &declared),
            "leavepulse has no action foo; it has: instances, launch"
        );
        assert_eq!(
            no_such_action("leavepulse", "foo", &[]),
            "leavepulse declares no actions"
        );
    }

    #[test]
    fn health_has_the_bytes_old_nodes_read() {
        let health = ClientMsg::Health {
            state: HealthState::Degraded,
            detail: Some("no api".into()),
        };
        assert_eq!(
            line(&health),
            "{\"msg\":\"health\",\"state\":\"degraded\",\"detail\":\"no api\"}\n"
        );
    }

    #[test]
    fn node_messages_have_the_bytes_old_sdks_read() {
        assert_eq!(
            line(&NodeMsg::Welcome {
                protocol: 1,
                machine: "Nocturne".into()
            }),
            "{\"msg\":\"welcome\",\"protocol\":1,\"machine\":\"Nocturne\"}\n"
        );
        assert_eq!(
            line(&NodeMsg::Refused {
                reason: "no".into()
            }),
            "{\"msg\":\"refused\",\"reason\":\"no\"}\n"
        );
        assert_eq!(
            line(&NodeMsg::Unsupported {
                feature: "call".into()
            }),
            "{\"msg\":\"unsupported\",\"feature\":\"call\"}\n"
        );
        assert_eq!(
            line(&NodeMsg::BuildReady {
                version: "0.6.1".into(),
                dir: PathBuf::from("/apps/leavepulse/versions/0.6.1"),
            }),
            "{\"msg\":\"build_ready\",\"version\":\"0.6.1\",\"dir\":\"/apps/leavepulse/versions/0.6.1\"}\n"
        );
    }

    #[test]
    fn an_at_sign_names_an_abstract_socket_and_anything_else_a_path() {
        use std::path::Path;
        assert_eq!(
            abstract_name(Path::new("@cursio-apps")),
            Some(ABSTRACT_NAME)
        );
        assert_eq!(
            abstract_name(Path::new("/run/user/1000/cursio/apps.sock")),
            None
        );
        assert_eq!(abstract_name(Path::new("@")), None);
        if cfg!(target_os = "android") {
            assert_eq!(abstract_name(&endpoint()), Some(ABSTRACT_NAME));
        } else {
            assert_eq!(abstract_name(&endpoint()), None);
        }
    }

    #[test]
    fn an_app_id_is_a_short_lowercase_name_and_never_cursio() {
        assert!(AppId::parse("leavepulse").is_ok());
        assert!(AppId::parse("my-app-2").is_ok());
        assert!(AppId::parse("").is_err());
        assert!(AppId::parse("LeavePulse").is_err());
        assert!(AppId::parse("../etc").is_err());
        assert!(AppId::parse("-flag").is_err());
        assert!(AppId::parse(&"a".repeat(33)).is_err());
        assert_eq!(AppId::parse("cursio"), Err(Error::ReservedAppId));
    }

    #[test]
    fn an_invalid_app_id_on_the_wire_does_not_decode() {
        let line = br#"{"msg":"hello","protocol":1,"app":"../x","version":"1","pid":1}"#;
        assert!(decode_line::<ClientMsg>(line).is_err());
    }

    #[test]
    fn a_log_spec_names_one_file_in_an_absolute_directory() {
        let spec = |dir: &str, file: &str| LogSpec {
            dir: dir.into(),
            file: file.into(),
        };
        // Absolute as each platform reads it: `/var/log` is relative to the
        // current drive on Windows.
        let abs = if cfg!(windows) {
            r"C:\logs\app"
        } else {
            "/var/log/app"
        };
        assert!(spec(abs, "app.log").check().is_ok());
        assert!(spec("logs", "app.log").check().is_err());
        assert!(spec(abs, "../secret").check().is_err());
        assert!(spec(abs, "a\\b").check().is_err());
        assert!(spec(abs, "..").check().is_err());
        assert!(spec(abs, "").check().is_err());
    }

    #[test]
    fn an_unknown_message_still_says_what_it_is() {
        let line = br#"{"msg":"call","name":"reload"}"#;
        assert!(decode_line::<ClientMsg>(line).is_err());
        assert_eq!(message_kind(line).as_deref(), Some("call"));
    }

    #[test]
    fn an_overlong_line_is_refused_before_parsing() {
        let line = vec![b' '; MAX_LINE + 1];
        assert_eq!(decode_line::<ClientMsg>(&line), Err(Error::TooLong));
    }

    #[test]
    fn a_user_name_is_made_safe_for_a_pipe_name() {
        assert_eq!(pipe_safe("Иван Smith"), "_____Smith");
        assert_eq!(pipe_safe("the-roer_1"), "the-roer_1");
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_lives_in_the_nodes_private_runtime_directory() {
        assert_eq!(endpoint(), runtime_dir().join("apps.sock"));
    }
}
