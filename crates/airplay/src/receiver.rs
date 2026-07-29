use std::sync::Arc;

use cursio_transport::discovery::DiscoveryConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::crypto::{CryptoProvider, CryptoSession};
use crate::discovery::AirPlayAnnouncement;
use crate::rtsp::{self, ProtocolError, RequestDecoder};
use crate::{AirPlayError, ReceiverConfig};

/// Bound AirPlay control listener and its live mDNS registrations.
pub struct Receiver {
    listener: TcpListener,
    config: Arc<ReceiverConfig>,
    _announcement: AirPlayAnnouncement,
    crypto: Option<Arc<dyn CryptoProvider>>,
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
        Self::bind_inner(config, discovery, None).await
    }

    pub async fn bind_with_crypto(
        config: ReceiverConfig,
        discovery: DiscoveryConfig,
        crypto: Arc<dyn CryptoProvider>,
    ) -> Result<Self, AirPlayError> {
        Self::bind_inner(config, discovery, Some(crypto)).await
    }

    async fn bind_inner(
        config: ReceiverConfig,
        discovery: DiscoveryConfig,
        crypto: Option<Arc<dyn CryptoProvider>>,
    ) -> Result<Self, AirPlayError> {
        let listener = TcpListener::bind(config.bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let announcement = AirPlayAnnouncement::register(&config, local_addr.port(), &discovery)?;
        Ok(Self {
            listener,
            config: Arc::new(config),
            _announcement: announcement,
            crypto,
        })
    }

    pub fn local_addr(&self) -> Result<std::net::SocketAddr, std::io::Error> {
        self.listener.local_addr()
    }

    /// Accept control connections until the listener fails.
    pub async fn serve(self) -> Result<(), AirPlayError> {
        loop {
            let (stream, peer) = self.listener.accept().await?;
            stream.set_nodelay(true)?;
            let config = self.config.clone();
            let crypto = self
                .crypto
                .as_ref()
                .map(|provider| provider.start_session());
            tokio::spawn(async move {
                if let Err(error) = serve_connection(stream, config, crypto).await {
                    tracing::warn!(%peer, %error, "AirPlay control connection ended");
                }
            });
        }
    }
}

async fn serve_connection<T>(
    mut stream: T,
    config: Arc<ReceiverConfig>,
    mut crypto: Option<Box<dyn CryptoSession>>,
) -> Result<(), AirPlayError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut decoder = RequestDecoder::new();
    let mut read_buffer = [0_u8; 8192];
    loop {
        let count = stream.read(&mut read_buffer).await?;
        if count == 0 {
            return if decoder.is_empty() {
                Ok(())
            } else {
                Err(ProtocolError::UnexpectedEof.into())
            };
        }
        decoder.push(&read_buffer[..count])?;

        while let Some(request) = decoder.next_request()? {
            let response = match crypto.as_mut() {
                Some(session) => {
                    rtsp::route_with_crypto(&request, &config, Some(session.as_mut()))?
                }
                None => rtsp::route(&request, &config)?,
            };
            let encoded = response.encode(request.version, request.header("CSeq"));
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
        let task = tokio::spawn(serve_connection(server, config(), None));

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
        let task = tokio::spawn(serve_connection(server, config(), None));
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
}
