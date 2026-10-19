//! The mesh's Bluetooth channel on Windows: Winsock RFCOMM, both ways.
//!
//! Windows has no BlueZ to hand sockets over, but Winsock speaks RFCOMM
//! directly (`AF_BTH`), so this side listens and dials itself. The listener
//! binds the channel every Cursio computer pins ([`crate::MESH_CHANNEL`]),
//! because a Linux machine dials that channel without asking SDP (see
//! `linux/rfcomm.rs`), and publishes an SDP record under [`crate::MESH_UUID`]
//! so a phone, which looks the service up by UUID, finds it too. The record
//! lives as long as the process does.
//!
//! Dialling goes by UUID instead: Windows runs the SDP query itself on every
//! connect, so a computer whose channel landed elsewhere is still reached.
//!
//! No `SO_BTH_AUTHENTICATE`: on a device that is not paired it makes Windows
//! pop its own pairing dialog from inside a background daemon. Only paired
//! devices are listed for dialling, and Noise refuses anyone without the
//! mesh key whatever the radio did.
//!
//! The sockets are plain blocking ones. Whether Winsock's Bluetooth provider
//! works under the readiness polling Tokio uses is undocumented, so the
//! node pumps each socket on two threads rather than finding out live.

use std::io;
use std::os::windows::io::{FromRawSocket, OwnedSocket, RawSocket};
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::Bluetooth::{
    BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindNextDevice, AF_BTH,
    BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS, BTHPROTO_RFCOMM, NS_BTH, SOCKADDR_BTH,
};
use windows_sys::Win32::Foundation::{
    GetLastError, SetHandleInformation, ERROR_NO_MORE_ITEMS, HANDLE, HANDLE_FLAG_INHERIT,
};
use windows_sys::Win32::Networking::WinSock::{
    accept, bind, connect, getsockname, listen, WSAGetLastError, WSASetServiceW, WSASocketW,
    WSAStartup, CSADDR_INFO, INVALID_SOCKET, RNRSERVICE_REGISTER, SOCKADDR, SOCKET, SOCKET_ADDRESS,
    SOCK_STREAM, WSADATA, WSAEADDRINUSE, WSAECONNREFUSED, WSAEHOSTDOWN, WSAQUERYSETW,
    WSASERVICE_NOT_FOUND, WSA_FLAG_NO_HANDLE_INHERIT,
};

use crate::{Address, BluetoothError, PairedDevice, MESH_CHANNEL};

/// `BT_PORT_ANY`, absent from the bindings: let the stack pick the channel.
const PORT_ANY: u32 = u32::MAX;
/// Bits 8..13 of the class of device.
const MAJOR_CLASS_COMPUTER: u32 = 0x01;

/// [`crate::MESH_UUID`] as Windows spells a UUID.
const MESH_GUID: GUID = GUID::from_u128(0x8a3f6c2e_0b4d_4e8a_9c1f_43757273696f);

/// The registered listener. Dropping it keeps listening: the thread owns the
/// socket, and the process' end takes both it and the SDP record.
pub struct MeshChannel {
    _private: (),
}

impl MeshChannel {
    /// Listen, publish the record, and return the sockets paired devices
    /// open, each with the device's address.
    pub fn start() -> Result<(Self, Receiver<(Address, OwnedSocket)>), BluetoothError> {
        startup()?;
        let listener = rfcomm_socket()?;
        let pinned = match bind_port(&listener, u32::from(MESH_CHANNEL)) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(WSAEADDRINUSE) => {
                bind_port(&listener, PORT_ANY).map_err(BluetoothError::Io)?;
                false
            }
            Err(e) => return Err(BluetoothError::RegisterChannel(e.to_string())),
        };
        // SAFETY: a bound socket we own.
        if unsafe { listen(raw(&listener), 4) } != 0 {
            return Err(BluetoothError::RegisterChannel(last_error().to_string()));
        }
        let mut local = SOCKADDR_BTH::default();
        let mut len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
        // SAFETY: local/len describe a writable SOCKADDR_BTH.
        if unsafe { getsockname(raw(&listener), sockaddr_mut(&mut local), &mut len) } != 0 {
            return Err(BluetoothError::RegisterChannel(last_error().to_string()));
        }
        publish(&mut local)?;
        let channel = local.port;
        if pinned {
            tracing::info!(
                channel,
                "paired devices can reach this machine over bluetooth"
            );
        } else {
            tracing::warn!(
                channel,
                pinned = MESH_CHANNEL,
                "another program holds the mesh's bluetooth channel; phones and Windows machines still reach this one, Linux machines cannot"
            );
        }

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("bluetooth-mesh".into())
            .spawn(move || loop {
                let mut peer = SOCKADDR_BTH::default();
                let mut len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
                // SAFETY: peer/len describe a writable SOCKADDR_BTH.
                let accepted = unsafe { accept(raw(&listener), sockaddr_mut(&mut peer), &mut len) };
                if accepted == INVALID_SOCKET {
                    tracing::warn!(error = %last_error(), "the mesh's bluetooth channel stopped accepting");
                    return;
                }
                // SAFETY: accept returned a socket nothing else owns.
                let accepted = unsafe { OwnedSocket::from_raw_socket(accepted as RawSocket) };
                not_inherited(&accepted);
                let address = address_of(peer.btAddr);
                tracing::info!(peer = %address, "a paired device opened the mesh's bluetooth channel");
                if tx.send((address, accepted)).is_err() {
                    return;
                }
            })?;
        Ok((MeshChannel { _private: () }, rx))
    }
}

