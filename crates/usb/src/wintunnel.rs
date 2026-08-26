//! Carrying URBs to `usbipd-win`, over the loopback it already listens on.
//!
//! On Linux this crate hands the kernel a socketpair and keeps the other end
//! ([`crate::pipe`]), which is what removes the listening port entirely. That
//! substitution is impossible here: `usbipd-win` owns its side of the driver
//! and offers no way to give it a descriptor.
//!
//! What it does offer is a plain USB/IP server on TCP 3240 — the same protocol,
//! the same framing, reachable on `127.0.0.1`. So Cursio connects to it as an
//! ordinary client and carries the bytes over the mesh:
//!
//! ```text
//! vhci-hcd ── socketpair ── Cursio ══ Noise ══ Cursio ── 127.0.0.1:3240 ── usbipd
//! ```
//!
//! ## Loopback, and only loopback
//!
//! `127.0.0.1` is not a detail — it is the invariant. `usbipd` authenticates
//! nobody: whoever reaches port 3240 takes the device, which is precisely what
//! `docs/usb-design.md` says the mesh exists to replace. Connecting from this
//! machine to itself means the only route in is still the Noise channel, with
//! every peer authenticated by SPAKE2 pairing.
//!
//! A user whose firewall exposes 3240 to the LAN has that hole with or without
//! Cursio; nothing here opens it, and nothing here needs it open.
//!
//! ## Blocking reads on their own thread
//!
//! Same reason as the Linux pipe: the read must not be cancelled, because a
//! partially-read USB/IP message leaves the stream misaligned and every later
//! message is then misparsed rather than dropped.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::UsbBackendError;

/// Where `usbipd-win` listens. Loopback by design — see the module docs.
const USBIPD_ADDR: &str = "127.0.0.1:3240";

/// How much is read at once. Matches the protocol's own chunk cap, so a read
/// never produces something the sender would have to split anyway.
const READ_CHUNK: usize = 256 * 1024;

/// A connection to the local USB/IP server, plus the thread reading it.
///
/// Dropping it closes the connection, which is how `usbipd` learns the
/// attachment is over — the same contract the Linux pipe has, reached a
/// different way.
pub struct WinTunnel {
    socket: TcpStream,
    inbound: Receiver<Vec<u8>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl WinTunnel {
    /// Connect to the local USB/IP server.
    ///
    /// Fails when `usbipd`'s service is not running, which is a different
    /// problem from the binary being absent and is worth saying so: the
    /// install is fine, the service is not.
    pub fn connect() -> Result<Self, UsbBackendError> {
        let socket = TcpStream::connect(USBIPD_ADDR).map_err(|e| {
            UsbBackendError::DriverMissing(format!(
                "cannot reach the usbipd service on {USBIPD_ADDR}: {e}; \
                 check that it is running (services.msc → usbipd)"
            ))
        })?;
        // USB/IP is request/response per URB, so Nagle would sit on a small
        // packet waiting for company that is not coming.
        socket
            .set_nodelay(true)
            .map_err(|e| UsbBackendError::Framing(format!("cannot set nodelay: {e}")))?;

        let read_half = socket
            .try_clone()
            .map_err(|e| UsbBackendError::Framing(format!("cannot clone the socket: {e}")))?;
        let (tx, inbound) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("cursio-usbipd".into())
            .spawn(move || read_loop(read_half, tx))
            .map_err(|e| UsbBackendError::Framing(format!("cannot spawn the reader: {e}")))?;

        Ok(Self {
            socket,
            inbound,
            reader: Some(reader),
        })
    }

    /// Hand bytes from the peer to the local USB/IP server.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), UsbBackendError> {
        self.socket
            .write_all(bytes)
            .map_err(|e| UsbBackendError::Framing(format!("cannot write to usbipd: {e}")))
    }

    /// Take whatever the server has produced since the last call.
    ///
    /// Never blocks: the caller is a session pump. `Err` means the connection
    /// is closed and everything already read has been handed over — draining
    /// first matters, because a transfer's last bytes arrive in the same breath
    /// as the close.
    pub fn drain(&mut self, out: &mut Vec<Vec<u8>>) -> Result<(), TunnelEnded> {
        loop {
            match self.inbound.try_recv() {
                Ok(chunk) => out.push(chunk),
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => return Err(TunnelEnded),
            }
        }
    }
}

