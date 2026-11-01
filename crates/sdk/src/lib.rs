//! Put an application on a Cursio mesh.
//!
//! ```no_run
//! let cursio = cursio_sdk::App::new("leavepulse")
//!     .version(env!("CARGO_PKG_VERSION"))
//!     .logs("/home/me/.local/share/leavepulse/logs", "leavepulse.log")
//!     .start()
//!     .expect("a valid application id");
//! // From any machine of the mesh: `cursio logs Nocturne --app leavepulse -f`.
//! # let _ = cursio;
//! ```
//!
//! The application never joins the mesh itself. It tells the Cursio node on
//! its own machine who it is and where it logs, and the node answers for it
//! to the rest of the mesh; see `docs/app-sdk-design.md` in Cursio.
//!
//! ## Cursio being absent is ordinary
//!
//! A program must start and run exactly as it did before it linked this, on
//! a machine where Cursio is not installed, not running yet, or restarting
//! into an update. So [`App::start`] fails only for a mistake in the program
//! itself (an invalid id or log path), never because no node answered: the
//! connection lives on its own thread with its own runtime — the program
//! need not run Tokio, and a program that does is not borrowed from —
//! connecting, and connecting again after the node goes away, with a backoff.
//! Calls made while no node is connected return [`Error::NotRunning`] and are
//! remembered, so the node hears the latest state as soon as it is back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_app_protocol::{
    decode_line, encode_line, no_such_action, ClientMsg, NodeMsg, SendFailure, MAX_ACTIONS,
    MAX_LINE, MAX_PAYLOAD, MAX_TOPICS, PROTOCOL,
};
pub use cursio_app_protocol::{
    ActionName, ActionSpec, AppId, BlobAlgo, BlobEntry, BlobHash, Devtools, HealthState, LogSpec,
    ProcessName, TopicName,
};
/// The JSON an action takes and answers.
pub use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

mod log;
#[cfg(feature = "tracing")]
pub use log::CursioLayer;
pub use log::LogLevel;
use log::LogShared;

/// First wait before connecting again.
const RETRY_MIN: Duration = Duration::from_millis(500);
/// Longest wait between attempts, and the wait after the node refused.
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How long the node has to answer `Hello`.
const WELCOME_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The id or the log location is not one the protocol carries.
    Invalid(cursio_app_protocol::Error),
    /// No Cursio node is connected right now; what was asked is kept and
    /// sent once one is.
    NotRunning,
    /// The node would not take this application, and said why.
    Refused(String),
    /// The connection's thread could not be started.
    Thread(String),
    /// A message's machine is not connected to this one now.
    Offline { machine: String },
    /// The machine is there, but no copy of this application on it listens
    /// to the message's topic now.
    CopyNotRunning { machine: String },
    /// That machine's person does not let this machine reach the application
    /// there (`[apps.<id>] actions`), or has not answered yet.
    NotAllowed { machine: String, reason: String },
    /// A message could not be carried for another reason, said here.
    Send(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(e) => e.fmt(f),
            Error::NotRunning => f.write_str("no Cursio node is running on this machine"),
            Error::Refused(why) => write!(f, "the Cursio node refused this application: {why}"),
            Error::Thread(why) => write!(f, "cannot start the Cursio connection: {why}"),
            Error::Offline { machine } => write!(f, "{machine} is not connected to this machine"),
            Error::CopyNotRunning { machine } => write!(
                f,
                "no copy of this application on {machine} listens to that topic now"
            ),
            Error::NotAllowed { machine, reason } => write!(f, "{machine} refused: {reason}"),
            Error::Send(why) => write!(f, "the message did not arrive: {why}"),
        }
    }
}

/// A message from a copy of this application on another machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The machine it came from, as the mesh names it.
    pub from_machine: String,
    /// Which process of the application there sent it; `None` is its main one.
    pub from_process: Option<ProcessName>,
    pub topic: TopicName,
    pub payload: Vec<u8>,
}

/// A machine of the mesh where this application is registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub machine: String,
    /// A copy is connected to that machine's Cursio node now.
    pub running: bool,
    pub version: String,
    /// Topics its running copies listen to.
    pub topics: Vec<TopicName>,
}

type OnMessage = Arc<dyn Fn(Message) + Send + Sync>;

/// One machine's outcome of [`Cursio::send_all`]: its name, and whether its
/// copies took the message.
pub type SentTo = (String, Result<(), Error>);

/// How long [`Cursio::send`] waits for the other machine to say what became
/// of a message: a little over the node's own wait, so the node's answer —
/// which says why — arrives first.
const SEND_WAIT: Duration = Duration::from_secs(35);
/// How long [`Cursio::peers`] waits; the node answers from what it holds.
const PEERS_WAIT: Duration = Duration::from_secs(5);
/// How long a fetch may go with no word from the node — progress or the
/// end — before it is given up: a node asking a slow machine answers within
/// half a minute, and progress comes every quarter second while bytes move.
pub const FETCH_QUIET: Duration = Duration::from_secs(120);

/// A request to the node that is answered by id, and where its answer goes.
struct Request {
    msg: ClientMsg,
    id: u64,
    reply: std::sync::mpsc::Sender<NodeMsg>,
}

impl std::error::Error for Error {}

/// A new build of the application, installed by the node and named by its
/// install root's `current`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub version: String,
    /// The directory holding its files.
    pub dir: PathBuf,
}

type OnUpdate = Arc<dyn Fn(Build) + Send + Sync>;

/// Which file holds a blob, as the application names it.
type OnBlob = Arc<dyn Fn(&BlobHash) -> Option<PathBuf> + Send + Sync>;

/// How far one blob of a [`Cursio::fetch`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub hash: BlobHash,
    pub received: u64,
    pub size: u64,
}

/// What became of a [`Cursio::fetch`], by the entries it was given: each
/// fetched one is in the destination under [`BlobHash::file_name`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchReport {
    pub fetched: Vec<BlobEntry>,
    /// No connected copy holds these; where else to get them is the
    /// application's to know.
    pub not_found: Vec<BlobEntry>,
    pub failed: Vec<(BlobEntry, String)>,
}

