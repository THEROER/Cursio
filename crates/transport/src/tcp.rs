//! Framed TCP transport (Wi-Fi/LAN): [`Message`]s over the channel's
//! [`TcpCarrier`].
//!
//! Each message is its bincode encoding in one length-prefixed frame. The
//! carrier bounds the kernel send backlog and how long sent data may go
//! unacknowledged before the first byte is written, and reports the kernel's
//! retransmission counters; see `cursio_channel::tcp`.

use std::net::SocketAddr;

use async_trait::async_trait;
use cursio_channel::tcp::TcpCarrier;
use cursio_channel::{Carrier, CarrierRx, CarrierTx};
use cursio_protocol::Message;
use tokio::net::{TcpListener, TcpStream};

use crate::{Transport, TransportError, TransportRx, TransportTx};

/// A message-oriented link over a single TCP connection.
pub struct TcpTransport {
    carrier: TcpCarrier,
}

impl TcpTransport {
    /// Dial `addr` and wrap the resulting connection.
    pub async fn connect(addr: SocketAddr) -> Result<Self, TransportError> {
        Ok(Self {
            carrier: TcpCarrier::connect(addr).await?,
        })
    }

    /// Wrap an already-accepted connection (server side).
    pub fn from_stream(stream: TcpStream) -> Result<Self, TransportError> {
        Ok(Self {
            carrier: TcpCarrier::from_stream(stream)?,
        })
    }

    /// The remote address of this connection.
    pub fn peer_addr(&self) -> SocketAddr {
        self.carrier.peer_addr()
    }
}

#[async_trait]
impl Transport for TcpTransport {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        Ok(self.carrier.send(msg.to_bytes()?).await?)
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        Ok(Message::from_bytes(&self.carrier.recv().await?)?)
    }

    fn kind(&self) -> &'static str {
        "tcp"
    }

    fn local_ip(&self) -> Option<std::net::IpAddr> {
        self.carrier.local_ip()
    }

    fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        let (tx, rx) = Box::new(self.carrier).split();
        (Box::new(TcpTx(tx)), Box::new(TcpRx(rx)))
    }
}

/// Send half of a split [`TcpTransport`].
pub struct TcpTx(Box<dyn CarrierTx>);

#[async_trait]
impl TransportTx for TcpTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        Ok(self.0.send(msg.to_bytes()?).await?)
    }

    fn tcp_counters(&self) -> Option<std::io::Result<crate::tcp_info::TcpCounters>> {
        self.0.counters()
    }
}

/// Receive half of a split [`TcpTransport`].
pub struct TcpRx(Box<dyn CarrierRx>);

#[async_trait]
impl TransportRx for TcpRx {
    async fn recv(&mut self) -> Result<Message, TransportError> {
        Ok(Message::from_bytes(&self.0.recv().await?)?)
    }
}

/// Listens for inbound TCP connections from peers.
pub struct TcpListenerHandle {
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl TcpListenerHandle {
    /// Bind a listener. Pass port `0` to let the OS choose a free port; read it
    /// back via [`TcpListenerHandle::local_addr`] to advertise over discovery.
    pub async fn bind(addr: SocketAddr) -> Result<Self, TransportError> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    /// The address the listener is actually bound to (resolves port `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Accept the next inbound connection as a [`TcpTransport`].
    pub async fn accept(&self) -> Result<TcpTransport, TransportError> {
        let (stream, _) = self.listener.accept().await?;
        TcpTransport::from_stream(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::{Hello, PeerId};

    #[tokio::test]
    async fn tcp_roundtrips_a_message_over_loopback() {
        let listener = TcpListenerHandle::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();

        let server = tokio::spawn(async move {
            let mut t = listener.accept().await.unwrap();
            t.recv().await.unwrap()
        });

        let mut client = TcpTransport::connect(addr).await.unwrap();
        let msg = Message::Hello(Hello::bare(PeerId::nil(), "client"));
        client.send(&msg).await.unwrap();

        let received = server.await.unwrap();
        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn recv_handles_two_frames_in_one_read() {
        let listener = TcpListenerHandle::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();

        let server = tokio::spawn(async move {
            let mut t = listener.accept().await.unwrap();
            let a = t.recv().await.unwrap();
            let b = t.recv().await.unwrap();
            (a, b)
        });

        let mut client = TcpTransport::connect(addr).await.unwrap();
        client.send(&Message::Heartbeat { nonce: 1 }).await.unwrap();
        client.send(&Message::Heartbeat { nonce: 2 }).await.unwrap();

        let (a, b) = server.await.unwrap();
        assert_eq!(a, Message::Heartbeat { nonce: 1 });
        assert_eq!(b, Message::Heartbeat { nonce: 2 });
    }

    /// The bytes on the socket are Cursio's own framing: a big-endian length,
    /// then the message's bincode — what the frozen update lane and every
    /// older build read.
    #[tokio::test]
    async fn a_message_is_its_length_then_its_bincode_on_the_wire() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let mut client = TcpTransport::connect(addr).await.unwrap();
        let mut raw = accept.await.unwrap();
        let msg = Message::Update(vec![0xab]);
        client.send(&msg).await.unwrap();
        let body = msg.to_bytes().unwrap();
        let mut got = vec![0u8; 4 + body.len()];
        raw.read_exact(&mut got).await.unwrap();
        assert_eq!(got[..4], (body.len() as u32).to_be_bytes());
        assert_eq!(got[4..], body[..]);
    }
}
