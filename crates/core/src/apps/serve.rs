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
            if let Err(error) = serve_connection(stream, &machine, &registry).await {
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
            if let Err(error) = serve_connection(connected, &machine, &registry).await {
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

async fn serve_connection<S: AsyncRead + AsyncWrite>(
    stream: S,
    machine: &str,
    registry: &Arc<Mutex<Registry>>,
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
    let (app, version, pid, logs, protocol) = match decode_line::<ClientMsg>(&first) {
        Ok(ClientMsg::Hello {
            protocol,
            app,
            version,
            pid,
            logs,
        }) => (app, version, pid, logs, protocol),
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
    lock(registry).connected(&app, version.clone(), pid, logs.clone());
    tracing::info!(%app, %version, pid, logs = ?logs.as_ref().map(|l| l.dir.join(&l.file)), "application connected");
    let welcome = NodeMsg::Welcome {
        protocol: protocol.min(PROTOCOL),
        machine: machine.to_owned(),
    };
    let ended = async {
        write.write_all(&encode_line(&welcome)).await?;
        while let Some(line) = read_line(&mut reader).await? {
            match decode_line::<ClientMsg>(&line) {
                Ok(ClientMsg::Health { state, detail }) => {
                    let detail =
                        detail.map(|d| d.chars().take(cursio_app_protocol::MAX_DETAIL).collect());
                    let mut registry = lock(registry);
                    // A change of state is news; a new detail under the same
                    // state is an application narrating itself, which at once
                    // a second was most of the node's log.
                    if registry.get(&app).map(|a| a.health) != Some(state) {
                        tracing::info!(%app, ?state, detail = ?detail, "application health");
                    } else {
                        tracing::trace!(%app, ?state, detail = ?detail, "application health");
                    }
                    registry.health(&app, state, detail);
                }
                Ok(ClientMsg::Hello { .. }) => {
                    let feature = "hello after hello".to_owned();
                    write
                        .write_all(&encode_line(&NodeMsg::Unsupported { feature }))
                        .await?;
                }
                Err(_) => {
                    let feature = message_kind(&line).unwrap_or_else(|| "not a message".into());
                    tracing::debug!(%app, %feature, "an application sent a message this node does not know");
                    write
                        .write_all(&encode_line(&NodeMsg::Unsupported { feature }))
                        .await?;
                }
            }
        }
        Ok::<(), std::io::Error>(())
    }
    .await;
    lock(registry).disconnected(&app);
    tracing::info!(%app, "application disconnected");
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
            tokio::spawn(async move { serve_connection(theirs, "Nocturne", &registry).await });
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
        })
    }

    async fn until(
        registry: &Arc<Mutex<Registry>>,
        check: impl Fn(&super::super::Registered) -> bool,
    ) {
        let id = AppId::parse("demo").unwrap();
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
            let app = AppId::parse("demo").unwrap();
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
                .get(&AppId::parse("demo").unwrap())
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
}
