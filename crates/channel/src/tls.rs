//! TLS under a carrier, and what it verified.
//!
//! The configuration is the caller's — its roots, its client verifier, its
//! certificate — and rustls refuses a handshake that configuration does not
//! accept, so a connection that completes is one the configuration verified.
//! What this module adds is the report a verifier needs afterwards
//! ([`TlsInfo`]): the peer's chain and the connection's RFC 9266
//! `tls-exporter`, which the session binds its tag to. The exporter is taken
//! only from TLS 1.3; TLS 1.2 defines one only with the extended master
//! secret, which rustls does not let a caller confirm, so a 1.2 connection
//! reports none and its tag goes bare.
//!
//! [`TlsCarrier`] is TCP behind TLS, frames behind the same 4-byte length as
//! [`crate::tcp`]; the WebSocket carrier over TLS is [`crate::ws`]'s.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::carrier::{encode_frame, FrameReader, TlsInfo, TLS_EXPORTER_LABEL};
use crate::{Carrier, CarrierRx, CarrierTx, ChannelError};

/// The rustls these carriers take configurations from, so a caller builds
/// them against the same version.
pub use rustls;
pub use rustls::{ClientConfig, ServerConfig};

/// What a finished handshake verified.
pub(crate) fn info_of(conn: &rustls::CommonState, exporter: Option<[u8; 32]>) -> TlsInfo {
    TlsInfo {
        peer_certificates: conn
            .peer_certificates()
            .unwrap_or_default()
            .iter()
            .map(|cert| cert.as_ref().to_vec())
            .collect(),
        exporter: exporter
            .filter(|_| conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3)),
    }
}

fn tls_error(e: std::io::Error) -> ChannelError {
    ChannelError::Other(format!("TLS handshake failed: {e}"))
}

/// Dial `addr` and complete TLS as a client, checking the server against
/// `config` under `server_name`.
pub async fn connect_tls(
    addr: SocketAddr,
    server_name: &str,
    config: Arc<ClientConfig>,
) -> Result<(tokio_rustls::client::TlsStream<TcpStream>, TlsInfo), ChannelError> {
    let name = ServerName::try_from(server_name.to_string())
        .map_err(|e| ChannelError::Other(format!("not a server name: {server_name}: {e}")))?;
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let stream = TlsConnector::from(config)
        .connect(name, tcp)
        .await
        .map_err(tls_error)?;
    let (_, conn) = stream.get_ref();
    let exporter = conn
        .export_keying_material([0u8; 32], TLS_EXPORTER_LABEL, Some(&[]))
        .ok();
    let info = info_of(conn, exporter);
    Ok((stream, info))
}

/// Complete TLS as a server on an accepted connection.
pub async fn accept_tls(
    tcp: TcpStream,
    config: Arc<ServerConfig>,
) -> Result<(tokio_rustls::server::TlsStream<TcpStream>, TlsInfo), ChannelError> {
    tcp.set_nodelay(true)?;
    let stream = TlsAcceptor::from(config)
        .accept(tcp)
        .await
        .map_err(tls_error)?;
    let (_, conn) = stream.get_ref();
    let exporter = conn
        .export_keying_material([0u8; 32], TLS_EXPORTER_LABEL, Some(&[]))
        .ok();
    let info = info_of(conn, exporter);
    Ok((stream, info))
}

/// Length-prefixed frames over a TLS stream.
pub struct TlsCarrier<S> {
    stream: S,
    reader: FrameReader,
    info: TlsInfo,
}

impl TlsCarrier<tokio_rustls::client::TlsStream<TcpStream>> {
    pub async fn connect(
        addr: SocketAddr,
        server_name: &str,
        config: Arc<ClientConfig>,
    ) -> Result<Self, ChannelError> {
        let (stream, info) = connect_tls(addr, server_name, config).await?;
        Ok(Self::new(stream, info))
    }
}

impl TlsCarrier<tokio_rustls::server::TlsStream<TcpStream>> {
    pub async fn accept(tcp: TcpStream, config: Arc<ServerConfig>) -> Result<Self, ChannelError> {
        let (stream, info) = accept_tls(tcp, config).await?;
        Ok(Self::new(stream, info))
    }
}

impl<S> TlsCarrier<S> {
    fn new(stream: S, info: TlsInfo) -> Self {
        Self {
            stream,
            reader: FrameReader::default(),
            info,
        }
    }
}

#[async_trait]
impl<S> Carrier for TlsCarrier<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        self.stream.write_all(&encode_frame(&frame)?).await?;
        Ok(self.stream.flush().await?)
    }

    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        self.reader.read(&mut self.stream).await
    }

    fn kind(&self) -> &'static str {
        "tls"
    }

    fn tls(&self) -> Option<&TlsInfo> {
        Some(&self.info)
    }

    fn split(self: Box<Self>) -> (Box<dyn CarrierTx>, Box<dyn CarrierRx>) {
        let (read, write) = tokio::io::split(self.stream);
        (
            Box::new(TlsTx(write)),
            Box::new(TlsRx {
                read,
                reader: self.reader,
            }),
        )
    }
}

struct TlsTx<S>(WriteHalf<S>);

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Send + 'static> CarrierTx for TlsTx<S> {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        self.0.write_all(&encode_frame(&frame)?).await?;
        Ok(self.0.flush().await?)
    }
}

struct TlsRx<S> {
    read: ReadHalf<S>,
    reader: FrameReader,
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Send + 'static> CarrierRx for TlsRx<S> {
    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        self.reader.read(&mut self.read).await
    }
}
