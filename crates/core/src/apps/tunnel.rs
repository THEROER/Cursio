//! Byte streams between two machines over the `apps` module: what carries a
//! DevTools connection (`docs/devtools-design.md`, rung 5).
//!
//! A stream is numbered by the side that opens it and lives on both ends as
//! one socket each: on the inspecting machine, a connection to the local
//! listener ([`listen`]); on the inspected one, a connection to the
//! application's own DevTools port. Bytes cross as [`AppsWire::TunnelData`],
//! small frames in the metadata class and large ones in bulk, never input.
//!
//! **Flow control.** Each side has at most [`WINDOW`] bytes of a stream in
//! flight, and the other side acknowledges what it has written out
//! ([`AppsWire::TunnelAck`]). A stream whose far socket stalls stops reading
//! its own instead of queueing without bound — what a page streaming a
//! screencast to a slow reader would otherwise do to the link.
//!
//! **Seen while it lasts.** While any stream of a machine into an application
//! is open, the inspected machine shows "<peer> is inspecting <app>" with a
//! Stop button, and every session ends with a log line `cursio report`
//! counts. A stream on the inspected side checks the grant every few seconds
//! and closes when it ends, so Stop and the end of an hour take effect on what
//! is already open, not only on what opens next.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use cursio_app_protocol::{AppId, ProcessName};
use cursio_protocol::apps::AppsWire;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Semaphore};

/// Bytes of one stream in flight one way before the sender waits.
pub const WINDOW: usize = 256 * 1024;

/// The largest frame a stream sends.
pub const FRAME: usize = 32 * 1024;

/// How long opening a stream waits for the far side.
pub const OPEN_WAIT: Duration = Duration::from_secs(15);

/// How often an open stream on the inspected side checks its grant.
const RECHECK: Duration = Duration::from_secs(3);

/// How long after the last stream closes a session counts as over.
const SESSION_LINGER: Duration = Duration::from_secs(60);

/// Where a stream's frames go: the link to a peer. A trait so streams can be
/// tested between two in-memory ends.
pub trait Wire: Send + Sync + 'static {
    fn send(&self, peer: &str, app: &str, wire: AppsWire);
}

/// The mesh's links.
pub struct MeshWire;

impl Wire for MeshWire {
    fn send(&self, peer: &str, app: &str, wire: AppsWire) {
        super::mesh::reply(peer, app, &wire);
    }
}

struct Stream {
    to_socket: mpsc::UnboundedSender<Vec<u8>>,
    credit: Arc<Semaphore>,
    reader: tokio::task::AbortHandle,
}

/// Every open stream, by the peer at its other end and its number.
#[derive(Default)]
pub struct Streams {
    open: Mutex<HashMap<(String, u64), Stream>>,
    opening: Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>,
}

impl Streams {
    fn open(&self) -> MutexGuard<'_, HashMap<(String, u64), Stream>> {
        self.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn opening(&self) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<(), String>>>> {
        self.opening.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Carry `socket` as stream `stream` to `peer`: what it reads goes out,
    /// what arrives is written to it. Returns at once; the stream ends when
    /// either side closes, or when `still` (checked every few seconds) turns
    /// false.
    pub fn carry<S, W>(
        self: &Arc<Self>,
        wire: Arc<W>,
        peer: &str,
        app: &str,
        stream: u64,
        socket: S,
        still: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    ) where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        W: Wire,
    {
        let (mut read, mut write) = tokio::io::split(socket);
        let (to_socket, mut from_peer) = mpsc::unbounded_channel::<Vec<u8>>();
        let credit = Arc::new(Semaphore::new(WINDOW));
        let (peer_s, app_s) = (peer.to_owned(), app.to_owned());

        let reader = {
            let (wire, credit, streams) = (wire.clone(), credit.clone(), self.clone());
            let (peer, app) = (peer_s.clone(), app_s.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; FRAME];
                let mut checked = Instant::now();
                let reason = loop {
                    let read = tokio::time::timeout(RECHECK, read.read(&mut buf)).await;
                    if let Some(still) = &still {
                        if checked.elapsed() >= RECHECK {
                            checked = Instant::now();
                            if !still() {
                                break Some("no longer allowed".to_owned());
                            }
                        }
                    }
                    let n = match read {
                        Err(_) => continue,
                        Ok(Ok(0)) => break None,
                        Ok(Ok(n)) => n,
                        Ok(Err(error)) => break Some(error.to_string()),
                    };
                    let Ok(permits) = credit.acquire_many(n as u32).await else {
                        break None;
                    };
                    permits.forget();
                    wire.send(
                        &peer,
                        &app,
                        AppsWire::TunnelData {
                            stream,
                            data: buf[..n].to_vec(),
                        },
                    );
                };
                wire.send(&peer, &app, AppsWire::TunnelClose { stream, reason });
                streams.closed(&peer, stream);
            })
            .abort_handle()
        };

