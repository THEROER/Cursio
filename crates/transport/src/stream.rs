//! The same framing over any byte stream: the Bluetooth channel.
//!
//! The mesh is one protocol whatever carries it. A Bluetooth link is not a
//! second protocol with its own rules — it is another byte pipe under the very
//! same `Hello`, pairing, Noise handshake, module negotiation and priority
//! classes that run over TCP. So this is the 4-byte length framing of
//! [`crate::tcp`], written against `AsyncRead + AsyncWrite` instead of a
//! socket type, and the RFCOMM socket BlueZ hands over (or the socket pair the
//! Android app pumps an RFCOMM stream through) is all it needs.
//!
//! TCP keeps its own type: it bounds the kernel send backlog and reports the
//! peer's IP address, neither of which means anything for a pipe.

use async_trait::async_trait;
use cursio_protocol::{decode_frame, encode_frame, Message};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

use crate::{Transport, TransportError, TransportRx, TransportTx};

/// The label [`Transport::kind`] reports for a Bluetooth link, which the
/// session layer reads to say how a peer is connected.
pub const BLUETOOTH: &str = "bluetooth";

/// Framed messages over a byte stream.
pub struct StreamTransport<S> {
    stream: S,
    read_buf: Vec<u8>,
    kind: &'static str,
}

impl<S> StreamTransport<S>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    /// Frame `stream`, reporting `kind` (for example [`BLUETOOTH`]).
    pub fn new(stream: S, kind: &'static str) -> Self {
        Self {
            stream,
            read_buf: Vec::new(),
            kind,
        }
    }
}

async fn write_message<W: AsyncWrite + Unpin>(
    write: &mut W,
    msg: &Message,
) -> Result<(), TransportError> {
    let payload = msg.to_bytes()?;
    let frame = encode_frame(&payload)?;
    write.write_all(&frame).await?;
    write.flush().await?;
    Ok(())
}

async fn read_message<R: AsyncRead + Unpin>(
    read: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Message, TransportError> {
    loop {
        if let Some((payload, consumed)) = decode_frame(buf)? {
            buf.drain(..consumed);
            return Ok(Message::from_bytes(&payload)?);
        }
        let mut chunk = [0u8; 8192];
        let n = read.read(&mut chunk).await?;
        if n == 0 {
            return Err(TransportError::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[async_trait]
impl<S> Transport for StreamTransport<S>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        write_message(&mut self.stream, msg).await
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        read_message(&mut self.stream, &mut self.read_buf).await
    }

    fn kind(&self) -> &'static str {
        self.kind
    }

    fn split(self: Box<Self>) -> (Box<dyn TransportTx>, Box<dyn TransportRx>) {
        let (read, write) = tokio::io::split(self.stream);
        (
            Box::new(StreamTx { write }),
            Box::new(StreamRx {
                read,
                read_buf: self.read_buf,
            }),
        )
    }
}

struct StreamTx<S> {
    write: WriteHalf<S>,
}

#[async_trait]
impl<S: AsyncWrite + Send + 'static> TransportTx for StreamTx<S> {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        write_message(&mut self.write, msg).await
    }
}

struct StreamRx<S> {
    read: ReadHalf<S>,
    read_buf: Vec<u8>,
}

#[async_trait]
impl<S: AsyncRead + Send + 'static> TransportRx for StreamRx<S> {
    async fn recv(&mut self) -> Result<Message, TransportError> {
        read_message(&mut self.read, &mut self.read_buf).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping(n: u64) -> Message {
        Message::Heartbeat { nonce: n }
    }

    #[tokio::test]
    async fn carries_messages_both_ways_over_a_byte_pipe() {
        let (a, b) = tokio::io::duplex(64);
        let mut left = StreamTransport::new(a, BLUETOOTH);
        let mut right = StreamTransport::new(b, BLUETOOTH);
        left.send(&ping(1)).await.unwrap();
        assert_eq!(right.recv().await.unwrap(), ping(1));
        right.send(&ping(2)).await.unwrap();
        assert_eq!(left.recv().await.unwrap(), ping(2));
        assert_eq!(left.kind(), "bluetooth");
    }

    #[tokio::test]
    async fn reassembles_frames_a_small_pipe_cuts_apart() {
        // A pipe narrower than one frame delivers it in pieces, as RFCOMM's
        // small MTU does; the reader must wait for the rest.
        let (a, b) = tokio::io::duplex(3);
        let left = Box::new(StreamTransport::new(a, BLUETOOTH));
        let (mut tx, _rx) = left.split();
        let right = Box::new(StreamTransport::new(b, BLUETOOTH));
        let (_tx2, mut rx) = right.split();
        let send = tokio::spawn(async move {
            for n in 0..20 {
                tx.send(&ping(n)).await.unwrap();
            }
        });
        for n in 0..20 {
            assert_eq!(rx.recv().await.unwrap(), ping(n));
        }
        send.await.unwrap();
    }

    #[tokio::test]
    async fn reports_a_closed_pipe_as_closed() {
        let (a, b) = tokio::io::duplex(64);
        let mut left = StreamTransport::new(a, BLUETOOTH);
        drop(b);
        assert!(matches!(left.recv().await, Err(TransportError::Closed)));
    }
}
