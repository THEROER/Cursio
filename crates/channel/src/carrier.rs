//! What moves framed bytes between the two ends.
//!
//! Frames, not bytes: a byte stream carries each frame behind a 4-byte
//! big-endian length prefix ([`encode_frame`], [`FrameReader`]), a message
//! transport carries one frame per message. Everything above — the handshake,
//! the seal, the session — is written against these traits, so a link is the
//! same link whichever carrier carries it.
//!
//! A carrier is split before a session uses it: the receive half then runs
//! in its own task and is never cancelled, because cancelling a read part-way
//! can desynchronise a stateful layer above it (a Noise cipher) and silently
//! lose every later frame.

use async_trait::async_trait;

use crate::ChannelError;

/// Upper bound on one frame, so a length prefix off the wire cannot make
/// this end allocate without limit.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// A connection to one peer that carries whole frames.
#[async_trait]
pub trait Carrier: Send {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError>;

    /// The next frame. [`ChannelError::Closed`] when the peer hung up cleanly.
    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError>;

    /// A short label for logs, such as `"tcp"`.
    fn kind(&self) -> &'static str;

    /// This end's address, for a carrier that has one.
    fn local_ip(&self) -> Option<std::net::IpAddr> {
        None
    }

    fn split(self: Box<Self>) -> (Box<dyn CarrierTx>, Box<dyn CarrierRx>);
}

/// The send half of a split [`Carrier`].
#[async_trait]
pub trait CarrierTx: Send {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), ChannelError>;

    /// Send `frames` in order, in as few writes as the carrier allows.
    ///
    /// The default sends them one by one, which is what a socket that bounds
    /// its own backlog (TCP) wants. A byte stream whose every write is a unit
    /// of its own — on RFCOMM one write is one radio frame with its own credit
    /// and its own slot on the air — overrides it to write them as one.
    async fn send_batch(&mut self, frames: Vec<Vec<u8>>) -> Result<(), ChannelError> {
        for frame in frames {
            self.send(frame).await?;
        }
        Ok(())
    }

    /// The kernel's counters for the TCP connection this writes to, for a
    /// carrier that writes to one; see [`crate::tcp::info`].
    fn counters(&self) -> Option<std::io::Result<crate::tcp::TcpCounters>> {
        None
    }
}

/// The receive half of a split [`Carrier`].
#[async_trait]
pub trait CarrierRx: Send {
    async fn recv(&mut self) -> Result<Vec<u8>, ChannelError>;
}

/// Prepend the 4-byte big-endian length to `payload`.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, ChannelError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(ChannelError::FrameTooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Cut one frame off the front of `buf`: `Some((payload, consumed))` when a
/// whole one is there, `None` when more bytes are needed.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Vec<u8>, usize)>, ChannelError> {
    let Some(prefix) = buf.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(prefix.try_into().expect("four bytes")) as usize;
    if len > MAX_FRAME_LEN {
        return Err(ChannelError::FrameTooLarge(len));
    }
    Ok(buf
        .get(4..4 + len)
        .map(|payload| (payload.to_vec(), 4 + len)))
}

/// Length-prefixed frames off a byte stream, carrying bytes read past a
/// frame boundary into the next read.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    /// The next frame from `read`. [`ChannelError::Closed`] at a clean end of
    /// the stream.
    pub async fn read<R>(&mut self, read: &mut R) -> Result<Vec<u8>, ChannelError>
    where
        R: tokio::io::AsyncRead + Unpin + ?Sized,
    {
        use tokio::io::AsyncReadExt;
        loop {
            if let Some((payload, consumed)) = decode_frame(&self.buf)? {
                self.buf.drain(..consumed);
                return Ok(payload);
            }
            let mut chunk = [0u8; 8192];
            let n = read.read(&mut chunk).await?;
            if n == 0 {
                return Err(ChannelError::Closed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_its_big_endian_length_then_its_bytes() {
        assert_eq!(
            encode_frame(b"abc").unwrap(),
            [0, 0, 0, 3, b'a', b'b', b'c']
        );
    }

    #[test]
    fn a_partial_frame_waits_for_the_rest() {
        let frame = encode_frame(b"hello").unwrap();
        assert!(decode_frame(&frame[..6]).unwrap().is_none());
        assert_eq!(
            decode_frame(&frame).unwrap(),
            Some((b"hello".to_vec(), frame.len()))
        );
    }

    #[test]
    fn a_length_past_the_limit_is_refused_before_allocating() {
        let prefix = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes();
        assert!(matches!(
            decode_frame(&prefix),
            Err(ChannelError::FrameTooLarge(_))
        ));
    }

    #[tokio::test]
    async fn frames_a_narrow_pipe_cuts_apart_are_reassembled() {
        use tokio::io::AsyncWriteExt;
        let (mut a, mut b) = tokio::io::duplex(3);
        let write = tokio::spawn(async move {
            for n in 0..20u8 {
                a.write_all(&encode_frame(&[n; 7]).unwrap()).await.unwrap();
            }
        });
        let mut reader = FrameReader::default();
        for n in 0..20u8 {
            assert_eq!(reader.read(&mut b).await.unwrap(), vec![n; 7]);
        }
        write.await.unwrap();
        assert!(matches!(
            reader.read(&mut b).await,
            Err(ChannelError::Closed)
        ));
    }
}
