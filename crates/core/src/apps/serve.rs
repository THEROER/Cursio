//! The apps socket: where applications on this machine reach the node.
//!
//! It stands beside the control socket rather than inside it. The control
//! vocabulary is the app's and the tray's, and changes whenever a settings
//! page does; an application built against an older `cursio-sdk` must not
//! break when the node updates, so it gets its own endpoint speaking only the
//! pinned `cursio-app-protocol`. Privacy is the control socket's — the same
//! private directory and owner-only socket on Unix, the same DACL on Windows —
//! through the same bind helpers ([`crate::control::transport`]).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_app_protocol::{
    decode_line, encode_line, message_kind, ClientMsg, NodeMsg, MAX_LINE, PROTOCOL,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use super::Registry;

/// How long a new connection has to say who it is.
const HELLO_WAIT: Duration = Duration::from_secs(5);

/// Longest version string an application may give.
const MAX_VERSION: usize = 128;

type Pushers =
    std::collections::BTreeMap<super::Key, Vec<(u64, tokio::sync::mpsc::UnboundedSender<NodeMsg>)>>;

/// Each connected copy of each application process, for what the node tells
/// it unasked.
fn pushers() -> std::sync::MutexGuard<'static, Pushers> {
    static PUSHERS: Mutex<Pushers> = Mutex::new(std::collections::BTreeMap::new());
    PUSHERS.lock().unwrap_or_else(|p| p.into_inner())
}

/// What a call's answer resolves to.
pub type CallResult = Result<serde_json::Value, String>;

/// Calls sent and not yet answered, by id: the connection (token) that
/// carries each, and where its answer goes.
type Pending = std::collections::HashMap<u64, (u64, std::sync::mpsc::Sender<CallResult>)>;

fn pending() -> std::sync::MutexGuard<'static, Pending> {
    static PENDING: std::sync::OnceLock<Mutex<Pending>> = std::sync::OnceLock::new();
    PENDING
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Tell every running copy of every process of `app` that `version` is
/// installed in `dir`. A copy that is not running hears nothing; it finds
/// `current` when it next starts.
pub fn build_ready(app: &cursio_app_protocol::AppId, version: &str, dir: &std::path::Path) {
    let msg = NodeMsg::BuildReady {
        version: version.to_owned(),
        dir: dir.to_path_buf(),
    };
    let mut told = 0;
    for (key, copies) in pushers().iter_mut() {
        if key.app == *app {
            copies.retain(|(_, tx)| tx.send(msg.clone()).is_ok());
            told += copies.len();
        }
    }
    match told {
        0 => {
            tracing::info!(%app, %version, "no copy of the application is running to tell of its build")
        }
        copies => tracing::info!(%app, %version, copies, "told the application its build is ready"),
    }
}

/// Ask the running copy of `key`'s process to do `action` with `args`, and
/// wait up to `wait` for its answer. Blocks: called from a diag answer, which
/// is already off the session's loop.
pub fn call(
    key: &super::Key,
    action: &cursio_app_protocol::ActionName,
    args: serde_json::Value,
    wait: Duration,
) -> CallResult {
    let copy = pushers()
        .get(key)
        .and_then(|copies| copies.first().cloned());
    let Some((token, tx)) = copy else {
        return Err(format!("{} is not running on this machine", describe(key)));
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (answer_tx, answer) = std::sync::mpsc::channel();
    pending().insert(id, (token, answer_tx));
    let msg = NodeMsg::Call {
        id,
        action: action.clone(),
        args,
    };
    if tx.send(msg).is_err() {
        pending().remove(&id);
        return Err(format!("{} closed as it was called", describe(key)));
    }
    let result = answer.recv_timeout(wait).unwrap_or_else(|_| {
        Err(format!(
            "{} did not answer {action} within {} s",
            describe(key),
            wait.as_secs()
        ))
    });
    pending().remove(&id);
    result
}

/// Ask the running copy of `key`'s process which file holds blob `hash`,
/// waiting up to `wait`. `None` when it has none, does not answer, or is not
/// running. Blocks, like [`call`].
pub fn blob_path(
    key: &super::Key,
    hash: &cursio_app_protocol::BlobHash,
    wait: Duration,
) -> Option<PathBuf> {
    let (token, tx) = pushers().get(key).and_then(|c| c.first().cloned())?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 62);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (answer_tx, answer) = std::sync::mpsc::channel();
    pending().insert(id, (token, answer_tx));
    let asked = tx.send(NodeMsg::BlobPath {
        id,
        hash: hash.clone(),
    });
    let result = match asked {
        Ok(()) => answer.recv_timeout(wait).ok(),
        Err(_) => None,
    };
    pending().remove(&id);
    match result {
        Some(Ok(serde_json::Value::String(path))) => Some(PathBuf::from(path)),
        _ => None,
    }
}