/// What an action does with its arguments: a JSON answer, or why not.
type Handler = Arc<dyn Fn(serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync>;

/// An action as the application declared it, checked at `start`.
#[derive(Clone)]
struct Declared {
    name: Result<ActionName, cursio_app_protocol::Error>,
    /// The name as the program wrote it, for saying which one was refused.
    asked: String,
    description: String,
    schema: serde_json::Value,
    handler: Handler,
}

/// What the connection does with what the node sends unasked.
#[derive(Clone, Default)]
struct Hooks {
    app: String,
    on_update: Option<OnUpdate>,
    actions: Arc<BTreeMap<ActionName, Handler>>,
    messages: Arc<BTreeMap<TopicName, OnMessage>>,
    on_blob: Option<OnBlob>,
}

/// An application about to introduce itself to the node.
#[derive(Clone)]
pub struct App {
    id: Result<AppId, cursio_app_protocol::Error>,
    version: String,
    logs: Option<LogSpec>,
    endpoint: Option<PathBuf>,
    on_update: Option<OnUpdate>,
    process: Option<(Result<ProcessName, cursio_app_protocol::Error>, String)>,
    actions: Vec<Declared>,
    topics: Vec<(
        Result<TopicName, cursio_app_protocol::Error>,
        String,
        OnMessage,
    )>,
    on_blob: Option<OnBlob>,
    devtools: Option<Devtools>,
}

/// In a debug build, a name that cannot be declared panics where it is
/// written; in a release build [`App::start`] skips it ([`reject`]).
fn check_now<T>(kind: RejectedKind, name: &str, parsed: &Result<T, cursio_app_protocol::Error>) {
    if let (true, Err(error)) = (cfg!(debug_assertions), parsed) {
        reject(kind, name, &error.to_string());
    }
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("logs", &self.logs)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl App {
    /// `id` is the application's name on every machine: lowercase letters,
    /// digits and `-`.
    pub fn new(id: &str) -> Self {
        App {
            id: AppId::parse(id),
            version: String::new(),
            logs: None,
            endpoint: None,
            on_update: None,
            process: None,
            actions: Vec::new(),
            topics: Vec::new(),
            on_blob: None,
            devtools: None,
        }
    }

    /// Let this application's copies on other machines fetch files from
    /// this one by content hash ([`Cursio::fetch`] there): `handler` names
    /// the file holding a hash, or `None`. The node reads only a file named
    /// here, and gives it only to a machine this machine's person lets drive
    /// the application. Runs off the connection's thread.
    pub fn on_blob(
        mut self,
        handler: impl Fn(&BlobHash) -> Option<PathBuf> + Send + Sync + 'static,
    ) -> Self {
        self.on_blob = Some(Arc::new(handler));
        self
    }

    /// This process opened a DevTools endpoint on loopback — for a Chromium
    /// web view, `--remote-debugging-port=<port>` — which another machine may
    /// inspect through Cursio (`cursio devtools`, `cursio mcp`'s `web_*`).
    ///
    /// DevTools run any code inside the application and read what it
    /// stores, so declare it only in a build or a run meant for it (behind
    /// a `--devtools` flag, never by default). The node forwards only a port
    /// this very process listens on, and only to a machine this machine's
    /// person allows for a limited time each time.
    pub fn devtools(mut self, endpoint: Devtools) -> Self {
        self.devtools = Some(endpoint);
        self
    }

    /// Hear messages about `topic` from copies of this application on other
    /// machines ([`Cursio::send`] there). `handler` runs on a thread of its
    /// own, never the connection's; one that panics loses that message only.
    ///
    /// A message arrives only from a machine this machine's person lets drive
    /// the application — the same answer as for its actions.
    pub fn on_message(
        mut self,
        topic: &str,
        handler: impl Fn(Message) + Send + Sync + 'static,
    ) -> Self {
        let parsed = TopicName::parse(topic);
        check_now(RejectedKind::Topic, topic, &parsed);
        self.topics
            .push((parsed, topic.to_owned(), Arc::new(handler)));
        self
    }

    /// Let another machine of the mesh ask this process to do `name`:
    /// `cursio apps call <machine> <id> <name> '<json>'`, or an agent through
    /// `cursio mcp`. `description` is one sentence of what it does, `schema`
    /// a JSON Schema of its arguments (`serde_json::Value::Null` for none) —
    /// what a caller reads to call it right the first time.
    ///
    /// `handler` gets the arguments and answers JSON or why not. It runs on
    /// a thread of its own, never the connection's, so a slow one holds up
    /// nothing else; one that panics answers that it did, and later calls
    /// are served as before. Call what the application's own buttons call,
    /// and what an agent does is what a person would see.
    pub fn action(
        mut self,
        name: &str,
        description: impl Into<String>,
        schema: serde_json::Value,
        handler: impl Fn(serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync + 'static,
    ) -> Self {
        let parsed = ActionName::parse(name);
        check_now(RejectedKind::Action, name, &parsed);
        self.actions.push(Declared {
            name: parsed,
            asked: name.to_owned(),
            description: description.into(),
            schema,
            handler: Arc::new(handler),
        });
        self
    }

    /// This is one of the application's several processes — `daemon` beside
    /// the window — each registering under the same id with its own log.
    /// `cursio logs <machine> --app <id>` merges them all, each line marked
    /// with its process; `--app <id>/<process>` reads one. Leave it unset for
    /// the main process.
    pub fn process(mut self, name: &str) -> Self {
        let parsed = ProcessName::parse(name);
        check_now(RejectedKind::Process, name, &parsed);
        self.process = Some((parsed, name.to_owned()));
        self
    }

    /// Call `handler` when a new build of this application arrives over the
    /// mesh. It runs on the SDK's own thread; nothing has been stopped, and
    /// when to restart into the build is the application's choice.
    pub fn on_update(mut self, handler: impl Fn(Build) + Send + Sync + 'static) -> Self {
        self.on_update = Some(Arc::new(handler));
        self
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Where the application writes its log: an absolute directory, and the
    /// file's name as the appender writes it. Daily-rotated files beside it
    /// (`app.log.2026-11-01`) are read with it.
    pub fn logs(mut self, dir: impl AsRef<Path>, file: impl Into<String>) -> Self {
        self.logs = Some(LogSpec {
            dir: dir.as_ref().to_path_buf(),
            file: file.into(),
        });
        self
    }

    /// Reach a node somewhere other than its usual socket — a test node.
    pub fn endpoint(mut self, endpoint: impl Into<PathBuf>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Start talking to the node, in the background. Fails only when the id
    /// or the log location is invalid. An invalid action, topic or process
    /// name panics in a debug build; a release build leaves it out, registers
    /// everything else, and lists it in [`Cursio::rejected`].
    pub fn start(self) -> Result<Cursio, Error> {
        let id = self.id.map_err(Error::Invalid)?;
        if let Some(logs) = &self.logs {
            logs.check().map_err(Error::Invalid)?;
        }
        let mut rejected = Vec::new();
        let process = match self.process {
            Some((Ok(process), _)) => Some(process),
            Some((Err(error), asked)) => {
                rejected.push(reject(RejectedKind::Process, &asked, &error.to_string()));
                None
            }
            None => None,
        };
        let (specs, handlers, skipped_actions) = declared(self.actions, reject);
        let (topics, listeners, skipped_topics) = listened(self.topics, reject);
        rejected.extend(skipped_actions);
        rejected.extend(skipped_topics);
        let hooks = Hooks {
            app: id.to_string(),
            on_update: self.on_update,
            actions: Arc::new(handlers),
            messages: Arc::new(listeners),
            on_blob: self.on_blob.clone(),
        };
        let hello = ClientMsg::Hello {
            protocol: PROTOCOL,
            app: id,
            version: self.version,
            pid: std::process::id(),
            logs: self.logs,
            process,
            actions: specs,
            topics,
            blobs: self.on_blob.is_some(),
            devtools: self.devtools,
        };
        let endpoint = self.endpoint.unwrap_or_else(cursio_app_protocol::endpoint);
        let shared = Arc::new(Mutex::new(Link::Connecting));
        let (health, health_rx) = watch::channel((HealthState::Ok, None));
        let (requests, requests_rx) = mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Thread(e.to_string()))?;
        let link = shared.clone();
        let logs = Arc::new(LogShared::default());
        let queued = logs.clone();
        std::thread::Builder::new()
            .name("cursio-sdk".into())
            .spawn(move || {
                runtime.block_on(run(
                    endpoint,
                    hello,
                    link,
                    health_rx,
                    requests_rx,
                    hooks,
                    queued,
                ))
            })
            .map_err(|e| Error::Thread(e.to_string()))?;
        Ok(Cursio {
            shared,
            health,
            requests,
            rejected,
            logs,
        })
    }
}

/// What judges a bad declaration: [`reject`] in the program, [`skipped`] to
/// test what a release build does.
type Judge = fn(RejectedKind, &str, &str) -> Rejected;

/// The topics the hello lists and their handlers; an invalid or repeated
/// one, or one past the limit, is judged and left out.
type Listened = (
    Vec<TopicName>,
    BTreeMap<TopicName, OnMessage>,
    Vec<Rejected>,
);

fn listened(
    topics: Vec<(
        Result<TopicName, cursio_app_protocol::Error>,
        String,
        OnMessage,
    )>,
    judge: Judge,
) -> Listened {
    let mut names = Vec::new();
    let mut handlers = BTreeMap::new();
    let mut rejected = Vec::new();
    for (topic, asked, handler) in topics {
        let topic = match topic {
            Ok(topic) => topic,
            Err(error) => {
                rejected.push(judge(RejectedKind::Topic, &asked, &error.to_string()));
                continue;
            }
        };
        if names.len() == MAX_TOPICS {
            let why = format!("at most {MAX_TOPICS} topics per process");
            rejected.push(judge(RejectedKind::Topic, &asked, &why));
            continue;
        }
        if handlers.contains_key(&topic) {
            rejected.push(judge(RejectedKind::Topic, &asked, "listened to twice"));
            continue;
        }
        handlers.insert(topic.clone(), handler);
        names.push(topic);
    }
    (names, handlers, rejected)
}

/// Hand a message to its topic's handler off the connection's thread; a
/// message about a topic nobody here listens to, or that is not valid
/// base64, is dropped.
fn hear(
    hooks: &Hooks,
    from: String,
    from_process: Option<ProcessName>,
    topic: TopicName,
    payload: &str,
) {
    let (Some(handler), Ok(payload)) = (
        hooks.messages.get(&topic).cloned(),
        cursio_app_protocol::decode_payload(payload),
    ) else {
        return;
    };
    let message = Message {
        from_machine: from,
        from_process,
        topic,
        payload,
    };
    tokio::task::spawn_blocking(move || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(message)));
    });
}

/// The declared actions as the hello lists them, and their handlers by name;
/// an invalid, oversized, repeated or surplus one is judged and left out.
type DeclaredActions = (
    Vec<ActionSpec>,
    BTreeMap<ActionName, Handler>,
    Vec<Rejected>,
);

fn declared(actions: Vec<Declared>, judge: Judge) -> DeclaredActions {
    let mut specs = Vec::new();
    let mut handlers = BTreeMap::new();
    let mut rejected = Vec::new();
    for action in actions {
        let asked = action.asked;
        let name = match action.name {
            Ok(name) => name,
            Err(error) => {
                rejected.push(judge(RejectedKind::Action, &asked, &error.to_string()));
                continue;
            }
        };
        let spec = ActionSpec {
            name: name.clone(),
            description: action.description,
            schema: action.schema,
        };
        let refusal = if specs.len() == MAX_ACTIONS {
            Some(format!("at most {MAX_ACTIONS} actions per process"))
        } else if handlers.contains_key(&name) {
            Some("declared twice".to_owned())
        } else {
            spec.check().err().map(|e| e.to_string())
        };
        if let Some(why) = refusal {
            rejected.push(judge(RejectedKind::Action, &asked, &why));
            continue;
        }
        handlers.insert(name, action.handler);
        specs.push(spec);
    }
    (specs, handlers, rejected)
}

/// Run `action` with `args` off the connection's thread and send its answer;
/// a panic in it is an answer too.
fn dispatch(
    hooks: &Hooks,
    id: u64,
    action: ActionName,
    args: serde_json::Value,
    answers: &mpsc::UnboundedSender<ClientMsg>,
) {
    let Some(handler) = hooks.actions.get(&action).cloned() else {
        let names: Vec<ActionName> = hooks.actions.keys().cloned().collect();
        let error = no_such_action(&hooks.app, action.as_str(), &names);
        let _ = answers.send(ClientMsg::answer(id, Err(error)));
        return;
    };
    let answers = answers.clone();
    tokio::task::spawn_blocking(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(args)));
        let result = outcome.unwrap_or_else(|panic| {
            let why = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned());
            Err(match why {
                Some(why) => format!("action {action} panicked: {why}"),
                None => format!("action {action} panicked"),
            })
        });
        let _ = answers.send(ClientMsg::answer(id, result));
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Link {
    Connecting,
    Connected { machine: String },
    Refused { reason: String },
}

