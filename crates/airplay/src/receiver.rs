use std::sync::Arc;
use std::time::Duration;

use cursio_transport::discovery::DiscoveryConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::crypto::{CryptoProvider, CryptoSession};
use crate::discovery::AirPlayAnnouncement;
use crate::media::{ConnectionContext, MediaSession};
use crate::rtsp::{self, ProtocolError, RequestDecoder};
use crate::{AirPlayError, ReceiverConfig};

const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// A sender with a negotiated media key polls this connection continuously
/// (`/feedback`, `GET_PARAMETER`). Silence for this long means it went away
/// without closing the socket, and the session's timing socket, mirror listener
/// and video window would otherwise be held until the process exits.
const ACTIVE_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound AirPlay control listener and its live mDNS registrations.
pub struct Receiver {
    listener: TcpListener,
    config: Arc<ReceiverConfig>,
    /// `None` when this node deliberately stays out of the picker: the control
    /// listener still serves, so a sender pointed at it directly works, but it
    /// publishes no mDNS record and so does not appear as a second Cursio.
    _announcement: Option<AirPlayAnnouncement>,
    crypto: Option<Arc<dyn CryptoProvider>>,
    screen_enabled: bool,
    tap: Option<crate::MirrorTap>,
    /// Survives the control connections that come and go, so a volume the user
    /// set once stays set.
    volume: tokio::sync::watch::Sender<f32>,
}

impl Receiver {
    /// Bind the control socket, then advertise the actual bound port.
    ///
    /// Binding first means callers may use port zero without publishing an
    /// unusable service record.
    pub async fn bind(
        config: ReceiverConfig,
        discovery: DiscoveryConfig,
    ) -> Result<Self, AirPlayError> {
        Self::bind_inner(config, discovery, None, true).await
    }

    pub async fn bind_with_crypto(
        config: ReceiverConfig,
        discovery: DiscoveryConfig,
        crypto: Arc<dyn CryptoProvider>,
    ) -> Result<Self, AirPlayError> {
        Self::bind_inner(config, discovery, Some(crypto), true).await
    }

    /// Bind without publishing an mDNS record.
    ///
    /// For a node that lost the mesh's AirPlay election: it must not appear in
    /// a sender's list, but it stays ready to serve so that pointing at it
    /// directly — or taking over when the announcing node goes away — needs no
    /// restart.
    pub async fn bind_silently(
        config: ReceiverConfig,
        discovery: DiscoveryConfig,
        crypto: Arc<dyn CryptoProvider>,
    ) -> Result<Self, AirPlayError> {
        Self::bind_inner(config, discovery, Some(crypto), false).await
    }

    async fn bind_inner(
        config: ReceiverConfig,
        discovery: DiscoveryConfig,
        crypto: Option<Arc<dyn CryptoProvider>>,
        announce: bool,
    ) -> Result<Self, AirPlayError> {
        let listener = TcpListener::bind(config.bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let announcement = if announce {
            Some(AirPlayAnnouncement::register(
                &config,
                local_addr.port(),
                &discovery,
            )?)
        } else {
            tracing::info!(name = %config.name, "AirPlay receiver bound without announcing");
            None
        };
        Ok(Self {
            listener,
            config: Arc::new(config),
            _announcement: announcement,
            crypto,
            screen_enabled: false,
            tap: None,
            volume: tokio::sync::watch::Sender::new(0.0),
        })
    }

    /// Enable the mirroring video sink and its native presentation window.
    ///
    /// The control listener remains usable when this is false, but mirror
    /// `SETUP` requests are rejected before a decoder or window is created.
    pub fn with_screen_enabled(mut self, enabled: bool) -> Self {
        self.screen_enabled = enabled;
        self
    }

    /// Publish the mirrored screen so the rest of Cursio can show it too.
    ///
    /// Without a tap the iPhone's screen exists only on this machine, which is
    /// what makes AirPlay feel bolted on rather than part of the mesh.
    pub fn with_mirror_tap(mut self, tap: crate::MirrorTap) -> Self {
        self.tap = Some(tap);
        self
    }

    pub fn local_addr(&self) -> Result<std::net::SocketAddr, std::io::Error> {
        self.listener.local_addr()
    }

    /// Accept control connections until the listener fails.
    pub async fn serve(self) -> Result<(), AirPlayError> {
        tracing::info!(
            address = %self.listener.local_addr()?,
            name = %self.config.name,
            crypto = if self.crypto.is_some() { "configured" } else { "unavailable" },
            screen = if self.screen_enabled { "enabled" } else { "disabled" },
            "cursio_airplay receiver started"
        );
        if self.crypto.is_none() {
            tracing::warn!(
                "AirPlay pairing and FairPlay are unavailable; this receiver currently supports discovery and /info probes only"
            );
        }
        loop {
            // A transient accept failure (descriptor exhaustion, an aborted
            // connection) must not retire the receiver for the lifetime of the
            // process; the mesh keeps running and AirPlay would silently stay
            // gone until the next restart.
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "AirPlay control accept failed");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            };
            let local_addr = match stream.set_nodelay(true).and_then(|_| stream.local_addr()) {
                Ok(local_addr) => local_addr,
                Err(error) => {
                    tracing::warn!(%peer, %error, "AirPlay control connection unusable");
                    continue;
                }
            };
            tracing::debug!(%peer, %local_addr, "AirPlay control connection accepted");
            let config = self.config.clone();
            let crypto = self
                .crypto
                .as_ref()
                .map(|provider| provider.start_session());
            let screen_enabled = self.screen_enabled;
            let tap = self.tap.clone();
            let volume = self.volume.clone();
            tokio::spawn(async move {
                let context = ConnectionContext {
                    local_addr,
                    peer_addr: peer,
                };
                if let Err(error) = serve_connection(
                    stream,
                    config,
                    crypto,
                    Some(context),
                    screen_enabled,
                    tap,
                    volume,
                )
                .await
                {
                    tracing::warn!(%peer, %error, "AirPlay control connection ended");
                }
            });
        }
    }
}