/// An answer `result` to call or question `id` came on connection `token`:
/// hand it to whoever waits, if it is that connection's to answer.
fn resolve(id: u64, token: u64, result: CallResult) {
    let waiting = pending().remove(&id);
    match waiting {
        Some((carrier, answer)) if carrier == token => {
            let _ = answer.send(result);
        }
        Some(other) => {
            pending().insert(id, other);
        }
        None => tracing::debug!(id, "an answer no longer waited for"),
    }
}

/// Hand a message from `peer`'s copy (its `from_process`) about `topic` to
/// every running copy of the processes `keys`; how many took it.
pub fn deliver(
    keys: &[super::Key],
    peer: &str,
    from_process: Option<cursio_app_protocol::ProcessName>,
    topic: &cursio_app_protocol::TopicName,
    payload: &[u8],
) -> usize {
    let msg = NodeMsg::Message {
        from: peer.to_owned(),
        from_process,
        topic: topic.clone(),
        payload: cursio_app_protocol::encode_payload(payload),
    };
    let mut told = 0;
    let mut pushers = pushers();
    for key in keys {
        if let Some(copies) = pushers.get_mut(key) {
            copies.retain(|(_, tx)| tx.send(msg.clone()).is_ok());
            told += copies.len();
        }
    }
    told
}

/// Fetch `entries` of `app` from its copies on the connected machines into
/// `dest`, telling the asking copy how far each blob is, and answer what
/// became of them.
async fn fetch_for(
    app: &cursio_app_protocol::AppId,
    id: u64,
    entries: Vec<cursio_app_protocol::BlobEntry>,
    dest: PathBuf,
    out: tokio::sync::mpsc::UnboundedSender<NodeMsg>,
) -> NodeMsg {
    use cursio_app_protocol::{BlobFailure, MAX_FETCH};
    let refuse_all =
        |entries: Vec<cursio_app_protocol::BlobEntry>, reason: String| NodeMsg::Fetched {
            id,
            fetched: Vec::new(),
            not_found: Vec::new(),
            failed: entries
                .into_iter()
                .map(|e| BlobFailure {
                    hash: e.hash,
                    reason: reason.clone(),
                })
                .collect(),
        };
    if !dest.is_absolute() {
        let why = format!("{} is not an absolute directory", dest.display());
        return refuse_all(entries, why);
    }
    if entries.len() > MAX_FETCH {
        let why = format!("one fetch names at most {MAX_FETCH} blobs");
        return refuse_all(entries, why);
    }
    let peers: Vec<String> = super::mesh::peers(app)
        .into_iter()
        .filter(|p| p.running)
        .map(|p| p.machine)
        .collect();
    // At most a few progress lines a second per blob, and always the last.
    let said = Arc::new(Mutex::new(std::collections::HashMap::<
        cursio_app_protocol::BlobHash,
        std::time::Instant,
    >::new()));
    let progress = Arc::new(
        move |hash: &cursio_app_protocol::BlobHash, received: u64, size: u64| {
            let now = std::time::Instant::now();
            let mut said = said.lock().unwrap_or_else(|p| p.into_inner());
            let due = said
                .get(hash)
                .is_none_or(|at| now.duration_since(*at) >= Duration::from_millis(250));
            if due || received == size {
                said.insert(hash.clone(), now);
                let _ = out.send(NodeMsg::Fetching {
                    id,
                    hash: hash.clone(),
                    received,
                    size,
                });
            }
        },
    );
    let report = super::blobs::fetch(
        Arc::new(super::mesh::MeshPeers),
        peers,
        app,
        entries,
        &dest,
        progress,
    )
    .await;
    NodeMsg::Fetched {
        id,
        fetched: report.fetched,
        not_found: report.not_found,
        failed: report.failed,
    }
}

/// `app`, or `app/process`, as a person reads it.
fn describe(key: &super::Key) -> String {
    match &key.process {
        Some(process) => format!("{}/{process}", key.app),
        None => key.app.to_string(),
    }
}

/// Serve the apps socket for this node, named `machine` on the mesh, until
/// the process ends.
pub fn start(machine: String) {
    let endpoint = match endpoint() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::warn!(%error, "no apps socket; applications cannot reach this node");
            return;
        }
    };
    let registry = super::registry().clone();
    tokio::spawn(async move {
        if let Err(error) = serve(endpoint, machine, registry).await {
            tracing::warn!(%error, "apps socket stopped; applications cannot reach this node");
        }
    });
}

/// Where applications reach this node, the directory made private first.
fn endpoint() -> Result<PathBuf, crate::control::transport::ControlError> {
    #[cfg(unix)]
    {
        Ok(crate::control::transport::unix::socket_dir()?.join(cursio_app_protocol::SOCKET_NAME))
    }
    #[cfg(not(unix))]
    {
        Ok(cursio_app_protocol::endpoint())
    }
}