/// What a declaration was: an action, a topic, the process name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectedKind {
    Action,
    Topic,
    Process,
}

impl std::fmt::Display for RejectedKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RejectedKind::Action => "action",
            RejectedKind::Topic => "topic",
            RejectedKind::Process => "process",
        })
    }
}

/// A declaration [`App::start`] left out, and why. Everything else registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub kind: RejectedKind,
    pub name: String,
    pub reason: String,
}

/// A bad declaration: at once in a debug build, where the developer is
/// looking; noted and skipped in a release build, where one misnamed action
/// took the whole application out of the mesh — its logs, its presence,
/// every other action — with nothing said.
fn reject(kind: RejectedKind, name: &str, reason: &str) -> Rejected {
    if cfg!(debug_assertions) {
        panic!(
            "cursio-sdk: {kind} {name:?} cannot be declared: {reason} \
             (names are lowercase letters, digits and `-`)"
        );
    }
    skipped(kind, name, reason)
}

/// A declaration left out, said once on standard error — the SDK takes no
/// logging dependency — and kept for [`Cursio::rejected`].
fn skipped(kind: RejectedKind, name: &str, reason: &str) -> Rejected {
    eprintln!("cursio-sdk: skipped {kind} {name:?}: {reason}");
    Rejected {
        kind,
        name: name.to_owned(),
        reason: reason.to_owned(),
    }
}

