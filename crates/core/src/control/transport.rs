//! Where the control socket lives, who may open it, and how lines cross it.
//!
//! **Unix**: `$XDG_RUNTIME_DIR/cursio/control.sock`. The runtime directory is
//! already the user's alone; the `cursio` directory inside it is made 0700
//! and the socket 0600 anyway, because the fallback when that variable is
//! unset — a directory under the shared temp dir — is not, and there the
//! directory is refused unless this user owns it and nobody else can enter.
//!
//! **Windows**: `\\.\pipe\cursio-control-<user>`, created with a DACL naming
//! this user's SID and SYSTEM and nobody else, a medium integrity label, and
//! remote clients rejected. The default pipe DACL grants Everyone read, and an
//! elevated node's pipe would otherwise carry a high label that the
//! unelevated app cannot write through — the node self-elevates, the app does
//! not, and they must still talk.
//!
//! **One node per user.** Binding first tries to connect: an answer means a
//! node is already running, and a second one would fight it for the mesh port
//! and the capture portal. No answer means a stale socket from a crash, which
//! is removed.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::{Command, Reply, Request, Response};

/// Longest line either side accepts. A status reply is a few kilobytes; a
/// megabyte means something is wrong, and buffering it forever would be worse.
pub const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("control socket i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("control message is not valid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("control line exceeds {MAX_LINE} bytes")]
    TooLong,
    #[error("the node closed the control connection")]
    Closed,
    #[error("a cursio node is already running for this user (control socket {0})")]
    AlreadyRunning(String),
    #[error("no cursio node is running (nothing at {0}); start one with `cursio mesh`")]
    NotRunning(String),
    #[error("{0} is not private to this user; remove it or set XDG_RUNTIME_DIR")]
    NotPrivate(String),
}

/// The control endpoint for this user.
pub fn endpoint() -> Result<PathBuf, ControlError> {
    #[cfg(unix)]
    {
        Ok(unix::socket_dir()?.join("control.sock"))
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
        let safe: String = user
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Ok(PathBuf::from(format!(r"\\.\pipe\cursio-control-{safe}")))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(ControlError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no control socket on this platform",
        )))
    }
}

/// Read one line, bounded.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<String>, ControlError> {
    let mut buf = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return match buf.is_empty() {
                true => Ok(None),
                false => Err(ControlError::Closed),
            };
        }
        match chunk.iter().position(|b| *b == b'\n') {
            Some(end) => {
                buf.extend_from_slice(&chunk[..end]);
                reader.consume(end + 1);
                if buf.len() > MAX_LINE {
                    return Err(ControlError::TooLong);
                }
                let line = String::from_utf8(buf)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                return Ok(Some(line));
            }
            None => {
                let n = chunk.len();
                buf.extend_from_slice(chunk);
                reader.consume(n);
                if buf.len() > MAX_LINE {
                    return Err(ControlError::TooLong);
                }
            }
        }
    }
}

pub async fn write_json<W: AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), ControlError> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

/// How often a subscribed connection looks at the status even when nothing
/// here said it changed: latency, problems and pairing expiry move on their
/// own.
const PUSH_REFRESH: std::time::Duration = std::time::Duration::from_secs(2);