#[cfg(unix)]
pub async fn serve(
    endpoint: PathBuf,
    machine: String,
    registry: Arc<Mutex<Registry>>,
) -> Result<(), crate::control::transport::ControlError> {
    let listener = crate::control::transport::unix::bind(&endpoint).await?;
    tracing::info!(socket = %endpoint.display(), "apps socket listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let (machine, registry) = (machine.clone(), registry.clone());
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, &machine, &registry, None).await {
                tracing::debug!(%error, "application connection ended");
            }
        });
    }
}

#[cfg(windows)]
pub async fn serve(
    endpoint: PathBuf,
    machine: String,
    registry: Arc<Mutex<Registry>>,
) -> Result<(), crate::control::transport::ControlError> {
    use crate::control::transport::windows::create;
    let mut pending = create(&endpoint, true)?;
    tracing::info!(pipe = %endpoint.display(), "apps pipe listening");
    loop {
        pending.connect().await?;
        let connected = std::mem::replace(&mut pending, create(&endpoint, false)?);
        let (machine, registry) = (machine.clone(), registry.clone());
        tokio::spawn(async move {
            if let Err(error) = serve_connection(connected, &machine, &registry, None).await {
                tracing::debug!(%error, "application connection ended");
            }
        });
    }
}

/// Serve applications on the abstract socket `name` — a phone's, where any
/// app can connect — taking each only past `gate`, which is told the uid the
/// kernel reports for the peer; see [`super::access`].
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn start_abstract(machine: String, name: &'static str, gate: super::access::Gate) {
    let registry = super::registry().clone();
    tokio::spawn(async move {
        if let Err(error) = serve_abstract(name, machine, registry, gate).await {
            tracing::warn!(%error, "apps socket stopped; applications cannot reach this node");
        }
    });
}

#[cfg(any(target_os = "linux", target_os = "android"))]
async fn serve_abstract(
    name: &str,
    machine: String,
    registry: Arc<Mutex<Registry>>,
    gate: super::access::Gate,
) -> std::io::Result<()> {
    #[cfg(target_os = "android")]
    use std::os::android::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    let listener = std::os::unix::net::UnixListener::bind_addr(&addr)?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    tracing::info!(socket = %format!("@{name}"), "apps socket listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let uid = match stream.peer_cred() {
            Ok(cred) => cred.uid(),
            Err(error) => {
                tracing::warn!(%error, "cannot tell who connected to the apps socket; dropping it");
                continue;
            }
        };
        let gate = gate.clone();
        let admit: Admit = Box::new(move |app| gate(uid, app));
        let (machine, registry) = (machine.clone(), registry.clone());
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, &machine, &registry, Some(admit)).await {
                tracing::debug!(%error, "application connection ended");
            }
        });
    }
}

#[cfg(not(any(unix, windows)))]
pub async fn serve(
    _endpoint: PathBuf,
    _machine: String,
    _registry: Arc<Mutex<Registry>>,
) -> Result<(), crate::control::transport::ControlError> {
    Err(crate::control::transport::ControlError::Io(
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no apps socket on this platform",
        ),
    ))
}

/// One line without its newline, `None` at the end of the stream.
async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = reader
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        let why = match line.len() > MAX_LINE {
            true => "an application sent a line longer than the protocol allows",
            false => "an application closed in the middle of a line",
        };
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, why));
    }
    line.pop();
    Ok(Some(line))
}

fn lock(registry: &Mutex<Registry>) -> std::sync::MutexGuard<'_, Registry> {
    registry.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether the connection may register as an application; `Err` is the
/// reason its hello is refused with.
type Admit = Box<dyn Fn(&cursio_app_protocol::AppId) -> Result<(), String> + Send>;