        {
            let (wire, streams) = (wire.clone(), self.clone());
            let (peer, app) = (peer_s.clone(), app_s.clone());
            tokio::spawn(async move {
                while let Some(data) = from_peer.recv().await {
                    if write.write_all(&data).await.is_err() {
                        break;
                    }
                    wire.send(
                        &peer,
                        &app,
                        AppsWire::TunnelAck {
                            stream,
                            bytes: data.len() as u64,
                        },
                    );
                }
                let _ = write.shutdown().await;
                streams.closed(&peer, stream);
            });
        }

        self.open().insert(
            (peer_s, stream),
            Stream {
                to_socket,
                credit,
                reader,
            },
        );
    }

    fn closed(&self, peer: &str, stream: u64) {
        if let Some(stream) = self.open().remove(&(peer.to_owned(), stream)) {
            stream.reader.abort();
        }
        on_closed(peer, stream);
    }

    /// Wait for the far side to answer that stream `stream` is open.
    pub async fn opened(&self, stream: u64) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.opening().insert(stream, tx);
        let answer = tokio::time::timeout(OPEN_WAIT, rx).await;
        self.opening().remove(&stream);
        match answer {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the link closed before the stream opened".into()),
            Err(_) => Err(format!(
                "the other machine did not open the stream within {} s",
                OPEN_WAIT.as_secs()
            )),
        }
    }

    /// A frame of the stream protocol from `peer`. Opening is not handled
    /// here: it needs the application's settings ([`on_wire`]).
    pub fn on_frame(&self, peer: &str, wire: AppsWire) {
        match wire {
            AppsWire::TunnelOpened { stream, error } => {
                if let Some(answer) = self.opening().remove(&stream) {
                    let _ = answer.send(error.map_or(Ok(()), Err));
                }
            }
            AppsWire::TunnelData { stream, data } => {
                if let Some(open) = self.open().get(&(peer.to_owned(), stream)) {
                    let _ = open.to_socket.send(data);
                }
            }
            AppsWire::TunnelAck { stream, bytes } => {
                if let Some(open) = self.open().get(&(peer.to_owned(), stream)) {
                    let room = WINDOW.saturating_sub(open.credit.available_permits());
                    open.credit.add_permits((bytes as usize).min(room));
                }
            }
            AppsWire::TunnelClose { stream, reason } => {
                if let Some(reason) = reason {
                    tracing::debug!(%peer, stream, %reason, "a DevTools stream closed");
                }
                if let Some(open) = self.open().remove(&(peer.to_owned(), stream)) {
                    open.reader.abort();
                    // Dropping the sender ends the writer, which shuts the
                    // socket's write half after what is queued.
                    drop(open.to_socket);
                }
                on_closed(peer, stream);
            }
            _ => {}
        }
    }

    /// The link to `peer` is gone: every stream to it ends.
    pub fn peer_gone(&self, peer: &str) {
        let gone: Vec<(String, u64)> = self
            .open()
            .keys()
            .filter(|(p, _)| p == peer)
            .cloned()
            .collect();
        for key in gone {
            if let Some(open) = self.open().remove(&key) {
                open.reader.abort();
            }
            on_closed(&key.0, key.1);
        }
    }
}

