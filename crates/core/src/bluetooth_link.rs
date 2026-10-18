//! The mesh over Bluetooth: a second path, not a second protocol.
//!
//! A phone that has lost the network — Wi-Fi off, out of the house's range,
//! on a hotspot the computer is not on — is still the same peer. So a
//! Bluetooth link is just another byte stream under the same `Hello`, pairing
//! key, Noise handshake, module negotiation and priority classes that run
//! over TCP; nothing here speaks a Bluetooth profile's protocol.
//!
//! **Who calls whom.** The phone dials, the computer listens. Android does not
//! let an app learn its own Bluetooth address, so the computer has nothing to
//! dial until the phone has already connected; the phone, on the other hand,
//! sees every computer it is paired with. The computer's side is a BlueZ
//! external profile under [`cursio_bluetooth::MESH_UUID`]
//! ([`cursio_bluetooth::MeshChannel`]), registered on a thread of its own
//! because zbus's blocking calls cannot run inside the node's runtime.
//!
//! **Which path wins.** The network, whenever it is there: Bluetooth carries a
//! few hundred kilobytes a second, enough for input, notifications, messages,
//! calls' control and Opus audio — the priority classes keep those ahead —
//! but not for a screen. The phone only dials over Bluetooth a computer that
//! the network has not reached for a while, and ends that link once the
//! computer is heard on the network again, so the ordinary dial takes over.

use std::os::fd::OwnedFd;

use cursio_transport::stream::{StreamTransport, BLUETOOTH};
use tokio::sync::mpsc;

/// A connected Bluetooth socket as a mesh transport.
///
/// Any stream socket descriptor works: the RFCOMM socket BlueZ hands over, or
/// the end of the socket pair the Android app pumps an RFCOMM stream through.
/// Needs the runtime, which registers it for readiness.
pub fn transport(fd: OwnedFd) -> std::io::Result<StreamTransport<tokio::net::UnixStream>> {
    let socket = std::os::unix::net::UnixStream::from(fd);
    socket.set_nonblocking(true)?;
    Ok(StreamTransport::new(
        tokio::net::UnixStream::from_std(socket)?,
        BLUETOOTH,
    ))
}

/// Paired devices opening the mesh's channel on this machine.
pub struct Listener {
    accepted: mpsc::UnboundedReceiver<(String, OwnedFd)>,
    /// Held where nothing can listen, so `accept` waits forever rather than
    /// reporting the channel closed on every turn of the caller's loop.
    _idle: Option<mpsc::UnboundedSender<(String, OwnedFd)>>,
}

impl Listener {
    /// The next device to connect, by its Bluetooth address.
    pub async fn accept(&mut self) -> Option<(String, StreamTransport<tokio::net::UnixStream>)> {
        loop {
            let (address, fd) = self.accepted.recv().await?;
            match transport(fd) {
                Ok(link) => return Some((address, link)),
                Err(e) => {
                    tracing::warn!(peer = %address, error = %e, "cannot use a bluetooth connection")
                }
            }
        }
    }
}

/// Start listening for paired phones on the mesh's Bluetooth channel.
///
/// Never fails the node: without Bluetooth, or with BlueZ refusing the
/// channel, the mesh simply has the network only, and the reason is logged.
#[cfg(target_os = "linux")]
pub fn listen() -> Listener {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawned = std::thread::Builder::new()
        .name("bluetooth-mesh".into())
        .spawn(move || {
            let (channel, accepted) = match cursio_bluetooth::MeshChannel::start() {
                Ok(started) => started,
                Err(e) => {
                    tracing::warn!(error = %e, "phones off the network cannot reach this machine over bluetooth");
                    return;
                }
            };
            // The registration lives as long as the bus connection.
            let _channel = channel;
            for accepted in accepted {
                if tx.send(accepted).is_err() {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the bluetooth channel");
    }
    Listener {
        accepted: rx,
        _idle: None,
    }
}

/// Start listening for paired phones on the mesh's Bluetooth channel.
#[cfg(not(target_os = "linux"))]
pub fn listen() -> Listener {
    tracing::warn!(
        "the mesh's bluetooth channel is not implemented on this platform; phones reach it over the network only"
    );
    let (tx, rx) = mpsc::unbounded_channel();
    Listener {
        accepted: rx,
        _idle: Some(tx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::Message;
    use cursio_transport::Transport;

    #[tokio::test]
    async fn a_socket_descriptor_becomes_a_bluetooth_transport() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut near = transport(OwnedFd::from(ours)).unwrap();
        let mut far = transport(OwnedFd::from(theirs)).unwrap();
        assert_eq!(near.kind(), BLUETOOTH);
        near.send(&Message::Heartbeat { nonce: 7 }).await.unwrap();
        assert_eq!(far.recv().await.unwrap(), Message::Heartbeat { nonce: 7 });
    }
}
