//! A WebSocket carrier: one binary message per frame, so a browser can be
//! one end.
//!
//! WebSocket already frames, so no length prefix rides inside the message.
//! A text message is not a frame of this channel and fails the carrier
//! rather than being guessed at; pings are answered by tungstenite itself
//! and never reach the session.
//!
//! Over plain `ws://` ([`connect`], [`accept`]) the carrier reports no TLS,
//! and [`crate::CarrierAuthenticated`] refuses it: what crosses there,
//! token included, crosses in the clear. Over `wss://` ([`connect_wss`],
//! [`accept_wss`], with the `tls` feature) it reports what TLS verified.
//! A token never rides in the URL or a header — the opening exchange
//! carries it — so a proxy's access log never holds one.

use std::net::SocketAddr;

use async_trait::async_trait;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::carrier::TlsInfo;
use crate::{Carrier, CarrierRx, CarrierTx, ChannelError};

/// One end of a WebSocket.
pub struct WsCarrier<S> {
    ws: WebSocketStream<S>,
    tls: Option<TlsInfo>,
}

fn ws_error(e: tokio_tungstenite::tungstenite::Error) -> ChannelError {
    match e {
        tokio_tungstenite::tungstenite::Error::ConnectionClosed
        | tokio_tungstenite::tungstenite::Error::AlreadyClosed => ChannelError::Closed,
        other => ChannelError::Other(format!("websocket: {other}")),
    }
}

/// Dial `url` (`ws://host:port/path`) at `addr` in the clear.
pub async fn connect(addr: SocketAddr, url: &str) -> Result<WsCarrier<TcpStream>, ChannelError> {
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let (ws, _) = tokio_tungstenite::client_async(url, tcp)
        .await
        .map_err(ws_error)?;
    Ok(WsCarrier { ws, tls: None })
}

/// Answer a WebSocket upgrade on an accepted connection, in the clear.
pub async fn accept(tcp: TcpStream) -> Result<WsCarrier<TcpStream>, ChannelError> {
    tcp.set_nodelay(true)?;
    let ws = tokio_tungstenite::accept_async(tcp)
        .await
        .map_err(ws_error)?;
    Ok(WsCarrier { ws, tls: None })
}

/// Dial `url` (`wss://…`) at `addr`, checking the server against `config`
/// under `server_name`.
#[cfg(feature = "tls")]
pub async fn connect_wss(
    addr: SocketAddr,
    url: &str,
    server_name: &str,
    config: std::sync::Arc<crate::tls::ClientConfig>,
) -> Result<WsCarrier<tokio_rustls::client::TlsStream<TcpStream>>, ChannelError> {
    let (stream, info) = crate::tls::connect_tls(addr, server_name, config).await?;
    let (ws, _) = tokio_tungstenite::client_async(url, stream)
        .await
        .map_err(ws_error)?;
    Ok(WsCarrier {
        ws,
        tls: Some(info),
    })
}

/// Complete TLS and then a WebSocket upgrade on an accepted connection.
#[cfg(feature = "tls")]
pub async fn accept_wss(
    tcp: TcpStream,
    config: std::sync::Arc<crate::tls::ServerConfig>,
) -> Result<WsCarrier<tokio_rustls::server::TlsStream<TcpStream>>, ChannelError> {
    let (stream, info) = crate::tls::accept_tls(tcp, config).await?;
    let ws = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(ws_error)?;
    Ok(WsCarrier {
        ws,
        tls: Some(info),
    })
}

type Incoming = Result<Message, tokio_tungstenite::tungstenite::Error>;

async fn next_frame(
    stream: &mut (impl futures::Stream<Item = Incoming> + Unpin),
) -> Result<Vec<u8>, ChannelError> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Binary(bytes))) => return Ok(bytes.into()),
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return Err(ChannelError::Closed),
            Some(Ok(Message::Text(_))) => {
                return Err(ChannelError::Other(
                    "a websocket text message is not a frame of this channel".into(),
                ))
            }
            Some(Err(e)) => return Err(ws_error(e)),
        }
    }
}

#[async_trait]
impl<S> Carrier for WsCarrier<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        self.ws
            .send(Message::Binary(frame.into()))
            .await
            .map_err(ws_error)
    }

    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        next_frame(&mut self.ws).await
    }

    fn kind(&self) -> &'static str {
        if self.tls.is_some() {
            "wss"
        } else {
            "ws"
        }
    }

    fn tls(&self) -> Option<&TlsInfo> {
        self.tls.as_ref()
    }

    fn split(self: Box<Self>) -> (Box<dyn CarrierTx>, Box<dyn CarrierRx>) {
        let (sink, stream) = self.ws.split();
        (Box::new(WsTx(sink)), Box::new(WsRx(stream)))
    }
}

struct WsTx<S>(SplitSink<WebSocketStream<S>, Message>);

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> CarrierTx for WsTx<S> {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        self.0
            .send(Message::Binary(frame.into()))
            .await
            .map_err(ws_error)
    }

    async fn send_batch(&mut self, frames: Vec<Vec<u8>>) -> Result<(), ChannelError> {
        for frame in frames {
            self.0
                .feed(Message::Binary(frame.into()))
                .await
                .map_err(ws_error)?;
        }
        self.0.flush().await.map_err(ws_error)
    }
}

struct WsRx<S>(SplitStream<WebSocketStream<S>>);

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> CarrierRx for WsRx<S> {
    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        next_frame(&mut self.0).await
    }
}
