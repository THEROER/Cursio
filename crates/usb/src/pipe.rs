//! The socket the kernel drives, and the bytes that cross it.
//!
//! Both halves of USB/IP expect a **socket**, not an API: `usbip-host` is given
//! one to serve a device over, and `vhci-hcd` is given one to reach a device
//! through. `usbip` makes that socket a TCP connection to port 3240 on another
//! machine. Cursio makes it a [`socketpair`], keeps the far end in this process,
//! and carries its bytes over the mesh link instead.
//!
//! That substitution is the whole design (invariant 5 in `docs/usb-design.md`):
//! no listening port, no unauthenticated TCP, and every attach already
//! authenticated as a peer that completed SPAKE2 pairing. Neither kernel can
//! tell the difference, because there is none to tell — it is a socket carrying
//! USB/IP either way.
//!
//! ## Blocking reads on their own thread
//!
//! A kernel socket has no async interface here, and the read must not be
//! cancelled: a partially-read USB/IP message leaves the stream misaligned, and
//! every later message is then misparsed rather than dropped. So the reader is
//! a dedicated thread doing ordinary blocking reads and handing whole buffers
//! to the session over a channel — the same shape the audio backends use, and
//! for the same reason.
//!
//! ## What this does *not* do
//!
//! It does not parse. Bytes arrive, bytes leave; [`crate::arbiter`] is what
//! looks at headers, and it is handed the same bytes rather than being put in
//! their path. Invariant 1: Cursio reads headers, never payloads.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::UsbBackendError;

/// How much is read from the kernel socket at once.
///
/// One read is one chunk on the wire, so this is also the largest
/// `UsbMsg::Data` this side produces. Matched to the protocol's own cap rather
/// than chosen freshly: a larger read would be split by the sender anyway, and
/// a much smaller one would put a frame's worth of overhead on every few
/// kilobytes of a disk transfer.
const READ_CHUNK: usize = 256 * 1024;

/// One end of the kernel's USB/IP socket, plus the thread reading it.
///
/// Dropping this closes the socket, which is how both kernels learn the
/// attachment is over: `vhci-hcd` tears the device down and `usbip-host` stops
/// serving it. That is deliberate — it means a dropped session cannot leave a
/// device half-attached, because there is nothing left to hold it.
pub struct KernelPipe {
    /// Our end. The kernel holds the other.
    socket: UnixStream,
    /// Whole reads from the kernel, in order.
    inbound: Receiver<Vec<u8>>,
    /// The reader, joined on drop so the thread cannot outlive the socket.
    reader: Option<std::thread::JoinHandle<()>>,
}

impl KernelPipe {
    /// Make a socket pair, keeping one end and returning the other for the
    /// kernel.
    ///
    /// The kernel's end is an [`OwnedFd`] rather than a raw number so it cannot
    /// be leaked by forgetting to close it on a failed attach — a leaked fd
    /// here is a device the kernel believes is still reachable.
    pub fn new() -> Result<(Self, OwnedFd), UsbBackendError> {
        let (ours, theirs) = UnixStream::pair()
            .map_err(|e| UsbBackendError::Framing(format!("cannot create a socket pair: {e}")))?;

        let read_half = ours
            .try_clone()
            .map_err(|e| UsbBackendError::Framing(format!("cannot clone the socket: {e}")))?;
        let (tx, inbound) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("cursio-usb-pipe".into())
            .spawn(move || read_loop(read_half, tx))
            .map_err(|e| UsbBackendError::Framing(format!("cannot spawn the reader: {e}")))?;

        Ok((
            Self {
                socket: ours,
                inbound,
                reader: Some(reader),
            },
            OwnedFd::from(theirs),
        ))
    }

    /// Hand bytes from the peer to the local kernel.
    ///
    /// One write per call, unsplit: USB/IP is a stream, so a short write would
    /// not corrupt it, but `write_all` keeps the ordering obvious and the error
    /// singular.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), UsbBackendError> {
        self.socket
            .write_all(bytes)
            .map_err(|e| UsbBackendError::Framing(format!("cannot write to the kernel: {e}")))
    }

    /// Take whatever the kernel has produced since the last call.
    ///
    /// Never blocks: the caller is a session pump that must return promptly, so
    /// an empty vector is the ordinary answer for an idle device.
    ///
    /// Returns [`PipeEnded`] once the socket is closed and everything already
    /// read has been handed over — draining first matters, because the last
    /// messages of a transfer arrive in the same breath as the close.
    pub fn drain(&mut self, out: &mut Vec<Vec<u8>>) -> Result<(), PipeEnded> {
        loop {
            match self.inbound.try_recv() {
                Ok(chunk) => out.push(chunk),
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => return Err(PipeEnded),
            }
        }
    }
}