/// The application's connection to the node. Dropping it closes the
/// connection and stops its thread.
#[derive(Debug)]
pub struct Cursio {
    shared: Arc<Mutex<Link>>,
    health: watch::Sender<(HealthState, Option<String>)>,
    requests: mpsc::UnboundedSender<Request>,
    rejected: Vec<Rejected>,
    logs: Arc<LogShared>,
}

/// A `tracing` layer that hands the application's events to its node, for
/// `cursio logs <machine> --app <id>` to read anywhere — the way to a log
/// for an application with no log file the node can open:
///
/// ```ignore
/// use tracing_subscriber::layer::SubscriberExt;
/// let cursio = cursio_sdk::App::new("my-app").start()?;
/// tracing_subscriber::registry().with(cursio_sdk::tracing_layer(&cursio)).init();
/// ```
#[cfg(feature = "tracing")]
pub fn tracing_layer(cursio: &Cursio) -> CursioLayer {
    CursioLayer {
        shared: cursio.logs.clone(),
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Cursio {
    /// The declarations a release build skipped because they were invalid;
    /// a debug build panics at them instead. Empty when all registered.
    pub fn rejected(&self) -> &[Rejected] {
        &self.rejected
    }

    /// Hand the node one log line, for it to keep in a file of its own that
    /// `cursio logs <machine> --app <id>` reads — for an application with no
    /// log file the node can open. Returns at once: the line is queued and
    /// sent in a batch, kept while the node is away, and when the
    /// application logs faster than lines leave, the oldest are dropped and
    /// counted in the log. With [`App::logs`] given, the application's own
    /// file is what the node reads, and these lines are not kept.
    pub fn log(&self, level: LogLevel, target: &str, message: &str) {
        self.logs.push(&log::format_line(
            std::time::SystemTime::now(),
            level,
            target,
            message,
        ));
    }

    fn link(&self) -> Link {
        self.shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The name of this machine on the mesh, while a node is connected.
    pub fn machine(&self) -> Option<String> {
        match self.link() {
            Link::Connected { machine } => Some(machine),
            _ => None,
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.link(), Link::Connected { .. })
    }

    /// Say how the application is doing. Kept and sent again whenever the
    /// node reconnects; [`Error::NotRunning`] says it is only kept for now.
    pub fn set_health(&self, state: HealthState, detail: Option<&str>) -> Result<(), Error> {
        let detail = detail.map(|d| {
            d.chars()
                .take(cursio_app_protocol::MAX_DETAIL)
                .collect::<String>()
        });
        self.health.send_replace((state, detail));
        self.connected()
    }

    fn connected(&self) -> Result<(), Error> {
        match self.link() {
            Link::Connected { .. } => Ok(()),
            Link::Connecting => Err(Error::NotRunning),
            Link::Refused { reason } => Err(Error::Refused(reason)),
        }
    }

    /// Ask the node `msg`, which carries `id`, and wait up to `wait` for its
    /// answer.
    fn ask(&self, id: u64, msg: ClientMsg, wait: Duration) -> Result<NodeMsg, Error> {
        self.connected()?;
        let (reply, answer) = std::sync::mpsc::channel();
        self.requests
            .send(Request { msg, id, reply })
            .map_err(|_| Error::NotRunning)?;
        answer.recv_timeout(wait).map_err(|e| match e {
            std::sync::mpsc::RecvTimeoutError::Timeout => Error::Send(format!(
                "the node did not answer within {} s",
                wait.as_secs()
            )),
            std::sync::mpsc::RecvTimeoutError::Disconnected => Error::NotRunning,
        })
    }

    /// Send `payload` (at most 512 KiB) about `topic` to the copies of this
    /// application on `machine` that listen to it ([`App::on_message`]
    /// there), and wait until that machine says it took it. Delivered at most
    /// once, to the copies running now; nothing is kept for later, so an
    /// error says which case it was — [`Error::Offline`],
    /// [`Error::CopyNotRunning`], [`Error::NotAllowed`] — and whether trying
    /// again means anything is the application's call.
    ///
    /// Blocks, up to about half a minute for a slow link: call it off an
    /// async executor's worker (`spawn_blocking`).
    pub fn send(&self, machine: &str, topic: &str, payload: &[u8]) -> Result<(), Error> {
        let topic = TopicName::parse(topic).map_err(Error::Invalid)?;
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Invalid(cursio_app_protocol::Error::TooLarge(
                payload.len(),
            )));
        }
        let id = next_id();
        let msg = ClientMsg::Send {
            id,
            to: machine.to_owned(),
            topic,
            payload: cursio_app_protocol::encode_payload(payload),
        };
        match self.ask(id, msg, SEND_WAIT)? {
            NodeMsg::Sent { error: None, .. } => Ok(()),
            NodeMsg::Sent {
                error: Some(error), ..
            } => {
                let machine = machine.to_owned();
                Err(match error.kind {
                    SendFailure::Offline => Error::Offline { machine },
                    SendFailure::NotRunning => Error::CopyNotRunning { machine },
                    SendFailure::Refused => Error::NotAllowed {
                        machine,
                        reason: error.reason,
                    },
                    SendFailure::Failed => Error::Send(error.reason),
                })
            }
            other => Err(Error::Send(format!("the node answered {other:?}"))),
        }
    }

    /// [`Cursio::send`] to every machine whose running copy listens to
    /// `topic`, answering each machine's outcome.
    pub fn send_all(&self, topic: &str, payload: &[u8]) -> Result<Vec<SentTo>, Error> {
        let wanted = TopicName::parse(topic).map_err(Error::Invalid)?;
        Ok(self
            .peers()?
            .into_iter()
            .filter(|p| p.running && p.topics.contains(&wanted))
            .map(|p| {
                let sent = self.send(&p.machine, topic, payload);
                (p.machine, sent)
            })
            .collect())
    }

    /// The connected machines where this application is registered, by what
    /// their nodes last said: whether a copy runs, its version, what it
    /// listens to.
    pub fn peers(&self) -> Result<Vec<Peer>, Error> {
        let id = next_id();
        match self.ask(id, ClientMsg::Peers { id }, PEERS_WAIT)? {
            NodeMsg::Peers { peers, .. } => Ok(peers
                .into_iter()
                .map(|p| Peer {
                    machine: p.machine,
                    running: p.running,
                    version: p.version,
                    topics: p.topics,
                })
                .collect()),
            other => Err(Error::Send(format!("the node answered {other:?}"))),
        }
    }
}

impl Cursio {
    /// Fetch `entries` from this application's copies on the connected
    /// machines ([`App::on_blob`] there) into the absolute directory `dest`,
    /// each under [`BlobHash::file_name`], calling `progress` as bytes
    /// arrive. A blob counts only once its bytes hash to what was asked; a
    /// fetch cut short keeps what arrived and goes on from there next time.
    /// What no connected copy has comes back in
    /// [`FetchReport::not_found`].
    ///
    /// Blocks until every blob is done: call it off an async executor's
    /// worker. Fails only when the node goes away or stops saying anything
    /// for [`FETCH_QUIET`].
    pub fn fetch(
        &self,
        entries: &[BlobEntry],
        dest: &Path,
        mut progress: impl FnMut(Progress),
    ) -> Result<FetchReport, Error> {
        self.connected()?;
        let id = next_id();
        let (reply, answers) = std::sync::mpsc::channel();
        let msg = ClientMsg::Fetch {
            id,
            entries: entries.to_vec(),
            dest: dest.to_path_buf(),
        };
        self.requests
            .send(Request { msg, id, reply })
            .map_err(|_| Error::NotRunning)?;
        loop {
            let answer = answers.recv_timeout(FETCH_QUIET).map_err(|e| match e {
                std::sync::mpsc::RecvTimeoutError::Timeout => Error::Send(format!(
                    "the node said nothing about the fetch for {} s",
                    FETCH_QUIET.as_secs()
                )),
                std::sync::mpsc::RecvTimeoutError::Disconnected => Error::NotRunning,
            })?;
            match answer {
                NodeMsg::Fetching {
                    hash,
                    received,
                    size,
                    ..
                } => progress(Progress {
                    hash,
                    received,
                    size,
                }),
                NodeMsg::Fetched {
                    fetched,
                    not_found,
                    failed,
                    ..
                } => return Ok(report_of(entries, &fetched, &not_found, &failed)),
                other => return Err(Error::Send(format!("the node answered {other:?}"))),
            }
        }
    }
}

/// The node's answer, by hash, as the caller's entries.
fn report_of(
    entries: &[BlobEntry],
    fetched: &[BlobHash],
    not_found: &[BlobHash],
    failed: &[cursio_app_protocol::BlobFailure],
) -> FetchReport {
    let mut report = FetchReport::default();
    for entry in entries {
        if fetched.contains(&entry.hash) {
            report.fetched.push(entry.clone());
        } else if let Some(failure) = failed.iter().find(|f| f.hash == entry.hash) {
            report.failed.push((entry.clone(), failure.reason.clone()));
        } else if not_found.contains(&entry.hash) {
            report.not_found.push(entry.clone());
        } else {
            let why = "the node said nothing of it".to_owned();
            report.failed.push((entry.clone(), why));
        }
    }
    report
}

/// Name the file for `hash` off the connection's thread; a handler that
/// panics names none.
fn name_blob(hooks: &Hooks, id: u64, hash: BlobHash, answers: &mpsc::UnboundedSender<ClientMsg>) {
    let Some(handler) = hooks.on_blob.clone() else {
        let _ = answers.send(ClientMsg::BlobPath { id, path: None });
        return;
    };
    let answers = answers.clone();
    tokio::task::spawn_blocking(move || {
        let path = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&hash)))
            .ok()
            .flatten();
        let _ = answers.send(ClientMsg::BlobPath { id, path });
    });
}

