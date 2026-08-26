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

/// USB/IP protocol version, binary-coded decimal (1.1.1).
///
/// `usbipd-win` refuses anything else outright — connecting and sending URBs
/// straight away is read as version `0.0.0` and the connection is dropped,
/// which is exactly the mistake this handshake exists to prevent.
const USBIP_VERSION: u16 = 0x0111;

/// "Import this device", the only opcode a client needs to send.
const OP_REQ_IMPORT: u16 = 0x8003;

/// The reply to it.
const OP_REP_IMPORT: u16 = 0x0003;

/// Length of the fixed-size bus id field in an import request.
const BUSID_LEN: usize = 32;

/// Header shared by the request and its reply: version, opcode, status.
const OP_HEADER_LEN: usize = 8;

/// `usbip_usb_device` as it follows a successful reply — path, bus id, and the
/// device's numbers. Read only to know where the reply ends: everything in it
/// is already known from the listing that led here.
const USB_DEVICE_LEN: usize = 312;

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
    /// Connect to the local USB/IP server and import `busid`.
    ///
    /// The import is not optional. `usbipd` speaks USB/IP, not a byte pipe:
    /// a client announces its version and asks for a device by bus id, and only
    /// once the server has agreed does the same connection carry URBs. Sending
    /// URBs first is read as protocol version `0.0.0` and the connection is
    /// closed — which looked, from the far end, exactly like a device that
    /// attaches and then vanishes every few seconds.
    ///
    /// Fails when `usbipd`'s service is not running, which is a different
    /// problem from the binary being absent and is worth saying so: the
    /// install is fine, the service is not.
    pub fn connect(busid: &str) -> Result<Self, UsbBackendError> {
        let mut socket = TcpStream::connect(USBIPD_ADDR).map_err(|e| {
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

        // Before anything else, and on the raw socket: the reader thread must
        // not be running yet, or it would swallow the reply this needs.
        import_device(&mut socket, busid)?;

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

/// Ask the server for a device, and read its answer.
///
/// Blocking and synchronous on purpose: nothing may share this socket until
/// the exchange is done, because the reply is on the same stream the URBs will
/// use and a reader started early would consume it.
fn import_device(socket: &mut TcpStream, busid: &str) -> Result<(), UsbBackendError> {
    // A fixed 32-byte field, null-padded. A longer bus id is not truncated but
    // refused: a truncated one names a different device, and importing the
    // wrong device is worse than failing to import.
    if busid.len() >= BUSID_LEN {
        return Err(UsbBackendError::Framing(format!(
            "bus id {busid} does not fit the protocol's {BUSID_LEN}-byte field"
        )));
    }

    let mut request = Vec::with_capacity(OP_HEADER_LEN + BUSID_LEN);
    request.extend_from_slice(&USBIP_VERSION.to_be_bytes());
    request.extend_from_slice(&OP_REQ_IMPORT.to_be_bytes());
    // Status is zero in a request; the server sets it in the reply.
    request.extend_from_slice(&0u32.to_be_bytes());
    request.extend_from_slice(busid.as_bytes());
    request.resize(OP_HEADER_LEN + BUSID_LEN, 0);

    socket
        .write_all(&request)
        .map_err(|e| UsbBackendError::Framing(format!("cannot ask usbipd for {busid}: {e}")))?;

    let mut header = [0u8; OP_HEADER_LEN];
    socket.read_exact(&mut header).map_err(|e| {
        UsbBackendError::Framing(format!("usbipd did not answer the import of {busid}: {e}"))
    })?;

    let reply = u16::from_be_bytes([header[2], header[3]]);
    let status = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if reply != OP_REP_IMPORT {
        return Err(UsbBackendError::Framing(format!(
            "usbipd answered {reply:#06x} rather than an import reply"
        )));
    }
    if status != 0 {
        // The usual cause is a device that was never `bind`-ed, which the
        // caller can act on; anything else is the server's own business.
        return Err(UsbBackendError::NoSuchDevice(format!(
            "usbipd refused to export {busid} (status {status})"
        )));
    }

    // The device description follows and is discarded: every field in it came
    // from the listing that led here, and reading it is only how the URB
    // stream is found to start.
    let mut device = [0u8; USB_DEVICE_LEN];
    socket.read_exact(&mut device).map_err(|e| {
        UsbBackendError::Framing(format!("usbipd sent a short device description: {e}"))
    })?;

    tracing::info!(busid, "imported a device from the local usbipd");
    Ok(())
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

    /// Answer an import request the way `usbipd` does: echo the header with a
    /// success status, then a device description of the right length.
    fn answer_import(socket: &mut TcpStream) {
        let mut request = [0u8; OP_HEADER_LEN + BUSID_LEN];
        socket.read_exact(&mut request).expect("request");
        let mut reply = Vec::new();
        reply.extend_from_slice(&USBIP_VERSION.to_be_bytes());
        reply.extend_from_slice(&OP_REP_IMPORT.to_be_bytes());
        reply.extend_from_slice(&0u32.to_be_bytes());
        reply.resize(OP_HEADER_LEN + USB_DEVICE_LEN, 0);
        socket.write_all(&reply).expect("reply");
    }

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
        let mut socket = TcpStream::connect(addr).expect("connect");
        socket.set_nodelay(true).expect("nodelay");
        import_device(&mut socket, "1-1").expect("import");
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
            answer_import(&mut socket);
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
            answer_import(&mut socket);
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
            let (mut socket, _) = listener.accept().expect("accept");
            answer_import(&mut socket);
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
            answer_import(&mut socket);
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

    /// The bug this cost an evening to find: sending URBs without importing
    /// first is read as protocol version 0.0.0, and usbipd drops the
    /// connection. The request's shape is therefore part of the contract.
    #[test]
    fn the_import_request_announces_the_version_and_names_the_device() {
        let (listener, addr) = echo_server();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut request = [0u8; OP_HEADER_LEN + BUSID_LEN];
            socket.read_exact(&mut request).expect("request");
            let mut reply = Vec::new();
            reply.extend_from_slice(&USBIP_VERSION.to_be_bytes());
            reply.extend_from_slice(&OP_REP_IMPORT.to_be_bytes());
            reply.extend_from_slice(&0u32.to_be_bytes());
            reply.resize(OP_HEADER_LEN + USB_DEVICE_LEN, 0);
            socket.write_all(&reply).expect("reply");
            request
        });

        let mut socket = TcpStream::connect(addr).expect("connect");
        import_device(&mut socket, "1-1").expect("import");
        let request = server.join().expect("server");

        assert_eq!(u16::from_be_bytes([request[0], request[1]]), USBIP_VERSION);
        assert_eq!(u16::from_be_bytes([request[2], request[3]]), OP_REQ_IMPORT);
        assert_eq!(&request[8..11], b"1-1");
        // Null-padded to the fixed field, or the server reads a longer name.
        assert!(request[11..].iter().all(|b| *b == 0));
    }

    /// A device that was never bound comes back as a non-zero status, and the
    /// caller can act on that — unlike a framing error.
    #[test]
    fn a_refused_import_is_reported_as_no_such_device() {
        let (listener, addr) = echo_server();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut request = [0u8; OP_HEADER_LEN + BUSID_LEN];
            socket.read_exact(&mut request).expect("request");
            let mut reply = Vec::new();
            reply.extend_from_slice(&USBIP_VERSION.to_be_bytes());
            reply.extend_from_slice(&OP_REP_IMPORT.to_be_bytes());
            reply.extend_from_slice(&1u32.to_be_bytes());
            socket.write_all(&reply).expect("reply");
        });

        let mut socket = TcpStream::connect(addr).expect("connect");
        let error = import_device(&mut socket, "9-9").expect_err("should refuse");
        assert!(
            matches!(error, UsbBackendError::NoSuchDevice(_)),
            "got {error:?}"
        );
    }

    /// A bus id that does not fit must be refused rather than truncated: a
    /// truncated one names a different device, and importing the wrong device
    /// is worse than importing none.
    #[test]
    fn an_oversized_bus_id_is_refused_not_truncated() {
        let (listener, addr) = echo_server();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let mut socket = TcpStream::connect(addr).expect("connect");
        let long = "1-".to_string() + &"9".repeat(BUSID_LEN);
        assert!(import_device(&mut socket, &long).is_err());
    }

    /// The address is the security property, not a configuration knob: a
    /// non-loopback default would put an unauthenticated USB/IP server on the
    /// network, which is the thing the mesh replaces.
    #[test]
    fn the_server_address_is_loopback() {
        assert!(USBIPD_ADDR.starts_with("127.0.0.1:"));
    }
}