/// The process's streams, and the runtime they run on: their own, so a
/// module's thread blocked on something else never stalls a stream.
pub fn streams() -> &'static Arc<Streams> {
    static STREAMS: OnceLock<Arc<Streams>> = OnceLock::new();
    STREAMS.get_or_init(Default::default)
}

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("apps-tunnel")
            .enable_all()
            .build()
            .expect("the tunnel runtime starts")
    })
}

/// A tunnel frame from `peer` about `app`, as the `apps` module hands it on.
pub fn on_wire(peer: &str, app: &str, wire: AppsWire) {
    match wire {
        AppsWire::TunnelOpen { stream, process } => {
            let (peer, app) = (peer.to_owned(), app.to_owned());
            runtime().spawn(async move {
                let opened = open_here(&peer, &app, process.as_deref()).await;
                let error = opened.as_ref().err().cloned();
                MeshWire.send(&peer, &app, AppsWire::TunnelOpened { stream, error });
                if let Ok(socket) = opened {
                    let id = AppId::parse(&app).expect("checked when opened");
                    #[cfg(feature = "web-tools")]
                    super::devtools_console::watch(
                        &peer,
                        &id,
                        process.as_deref().and_then(|p| ProcessName::parse(p).ok()),
                    );
                    let still_peer = peer.clone();
                    let still: Box<dyn Fn() -> bool + Send + Sync> =
                        Box::new(move || super::devtools::still_allowed(&still_peer, &id));
                    on_opened(&peer, &app, stream);
                    streams().carry(Arc::new(MeshWire), &peer, &app, stream, socket, Some(still));
                }
            });
        }
        other => streams().on_frame(peer, other),
    }
}

/// Open a connection to `app`'s DevTools here for `peer`, if this machine's
/// person allows it and the port is the application's own.
async fn open_here(
    peer: &str,
    app: &str,
    process: Option<&str>,
) -> Result<tokio::net::TcpStream, String> {
    let id = AppId::parse(app).map_err(|e| e.to_string())?;
    let process = process
        .map(ProcessName::parse)
        .transpose()
        .map_err(|e| e.to_string())?;
    super::devtools::allowed(peer, &id)?;
    let port = super::devtools::endpoint(&id, process.as_ref())?;
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot reach {app}'s DevTools on port {port}: {e}"))
}

/// How long a listener with no connection stays.
const LISTEN_IDLE: Duration = Duration::from_secs(15 * 60);

/// Longest request head the listener reads before forwarding.
const MAX_HEAD: usize = 16 * 1024;

/// Longest DevTools HTTP answer the listener rewrites.
const MAX_ANSWER: usize = 8 * 1024 * 1024;

/// A listener on this machine for one application's DevTools elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listening {
    pub port: u16,
    pub token: String,
}

impl Listening {
    /// What Playwright's `connectOverCDP` and a browser take.
    pub fn http(&self) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, self.token)
    }
}

struct Listener {
    listening: Listening,
    stop: tokio::sync::watch::Sender<bool>,
}

fn listeners() -> MutexGuard<'static, HashMap<String, Listener>> {
    static LISTENERS: OnceLock<Mutex<HashMap<String, Listener>>> = OnceLock::new();
    LISTENERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn listener_key(machine: &str, app: &str, process: Option<&str>) -> String {
    format!("{machine}\n{app}\n{}", process.unwrap_or_default())
}

/// Ask `machine` to open a stream to `app`'s DevTools, answering its number.
async fn open_there<W: Wire>(
    wire: &W,
    machine: &str,
    app: &str,
    process: Option<&str>,
) -> Result<u64, String> {
    let stream: u64 = rand::random();
    let opened = streams().opened(stream);
    wire.send(
        machine,
        app,
        AppsWire::TunnelOpen {
            stream,
            process: process.map(str::to_owned),
        },
    );
    opened.await.map(|()| stream)
}

/// How long `--launch` waits for an application it started to declare
/// DevTools and listen on them.
pub const LAUNCH_WAIT: Duration = Duration::from_secs(30);