async fn serve_connection<S: AsyncRead + AsyncWrite>(
    stream: S,
    machine: &str,
    registry: &Arc<Mutex<Registry>>,
    admit: Option<Admit>,
) -> std::io::Result<()> {
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let first = match tokio::time::timeout(HELLO_WAIT, read_line(&mut reader)).await {
        Ok(line) => line?,
        Err(_) => {
            tracing::debug!("an application connected and said nothing");
            return Ok(());
        }
    };
    let Some(first) = first else {
        return Ok(());
    };
    let (app, process, version, pid, logs, protocol, actions, topics, blobs, devtools) =
        match decode_line::<ClientMsg>(&first) {
            Ok(ClientMsg::Hello {
                protocol,
                app,
                version,
                pid,
                logs,
                process,
                actions,
                topics,
                blobs,
                devtools,
            }) => (
                app, process, version, pid, logs, protocol, actions, topics, blobs, devtools,
            ),
            Ok(_) => return refuse(&mut write, "the first message must be hello".into()).await,
            Err(error) => return refuse(&mut write, error.to_string()).await,
        };
    if protocol == 0 {
        return refuse(&mut write, "protocol version 0 does not exist".into()).await;
    }
    if version.len() > MAX_VERSION {
        return refuse(
            &mut write,
            format!("a version is at most {MAX_VERSION} bytes"),
        )
        .await;
    }
    if let Some(Err(error)) = logs.as_ref().map(|spec| spec.check()) {
        return refuse(&mut write, error.to_string()).await;
    }
    if actions.len() > cursio_app_protocol::MAX_ACTIONS {
        let limit = cursio_app_protocol::MAX_ACTIONS;
        return refuse(&mut write, format!("at most {limit} actions per process")).await;
    }
    if let Some(Err(error)) = actions.iter().map(|a| a.check()).find(Result::is_err) {
        return refuse(&mut write, error.to_string()).await;
    }
    if topics.len() > cursio_app_protocol::MAX_TOPICS {
        let limit = cursio_app_protocol::MAX_TOPICS;
        return refuse(&mut write, format!("at most {limit} topics per process")).await;
    }
    if let Some(Err(why)) = admit.as_ref().map(|admit| admit(&app)) {
        return refuse(&mut write, why).await;
    }
    let key = super::Key {
        app: app.clone(),
        process,
    };
    {
        let mut registry = lock(registry);
        registry.connected(&key, version.clone(), pid, logs.clone());
        registry.set_actions(&key, actions);
        registry.set_topics(&key, topics);
        registry.set_blobs(&key, blobs);
        registry.set_devtools(&key, devtools);
    }
    tracing::info!(%app, process = ?key.process, %version, pid, logs = ?logs.as_ref().map(|l| l.dir.join(&l.file)), "application connected");
    let welcome = NodeMsg::Welcome {
        protocol: protocol.min(PROTOCOL),
        machine: machine.to_owned(),
    };
    // Everything to the application goes through one queue, so a message the
    // node sends unasked never interleaves with an answer mid-line, and the
    // reading side is never cancelled halfway through a line to send one.
    let (out, mut outbox) = tokio::sync::mpsc::unbounded_channel::<NodeMsg>();
    let token: u64 = rand::random();
    pushers()
        .entry(key.clone())
        .or_default()
        .push((token, out.clone()));
    let _ = out.send(welcome);
    let writing = async {
        while let Some(msg) = outbox.recv().await {
            write.write_all(&encode_line(&msg)).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    let reading = async {
        while let Some(line) = read_line(&mut reader).await? {
            match decode_line::<ClientMsg>(&line) {
                Ok(ClientMsg::Health { state, detail }) => {
                    let detail =
                        detail.map(|d| d.chars().take(cursio_app_protocol::MAX_DETAIL).collect());
                    let mut registry = lock(registry);
                    // A change of state is news; a new detail under the same
                    // state is an application narrating itself, which at once
                    // a second was most of the node's log.
                    if registry.get(&key).map(|a| a.health) != Some(state) {
                        tracing::info!(%app, process = ?key.process, ?state, detail = ?detail, "application health");
                    } else {
                        tracing::trace!(%app, ?state, detail = ?detail, "application health");
                    }
                    registry.health(&key, state, detail);
                }
                Ok(ClientMsg::Hello { .. }) => {
                    let feature = "hello after hello".to_owned();
                    let _ = out.send(NodeMsg::Unsupported { feature });
                }
                Ok(ClientMsg::Log { lines, dropped }) => {
                    let keeper = super::app_logs::app_logs();
                    let keep = lock(registry).logs_through_node(&key, keeper.spec(&key));
                    if keep {
                        if let Err(error) =
                            keeper.append(&key, &lines, dropped, std::time::SystemTime::now())
                        {
                            tracing::warn!(%app, %error, "cannot keep an application's log lines");
                        }
                    } else {
                        tracing::trace!(%app, "log lines from an application that logs to a file of its own");
                    }
                }
                // Only the connection a call or question went out on answers it.
                Ok(ClientMsg::Answer { id, ok, error }) => {
                    let result = match error {
                        Some(error) => Err(error),
                        None => Ok(ok.unwrap_or(serde_json::Value::Null)),
                    };
                    resolve(id, token, result);
                }
                Ok(ClientMsg::BlobPath { id, path }) => {
                    let path = path
                        .map(|p| serde_json::Value::String(p.to_string_lossy().into_owned()))
                        .unwrap_or(serde_json::Value::Null);
                    resolve(id, token, Ok(path));
                }
                Ok(ClientMsg::Fetch { id, entries, dest }) => {
                    let (app, out) = (app.clone(), out.clone());
                    tokio::spawn(async move {
                        let answer = fetch_for(&app, id, entries, dest, out.clone()).await;
                        let _ = out.send(answer);
                    });
                }
                Ok(ClientMsg::Send {
                    id,
                    to,
                    topic,
                    payload,
                }) => {
                    let (app, process, out) = (app.clone(), key.process.clone(), out.clone());
                    // Off the reading loop: a slow peer must not hold up this
                    // copy's other messages, nor its answers to calls.
                    tokio::spawn(async move {
                        let sent = match cursio_app_protocol::decode_payload(&payload) {
                            Ok(bytes) => {
                                super::mesh::send(&app, process.as_ref(), &to, &topic, bytes)
                                    .await
                                    .map(drop)
                            }
                            Err(error) => Err(cursio_app_protocol::SendError {
                                kind: cursio_app_protocol::SendFailure::Failed,
                                reason: error.to_string(),
                            }),
                        };
                        if let Err(error) = &sent {
                            tracing::debug!(%app, %to, %topic, reason = %error.reason, "a message between copies did not arrive");
                        }
                        let _ = out.send(NodeMsg::Sent {
                            id,
                            error: sent.err(),
                        });
                    });
                }
                Ok(ClientMsg::Peers { id }) => {
                    let peers = super::mesh::peers(&app);
                    let _ = out.send(NodeMsg::Peers { id, peers });
                }
                Err(_) => {
                    let feature = message_kind(&line).unwrap_or_else(|| "not a message".into());
                    tracing::debug!(%app, %feature, "an application sent a message this node does not know");
                    let _ = out.send(NodeMsg::Unsupported { feature });
                }
            }
        }
        Ok::<(), std::io::Error>(())
    };
    let ended = tokio::select! {
        ended = reading => ended,
        ended = writing => ended,
    };
    if let Some(copies) = pushers().get_mut(&key) {
        copies.retain(|(t, _)| *t != token);
    }
    // A call this copy will now never answer is answered for it, rather than
    // left to run out its wait.
    pending().retain(|_, (carrier, answer)| {
        if *carrier == token {
            let _ = answer.send(Err(format!("{} closed before answering", describe(&key))));
            return false;
        }
        true
    });
    lock(registry).disconnected(&key);
    tracing::info!(%app, process = ?key.process, "application disconnected");
    ended
}

async fn refuse<W: AsyncWrite + Unpin>(write: &mut W, reason: String) -> std::io::Result<()> {
    tracing::info!(%reason, "refused an application");
    write
        .write_all(&encode_line(&NodeMsg::Refused { reason }))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_app_protocol::{AppId, HealthState, LogSpec};

    /// Every test here talks over a socket; one that stops answering fails
    /// instead of holding the test binary forever.
    async fn bounded<F: std::future::Future<Output = ()>>(test: F) {
        tokio::time::timeout(Duration::from_secs(10), test)
            .await
            .expect("the test hung on its socket");
    }

    async fn talk(
        registry: &Arc<Mutex<Registry>>,
    ) -> (
        tokio::task::JoinHandle<std::io::Result<()>>,
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let registry = registry.clone();
        let task =
            tokio::spawn(
                async move { serve_connection(theirs, "Nocturne", &registry, None).await },
            );
        let (read, write) = tokio::io::split(ours);
        (task, BufReader::new(read), write)
    }

    async fn answer<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> NodeMsg {
        let line = read_line(reader).await.unwrap().unwrap();
        decode_line(&line).unwrap()
    }

    fn hello(logs: Option<LogSpec>) -> Vec<u8> {
        encode_line(&ClientMsg::Hello {
            protocol: 7,
            app: AppId::parse("demo").unwrap(),
            version: "1.2".into(),
            pid: 99,
            logs,
            process: None,
            actions: Vec::new(),
            topics: Vec::new(),
            blobs: false,
            devtools: None,
        })
    }

    async fn until(
        registry: &Arc<Mutex<Registry>>,
        check: impl Fn(&super::super::Registered) -> bool,
    ) {
        let id = super::super::Key::main(AppId::parse("demo").unwrap());
        while !lock(registry).get(&id).is_some_and(&check) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn an_application_is_welcomed_registered_and_let_go() {
        bounded(async {
            let registry = Arc::new(Mutex::new(Registry::load(None)));
            let (task, mut reader, mut write) = talk(&registry).await;
            let spec = LogSpec {
                dir: "/tmp/demo".into(),
                file: "demo.log".into(),
            };
            write.write_all(&hello(Some(spec.clone()))).await.unwrap();
            assert_eq!(
                answer(&mut reader).await,
                NodeMsg::Welcome {
                    protocol: PROTOCOL,
                    machine: "Nocturne".into()
                }
            );
            let health = ClientMsg::Health {
                state: HealthState::Degraded,
                detail: Some("no api".into()),
            };
            write.write_all(&encode_line(&health)).await.unwrap();
            write
                .write_all(b"{\"msg\":\"call\",\"name\":\"x\"}\n")
                .await
                .unwrap();
            assert_eq!(
                answer(&mut reader).await,
                NodeMsg::Unsupported {
                    feature: "call".into()
                }
            );
            let app = super::super::Key::main(AppId::parse("demo").unwrap());
            {
                let registry = lock(&registry);
                let entry = registry.get(&app).unwrap();
                assert!(entry.running());
                assert_eq!(entry.health, HealthState::Degraded);
                assert_eq!(entry.logs, Some(spec));
            }
            // Dropping only the write half leaves the stream open while the
            // read half lives; shutting it down is what the node sees as the
            // application going away.
            write.shutdown().await.unwrap();
            task.await.unwrap().unwrap();
            assert!(!lock(&registry).get(&app).unwrap().running());
        })
        .await;
    }

    #[tokio::test]
    async fn a_log_outside_a_bare_name_is_refused_and_not_registered() {
        bounded(async {
            let registry = Arc::new(Mutex::new(Registry::load(None)));
            let (task, mut reader, mut write) = talk(&registry).await;
            let spec = LogSpec {
                dir: "/tmp".into(),
                file: "../../etc/shadow".into(),
            };
            write.write_all(&hello(Some(spec))).await.unwrap();
            assert!(matches!(answer(&mut reader).await, NodeMsg::Refused { .. }));
            task.await.unwrap().unwrap();
            assert!(lock(&registry)
                .get(&super::super::Key::main(AppId::parse("demo").unwrap()))
                .is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn a_first_message_other_than_hello_is_refused() {
        bounded(async {
            let registry = Arc::new(Mutex::new(Registry::load(None)));
            let (task, mut reader, mut write) = talk(&registry).await;
            let health = ClientMsg::Health {
                state: HealthState::Ok,
                detail: None,
            };
            write.write_all(&encode_line(&health)).await.unwrap();
            assert!(matches!(answer(&mut reader).await, NodeMsg::Refused { .. }));
            task.await.unwrap().unwrap();
        })
        .await;
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cursio-apps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_socket_accepts_applications_and_only_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("serve");
        let path = dir.join("apps.sock");
        let registry = Arc::new(Mutex::new(Registry::load(None)));
        let server = tokio::spawn(serve(path.clone(), "Nocturne".into(), registry));
        bounded(async {
            let stream = loop {
                match tokio::net::UnixStream::connect(&path).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            };
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
            let (read, mut write) = tokio::io::split(stream);
            let mut reader = BufReader::new(read);
            write.write_all(&hello(None)).await.unwrap();
            assert!(matches!(answer(&mut reader).await, NodeMsg::Welcome { .. }));
        })
        .await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_sdk_registers_with_the_node_and_reports_its_health() {
        let dir = scratch("sdk");
        let path = dir.join("apps.sock");
        let registry = Arc::new(Mutex::new(Registry::load(None)));
        let server = tokio::spawn(serve(path.clone(), "Nocturne".into(), registry.clone()));
        bounded(async {
            let app = cursio_sdk::App::new("demo")
                .version("3.1")
                .logs(&dir, "demo.log")
                .endpoint(&path)
                .start()
                .unwrap();
            until(&registry, |a| a.running()).await;
            while !app.is_connected() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(app.machine().as_deref(), Some("Nocturne"));
            until(&registry, |a| {
                a.version == "3.1" && a.logs.as_ref().is_some_and(|l| l.file == "demo.log")
            })
            .await;
            app.set_health(HealthState::Failing, Some("no api"))
                .unwrap();
            until(&registry, |a| {
                a.health == HealthState::Failing && a.detail.as_deref() == Some("no api")
            })
            .await;
            drop(app);
            until(&registry, |a| !a.running()).await;
        })
        .await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_running_application_hears_its_new_build_is_ready() {
        let dir = scratch("build-ready");
        let path = dir.join("apps.sock");
        let registry = Arc::new(Mutex::new(Registry::load(None)));
        let server = tokio::spawn(serve(path.clone(), "Nocturne".into(), registry.clone()));
        bounded(async {
            let (tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
            let app = cursio_sdk::App::new("ready-demo")
                .version("1")
                .endpoint(&path)
                .on_update(move |build| {
                    let _ = tx.send(build);
                })
                .start()
                .unwrap();
            while !app.is_connected() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let id = cursio_app_protocol::AppId::parse("ready-demo").unwrap();
            build_ready(&id, "2", &dir.join("versions/2"));
            let build = heard.recv().await.unwrap();
            assert_eq!(build.version, "2");
            assert_eq!(build.dir, dir.join("versions/2"));
            drop(app);
        })
        .await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_application_hears_its_copies_sees_its_peers_and_is_told_why_a_send_failed() {
        let dir = scratch("messages");
        let path = dir.join("apps.sock");
        let registry = super::super::registry().clone();
        let server = tokio::spawn(serve(path.clone(), "Nocturne".into(), registry.clone()));
        bounded(async {
            let (tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
            let app = cursio_sdk::App::new("e2e-messages")
                .version("3")
                .endpoint(&path)
                .on_message("sync", move |message| {
                    let _ = tx.send(message);
                })
                .start()
                .unwrap();
            let key = super::super::Key::main(AppId::parse("e2e-messages").unwrap());
            let topic = cursio_app_protocol::TopicName::parse("sync").unwrap();
            while lock(&registry).listening(&key.app, &topic).is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            // A copy elsewhere sent one: it reaches the handler whole.
            assert_eq!(
                deliver(
                    std::slice::from_ref(&key),
                    "Lightmoon",
                    None,
                    &topic,
                    b"state"
                ),
                1
            );
            let message = heard.recv().await.unwrap();
            assert_eq!(message.from_machine, "Lightmoon");
            assert_eq!(message.payload, b"state");

            // A connected machine that said it runs the application is a peer.
            let (link_tx, _link_rx) = tokio::sync::mpsc::unbounded_channel();
            let token = super::super::mesh::link("e2e-msg-peer", link_tx);
            super::super::mesh::set_presence(
                "e2e-msg-peer",
                vec![cursio_protocol::apps::AppPresence {
                    app: "e2e-messages".into(),
                    running: true,
                    version: "3".into(),
                    topics: vec!["sync".into()],
                }],
            );
            let app = std::sync::Arc::new(app);
            let asking = app.clone();
            let peers = tokio::task::spawn_blocking(move || asking.peers())
                .await
                .unwrap()
                .unwrap();
            assert!(
                peers
                    .iter()
                    .any(|p| p.machine == "e2e-msg-peer" && p.running),
                "{peers:?}"
            );
            super::super::mesh::unlink("e2e-msg-peer", token);

            let asking = app.clone();
            let offline =
                tokio::task::spawn_blocking(move || asking.send("e2e-nowhere", "sync", b"x"))
                    .await
                    .unwrap();
            assert_eq!(
                offline,
                Err(cursio_sdk::Error::Offline {
                    machine: "e2e-nowhere".into()
                })
            );
        })
        .await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_peer_calls_an_action_the_running_application_declared() {
        use serde_json::json;
        let dir = scratch("actions");
        let path = dir.join("apps.sock");
        // The node's own registry: what a peer's call looks in.
        let registry = super::super::registry().clone();
        let server = tokio::spawn(serve(path.clone(), "Nocturne".into(), registry.clone()));
        bounded(async {
            let app = cursio_sdk::App::new("e2e-actions")
                .endpoint(&path)
                .action(
                    "echo",
                    "Answer the arguments",
                    json!({"type": "object"}),
                    Ok,
                )
                .action("boom", "Panic", serde_json::Value::Null, |_| {
                    panic!("no such instance")
                })
                .start()
                .unwrap();
            let key = super::super::Key::main(AppId::parse("e2e-actions").unwrap());
            while !lock(&registry).get(&key).is_some_and(|r| r.running()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            let listed = crate::apps::actions::list("e2e-actions").unwrap();
            let names: Vec<&str> = listed.iter().map(|a| a.name.as_str()).collect();
            assert_eq!(names, ["echo", "boom"]);
            assert!(listed.iter().all(|a| a.running));

            let (echo, boom) = {
                let key = key.clone();
                tokio::task::spawn_blocking(move || {
                    let echo = call(
                        &key,
                        &cursio_app_protocol::ActionName::parse("echo").unwrap(),
                        json!({"instance": "vanilla"}),
                        Duration::from_secs(5),
                    );
                    let boom = call(
                        &key,
                        &cursio_app_protocol::ActionName::parse("boom").unwrap(),
                        serde_json::Value::Null,
                        Duration::from_secs(5),
                    );
                    (echo, boom)
                })
                .await
                .unwrap()
            };
            assert_eq!(echo, Ok(json!({"instance": "vanilla"})));
            assert_eq!(
                boom,
                Err("action boom panicked: no such instance".to_owned())
            );

            let wrong = tokio::task::spawn_blocking(|| {
                crate::apps::actions::call("Nocturne", "e2e-actions", None, "nope", "{}")
            })
            .await
            .unwrap();
            assert_eq!(
                wrong,
                Err("e2e-actions has no action nope; it has: boom, echo".to_owned()),
                "the node's words are the SDK's"
            );
            let unasked = tokio::task::spawn_blocking(|| {
                crate::apps::actions::call("Nocturne", "e2e-actions", None, "echo", "{}")
            })
            .await
            .unwrap();
            assert!(
                unasked
                    .unwrap_err()
                    .contains("wait for this machine's person"),
                "nothing is called before this machine's person allows it"
            );
            crate::diagnostics::clear("app.e2e-actions.actions");
            drop(app);
        })
        .await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_application_names_its_files_and_a_fetch_with_no_copy_around_finds_none() {
        let dir = scratch("blobs");
        let path = dir.join("apps.sock");
        let registry = super::super::registry().clone();
        let server = tokio::spawn(serve(path.clone(), "Nocturne".into(), registry.clone()));
        let held = cursio_app_protocol::BlobHash::new(
            cursio_app_protocol::BlobAlgo::Sha1,
            &"c".repeat(40),
        )
        .unwrap();
        let file = dir.join("mod.jar");
        std::fs::write(&file, b"jar").unwrap();
        bounded(async {
            let (named, wanted) = (file.clone(), held.clone());
            let app = cursio_sdk::App::new("e2e-blobs")
                .endpoint(&path)
                .on_blob(move |hash| (*hash == wanted).then(|| named.clone()))
                .start()
                .unwrap();
            let key = super::super::Key::main(AppId::parse("e2e-blobs").unwrap());
            while !lock(&registry).get(&key).is_some_and(|r| r.running()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(
                lock(&registry).blob_servers(&key.app),
                vec![key.clone()],
                "its hello said it serves files"
            );

            let other = cursio_app_protocol::BlobHash::new(
                cursio_app_protocol::BlobAlgo::Sha1,
                &"d".repeat(40),
            )
            .unwrap();
            let (found, missing) = {
                let (key, held, other) = (key.clone(), held.clone(), other.clone());
                tokio::task::spawn_blocking(move || {
                    let wait = Duration::from_secs(5);
                    (blob_path(&key, &held, wait), blob_path(&key, &other, wait))
                })
                .await
                .unwrap()
            };
            assert_eq!(found, Some(file.clone()));
            assert_eq!(missing, None);

            let entry = cursio_app_protocol::BlobEntry {
                path: "mods/mod.jar".into(),
                hash: other,
                size: 3,
            };
            let dest = dir.join("fetched");
            let report = tokio::task::spawn_blocking(move || {
                app.fetch(std::slice::from_ref(&entry), &dest, |_| {})
            })
            .await
            .unwrap()
            .unwrap();
            assert!(report.fetched.is_empty());
            assert_eq!(report.not_found.len(), 1, "no copy elsewhere holds it");
        })
        .await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A phone's socket is abstract, so any app could connect: the hello is
    /// answered only past the gate, which is told the peer's real uid.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_app_on_an_abstract_socket_is_taken_only_past_the_gate() {
        use std::os::linux::net::SocketAddrExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        bounded(async {
            let name: &'static str =
                Box::leak(format!("cursio-test-apps-{}", std::process::id()).into_boxed_str());
            let allowed = Arc::new(AtomicBool::new(false));
            let gate: super::super::access::Gate = {
                let allowed = allowed.clone();
                Arc::new(move |uid, _app| {
                    // SAFETY: getuid cannot fail and touches no memory.
                    assert_eq!(uid, unsafe { libc::getuid() }, "the kernel's uid");
                    match allowed.load(Ordering::SeqCst) {
                        true => Ok(()),
                        false => Err("not allowed yet".into()),
                    }
                })
            };
            let registry = Arc::new(Mutex::new(Registry::load(None)));
            let server = tokio::spawn(serve_abstract(name, "POCO".into(), registry.clone(), gate));
            let connect = || async {
                let addr = std::os::unix::net::SocketAddr::from_abstract_name(name).unwrap();
                loop {
                    if let Ok(stream) = std::os::unix::net::UnixStream::connect_addr(&addr) {
                        stream.set_nonblocking(true).unwrap();
                        return tokio::net::UnixStream::from_std(stream).unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };
            let (read, mut write) = tokio::io::split(connect().await);
            let mut reader = BufReader::new(read);
            write.write_all(&hello(None)).await.unwrap();
            assert_eq!(
                answer(&mut reader).await,
                NodeMsg::Refused {
                    reason: "not allowed yet".into()
                }
            );
            allowed.store(true, Ordering::SeqCst);
            let (read, mut write) = tokio::io::split(connect().await);
            let mut reader = BufReader::new(read);
            write.write_all(&hello(None)).await.unwrap();
            assert!(matches!(answer(&mut reader).await, NodeMsg::Welcome { .. }));
            server.abort();
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lines_an_application_sends_become_its_log_read_like_any_other() {
        let dir = scratch("sdk-logs");
        let path = dir.join("apps.sock");
        // The process's registry: what a peer's `app:<id>` read resolves
        // through. A test's is never saved.
        let registry = super::super::registry().clone();
        let server = tokio::spawn(serve(path.clone(), "Nocturne".into(), registry.clone()));
        bounded(async {
            let app = cursio_sdk::App::new("sends-logs")
                .version("1")
                .endpoint(&path)
                .start()
                .unwrap();
            app.log(
                cursio_sdk::LogLevel::Info,
                "sends_logs::ui",
                "window opened",
            );
            let tail = || crate::log_tail::Tail {
                log: Some("app:sends-logs".into()),
                max_lines: 10,
                ..Default::default()
            };
            let lines = loop {
                if let Ok(cursio_protocol::diag::DiagResponse::Logs { lines, .. }) =
                    crate::diag_session::own_log(tail())
                {
                    if !lines.is_empty() {
                        break lines;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            assert_eq!(lines.len(), 1);
            assert!(
                lines[0].ends_with("  INFO sends_logs::ui: window opened"),
                "{}",
                lines[0]
            );
            drop(app);
            let key = super::super::Key::main(AppId::parse("sends-logs").unwrap());
            until_gone(&registry, &key).await;
            assert!(
                crate::diag_session::own_log(tail()).is_ok(),
                "still readable once the application is gone"
            );
        })
        .await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn until_gone(registry: &Arc<Mutex<Registry>>, key: &super::super::Key) {
        while lock(registry).get(key).is_some_and(|a| a.running()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