fn next_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

enum Ended {
    /// The connection closed or never opened; try again soon.
    Closed,
    /// The node said no; try again rarely, in case a newer node says yes.
    Refused(String),
    /// The application dropped its handle.
    Stop,
}

async fn run(
    endpoint: PathBuf,
    hello: ClientMsg,
    shared: Arc<Mutex<Link>>,
    mut health: watch::Receiver<(HealthState, Option<String>)>,
    mut requests: mpsc::UnboundedReceiver<Request>,
    hooks: Hooks,
    logs: Arc<LogShared>,
) {
    let set = |link: Link| *shared.lock().unwrap_or_else(|p| p.into_inner()) = link;
    let mut wait = RETRY_MIN;
    loop {
        let ended = match connect(&endpoint).await {
            Ok(stream) => {
                session(
                    stream,
                    &hello,
                    &set,
                    &mut health,
                    &mut requests,
                    &hooks,
                    &logs,
                )
                .await
            }
            Err(_) => Ended::Closed,
        };
        match ended {
            Ended::Stop => return,
            Ended::Refused(reason) => {
                set(Link::Refused { reason });
                wait = RETRY_MAX;
            }
            Ended::Closed => {
                if matches!(
                    *shared.lock().unwrap_or_else(|p| p.into_inner()),
                    Link::Connected { .. }
                ) {
                    wait = RETRY_MIN;
                }
                set(Link::Connecting);
            }
        }
        // A health change while waiting cuts the wait short, which is
        // harmless; the handle being dropped ends the thread.
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            changed = health.changed() => if changed.is_err() {
                return;
            },
        }
        wait = (wait * 2).min(RETRY_MAX);
    }
}