/// Connect to `address`'s mesh channel, found by SDP.
///
/// Blocking for as long as the radio pages the device (seconds; Windows
/// picks the timeout), so call it off the node's runtime. Nothing listening
/// there is [`BluetoothError::NoService`].
pub fn dial(address: &str) -> Result<OwnedSocket, BluetoothError> {
    startup()?;
    let remote = bth_addr(address)?;
    let socket = rfcomm_socket()?;
    let target = SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: remote,
        serviceClassId: MESH_GUID,
        port: 0,
    };
    // SAFETY: target is a valid SOCKADDR_BTH for the length passed.
    let r = unsafe {
        connect(
            raw(&socket),
            &target as *const SOCKADDR_BTH as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_BTH>() as i32,
        )
    };
    if r != 0 {
        let e = last_error();
        return Err(match e.raw_os_error() {
            // Nothing on the channel, no such record, or a refusal: the
            // device is there and is not offering the mesh.
            Some(WSAEHOSTDOWN | WSASERVICE_NOT_FOUND | WSAECONNREFUSED) => {
                BluetoothError::NoService(address.to_string())
            }
            _ => BluetoothError::Io(e),
        });
    }
    Ok(socket)
}

/// Computers paired with this one, as Windows remembers them. No inquiry:
/// only what is already paired, so listing costs no radio time.
pub fn paired_computers() -> Result<Vec<PairedDevice>, BluetoothError> {
    let params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: 1,
        fReturnRemembered: 1,
        fReturnUnknown: 0,
        fReturnConnected: 1,
        fIssueInquiry: 0,
        cTimeoutMultiplier: 0,
        hRadio: std::ptr::null_mut(),
    };
    let mut info = BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    // SAFETY: params and info are initialised with their sizes.
    let find = unsafe { BluetoothFindFirstDevice(&params, &mut info) };
    if find.is_null() {
        // SAFETY: plain thread-local error read.
        let code = unsafe { GetLastError() };
        return if code == ERROR_NO_MORE_ITEMS {
            Ok(Vec::new())
        } else {
            Err(BluetoothError::Io(io::Error::from_raw_os_error(
                code as i32,
            )))
        };
    }
    let mut computers = Vec::new();
    loop {
        if info.fAuthenticated != 0 && is_computer_class(info.ulClassofDevice) {
            // SAFETY: every bit pattern of the union is a valid u64.
            let address = address_of(unsafe { info.Address.Anonymous.ullLong });
            let name = String::from_utf16_lossy(&info.szName);
            let name = name.trim_end_matches('\0').to_string();
            computers.push(PairedDevice {
                name: if name.is_empty() {
                    address.clone()
                } else {
                    name
                },
                address,
                connected: info.fConnected != 0,
            });
        }
        // SAFETY: find is a live handle and info keeps its size.
        if unsafe { BluetoothFindNextDevice(find, &mut info) } == 0 {
            break;
        }
    }
    // SAFETY: find came from BluetoothFindFirstDevice and is closed once.
    unsafe { BluetoothFindDeviceClose(find) };
    computers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(computers)
}

/// Whether a Class of Device has the computer major class (bits 8–12).
fn is_computer_class(class: u32) -> bool {
    (class >> 8) & 0x1F == MAJOR_CLASS_COMPUTER
}

/// Winsock has to be started once per process before any socket call. The
/// standard library starts it too, but only on its own first socket.
fn startup() -> Result<(), BluetoothError> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        let mut data = std::mem::MaybeUninit::<WSADATA>::uninit();
        // SAFETY: WSAStartup only writes the WSADATA it is given.
        unsafe { WSAStartup(0x0202, data.as_mut_ptr()) }
    });
    match code {
        0 => Ok(()),
        code => Err(BluetoothError::Io(io::Error::from_raw_os_error(code))),
    }
}