/// Open with `open`; when the far side answers that no running process
/// declares DevTools, start the application with them through `launch` and
/// try again every `every` until it does, at most `wait`. Answers what
/// opened and what the launch said. Any other refusal — consent, a port
/// someone else holds — is answered at once: launching would not change it,
/// and restarting an application its person did not allow inspecting
/// would only cost them its state.
pub async fn open_launching<T, O, OF, L, LF>(
    mut open: O,
    launch: L,
    wait: Duration,
    every: Duration,
) -> Result<(T, Option<String>), String>
where
    O: FnMut() -> OF,
    OF: std::future::Future<Output = Result<T, String>>,
    L: FnOnce() -> LF,
    LF: std::future::Future<Output = Result<String, String>>,
{
    use super::devtools::{NOT_DECLARED, NOT_LISTENING};
    match open().await {
        Ok(opened) => return Ok((opened, None)),
        Err(why) if why.contains(NOT_DECLARED) => {}
        Err(why) => return Err(why),
    }
    let launched = launch().await?;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        tokio::time::sleep(every).await;
        match open().await {
            Ok(opened) => return Ok((opened, Some(launched))),
            Err(why) if why.contains(NOT_DECLARED) || why.contains(NOT_LISTENING) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!(
                        "{launched}, but it offered no DevTools within {} s: {why}",
                        wait.as_secs()
                    ));
                }
            }
            Err(why) => return Err(why),
        }
    }
}

/// Listen on 127.0.0.1 for `app`'s DevTools on `machine` (`process`'s when
/// named), on `port` or any free one when 0. The answer's token is the only
/// way in: a request whose path does not start with it is refused here and
/// never reaches the other machine. The same listener is answered again for
/// the same application while it lasts.
pub async fn listen(
    machine: &str,
    app: &str,
    process: Option<&str>,
    port: u16,
) -> Result<Listening, String> {
    let key = listener_key(machine, app, process);
    if let Some(open) = listeners().get(&key) {
        return Ok(open.listening.clone());
    }
    if !super::mesh::is_linked(machine) {
        return Err(format!("{machine} is not connected to this machine"));
    }
    // Asked once before anything listens, so the person sees at once that
    // the other machine refuses or is asking its person.
    let probe = open_there(&MeshWire, machine, app, process).await?;
    MeshWire.send(
        machine,
        app,
        AppsWire::TunnelClose {
            stream: probe,
            reason: None,
        },
    );
    let socket = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let port = socket.local_addr().map_err(|e| e.to_string())?.port();
    let token: String = (0..16)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    let listening = Listening { port, token };
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    listeners().insert(
        key.clone(),
        Listener {
            listening: listening.clone(),
            stop,
        },
    );
    let (machine, app) = (machine.to_owned(), app.to_owned());
    let process = process.map(str::to_owned);
    let serving = listening.clone();
    runtime().spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = tokio::time::timeout(LISTEN_IDLE, socket.accept()) => accepted,
                _ = stopped.changed() => break,
            };
            let Ok(Ok((client, _))) = accepted else {
                break;
            };
            let (machine, app, process, serving) = (
                machine.clone(),
                app.clone(),
                process.clone(),
                serving.clone(),
            );
            tokio::spawn(async move {
                if let Err(why) =
                    serve_client(client, &machine, &app, process.as_deref(), &serving).await
                {
                    tracing::debug!(%machine, %app, %why, "a DevTools connection ended");
                }
            });
        }
        listeners().remove(&key);
        tracing::info!(%machine, %app, "stopped listening for DevTools");
    });
    tracing::info!(
        port = listening.port,
        "listening on 127.0.0.1 for a machine's DevTools"
    );
    Ok(listening)
}

/// Stop listening for `app`'s DevTools on `machine`; whether one listened.
pub fn close(machine: &str, app: &str, process: Option<&str>) -> bool {
    match listeners().remove(&listener_key(machine, app, process)) {
        Some(open) => {
            let _ = open.stop.send(true);
            true
        }
        None => false,
    }
}