#[cfg(unix)]
async fn connect(endpoint: &Path) -> std::io::Result<tokio::net::UnixStream> {
    match cursio_app_protocol::abstract_name(endpoint) {
        Some(name) => connect_abstract(name),
        None => tokio::net::UnixStream::connect(endpoint).await,
    }
}

/// An abstract socket — the node on Android, where Cursio's app hosts it and
/// no other app can reach a file of its. A local connect does not block.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn connect_abstract(name: &str) -> std::io::Result<tokio::net::UnixStream> {
    #[cfg(target_os = "android")]
    use std::os::android::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    let stream = std::os::unix::net::UnixStream::connect_addr(&addr)?;
    stream.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(stream)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn connect_abstract(name: &str) -> std::io::Result<tokio::net::UnixStream> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("@{name} is an abstract socket, which only Linux and Android have"),
    ))
}

#[cfg(windows)]
async fn connect(
    endpoint: &Path,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint.as_os_str())
}

#[cfg(not(any(unix, windows)))]
async fn connect(_endpoint: &Path) -> std::io::Result<tokio::io::DuplexStream> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no Cursio node on this platform",
    ))
}

async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = reader
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if read == 0 || line.last() != Some(&b'\n') {
        return Ok(None);
    }
    line.pop();
    Ok(Some(line))
}

/// Send every queued log line, in batches. A batch the connection would not
/// take goes back on the queue, for the next connection.
async fn send_logs<W: AsyncWrite + Unpin>(write: &mut W, logs: &LogShared) -> std::io::Result<()> {
    while let Some((lines, dropped)) = logs.take_batch() {
        let line = encode_line(&ClientMsg::Log {
            lines: lines.clone(),
            dropped,
        });
        if let Err(error) = write.write_all(&line).await {
            logs.put_back(lines, dropped);
            return Err(error);
        }
    }
    Ok(())
}

