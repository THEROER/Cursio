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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_app_protocol::{decode_line, encode_line, ClientMsg, NodeMsg, MAX_LINE, PROTOCOL};
pub use cursio_app_protocol::{AppId, HealthState, LogSpec, ProcessName};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;

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
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(e) => e.fmt(f),
            Error::NotRunning => f.write_str("no Cursio node is running on this machine"),
            Error::Refused(why) => write!(f, "the Cursio node refused this application: {why}"),
            Error::Thread(why) => write!(f, "cannot start the Cursio connection: {why}"),
        }
    }
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

/// An application about to introduce itself to the node.
#[derive(Clone)]
pub struct App {
    id: Result<AppId, cursio_app_protocol::Error>,
    version: String,
    logs: Option<LogSpec>,
    endpoint: Option<PathBuf>,
    on_update: Option<OnUpdate>,
    process: Option<Result<ProcessName, cursio_app_protocol::Error>>,
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
        }
    }

    /// This is one of the application's several processes — `daemon` beside
    /// the window — each registering under the same id with its own log.
    /// `cursio logs <machine> --app <id>` merges them all, each line marked
    /// with its process; `--app <id>/<process>` reads one. Leave it unset for
    /// the main process.
    pub fn process(mut self, name: &str) -> Self {
        self.process = Some(ProcessName::parse(name));
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
    /// or the log location is invalid.
    pub fn start(self) -> Result<Cursio, Error> {
        let id = self.id.map_err(Error::Invalid)?;
        if let Some(logs) = &self.logs {
            logs.check().map_err(Error::Invalid)?;
        }
        let process = self.process.transpose().map_err(Error::Invalid)?;
        let hello = ClientMsg::Hello {
            protocol: PROTOCOL,
            app: id,
            version: self.version,
            pid: std::process::id(),
            logs: self.logs,
            process,
        };
        let endpoint = self.endpoint.unwrap_or_else(cursio_app_protocol::endpoint);
        let shared = Arc::new(Mutex::new(Link::Connecting));
        let (health, health_rx) = watch::channel((HealthState::Ok, None));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Thread(e.to_string()))?;
        let link = shared.clone();
        let on_update = self.on_update;
        std::thread::Builder::new()
            .name("cursio-sdk".into())
            .spawn(move || runtime.block_on(run(endpoint, hello, link, health_rx, on_update)))
            .map_err(|e| Error::Thread(e.to_string()))?;
        Ok(Cursio { shared, health })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Link {
    Connecting,
    Connected { machine: String },
    Refused { reason: String },
}

/// The application's connection to the node. Dropping it closes the
/// connection and stops its thread.
#[derive(Debug)]
pub struct Cursio {
    shared: Arc<Mutex<Link>>,
    health: watch::Sender<(HealthState, Option<String>)>,
}

impl Cursio {
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
        match self.link() {
            Link::Connected { .. } => Ok(()),
            Link::Connecting => Err(Error::NotRunning),
            Link::Refused { reason } => Err(Error::Refused(reason)),
        }
    }
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
    on_update: Option<OnUpdate>,
) {
    let set = |link: Link| *shared.lock().unwrap_or_else(|p| p.into_inner()) = link;
    let mut wait = RETRY_MIN;
    loop {
        let ended = match connect(&endpoint).await {
            Ok(stream) => session(stream, &hello, &set, &mut health, on_update.as_ref()).await,
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
    tokio::net::UnixStream::connect(endpoint).await
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

async fn session<S: AsyncRead + AsyncWrite>(
    stream: S,
    hello: &ClientMsg,
    set: &impl Fn(Link),
    health: &mut watch::Receiver<(HealthState, Option<String>)>,
    on_update: Option<&OnUpdate>,
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
    loop {
        tokio::select! {
            changed = health.changed() => {
                if changed.is_err() {
                    return Ended::Stop;
                }
                let current = health.borrow_and_update().clone();
                if write.write_all(&send_health(current)).await.is_err() {
                    return Ended::Closed;
                }
            }
            line = read_line(&mut reader) => match line {
                Ok(Some(line)) => match decode_line::<NodeMsg>(&line) {
                    Ok(NodeMsg::Refused { reason }) => return Ended::Refused(reason),
                    Ok(NodeMsg::BuildReady { version, dir }) => {
                        if let Some(handler) = on_update {
                            handler(Build { version, dir });
                        }
                    }
                    // A message a newer node sends that this SDK does not
                    // know is skipped, as the protocol promises.
                    _ => {}
                },
                _ => return Ended::Closed,
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
                                let _ = write.write_all(&encode_line(&answer)).await;
                            }
                        }
                    });
                }
            });
        });
        rx
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