/// Whether a control exchange earns an `info` line.
///
/// A mirroring iPhone polls `/feedback` and `GET_PARAMETER` about once a
/// second for the whole session. At `info` those bury the handful of exchanges
/// that describe what the session is actually doing — pairing, FairPlay,
/// `SETUP`, `RECORD`, `TEARDOWN` — so the routine polling drops to `debug`.
/// Any non-success status stays visible whatever the endpoint.
fn is_notable(request: &rtsp::Request, status: u16) -> bool {
    if status >= 300 {
        return true;
    }
    let target = request.target.split('?').next().unwrap_or(&request.target);
    if matches!(target, "/feedback" | "/command" | "/audioMode") {
        return false;
    }
    !matches!(
        request.method.to_ascii_uppercase().as_str(),
        "OPTIONS" | "GET_PARAMETER" | "SET_PARAMETER"
    )
}

async fn serve_connection<T>(
    mut stream: T,
    config: Arc<ReceiverConfig>,
    mut crypto: Option<Box<dyn CryptoSession>>,
    context: Option<ConnectionContext>,
    screen_enabled: bool,
    tap: Option<crate::MirrorTap>,
    volume: tokio::sync::watch::Sender<f32>,
) -> Result<(), AirPlayError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut decoder = RequestDecoder::new();
    let (width, height) = config.profile.resolution.dimensions();
    let mut media = MediaSession::new(
        screen_enabled,
        tap,
        volume,
        crate::video::ScreenSize::new(width, height),
    );
    let mut read_buffer = [0_u8; 8192];
    loop {
        let count = if media.is_active() {
            // Dropping this connection is how the phone is told to stop: there
            // is no AirPlay message meaning "the viewer closed the window".
            let read = tokio::time::timeout(ACTIVE_SESSION_IDLE_TIMEOUT, async {
                tokio::select! {
                    read = stream.read(&mut read_buffer) => Some(read),
                    () = media.screen_closed() => None,
                }
            });
            match read.await {
                Ok(Some(count)) => count?,
                Ok(None) => return Ok(()),
                Err(_) => {
                    tracing::warn!("AirPlay sender went silent; ending the media session");
                    return Ok(());
                }
            }
        } else {
            stream.read(&mut read_buffer).await?
        };
        if count == 0 {
            return if decoder.is_empty() {
                Ok(())
            } else {
                Err(ProtocolError::UnexpectedEof.into())
            };
        }
        decoder.push(&read_buffer[..count])?;

        while let Some(request) = decoder.next_request()? {
            tracing::debug!(
                method = %request.method,
                target = %request.target,
                version = %request.version,
                headers = request.headers.len(),
                body_bytes = request.body.len(),
                cseq = request.header("CSeq").unwrap_or("<none>"),
                "AirPlay request decoded"
            );
            let target = request.target.split('?').next().unwrap_or(&request.target);
            if screen_enabled
                && context.is_some()
                && request.method.eq_ignore_ascii_case("GET")
                && matches!(target, "/info" | "/server-info")
            {
                // Presentation is optional: a receiver that cannot open its
                // window must still answer probes instead of dropping the
                // connection the sender is negotiating on.
                if let Err(error) = crate::video::prepare_window() {
                    tracing::warn!(%error, "AirPlay screen window is unavailable");
                }
            }
            if request.method.eq_ignore_ascii_case("POST") && target == "/reverse" {
                stream
                    .write_all(
                        b"HTTP/1.1 101 Switching Protocols\r\n\
                          Upgrade: PTTH/1.0\r\n\
                          Connection: Upgrade\r\n\r\n",
                    )
                    .await?;
                stream.flush().await?;
                tracing::info!(
                    method = %request.method,
                    target = %request.target,
                    status = 101,
                    "AirPlay reverse event channel upgraded"
                );
                loop {
                    if stream.read(&mut read_buffer).await? == 0 {
                        return Ok(());
                    }
                }
            }
            let response = if let Some(response) = media
                .handle_request(
                    &request,
                    context,
                    crypto
                        .as_mut()
                        .map(|session| session.as_mut() as &mut dyn CryptoSession),
                )
                .await?
            {
                response
            } else {
                match crypto.as_mut() {
                    Some(session) => {
                        rtsp::route_with_crypto(&request, &config, Some(session.as_mut()))?
                    }
                    None => rtsp::route(&request, &config)?,
                }
            };
            let encoded = response.encode(request.version, request.header("CSeq"));
            if is_notable(&request, response.status) {
                tracing::info!(
                    method = %request.method,
                    target = %request.target,
                    status = response.status,
                    "airplay request"
                );
            } else {
                tracing::debug!(
                    method = %request.method,
                    target = %request.target,
                    status = response.status,
                    "airplay request"
                );
            }
            stream.write_all(&encoded).await?;
            stream.flush().await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Resolution, VideoProfile};
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    fn config() -> Arc<ReceiverConfig> {
        Arc::new(
            ReceiverConfig::new(
                "Duplex TV",
                Uuid::nil(),
                SocketAddr::from(([127, 0, 0, 1], 0)),
                VideoProfile::new(Resolution::FullHd, 60).unwrap(),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn serves_fragmented_and_pipelined_requests() {
        let (mut client, server) = tokio::io::duplex(32 * 1024);
        let task = tokio::spawn(serve_connection(
            server,
            config(),
            None,
            None,
            false,
            None,
            tokio::sync::watch::Sender::new(0.0),
        ));

        client
            .write_all(b"GET /info RTSP/1.0\r\nCSeq: 1\r\nContent-L")
            .await
            .unwrap();
        client
            .write_all(
                b"ength: 0\r\n\r\nOPTIONS * RTSP/1.0\r\nCSeq: 2\r\nContent-Length: 0\r\n\r\n",
            )
            .await
            .unwrap();
        client.shutdown().await.unwrap();

        let mut responses = Vec::new();
        client.read_to_end(&mut responses).await.unwrap();
        let text = String::from_utf8_lossy(&responses);
        assert_eq!(text.matches("RTSP/1.0 200 OK").count(), 2);
        assert!(text.contains("CSeq: 1"));
        assert!(text.contains("CSeq: 2"));
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn truncated_request_is_rejected() {
        let (mut client, server) = tokio::io::duplex(1024);
        let task = tokio::spawn(serve_connection(
            server,
            config(),
            None,
            None,
            false,
            None,
            tokio::sync::watch::Sender::new(0.0),
        ));
        client
            .write_all(b"GET /info RTSP/1.0\r\nContent-Length: 4\r\n\r\nab")
            .await
            .unwrap();
        client.shutdown().await.unwrap();

        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            AirPlayError::Control(ProtocolError::UnexpectedEof)
        ));
    }

    #[test]
    fn routine_polling_is_not_logged_at_info() {
        let request = |method: &str, target: &str| rtsp::Request {
            method: method.to_string(),
            target: target.to_string(),
            version: rtsp::Version::Rtsp10,
            headers: Vec::new(),
            body: Vec::new(),
        };

        assert!(!is_notable(&request("POST", "/feedback"), 200));
        assert!(!is_notable(&request("GET_PARAMETER", "rtsp://x/1"), 200));
        assert!(!is_notable(&request("OPTIONS", "*"), 200));

        assert!(is_notable(&request("SETUP", "rtsp://x/1"), 200));
        assert!(is_notable(&request("TEARDOWN", "rtsp://x/1"), 200));
        assert!(is_notable(&request("GET", "/info"), 200));
        // A failure is worth seeing even on an endpoint that normally is not.
        assert!(is_notable(&request("POST", "/feedback"), 501));
    }

    #[tokio::test]
    async fn upgrades_reverse_event_channel() {
        let (mut client, server) = tokio::io::duplex(1024);
        let task = tokio::spawn(serve_connection(
            server,
            config(),
            None,
            None,
            false,
            None,
            tokio::sync::watch::Sender::new(0.0),
        ));
        client
            .write_all(b"POST /reverse HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();

        let mut response = [0_u8; 85];
        let count = client.read(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response[..count]);
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(response.contains("Upgrade: PTTH/1.0\r\n"));
        assert!(response.ends_with("\r\n\r\n"));

        client.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }
}