impl Drop for WinTunnel {
    fn drop(&mut self) {
        // Shut down before joining: the reader is parked in a blocking read,
        // and only this wakes it.
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// The connection is closed and drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelEnded;

impl std::fmt::Display for TunnelEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the usbipd connection closed")
    }
}

/// Read whole buffers until the connection closes.
fn read_loop(mut socket: TcpStream, tx: std::sync::mpsc::Sender<Vec<u8>>) {
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        match socket.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buffer[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::debug!(error = %e, "usbipd connection read ended");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Stand in for `usbipd`: accept one connection and echo, so the tunnel's
    /// own behaviour can be tested without the service.
    fn echo_server() -> (TcpListener, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        (listener, addr)
    }

    /// Connect to an arbitrary address, for tests only: the real one is fixed
    /// at loopback 3240 and must stay that way.
    fn connect_to(addr: std::net::SocketAddr) -> WinTunnel {
        let socket = TcpStream::connect(addr).expect("connect");
        socket.set_nodelay(true).expect("nodelay");
        let read_half = socket.try_clone().expect("clone");
        let (tx, inbound) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || read_loop(read_half, tx));
        WinTunnel {
            socket,
            inbound,
            reader: Some(reader),
        }
    }

    /// The server's bytes must reach us — that is the whole substitution for
    /// the socketpair Linux gets.
    #[test]
    fn bytes_from_the_server_arrive() {
        let (listener, addr) = echo_server();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket.write_all(b"urb-in").expect("write");
            // Hold the connection open so the read loop does not see a close
            // before the test has drained.
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        let mut tunnel = connect_to(addr);
        let mut chunks = Vec::new();
        for _ in 0..100 {
            let _ = tunnel.drain(&mut chunks);
            if !chunks.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(chunks, vec![b"urb-in".to_vec()]);
        drop(tunnel);
        server.join().expect("server");
    }

    /// And ours must reach the server.
    #[test]
    fn bytes_we_write_reach_the_server() {
        let (listener, addr) = echo_server();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 16];
            let n = socket.read(&mut buffer).expect("read");
            buffer[..n].to_vec()
        });

        let mut tunnel = connect_to(addr);
        tunnel.write(b"urb-out").expect("write");
        let got = server.join().expect("server");
        assert_eq!(got, b"urb-out");
    }

    /// An idle device is the common case, and a pump that blocked on it would
    /// stall every other module sharing the session loop.
    #[test]
    fn draining_an_idle_tunnel_returns_nothing() {
        let (listener, addr) = echo_server();
        let server = std::thread::spawn(move || {
            let _ = listener.accept();
            std::thread::sleep(std::time::Duration::from_millis(150));
        });

        let mut tunnel = connect_to(addr);
        let mut chunks = Vec::new();
        tunnel.drain(&mut chunks).expect("still open");
        assert!(chunks.is_empty());
        drop(tunnel);
        server.join().expect("server");
    }

    /// A closed connection must hand over its last bytes before reporting the
    /// end, or the tail of every transfer is lost.
    #[test]
    fn a_closed_tunnel_yields_its_last_bytes_first() {
        let (listener, addr) = echo_server();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket.write_all(b"final").expect("write");
        });

        let mut tunnel = connect_to(addr);
        let mut chunks = Vec::new();
        let mut ended = false;
        for _ in 0..100 {
            if tunnel.drain(&mut chunks).is_err() {
                ended = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ended, "the tunnel never reported the close");
        assert_eq!(chunks, vec![b"final".to_vec()]);
    }

    /// The address is the security property, not a configuration knob: a
    /// non-loopback default would put an unauthenticated USB/IP server on the
    /// network, which is the thing the mesh replaces.
    #[test]
    fn the_server_address_is_loopback() {
        assert!(USBIPD_ADDR.starts_with("127.0.0.1:"));
    }
}
