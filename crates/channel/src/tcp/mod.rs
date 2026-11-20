//! Frames over TCP: the 4-byte length prefix of [`crate::carrier`] on a
//! [`TcpStream`].
//!
//! Every connection, dialled or accepted, has its kernel send backlog bounded
//! before the first byte is written ([`send_backlog`]): the scheduler's
//! ordering only holds if the socket does not keep a deep queue of its own —
//! and how long sent data may go unacknowledged bounded too ([`dead_peer`]),
//! so a peer that vanished mid-write is let go of in seconds rather than after
//! minutes of retransmission. The write half reports the kernel's own
//! retransmission counters ([`info`]).

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::carrier::{encode_frame, FrameReader};
use crate::{Carrier, CarrierRx, CarrierTx, ChannelError};

pub mod dead_peer;
pub mod info;
mod send_backlog;

pub use info::{CountUnit, TcpCounters};

/// How long a dial may wait for the peer to answer.
///
/// The OS waits minutes for a SYN nobody acknowledges, and an address that
/// stopped being on this network — the home LAN, dialled from a phone's
/// hotspot whose traffic leaves over mobile data — is exactly that. A peer on
/// the same LAN answers in milliseconds; three seconds is far above that and
/// short enough that a sweep of stale addresses does not hold up the caller.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Frames over one TCP connection.
pub struct TcpCarrier {
    stream: TcpStream,
    reader: FrameReader,
    peer_addr: SocketAddr,
}

impl TcpCarrier {
    /// Dial `addr`.
    pub async fn connect(addr: SocketAddr) -> Result<Self, ChannelError> {
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

    /// Carry frames over an accepted or dialled connection.
    pub fn from_stream(stream: TcpStream) -> Result<Self, ChannelError> {
        send_backlog::bound(&stream);
        dead_peer::bound(&stream);
        Self::wrap(stream)
    }

    /// A connection that keeps the kernel's own, autotuned send buffer: what
    /// every link was before the backlog was bounded, kept so tests can show
    /// the difference the bound makes.
    #[cfg(test)]
    pub(crate) fn from_stream_unbounded(stream: TcpStream) -> Result<Self, ChannelError> {
        Self::wrap(stream)
    }

    fn wrap(stream: TcpStream) -> Result<Self, ChannelError> {
        stream.set_nodelay(true)?;
        let peer_addr = stream.peer_addr()?;
        Ok(Self {
            stream,
            reader: FrameReader::default(),
            peer_addr,
        })
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    write: &mut W,
    frame: &[u8],
) -> Result<(), ChannelError> {
    write.write_all(&encode_frame(frame)?).await?;
    write.flush().await?;
    Ok(())
}

#[async_trait]
impl Carrier for TcpCarrier {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        write_frame(&mut self.stream, &frame).await
    }

    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        self.reader.read(&mut self.stream).await
    }

    fn kind(&self) -> &'static str {
        "tcp"
    }

    fn local_ip(&self) -> Option<std::net::IpAddr> {
        self.stream.local_addr().ok().map(|a| a.ip())
    }

    fn split(self: Box<Self>) -> (Box<dyn CarrierTx>, Box<dyn CarrierRx>) {
        // The read half carries the leftover buffer, so no bytes already read
        // past a frame boundary are lost across the split.
        let (read, write) = self.stream.into_split();
        (
            Box::new(TcpTx {
                write,
                #[cfg(target_os = "windows")]
                backlog: send_backlog::IdealBacklog::new(),
            }),
            Box::new(TcpRx {
                read,
                reader: self.reader,
            }),
        )
    }
}

/// Send half of a split [`TcpCarrier`].
pub struct TcpTx {
    write: tokio::net::tcp::OwnedWriteHalf,
    /// The split half is what a scheduler writes through, so this is where
    /// the send buffer follows the stack's estimate as a link ramps up.
    #[cfg(target_os = "windows")]
    backlog: send_backlog::IdealBacklog,
}

#[async_trait]
impl CarrierTx for TcpTx {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError> {
        #[cfg(target_os = "windows")]
        self.backlog.retune(self.write.as_ref());
        write_frame(&mut self.write, &frame).await
    }

    fn counters(&self) -> Option<std::io::Result<TcpCounters>> {
        Some(info::read(self.write.as_ref()))
    }
}

/// Receive half of a split [`TcpCarrier`].
pub struct TcpRx {
    read: tokio::net::tcp::OwnedReadHalf,
    reader: FrameReader,
}

#[async_trait]
impl CarrierRx for TcpRx {
    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError> {
        self.reader.read(&mut self.read).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn connected() -> (TcpCarrier, TcpCarrier) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let client = TcpCarrier::connect(addr).await.unwrap();
        let server = TcpCarrier::from_stream(accept.await.unwrap()).unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn frames_cross_loopback_whole_and_in_order() {
        let (mut client, mut server) = connected().await;
        client.send(b"one".to_vec()).await.unwrap();
        client.send(b"two".to_vec()).await.unwrap();
        assert_eq!(server.recv().await.unwrap(), b"one");
        assert_eq!(server.recv().await.unwrap(), b"two");
    }

    #[tokio::test]
    async fn the_split_halves_keep_bytes_read_past_a_frame() {
        let (mut client, server) = connected().await;
        client.send(b"a".to_vec()).await.unwrap();
        client.send(b"b".to_vec()).await.unwrap();
        let mut server = server;
        assert_eq!(server.recv().await.unwrap(), b"a");
        let (_tx, mut rx) = Box::new(server).split();
        assert_eq!(rx.recv().await.unwrap(), b"b");
    }
}