/// Abort a task when its owner is done with it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn session<S: AsyncRead + AsyncWrite + Send + 'static>(
    stream: S,
    hello: &ClientMsg,
    set: &impl Fn(Link),
    health: &mut watch::Receiver<(HealthState, Option<String>)>,
    requests: &mut mpsc::UnboundedReceiver<Request>,
    hooks: &Hooks,
    logs: &LogShared,
) -> Ended {
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    if write.write_all(&encode_line(hello)).await.is_err() {
        return Ended::Closed;
    }
    let answer = match tokio::time::timeout(WELCOME_WAIT, read_line(&mut reader)).await {
        Ok(Ok(Some(line))) => decode_line::<NodeMsg>(&line),
        _ => return Ended::Closed,
    };
    match answer {
        Ok(NodeMsg::Welcome { machine, .. }) => set(Link::Connected { machine }),
        Ok(NodeMsg::Refused { reason }) => return Ended::Refused(reason),
        _ => return Ended::Closed,
    }
    let send_health = |(state, detail): (HealthState, Option<String>)| {
        encode_line(&ClientMsg::Health { state, detail })
    };
    let current = health.borrow_and_update().clone();
    if current != (HealthState::Ok, None) && write.write_all(&send_health(current)).await.is_err() {
        return Ended::Closed;
    }
    // Lines are read by a task of their own and handed over whole: reading
    // inside the `select!` below lost the part of a line already read
    // whenever another branch won.
    let (lines_tx, mut lines) = mpsc::unbounded_channel::<Vec<u8>>();
    let _reader = AbortOnDrop(tokio::spawn(async move {
        while let Ok(Some(line)) = read_line(&mut reader).await {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    }));
    let (answers_tx, mut answers) = mpsc::unbounded_channel::<ClientMsg>();
    // Requests sent and waiting for the node's answer, by id; dropped with
    // the session, which tells each waiter the node went.
    let mut waiting: BTreeMap<u64, std::sync::mpsc::Sender<NodeMsg>> = BTreeMap::new();
    // The first tick is at once: lines queued before the node answered go
    // out first.
    let mut flush = tokio::time::interval(log::FLUSH_EVERY);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = flush.tick() => {
                if send_logs(&mut write, logs).await.is_err() {
                    return Ended::Closed;
                }
            }
            () = logs.ready.notified() => {
                if send_logs(&mut write, logs).await.is_err() {
                    return Ended::Closed;
                }
            }
            Some(request) = requests.recv() => {
                if write.write_all(&encode_line(&request.msg)).await.is_err() {
                    return Ended::Closed;
                }
                waiting.insert(request.id, request.reply);
            }
            changed = health.changed() => {
                if changed.is_err() {
                    return Ended::Stop;
                }
                let current = health.borrow_and_update().clone();
                if write.write_all(&send_health(current)).await.is_err() {
                    return Ended::Closed;
                }
            }
            Some(answer) = answers.recv() => {
                if write.write_all(&encode_line(&answer)).await.is_err() {
                    return Ended::Closed;
                }
            }
            line = lines.recv() => match line {
                Some(line) => match decode_line::<NodeMsg>(&line) {
                    Ok(NodeMsg::Refused { reason }) => return Ended::Refused(reason),
                    Ok(NodeMsg::BuildReady { version, dir }) => {
                        if let Some(handler) = &hooks.on_update {
                            handler(Build { version, dir });
                        }
                    }
                    Ok(NodeMsg::Call { id, action, args }) => {
                        dispatch(hooks, id, action, args, &answers_tx);
                    }
                    Ok(answer @ (NodeMsg::Sent { id, .. }
                        | NodeMsg::Peers { id, .. }
                        | NodeMsg::Fetched { id, .. })) => {
                        if let Some(reply) = waiting.remove(&id) {
                            let _ = reply.send(answer);
                        }
                    }
                    // Progress: the fetch goes on waiting for its outcome.
                    Ok(progress @ NodeMsg::Fetching { id, .. }) => {
                        if let Some(reply) = waiting.get(&id) {
                            let _ = reply.send(progress);
                        }
                    }
                    Ok(NodeMsg::BlobPath { id, hash }) => {
                        name_blob(hooks, id, hash, &answers_tx);
                    }
                    Ok(NodeMsg::Message { from, from_process, topic, payload }) => {
                        hear(hooks, from, from_process, topic, &payload);
                    }
                    // A message a newer node sends that this SDK does not
                    // know is skipped, as the protocol promises.
                    _ => {}
                },
                None => return Ended::Closed,
            },
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;
    use tokio::net::UnixListener;

    fn action(name: &str) -> Declared {
        Declared {
            name: ActionName::parse(name),
            asked: name.to_owned(),
            description: "does it".into(),
            schema: serde_json::Value::Null,
            handler: Arc::new(|_| Ok(serde_json::Value::Null)),
        }
    }

    #[test]
    fn a_misnamed_action_is_left_out_and_the_rest_still_register() {
        // What a release build does: `skipped` is its judge.
        let (specs, handlers, rejected) = declared(
            vec![
                action("launch"),
                action("game_log"),
                action("stop"),
                action("launch"),
            ],
            skipped,
        );
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["launch", "stop"]);
        assert_eq!(handlers.len(), 2);
        assert_eq!(rejected.len(), 2);
        assert_eq!(rejected[0].kind, RejectedKind::Action);
        assert_eq!(rejected[0].name, "game_log");
        assert_eq!(rejected[1].reason, "declared twice");
    }

    #[test]
    fn a_misnamed_topic_is_left_out_and_the_rest_still_listen() {
        let on: OnMessage = Arc::new(|_| {});
        let (names, _, rejected) = listened(
            vec![
                (TopicName::parse("sync"), "sync".into(), on.clone()),
                (
                    TopicName::parse("Bad Topic"),
                    "Bad Topic".into(),
                    on.clone(),
                ),
            ],
            skipped,
        );
        assert_eq!(names.len(), 1);
        assert_eq!(rejected[0].kind, RejectedKind::Topic);
        assert_eq!(rejected[0].name, "Bad Topic");
    }

    #[test]
    #[should_panic(expected = "game_log")]
    fn a_debug_build_names_a_misnamed_action_where_it_is_written() {
        let _ = App::new("demo").action("game_log", "", serde_json::Value::Null, |_| {
            Ok(serde_json::Value::Null)
        });
    }

    fn socket() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cursio-sdk-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("apps.sock")
    }

    fn eventually(what: &str, check: impl Fn() -> bool) {
        let start = Instant::now();
        while !check() {
            assert!(start.elapsed() < Duration::from_secs(10), "never: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A node that answers each connection with `answer` and hands back
    /// every line it is sent.
    fn fake_node(path: PathBuf, answer: NodeMsg) -> std::sync::mpsc::Receiver<ClientMsg> {
        fake_node_saying(path, vec![answer])
    }

    /// A node that answers every hello with `said`, in order, and reports
    /// every line it hears.
    fn fake_node_saying(path: PathBuf, said: Vec<NodeMsg>) -> std::sync::mpsc::Receiver<ClientMsg> {
        let answer = said;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = UnixListener::bind(&path).unwrap();
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (tx, answer) = (tx.clone(), answer.clone());
                    tokio::spawn(async move {
                        let (read, mut write) = tokio::io::split(stream);
                        let mut reader = BufReader::new(read);
                        let mut first = true;
                        while let Ok(Some(line)) = read_line(&mut reader).await {
                            let _ = tx.send(decode_line::<ClientMsg>(&line).unwrap());
                            if first {
                                first = false;
                                for message in &answer {
                                    let _ = write.write_all(&encode_line(message)).await;
                                }
                            }
                        }
                    });
                }
            });
        });
        rx
    }

    #[test]
    fn a_declared_action_is_called_off_the_socket_and_a_panic_or_a_wrong_name_is_answered() {
        let path = socket();
        let call = |id, action: &str, args| NodeMsg::Call {
            id,
            action: ActionName::parse(action).unwrap(),
            args,
        };
        let heard = fake_node_saying(
            path.clone(),
            vec![
                NodeMsg::Welcome {
                    protocol: 1,
                    machine: "Nocturne".into(),
                },
                call(1, "boom", Value::Null),
                call(2, "foo", Value::Null),
                call(3, "echo", json!({"instance": "vanilla"})),
            ],
        );
        let _app = App::new("leavepulse")
            .endpoint(&path)
            .action(
                "echo",
                "Answer the arguments",
                json!({"type": "object"}),
                Ok,
            )
            .action("boom", "Panic", Value::Null, |_| {
                panic!("the instance vanished")
            })
            .start()
            .unwrap();
        let ClientMsg::Hello { actions, .. } = heard.recv_timeout(Duration::from_secs(10)).unwrap()
        else {
            panic!("expected hello");
        };
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["echo", "boom"]);
        let mut answers = std::collections::BTreeMap::new();
        while answers.len() < 3 {
            if let ClientMsg::Answer { id, ok, error } =
                heard.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                answers.insert(id, (ok, error));
            }
        }
        assert_eq!(
            answers[&1].1.as_deref(),
            Some("action boom panicked: the instance vanished")
        );
        assert_eq!(
            answers[&2].1.as_deref(),
            Some("leavepulse has no action foo; it has: boom, echo")
        );
        assert_eq!(
            answers[&3].0,
            Some(json!({"instance": "vanilla"})),
            "a later call is served after the panic"
        );
    }

    #[test]
    #[should_panic(expected = "declared twice")]
    fn a_debug_build_names_an_action_declared_twice_when_it_starts() {
        let _ = App::new("demo")
            .action("go", "", Value::Null, |_| Ok(Value::Null))
            .action("go", "", Value::Null, |_| Ok(Value::Null))
            .endpoint(socket())
            .start();
    }

    #[test]
    fn an_invalid_id_is_the_only_way_start_fails() {
        assert!(matches!(
            App::new("Not Valid").start(),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            App::new("demo").logs("relative", "x.log").start(),
            Err(Error::Invalid(_))
        ));
        let nowhere = App::new("demo").endpoint(socket()).start().unwrap();
        assert!(!nowhere.is_connected());
        assert_eq!(
            nowhere.set_health(HealthState::Degraded, None),
            Err(Error::NotRunning)
        );
    }

    #[test]
    fn a_node_that_starts_later_hears_the_hello_and_the_health_kept_meanwhile() {
        let path = socket();
        let app = App::new("demo")
            .version("1.0")
            .logs("/var/log/demo", "demo.log")
            .endpoint(&path)
            .start()
            .unwrap();
        assert_eq!(
            app.set_health(HealthState::Failing, Some("no api")),
            Err(Error::NotRunning)
        );
        let heard = fake_node(
            path,
            NodeMsg::Welcome {
                protocol: 1,
                machine: "Nocturne".into(),
            },
        );
        match heard.recv_timeout(Duration::from_secs(10)).unwrap() {
            ClientMsg::Hello {
                app, version, logs, ..
            } => {
                assert_eq!(app.as_str(), "demo");
                assert_eq!(version, "1.0");
                assert_eq!(logs.unwrap().file, "demo.log");
            }
            other => panic!("expected hello, got {other:?}"),
        }
        assert_eq!(
            heard.recv_timeout(Duration::from_secs(10)).unwrap(),
            ClientMsg::Health {
                state: HealthState::Failing,
                detail: Some("no api".into())
            }
        );
        eventually("connected", || app.is_connected());
        assert_eq!(app.machine().as_deref(), Some("Nocturne"));
        assert_eq!(app.set_health(HealthState::Ok, None), Ok(()));
        assert_eq!(
            heard.recv_timeout(Duration::from_secs(10)).unwrap(),
            ClientMsg::Health {
                state: HealthState::Ok,
                detail: None
            }
        );
    }

    #[test]
    fn a_refusal_is_reported_with_its_reason() {
        let path = socket();
        let _heard = fake_node(
            path.clone(),
            NodeMsg::Refused {
                reason: "not today".into(),
            },
        );
        let app = App::new("demo").endpoint(&path).start().unwrap();
        eventually("refused", || {
            app.set_health(HealthState::Ok, None) == Err(Error::Refused("not today".into()))
        });
    }
}

#[cfg(test)]
mod fetch_tests {
    use super::*;

    fn entry(path: &str, digit: char) -> BlobEntry {
        BlobEntry {
            path: path.into(),
            hash: BlobHash::new(BlobAlgo::Sha1, &digit.to_string().repeat(40)).unwrap(),
            size: 1,
        }
    }

    #[test]
    fn the_report_speaks_of_the_callers_own_entries_two_paths_one_content() {
        let (a, b, c, d) = (
            entry("mods/a.jar", 'a'),
            entry("mods/copy-of-a.jar", 'a'),
            entry("mods/c.jar", 'c'),
            entry("mods/d.jar", 'd'),
        );
        let failed = vec![cursio_app_protocol::BlobFailure {
            hash: c.hash.clone(),
            reason: "corrupt".into(),
        }];
        let report = report_of(
            &[a.clone(), b.clone(), c.clone(), d.clone()],
            std::slice::from_ref(&a.hash),
            std::slice::from_ref(&d.hash),
            &failed,
        );
        assert_eq!(report.fetched, vec![a, b], "one file serves both paths");
        assert_eq!(report.failed, vec![(c, "corrupt".to_owned())]);
        assert_eq!(report.not_found, vec![d]);
    }
}