async fn reply(client: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let answer = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(answer.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// One connection to the listener: refuse it without the token, forward it
/// with the token taken off.
async fn serve_client(
    mut client: tokio::net::TcpStream,
    machine: &str,
    app: &str,
    process: Option<&str>,
    listening: &Listening,
) -> Result<(), String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        if let Some(end) = find(&head, b"\r\n\r\n") {
            break end + 4;
        }
        if head.len() > MAX_HEAD {
            reply(&mut client, "431 Request Header Fields Too Large", "").await;
            return Err("request head too large".into());
        }
        let n = client.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    };
    let rest = head.split_off(end);
    let head = String::from_utf8_lossy(&head).into_owned();
    let Some(request) = forwardable(&head, listening) else {
        reply(&mut client, "404 Not Found", "not found\n").await;
        return Err("a request without the token".into());
    };
    let stream = match open_there(&MeshWire, machine, app, process).await {
        Ok(stream) => stream,
        Err(why) => {
            reply(&mut client, "403 Forbidden", &format!("{why}\n")).await;
            return Err(why);
        }
    };
    let mut first = request.head.into_bytes();
    first.extend_from_slice(&rest);
    if request.upgrade {
        MeshWire.send(
            machine,
            app,
            AppsWire::TunnelData {
                stream,
                data: first,
            },
        );
        streams().carry(Arc::new(MeshWire), machine, app, stream, client, None);
        return Ok(());
    }
    let (near, mut far) = tokio::io::duplex(WINDOW);
    streams().carry(Arc::new(MeshWire), machine, app, stream, near, None);
    far.write_all(&first).await.map_err(|e| e.to_string())?;
    let mut answer = Vec::new();
    (&mut far)
        .take(MAX_ANSWER as u64)
        .read_to_end(&mut answer)
        .await
        .map_err(|e| e.to_string())?;
    let answer = rewrite_answer(&answer, listening);
    client.write_all(&answer).await.map_err(|e| e.to_string())?;
    let _ = client.shutdown().await;
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A request head as it is forwarded.
#[derive(Debug, PartialEq, Eq)]
struct Forward {
    head: String,
    upgrade: bool,
}

/// `head` with the token taken off its path, `Host` naming this listener and,
/// unless it upgrades to a WebSocket, `Connection: close` so the whole
/// answer can be read and rewritten; `None` when its path lacks the token.
fn forwardable(head: &str, listening: &Listening) -> Option<Forward> {
    let mut lines = head.split("\r\n");
    let request = lines.next()?;
    let mut parts = request.splitn(3, ' ');
    let (method, path, version) = (parts.next()?, parts.next()?, parts.next()?);
    let prefix = format!("/{}", listening.token);
    let path = match path.strip_prefix(&prefix) {
        Some("") => "/",
        Some(rest) if rest.starts_with('/') || rest.starts_with('?') => rest,
        _ => return None,
    };
    let mut upgrade = false;
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let name = line
            .split(':')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match name.as_str() {
            "host" => {}
            "upgrade" => {
                upgrade |= line.to_ascii_lowercase().contains("websocket");
                headers.push(line.to_owned());
            }
            _ => headers.push(line.to_owned()),
        }
    }
    if !upgrade {
        headers.retain(|h| !h.to_ascii_lowercase().starts_with("connection:"));
        headers.push("Connection: close".into());
    }
    let mut out = format!(
        "{method} {path} {version}\r\nHost: 127.0.0.1:{}\r\n",
        listening.port
    );
    for header in headers {
        out.push_str(&header);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    Some(Forward { head: out, upgrade })
}

/// DevTools URLs in `body` made to come back through this listener, token
/// included: `ws://127.0.0.1:<any>/devtools/…`, `localhost:<any>/devtools/…`
/// and the front end's `"/devtools/…` paths.
fn rewrite_urls(body: &str, listening: &Listening) -> String {
    let target = format!("127.0.0.1:{}/{}/devtools/", listening.port, listening.token);
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while !rest.is_empty() {
        let host = ["127.0.0.1:", "localhost:"]
            .iter()
            .filter_map(|h| rest.find(h).map(|at| (at, h.len())))
            .min();
        let Some((at, len)) = host else {
            out.push_str(rest);
            break;
        };
        let after = &rest[at + len..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with("/devtools/") {
            out.push_str(&rest[..at]);
            out.push_str(&target);
            rest = &after[digits + "/devtools/".len()..];
        } else {
            out.push_str(&rest[..at + len]);
            rest = after;
        }
    }
    out.replace("\"/devtools/", &format!("\"/{}/devtools/", listening.token))
}

/// An HTTP answer with its body's DevTools URLs rewritten and its length
/// set to match; an answer without a length is passed on as it is.
fn rewrite_answer(answer: &[u8], listening: &Listening) -> Vec<u8> {
    let Some(end) = find(answer, b"\r\n\r\n") else {
        return answer.to_vec();
    };
    let head = String::from_utf8_lossy(&answer[..end]);
    if !head.to_ascii_lowercase().contains("content-length:") {
        return answer.to_vec();
    }
    let Ok(body) = std::str::from_utf8(&answer[end + 4..]) else {
        return answer.to_vec();
    };
    let body = rewrite_urls(body, listening);
    let mut out = String::new();
    for line in head.split("\r\n") {
        if line.to_ascii_lowercase().starts_with("content-length:") {
            out.push_str(&format!("Content-Length: {}", body.len()));
        } else {
            out.push_str(line);
        }
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.push_str(&body);
    out.into_bytes()
}

/// One machine inspecting one application here: its open streams, and when
/// it began and last had one.
struct Session {
    app: String,
    streams: Vec<u64>,
    began: Instant,
    /// Bumped whenever its last stream closes, so a stale end-of-session
    /// timer can tell it was overtaken.
    epoch: u64,
}

fn sessions() -> MutexGuard<'static, HashMap<String, Session>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    SESSIONS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn session_key(peer: &str, app: &str) -> String {
    format!("{peer}\n{app}")
}

fn active_problem(peer: &str, app: &str) -> String {
    format!("app.{app}.devtools.active.{peer}")
}

/// A stream from `peer` into `app` opened on this, the inspected, machine.
fn on_opened(peer: &str, app: &str, stream: u64) {
    let mut sessions = sessions();
    let session = sessions.entry(session_key(peer, app)).or_insert_with(|| {
        tracing::warn!(%peer, %app, "devtools session began: {peer} is inspecting {app}");
        crate::diagnostics::raise(crate::diagnostics::Problem {
            id: active_problem(peer, app),
            severity: crate::diagnostics::Severity::Warning,
            summary: format!("{peer} is inspecting {app}"),
            hint: Some("it can run any code inside it while this lasts".into()),
            actions: vec![crate::diagnostics::ProblemAction {
                label: "Stop".into(),
                edit: crate::config::ConfigEdit::StopDevtools {
                    app: app.to_owned(),
                },
            }],
            since: std::time::SystemTime::now(),
        });
        Session {
            app: app.to_owned(),
            streams: Vec::new(),
            began: Instant::now(),
            epoch: 0,
        }
    });
    session.streams.push(stream);
}

/// Stream `stream` with `peer` closed, on either side; on the inspected side
/// the session ends a while after its last stream.
fn on_closed(peer: &str, stream: u64) {
    let mut ending = None;
    {
        let mut sessions = sessions();
        for (key, session) in sessions.iter_mut() {
            if !key.starts_with(&format!("{peer}\n")) {
                continue;
            }
            let before = session.streams.len();
            session.streams.retain(|s| *s != stream);
            if before != session.streams.len() && session.streams.is_empty() {
                session.epoch += 1;
                ending = Some((key.clone(), session.epoch));
            }
        }
    }
    let Some((key, epoch)) = ending else {
        return;
    };
    runtime().spawn(async move {
        tokio::time::sleep(SESSION_LINGER).await;
        let mut sessions = sessions();
        let over = sessions
            .get(&key)
            .is_some_and(|s| s.epoch == epoch && s.streams.is_empty());
        if over {
            if let Some(session) = sessions.remove(&key) {
                let peer = key.split('\n').next().unwrap_or_default();
                let secs = session.began.elapsed().as_secs();
                tracing::warn!(
                    %peer,
                    app = %session.app,
                    secs,
                    "devtools session ended: {peer} inspected {} for {secs} s",
                    session.app
                );
                crate::diagnostics::clear(&active_problem(peer, &session.app));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listening() -> Listening {
        Listening {
            port: 9333,
            token: "abc123".into(),
        }
    }

    #[test]
    fn a_request_without_the_token_is_not_forwarded() {
        let head = "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:9333\r\n\r\n";
        assert_eq!(forwardable(head, &listening()), None);
        let head = "GET /abc123x/json HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(
            forwardable(head, &listening()),
            None,
            "a longer token is another token"
        );
    }

    #[test]
    fn the_token_comes_off_and_a_plain_request_is_closed_after_its_answer() {
        let head = "GET /abc123/json/version HTTP/1.1\r\nHost: evil.example\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n";
        let forward = forwardable(head, &listening()).unwrap();
        assert!(!forward.upgrade);
        assert_eq!(
            forward.head,
            "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:9333\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn a_websocket_upgrade_keeps_its_headers() {
        let head = "GET /abc123/devtools/page/X HTTP/1.1\r\nHost: 127.0.0.1:9333\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: k\r\n\r\n";
        let forward = forwardable(head, &listening()).unwrap();
        assert!(forward.upgrade);
        assert!(forward
            .head
            .starts_with("GET /devtools/page/X HTTP/1.1\r\n"));
        assert!(forward.head.contains("Connection: Upgrade\r\n"));
    }

    #[test]
    fn devtools_urls_in_an_answer_come_back_through_the_listener_with_the_token() {
        let body = r#"[{"webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/A","devtoolsFrontendUrl":"/devtools/inspector.html?ws=localhost:9222/devtools/page/A","url":"http://127.0.0.1:8080/app"}]"#;
        let answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let out = String::from_utf8(rewrite_answer(answer.as_bytes(), &listening())).unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap();
        assert!(body.contains("ws://127.0.0.1:9333/abc123/devtools/page/A"));
        assert!(body.contains(
            "\"/abc123/devtools/inspector.html?ws=127.0.0.1:9333/abc123/devtools/page/A\""
        ));
        assert!(
            body.contains("http://127.0.0.1:8080/app"),
            "other URLs are left alone"
        );
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
    }

    /// One end's frames handed to the other end, as the mesh would.
    struct Cross {
        to: Arc<Streams>,
        from: &'static str,
    }

    impl Wire for Cross {
        fn send(&self, _peer: &str, _app: &str, wire: AppsWire) {
            self.to.on_frame(self.from, wire);
        }
    }

    fn pair(
        stream: u64,
    ) -> (
        Arc<Streams>,
        tokio::io::DuplexStream,
        tokio::io::DuplexStream,
    ) {
        let (a, b) = (Arc::new(Streams::default()), Arc::new(Streams::default()));
        let (a_near, a_far) = tokio::io::duplex(FRAME);
        let (b_near, b_far) = tokio::io::duplex(FRAME);
        let to_b = Arc::new(Cross {
            to: b.clone(),
            from: "A",
        });
        let to_a = Arc::new(Cross {
            to: a.clone(),
            from: "B",
        });
        a.carry(to_b, "B", "demo", stream, a_near, None);
        b.carry(to_a, "A", "demo", stream, b_near, None);
        (a, a_far, b_far)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bytes_cross_a_stream_whole_and_in_order() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let (_a, mut a_far, mut b_far) = pair(1);
            let sent: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
            let writing = {
                let sent = sent.clone();
                tokio::spawn(async move {
                    a_far.write_all(&sent).await.unwrap();
                    a_far.shutdown().await.unwrap();
                    a_far
                })
            };
            let mut got = Vec::new();
            b_far.read_to_end(&mut got).await.unwrap();
            writing.await.unwrap();
            assert_eq!(got, sent);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_reader_stops_the_sender_at_the_window() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let (a, mut a_far, _b_far) = pair(2);
            let writing = tokio::spawn(async move {
                let chunk = vec![7u8; FRAME];
                loop {
                    if a_far.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
            });
            tokio::time::sleep(Duration::from_millis(500)).await;
            let credit = a.open().get(&("B".to_owned(), 2)).unwrap().credit.clone();
            assert_eq!(
                credit.available_permits(),
                0,
                "the window is spent and not refilled"
            );
            assert!(
                !writing.is_finished(),
                "the writer is held back, not failed"
            );
            writing.abort();
        })
        .await
        .unwrap();
    }

    /// `open` answering `answers` in turn (the last one repeated), counting
    /// the calls.
    fn answering(
        answers: Vec<Result<u16, String>>,
        calls: Arc<Mutex<usize>>,
    ) -> impl FnMut() -> std::future::Ready<Result<u16, String>> {
        move || {
            let mut n = calls.lock().unwrap();
            let answer = answers[(*n).min(answers.len() - 1)].clone();
            *n += 1;
            std::future::ready(answer)
        }
    }

    const UNDECLARED: &str = "no running process of demo here declares a DevTools endpoint";
    const SECOND: Duration = Duration::from_secs(1);

    #[tokio::test(start_paused = true)]
    async fn launch_is_not_asked_when_devtools_are_already_offered() {
        let calls = Arc::new(Mutex::new(0));
        let opened = open_launching(
            answering(vec![Ok(9333)], calls.clone()),
            || async { panic!("nothing to launch") },
            LAUNCH_WAIT,
            SECOND,
        )
        .await;
        assert_eq!(opened, Ok((9333, None)));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn launching_waits_through_undeclared_and_unlistened_until_offered() {
        let calls = Arc::new(Mutex::new(0));
        let opened = open_launching(
            answering(
                vec![
                    Err(UNDECLARED.into()),
                    Err(UNDECLARED.into()),
                    Err("nothing listens on demo's declared DevTools port 9222".into()),
                    Ok(9333),
                ],
                calls.clone(),
            ),
            || async { Ok("started demo 1.0".to_owned()) },
            LAUNCH_WAIT,
            SECOND,
        )
        .await;
        assert_eq!(opened, Ok((9333, Some("started demo 1.0".to_owned()))));
        assert_eq!(*calls.lock().unwrap(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn launching_gives_up_after_the_wait_saying_why() {
        let calls = Arc::new(Mutex::new(0));
        let opened = open_launching(
            answering(vec![Err(UNDECLARED.into())], calls.clone()),
            || async { Ok("started demo 1.0".to_owned()) },
            Duration::from_secs(5),
            SECOND,
        )
        .await;
        let why = opened.unwrap_err();
        assert!(why.starts_with("started demo 1.0, but it offered no DevTools within 5 s"));
        assert_eq!(*calls.lock().unwrap(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_answered_without_launching() {
        let calls = Arc::new(Mutex::new(0));
        let opened: Result<(u16, _), _> = open_launching(
            answering(
                vec![Err("DevTools of demo are off on that machine".into())],
                calls,
            ),
            || async { panic!("a refusal must not launch") },
            LAUNCH_WAIT,
            SECOND,
        )
        .await;
        assert_eq!(
            opened,
            Err("DevTools of demo are off on that machine".into())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_launch_is_the_answer() {
        let calls = Arc::new(Mutex::new(0));
        let opened = open_launching(
            answering(vec![Err(UNDECLARED.into())], calls.clone()),
            || async { Err("demo asks to run as an administrator".to_owned()) },
            LAUNCH_WAIT,
            SECOND,
        )
        .await;
        assert_eq!(opened, Err("demo asks to run as an administrator".into()));
        assert_eq!(*calls.lock().unwrap(), 1);
    }
}
