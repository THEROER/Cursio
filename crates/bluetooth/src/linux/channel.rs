//! The mesh's own channel over Bluetooth: an RFCOMM socket and nothing else.
//!
//! Unlike the hands-free and speaker roles, nothing here speaks a Bluetooth
//! profile's protocol. The service is a bare RFCOMM listener under Cursio's
//! own UUID, so a paired phone can open a byte stream to this machine; what
//! crosses it is the mesh — `Hello`, Noise, modules — exactly as over TCP.
//!
//! BlueZ owns the adapter and SDP, so the listener is an external profile:
//! `RegisterProfile` with the server role makes BlueZ pick a channel, publish
//! the record and hand every accepted socket to `NewConnection`. The phone
//! dials, never this side: Android does not tell its own Bluetooth address,
//! so the computer has nothing to dial until the phone has said hello.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

use zbus::blocking::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::{Address, BluetoothError};

const PROFILE_PATH: &str = "/dev/cursio/mesh";

struct Profile {
    accepted: Mutex<Sender<(Address, OwnedFd)>>,
}

#[zbus::interface(name = "org.bluez.Profile1")]
impl Profile {
    fn release(&self) {
        tracing::info!("bluez released the mesh's bluetooth channel");
    }

    fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zbus::zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) {
        let address =
            super::address_from_path(device.as_str()).unwrap_or_else(|| device.to_string());
        tracing::info!(peer = %address, "a paired device opened the mesh's bluetooth channel");
        let fd: OwnedFd = fd.into();
        let _ = self
            .accepted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send((address, fd));
    }

    fn request_disconnection(&self, _device: OwnedObjectPath) {}
}

/// The registered listener. Dropping it keeps the registration (BlueZ drops
/// it with the bus connection, when the process ends).
pub struct MeshChannel {
    _bus: Connection,
}

impl MeshChannel {
    /// Register the listener and return the sockets paired devices open.
    ///
    /// Blocking D-Bus: call it from a thread outside any async runtime.
    pub fn start() -> Result<(Self, Receiver<(Address, OwnedFd)>), BluetoothError> {
        let (tx, rx) = mpsc::channel();
        let bus = zbus::blocking::connection::Builder::system()
            .and_then(|b| {
                b.serve_at(
                    PROFILE_PATH,
                    Profile {
                        accepted: Mutex::new(tx),
                    },
                )
            })
            .and_then(|b| b.build())
            .map_err(|e| BluetoothError::Bus(e.to_string()))?;
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("Name", Value::from("Cursio"));
        options.insert("Role", Value::from("server"));
        // Only a device the user paired may open it, and the link is
        // encrypted by the radio as well as by Noise above it.
        options.insert("RequireAuthentication", Value::Bool(true));
        options.insert("RequireAuthorization", Value::Bool(false));
        let path = ObjectPath::try_from(PROFILE_PATH).expect("constant path is valid");
        bus.call_method(
            Some("org.bluez"),
            "/org/bluez",
            Some("org.bluez.ProfileManager1"),
            "RegisterProfile",
            &(path, crate::MESH_UUID, options),
        )
        .map_err(|e| BluetoothError::RegisterChannel(e.to_string()))?;
        tracing::info!("paired phones can reach this machine over bluetooth");
        Ok((MeshChannel { _bus: bus }, rx))
    }
}