/// Answer requests on one connection until the client goes away, and push the
/// status after it subscribes.
///
/// Lines are read on a task of their own, because a half-read line cannot be
/// abandoned when a push wins the race and picked up again later. A push is
/// sent only when the status differs from the last one pushed, so an idle
/// node sends nothing.
///
/// A malformed line gets an error reply with id 0 rather than a dropped
/// connection, so a client with a newer command than this node knows learns
/// why it was not understood.
pub async fn serve_connection<S>(stream: S, state: super::NodeState) -> Result<(), ControlError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read, mut write) = tokio::io::split(stream);
    let (lines_tx, mut lines) = tokio::sync::mpsc::channel(16);
    let reader = tokio::spawn(async move {
        let mut read = BufReader::new(read);
        loop {
            let line = read_line(&mut read).await;
            let end = !matches!(line, Ok(Some(_)));
            if lines_tx.send(line).await.is_err() || end {
                break;
            }
        }
    });

    let mut revisions = state.revisions();
    let mut refresh = tokio::time::interval(PUSH_REFRESH);
    let mut subscribed = false;
    // Held while this connection is an app whose tray stands in for the
    // remote-screen views' icons; dropping it on the way out brings them back.
    let mut _tray: Option<crate::screen_views::TrayGuard> = None;
    let mut last_pushed: Option<super::Status> = None;
    // Answers to commands that wait on a peer, written whenever they arrive;
    // ids let the client match them however late.
    let (late_tx, mut late) = tokio::sync::mpsc::unbounded_channel::<Response>();
    let result = loop {
        tokio::select! {
            Some(response) = late.recv() => {
                if let Err(e) = write_json(&mut write, &response).await {
                    break Err(e);
                }
            }
            line = lines.recv() => {
                let line = match line {
                    Some(Ok(Some(line))) => line,
                    Some(Ok(None)) | None => break Ok(()),
                    Some(Err(e)) => break Err(e),
                };
                let response = match serde_json::from_str::<Request>(&line) {
                    Ok(request) if request.command.waits_on_peer() => {
                        let state = state.clone();
                        let late_tx = late_tx.clone();
                        tokio::spawn(async move {
                            let reply = super::handle_waiting(&state, request.command).await;
                            let _ = late_tx.send(Response { id: request.id, reply });
                        });
                        continue;
                    }
                    Ok(request) => {
                        subscribed |= request.command == Command::Subscribe;
                        if request.command == Command::TrayAttached && _tray.is_none() {
                            _tray = Some(crate::screen_views::TRAYS.attach());
                        }
                        Response {
                            id: request.id,
                            reply: state.handle(request.command),
                        }
                    }
                    Err(e) => Response {
                        id: 0,
                        reply: Reply::Error {
                            message: format!("request not understood: {e}"),
                        },
                    },
                };
                if let Err(e) = write_json(&mut write, &response).await {
                    break Err(e);
                }
                if subscribed {
                    if let Err(e) = push(&mut write, &state, &mut last_pushed).await {
                        break Err(e);
                    }
                }
            }
            changed = revisions.changed(), if subscribed => {
                if changed.is_err() {
                    break Ok(());
                }
                if let Err(e) = push(&mut write, &state, &mut last_pushed).await {
                    break Err(e);
                }
            }
            _ = refresh.tick(), if subscribed => {
                if let Err(e) = push(&mut write, &state, &mut last_pushed).await {
                    break Err(e);
                }
            }
        }
    };
    reader.abort();
    result
}

/// Push the status if it differs from what this connection last received.
async fn push<W: AsyncWrite + Unpin>(
    write: &mut W,
    state: &super::NodeState,
    last: &mut Option<super::Status>,
) -> Result<(), ControlError> {
    let status = state.status();
    if last.as_ref() == Some(&status) {
        return Ok(());
    }
    write_json(
        write,
        &Response {
            id: super::PUSH_ID,
            reply: Reply::Status(Box::new(status.clone())),
        },
    )
    .await?;
    *last = Some(status);
    Ok(())
}

type Pending = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Reply>>>,
>;

/// A connection to a running node, shared by whoever needs it.
///
/// Requests go out through one writer task and replies are matched back by
/// id, so any number may be in flight: a status request never waits behind a
/// slow one. Pushes (id 0) arrive on the receiver [`Client::start`] returns.
/// Cheap to clone — the LeavePulse launcher's daemon client, in this
/// vocabulary.
#[derive(Clone)]
pub struct Client {
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pending: Pending,
    writer: tokio::sync::mpsc::Sender<Request>,
}