impl Drop for KernelPipe {
    fn drop(&mut self) {
        // Shut the socket down before joining: the reader is parked in a
        // blocking `read`, and only the shutdown wakes it. Without this the
        // join waits for a device that will never send again.
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl AsRawFd for KernelPipe {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

/// The pipe is closed and drained; no more bytes will come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeEnded;

impl std::fmt::Display for PipeEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the kernel closed the usb/ip socket")
    }
}

/// Read whole buffers until the socket closes.
///
/// Each read is passed on as it arrives rather than accumulated: USB/IP framing
/// is the kernels' business, and holding bytes back to make tidier chunks would
/// add latency to every transfer for no one's benefit.
fn read_loop(mut socket: UnixStream, tx: std::sync::mpsc::Sender<Vec<u8>>) {
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        match socket.read(&mut buffer) {
            // Clean close: the kernel is done with this attachment.
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buffer[..n].to_vec()).is_err() {
                    // The session dropped the pipe; nothing left to feed.
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::debug!(error = %e, "usb/ip socket read ended");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    /// The kernel's end must be a real socket carrying bytes both ways — that
    /// is the whole substitution for `usbip`'s TCP connection.
    #[test]
    fn bytes_written_by_the_kernel_end_arrive_at_ours() {
        let (mut pipe, kernel) = KernelPipe::new().expect("pair");
        let mut kernel = UnixStream::from(kernel);

        kernel.write_all(b"urb-in").expect("kernel write");

        let mut chunks = Vec::new();
        // The reader thread is real, so allow it a moment to see the write.
        for _ in 0..100 {
            pipe.drain(&mut chunks).expect("still open");
            if !chunks.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(chunks, vec![b"urb-in".to_vec()]);
    }

    /// And the other direction: what a peer sends must reach the local kernel.
    #[test]
    fn bytes_we_write_reach_the_kernel_end() {
        let (mut pipe, kernel) = KernelPipe::new().expect("pair");
        let mut kernel = UnixStream::from(kernel);

        pipe.write(b"urb-out").expect("write");

        let mut buffer = [0u8; 16];
        let n = kernel.read(&mut buffer).expect("kernel read");
        assert_eq!(&buffer[..n], b"urb-out");
    }

    /// An idle device is the common case, and a pump that blocked on it would
    /// stall every other module sharing the session loop.
    #[test]
    fn draining_an_idle_pipe_returns_nothing_rather_than_blocking() {
        let (mut pipe, _kernel) = KernelPipe::new().expect("pair");
        let mut chunks = Vec::new();
        pipe.drain(&mut chunks).expect("still open");
        assert!(chunks.is_empty());
    }

    /// The last messages of a transfer arrive in the same breath as the close,
    /// so a closed pipe must hand them over before reporting the end.
    #[test]
    fn a_closed_pipe_yields_its_last_bytes_before_reporting_the_end() {
        let (mut pipe, kernel) = KernelPipe::new().expect("pair");
        let mut kernel = UnixStream::from(kernel);
        kernel.write_all(b"final").expect("kernel write");
        drop(kernel);

        let mut chunks = Vec::new();
        let mut ended = false;
        for _ in 0..100 {
            if pipe.drain(&mut chunks).is_err() {
                ended = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ended, "the pipe never reported the close");
        assert_eq!(chunks, vec![b"final".to_vec()]);
    }

    /// Dropping must close the socket: that is how a lost session stops a
    /// device being held by a kernel nobody is talking to any more.
    #[test]
    fn dropping_the_pipe_closes_the_kernels_end() {
        let (pipe, kernel) = KernelPipe::new().expect("pair");
        let mut kernel = UnixStream::from(kernel);
        drop(pipe);

        let mut buffer = [0u8; 8];
        assert_eq!(kernel.read(&mut buffer).expect("read"), 0);
    }

    /// The fd handed to the kernel must outlive the call that produced it, and
    /// be an owned one so a failed attach cannot leak it.
    #[test]
    fn the_kernels_end_is_a_usable_owned_descriptor() {
        let (_pipe, kernel) = KernelPipe::new().expect("pair");
        assert!(kernel.as_fd().as_raw_fd() >= 0);
    }
}
