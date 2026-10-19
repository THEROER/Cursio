//! Framed TCP transport (Wi-Fi/LAN).
//!
//! Wraps a [`tokio::net::TcpStream`] and speaks the length-prefixed framing
//! from `cursio-protocol`. Incoming bytes accumulate in a read buffer until a
//! whole frame is available, so partial reads are handled transparently.
//!
//! Every connection, dialled or accepted, has its kernel send backlog bounded
//! before the first byte is written ([`crate::send_backlog`]): the priority
//! drainer's ordering only holds if the socket does not keep a deep queue of
//! its own — and how long sent data may go unacknowledged bounded too
//! ([`crate::dead_peer`]), so a peer that vanished mid-write is let go of in
//! seconds rather than after minutes of retransmission.

use std::net::SocketAddr;

use async_trait::async_trait;
use cursio_protocol::{decode_frame, encode_frame, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{Transport, TransportError, TransportRx, TransportTx};

/// How long a dial may wait for the peer to answer.
///
/// The OS waits minutes for a SYN nobody acknowledges, and an address that
/// stopped being on this network — the home LAN, dialled from a phone's
/// hotspot whose traffic leaves over mobile data — is exactly that. A peer on
/// the same LAN answers in milliseconds; three seconds is far above that and
/// short enough that a sweep of stale addresses does not hold up the node.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// A message-oriented link over a single TCP connection.
pub struct TcpTransport {
    stream: TcpStream,
    /// Leftover bytes read past a frame boundary, carried into the next `recv`.
    read_buf: Vec<u8>,
    peer_addr: SocketAddr,
}

impl TcpTransport {
    /// Dial `addr` and wrap the resulting connection.
    pub async fn connect(addr: SocketAddr) -> Result<Self, TransportError> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "no answer from {addr} within {}s",
                        CONNECT_TIMEOUT.as_secs()
                    ),
                )
            })??;
        Self::from_stream(stream)
    }

    /// Wrap an already-accepted connection (server side).
    pub fn from_stream(stream: TcpStream) -> Result<Self, TransportError> {
        crate::send_backlog::bound(&stream);
        crate::dead_peer::bound(&stream);
        Self::wrap(stream)
    }

    /// A connection that keeps the kernel's own, autotuned send buffer: what
    /// every link was before the backlog was bounded, kept so tests can show
    /// the difference the bound makes.
    #[cfg(test)]
    pub(crate) fn from_stream_unbounded(stream: TcpStream) -> Result<Self, TransportError> {
        Self::wrap(stream)
    }

    fn wrap(stream: TcpStream) -> Result<Self, TransportError> {
        stream.set_nodelay(true)?;
        let peer_addr = stream.peer_addr()?;
        Ok(Self {
            stream,
            read_buf: Vec::new(),
            peer_addr,
        })
    }

    /// The remote address of this connection.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
}

#[async_trait]
impl Transport for TcpTransport {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        let payload = msg.to_bytes()?;
        let frame = encode_frame(&payload)?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        loop {
            // Try to satisfy the read from what we already buffered first.
            if let Some((payload, consumed)) = decode_frame(&self.read_buf)? {
                self.read_buf.drain(..consumed);
                return Ok(Message::from_bytes(&payload)?);
            }

            // Need more bytes from the socket.
            let mut chunk = [0u8; 8192];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(TransportError::Closed);
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn kind(&self) -> &'static str {
        "tcp"
    }

    fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        // Owned split gives independent read/write halves that can live in
        // separate tasks. The read half carries our leftover buffer so no
        // already-read bytes are lost across the split.
        let (read, write) = self.stream.into_split();
        (
            Box::new(TcpTx {
                write,
                #[cfg(target_os = "windows")]
                backlog: crate::send_backlog::IdealBacklog::new(),
            }),
            Box::new(TcpRx {
                read,
                read_buf: self.read_buf,
            }),
        )
    }
}

/// Send half of a split [`TcpTransport`].
pub struct TcpTx {
    write: tokio::net::tcp::OwnedWriteHalf,
    /// The split tx is what the priority drainer writes through, so this is
    /// where the send buffer follows the stack's estimate as a link ramps up.
    #[cfg(target_os = "windows")]
    backlog: crate::send_backlog::IdealBacklog,
}

#[async_trait]
impl TransportTx for TcpTx {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        #[cfg(target_os = "windows")]
        self.backlog.retune(self.write.as_ref());
        let payload = msg.to_bytes()?;
        let frame = encode_frame(&payload)?;
        self.write.write_all(&frame).await?;
        self.write.flush().await?;
        Ok(())
    }
}

/// Receive half of a split [`TcpTransport`].
pub struct TcpRx {
    read: tokio::net::tcp::OwnedReadHalf,
    read_buf: Vec<u8>,
}

#[async_trait]
impl TransportRx for TcpRx {
    async fn recv(&mut self) -> Result<Message, TransportError> {
        loop {
            if let Some((payload, consumed)) = decode_frame(&self.read_buf)? {
                self.read_buf.drain(..consumed);
                return Ok(Message::from_bytes(&payload)?);
            }
            let mut chunk = [0u8; 8192];
            let n = self.read.read(&mut chunk).await?;
            if n == 0 {
                return Err(TransportError::Closed);
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
        }
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
}