impl Client {
    /// Start the reader and writer tasks for `stream`. The receiver yields
    /// what the node pushes, and closes when the connection does.
    pub fn start<S>(stream: S) -> (Self, tokio::sync::mpsc::Receiver<Reply>)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, mut write) = tokio::io::split(stream);
        let (writer, mut outgoing) = tokio::sync::mpsc::channel::<Request>(64);
        let (pushes_tx, pushes) = tokio::sync::mpsc::channel(64);
        let pending: Pending = std::sync::Arc::default();

        let waiting = pending.clone();
        tokio::spawn(async move {
            let mut read = BufReader::new(read);
            while let Ok(Some(line)) = read_line(&mut read).await {
                let Ok(response) = serde_json::from_str::<Response>(&line) else {
                    tracing::debug!("unreadable line from the node; skipping it");
                    continue;
                };
                if response.id == super::PUSH_ID {
                    if let Reply::Error { message } = &response.reply {
                        tracing::debug!(%message, "the node could not read a request");
                        continue;
                    }
                    // A full channel means nobody is reading pushes; dropping
                    // one loses nothing the next push will not carry.
                    let _ = pushes_tx.try_send(response.reply);
                    continue;
                }
                let waiter = waiting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&response.id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(response.reply);
                }
            }
            // Every request still waiting learns the connection is gone.
            waiting.lock().unwrap_or_else(|p| p.into_inner()).clear();
        });
        tokio::spawn(async move {
            while let Some(request) = outgoing.recv().await {
                if write_json(&mut write, &request).await.is_err() {
                    break;
                }
            }
        });

        (
            Self {
                next_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
                pending,
                writer,
            },
            pushes,
        )
    }

    /// False once the connection has broken.
    pub fn is_connected(&self) -> bool {
        !self.writer.is_closed()
    }

    pub async fn request(&self, command: Command) -> Result<Reply, ControlError> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        if self.writer.send(Request { id, command }).await.is_err() {
            self.forget(id);
            return Err(ControlError::Closed);
        }
        rx.await.map_err(|_| {
            self.forget(id);
            ControlError::Closed
        })
    }

    fn forget(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }
}

#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;
#[cfg(windows)]
pub type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Connect to the node at `endpoint`.
#[cfg(any(unix, windows))]
pub async fn connect(
    endpoint: &Path,
) -> Result<(Client, tokio::sync::mpsc::Receiver<Reply>), ControlError> {
    let not_running = |_| ControlError::NotRunning(endpoint.display().to_string());
    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(endpoint)
        .await
        .map_err(not_running)?;
    #[cfg(windows)]
    let stream = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(endpoint.as_os_str())
        .map_err(not_running)?;
    Ok(Client::start(stream))
}

/// Serve the control socket until the process ends. Each connection is its
/// own task, so a client that stops reading holds up nobody else.
#[cfg(unix)]
pub async fn serve(endpoint: PathBuf, state: super::NodeState) -> Result<(), ControlError> {
    let listener = unix::bind(&endpoint).await?;
    tracing::info!(socket = %endpoint.display(), "control socket listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_connection(stream, state).await {
                tracing::debug!(error = %e, "control client ended");
            }
        });
    }
}

#[cfg(windows)]
pub async fn serve(endpoint: PathBuf, state: super::NodeState) -> Result<(), ControlError> {
    let mut pending = windows::create(&endpoint, true)?;
    tracing::info!(pipe = %endpoint.display(), "control pipe listening");
    loop {
        pending.connect().await?;
        let connected = std::mem::replace(&mut pending, windows::create(&endpoint, false)?);
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_connection(connected, state).await {
                tracing::debug!(error = %e, "control client ended");
            }
        });
    }
}

#[cfg(not(any(unix, windows)))]
pub async fn serve(_endpoint: PathBuf, _state: super::NodeState) -> Result<(), ControlError> {
    Err(ControlError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no control socket on this platform",
    )))
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use super::ControlError;

    pub(super) fn socket_dir() -> Result<PathBuf, ControlError> {
        let (root, shared) = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => (PathBuf::from(dir), false),
            _ => (std::env::temp_dir(), true),
        };
        // SAFETY: getuid cannot fail and touches no memory.
        let uid = unsafe { libc::getuid() };
        let dir = match shared {
            true => root.join(format!("cursio-{uid}")),
            false => root.join("cursio"),
        };
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let meta = std::fs::symlink_metadata(&dir)?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(ControlError::NotPrivate(dir.display().to_string()));
        }
        Ok(dir)
    }

    pub(super) async fn bind(path: &Path) -> Result<tokio::net::UnixListener, ControlError> {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(ControlError::AlreadyRunning(path.display().to_string()));
        }
        match std::fs::remove_file(path) {
            Ok(()) => tracing::debug!(socket = %path.display(), "removed a stale control socket"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }
}