/// A new RFCOMM socket that no child process inherits.
///
/// Winsock sockets are inheritable by default, and the node starts children
/// — the update helper, which starts the next node, and the drive helper. A
/// listener inherited by them outlived the node that bound it: after one
/// update switch the next node found the pinned channel held and fell back
/// to another, and each switch after that took one more (measured on
/// Windows: 23, then 4, then 5).
fn rfcomm_socket() -> Result<OwnedSocket, BluetoothError> {
    // SAFETY: plain WSASocketW with no protocol info; the result is checked
    // before it is owned.
    let s = unsafe {
        WSASocketW(
            AF_BTH as i32,
            SOCK_STREAM,
            BTHPROTO_RFCOMM as i32,
            std::ptr::null(),
            0,
            WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if s == INVALID_SOCKET {
        let e = last_error();
        return Err(BluetoothError::Unavailable(e.to_string()));
    }
    // SAFETY: s was just returned by WSASocketW and nothing else owns it.
    Ok(unsafe { OwnedSocket::from_raw_socket(s as RawSocket) })
}

/// Keep an accepted socket out of child processes too; whether it takes the
/// listener's setting is not documented.
fn not_inherited(socket: &OwnedSocket) {
    // SAFETY: the socket is a live handle we own; only its inherit flag is
    // changed.
    if unsafe { SetHandleInformation(raw(socket) as HANDLE, HANDLE_FLAG_INHERIT, 0) } == 0 {
        tracing::debug!(error = %io::Error::last_os_error(), "an accepted bluetooth socket stays inheritable");
    }
}

fn bind_port(socket: &OwnedSocket, port: u32) -> io::Result<()> {
    let local = SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: 0,
        serviceClassId: GUID::from_u128(0),
        port,
    };
    // SAFETY: local is a valid SOCKADDR_BTH for the length passed.
    let r = unsafe {
        bind(
            raw(socket),
            &local as *const SOCKADDR_BTH as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_BTH>() as i32,
        )
    };
    if r != 0 {
        return Err(last_error());
    }
    Ok(())
}

/// Publish the SDP record for the bound socket at `local`.
fn publish(local: &mut SOCKADDR_BTH) -> Result<(), BluetoothError> {
    let mut name: Vec<u16> = "Cursio".encode_utf16().chain(Some(0)).collect();
    let mut class = MESH_GUID;
    let mut address = CSADDR_INFO {
        LocalAddr: SOCKET_ADDRESS {
            lpSockaddr: sockaddr_mut(local),
            iSockaddrLength: std::mem::size_of::<SOCKADDR_BTH>() as i32,
        },
        RemoteAddr: SOCKET_ADDRESS {
            lpSockaddr: std::ptr::null_mut(),
            iSockaddrLength: 0,
        },
        iSocketType: SOCK_STREAM,
        iProtocol: BTHPROTO_RFCOMM as i32,
    };
    let query = WSAQUERYSETW {
        dwSize: std::mem::size_of::<WSAQUERYSETW>() as u32,
        lpszServiceInstanceName: name.as_mut_ptr(),
        lpServiceClassId: &mut class,
        dwNameSpace: NS_BTH,
        dwNumberOfCsAddrs: 1,
        lpcsaBuffer: &mut address,
        ..Default::default()
    };
    // SAFETY: every pointer in query outlives the call, which copies them.
    if unsafe { WSASetServiceW(&query, RNRSERVICE_REGISTER, 0) } != 0 {
        return Err(BluetoothError::RegisterChannel(last_error().to_string()));
    }
    Ok(())
}

fn raw(socket: &OwnedSocket) -> SOCKET {
    use std::os::windows::io::AsRawSocket;
    socket.as_raw_socket() as SOCKET
}

fn sockaddr_mut(addr: &mut SOCKADDR_BTH) -> *mut SOCKADDR {
    addr as *mut SOCKADDR_BTH as *mut SOCKADDR
}

fn last_error() -> io::Error {
    // SAFETY: plain thread-local error read.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// `"9C:9E:D5:78:FE:C4"` as the 48-bit number Windows keeps it in.
fn bth_addr(address: &str) -> Result<u64, BluetoothError> {
    let bad = || {
        BluetoothError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad bluetooth address",
        ))
    };
    let parts: Vec<&str> = address.split(':').collect();
    if parts.len() != 6 {
        return Err(bad());
    }
    parts.iter().try_fold(0u64, |acc, part| {
        u8::from_str_radix(part, 16)
            .map(|byte| (acc << 8) | u64::from(byte))
            .map_err(|_| bad())
    })
}

fn address_of(addr: u64) -> String {
    (0..6)
        .rev()
        .map(|i| format!("{:02X}", (addr >> (i * 8)) & 0xFF))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_survives_the_round_trip_through_windows_number() {
        let n = bth_addr("04:68:74:98:F7:C6").unwrap();
        assert_eq!(n, 0x0468_7498_F7C6);
        assert_eq!(address_of(n), "04:68:74:98:F7:C6");
    }

    #[test]
    fn a_laptops_class_is_a_computer_and_a_phones_is_not() {
        assert!(is_computer_class(0x2E410C));
        assert!(!is_computer_class(0x5A420C));
    }
}