#[cfg(windows)]
pub(crate) mod windows {
    use std::path::Path;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::ControlError;

    fn os_error(e: windows::core::Error) -> ControlError {
        ControlError::Io(std::io::Error::other(e))
    }

    /// This process's user SID in `S-1-5-…` form. The token's *user*, not
    /// its owner: an elevated token's default owner is Administrators, which
    /// the unelevated app is not acting as.
    fn user_sid() -> Result<String, ControlError> {
        // SAFETY: every handle opened here is closed before returning, the
        // buffer is sized by the first GetTokenInformation call, and the SID
        // string is freed with LocalFree as the API requires.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(os_error)?;
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let got = GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                len,
                &mut len,
            );
            let _ = CloseHandle(token);
            got.map_err(os_error)?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(os_error)?;
            let sid = text
                .to_string()
                .map_err(|e| ControlError::Io(std::io::Error::other(e)));
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            sid
        }
    }

    pub(crate) fn create(endpoint: &Path, first: bool) -> Result<NamedPipeServer, ControlError> {
        // Protected DACL: this user and SYSTEM, full access; nobody else. The
        // medium no-write-up label lets an unelevated client in when the node
        // itself is elevated.
        let sddl = format!("D:P(A;;GA;;;{})(A;;GA;;;SY)S:(ML;;NW;;;ME)", user_sid()?);
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the
        // descriptor is freed with LocalFree once the pipe has copied it.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .map_err(os_error)?;
        }
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: `attributes` points at a valid descriptor for the duration
        // of the call, which copies it into the pipe object.
        let created = unsafe {
            options.create_with_security_attributes_raw(
                endpoint.as_os_str(),
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
        };
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        created.map_err(
            |e| match first && e.kind() == std::io::ErrorKind::PermissionDenied {
                true => ControlError::AlreadyRunning(endpoint.display().to_string()),
                false => e.into(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Overrides};
    use crate::pairing_window::PairingWindow;

    fn node(dir: &Path) -> super::super::NodeState {
        super::super::NodeState::new(
            &crate::NodeIdentity {
                peer_id: cursio_protocol::PeerId::new_v4(),
                display_name: "desk".into(),
            },
            dir.join("config.toml"),
            Overrides::default(),
            Config::default(),
            PairingWindow::closed(),
            std::sync::Arc::new(|_: &Config, _: &Config| {}),
        )
    }

    #[tokio::test]
    async fn requests_round_trip_over_an_in_memory_stream() {
        let dir = std::env::temp_dir().join(format!("cursio-ctl-mem-{}", std::process::id()));
        let (client_side, server_side) = tokio::io::duplex(4096);
        tokio::spawn(serve_connection(server_side, node(&dir)));
        let (client, _pushes) = Client::start(client_side);
        assert_eq!(
            client.request(Command::Ping).await.unwrap(),
            Reply::Pong {
                version: super::super::CONTROL_VERSION
            }
        );
        let Reply::Status(status) = client.request(Command::Status).await.unwrap() else {
            panic!("expected status");
        };
        assert_eq!(status.node.name, "desk");
    }

    /// Several requests in flight on one connection each get their own
    /// answer, whichever order they were sent in.
    #[tokio::test]
    async fn concurrent_requests_on_one_connection_each_get_their_own_reply() {
        let dir = std::env::temp_dir().join(format!("cursio-ctl-many-{}", std::process::id()));
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(serve_connection(server_side, node(&dir)));
        let (client, _pushes) = Client::start(client_side);
        let (ping, status) = tokio::join!(
            client.request(Command::Ping),
            client.request(Command::Status)
        );
        assert!(matches!(ping.unwrap(), Reply::Pong { .. }));
        assert!(matches!(status.unwrap(), Reply::Status(_)));
    }

    /// A request waiting on a phone that has not answered yet does not hold
    /// up a quick one sent after it on the same connection.
    #[tokio::test]
    async fn a_request_waiting_on_a_phone_does_not_hold_up_the_next_one() {
        let dir = std::env::temp_dir().join(format!("cursio-ctl-wait-{}", std::process::id()));
        let phone = cursio_protocol::PeerId::from_bytes([0x61; 16]);
        let (_outbox, _token) = crate::messages_hub::hub().register(phone, "slowphone".into());
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(serve_connection(server_side, node(&dir)));
        let (client, _pushes) = Client::start(client_side);
        let waiting = client.request(Command::ListConversations {
            peer: "slowphone".into(),
            limit: 10,
            before_ms: None,
        });
        tokio::pin!(waiting);
        let ping = tokio::select! {
            _ = &mut waiting => panic!("nobody answered for the phone, yet it returned"),
            ping = client.request(Command::Ping) => ping,
        };
        assert!(matches!(ping.unwrap(), Reply::Pong { .. }));
    }

    /// A subscriber is sent the status at once and again when something it
    /// shows changes — and nothing when nothing did.
    #[tokio::test]
    async fn a_subscriber_is_pushed_the_status_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("cursio-ctl-push-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let state = node(&dir);
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(serve_connection(server_side, state.clone()));
        let (client, mut pushes) = Client::start(client_side);
        assert_eq!(
            client.request(Command::Subscribe).await.unwrap(),
            Reply::Subscribed
        );
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), pushes.recv())
            .await
            .expect("no initial push")
            .unwrap();
        assert!(matches!(first, Reply::Status(_)));

        state.set_known(vec![super::super::KnownPeerView {
            id: cursio_protocol::PeerId::new_v4(),
            name: "laptop".into(),
            presence: super::super::Presence::Offline,
        }]);
        let Reply::Status(status) =
            tokio::time::timeout(std::time::Duration::from_secs(1), pushes.recv())
                .await
                .expect("the change was not pushed")
                .unwrap()
        else {
            panic!("expected status");
        };
        assert_eq!(status.peers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_line_it_cannot_read_is_answered_not_dropped() {
        let dir = std::env::temp_dir().join(format!("cursio-ctl-bad-{}", std::process::id()));
        let (client_side, server_side) = tokio::io::duplex(4096);
        tokio::spawn(serve_connection(server_side, node(&dir)));
        let (read, mut write) = tokio::io::split(client_side);
        let mut read = BufReader::new(read);
        write
            .write_all(b"{\"id\":3,\"cmd\":\"fly\"}\n")
            .await
            .unwrap();
        let reply: Response =
            serde_json::from_str(&read_line(&mut read).await.unwrap().unwrap()).unwrap();
        assert!(matches!(reply.reply, Reply::Error { .. }));
        // And the connection is still usable afterwards.
        write_json(
            &mut write,
            &Request {
                id: 4,
                command: Command::Ping,
            },
        )
        .await
        .unwrap();
        let reply: Response =
            serde_json::from_str(&read_line(&mut read).await.unwrap().unwrap()).unwrap();
        assert_eq!(reply.id, 4);
    }

    #[tokio::test]
    async fn an_overlong_line_is_refused() {
        let (mut a, b) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = a.write_all(&vec![b'x'; MAX_LINE + 10]).await;
        });
        let mut reader = BufReader::new(b);
        assert!(matches!(
            read_line(&mut reader).await,
            Err(ControlError::TooLong)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_socket_is_private_and_a_second_node_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cursio-ctl-sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let server = tokio::spawn(serve(path.clone(), node(&dir)));
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let (client, _pushes) = connect(&path).await.unwrap();
        assert!(matches!(
            client.request(Command::Ping).await.unwrap(),
            Reply::Pong { .. }
        ));
        assert!(matches!(
            serve(path.clone(), node(&dir)).await,
            Err(ControlError::AlreadyRunning(_))
        ));
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
