//! Calling another computer's mesh channel: a raw RFCOMM socket.
//!
//! BlueZ could place the call (`Device1.ConnectProfile` with the mesh's UUID),
//! but it looks the UUID up in the remote's service list as cached at
//! pairing, and refreshes that cache on no schedule anyone can rely on. A
//! computer that started offering the channel after it was paired — every
//! machine paired before this existed — is then "not available" until the
//! pairing is redone. So the call goes straight to the channel every Cursio
//! computer pins ([`crate::MESH_CHANNEL`]), with no SDP lookup at all.
//!
//! The socket asks for an authenticated, encrypted link, as the listener
//! does: only a device the user paired gets through, and the radio encrypts
//! under Noise.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use super::sco::bdaddr;
use crate::BluetoothError;

const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_RFCOMM: libc::c_int = 3;
const SOL_BLUETOOTH: libc::c_int = 274;
const BT_SECURITY: libc::c_int = 4;
/// Authenticated and encrypted, with the key paired devices already share.
const BT_SECURITY_MEDIUM: u8 = 2;

#[repr(C)]
struct SockaddrRc {
    family: libc::sa_family_t,
    bdaddr: [u8; 6],
    channel: u8,
}

#[repr(C)]
struct BtSecurity {
    level: u8,
    key_size: u8,
}

/// Connect to `address`'s mesh channel, giving up after `timeout`.
///
/// Blocking: the radio pages the device for up to a few seconds, so call it
/// off the node's runtime. A device that answers but has no listener on the
/// channel is [`BluetoothError::NoService`], which callers treat as "not a
/// Cursio machine, or one with Bluetooth off" rather than "out of range".
pub fn dial(address: &str, channel: u8, timeout: Duration) -> Result<OwnedFd, BluetoothError> {
    let remote = bdaddr(address)?;
    // SAFETY: plain socket(2); the result is checked before it is owned.
    let fd = unsafe {
        libc::socket(
            AF_BLUETOOTH,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            BTPROTO_RFCOMM,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: fd was just returned by socket(2) and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let security = BtSecurity {
        level: BT_SECURITY_MEDIUM,
        key_size: 0,
    };
    // SAFETY: security is a valid struct bt_security for the length passed.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            SOL_BLUETOOTH,
            BT_SECURITY,
            &security as *const BtSecurity as *const libc::c_void,
            std::mem::size_of::<BtSecurity>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let addr = SockaddrRc {
        family: AF_BLUETOOTH as libc::sa_family_t,
        bdaddr: remote,
        channel,
    };
    // SAFETY: addr is a valid sockaddr_rc for the length passed.
    let r = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const SockaddrRc as *const libc::sockaddr,
            std::mem::size_of::<SockaddrRc>() as libc::socklen_t,
        )
    };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(refusal(e, address));
        }
        wait_connected(&fd, timeout).map_err(|e| refusal(e, address))?;
    }
    Ok(fd)
}

/// Whether nothing on this machine listens on RFCOMM `channel`.
///
/// Asked before BlueZ is told to listen there, because BlueZ does not say
/// when it cannot: `RegisterProfile` succeeds with no listener and no
/// service record behind it, and the mesh would wait for calls that cannot
/// arrive. The kernel judges a bind clashing only against a listener on the
/// same local address, and BlueZ binds the adapter's own, so both that and
/// the wildcard are tried.
pub fn channel_free(channel: u8, adapter: Option<&str>) -> bool {
    let adapter = adapter.and_then(|a| bdaddr(a).ok());
    std::iter::once([0; 6])
        .chain(adapter)
        .all(|local| bindable(channel, local))
}

fn bindable(channel: u8, local: [u8; 6]) -> bool {
    // SAFETY: plain socket(2); the result is checked before it is owned.
    let fd = unsafe {
        libc::socket(
            AF_BLUETOOTH,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            BTPROTO_RFCOMM,
        )
    };
    if fd < 0 {
        return false;
    }
    // SAFETY: fd was just returned by socket(2) and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let addr = SockaddrRc {
        family: AF_BLUETOOTH as libc::sa_family_t,
        bdaddr: local,
        channel,
    };
    // SAFETY: addr is a valid sockaddr_rc for the length passed.
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const SockaddrRc as *const libc::sockaddr,
            std::mem::size_of::<SockaddrRc>() as libc::socklen_t,
        )
    };
    r == 0
}

fn wait_connected(fd: &OwnedFd, timeout: Duration) -> io::Result<()> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd.
    let r = unsafe { libc::poll(&mut pfd, 1, ms) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if r == 0 {
        return Err(io::Error::from(io::ErrorKind::TimedOut));
    }
    let mut error: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: SO_ERROR fills one c_int; len bounds the write.
    let r = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut error as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    Ok(())
}

/// A refused connection means the device is there and nothing listens on
/// the channel; anything else (down, out of range, timed out) is the radio.
fn refusal(e: io::Error, address: &str) -> BluetoothError {
    if e.raw_os_error() == Some(libc::ECONNREFUSED) {
        BluetoothError::NoService(address.to_string())
    } else {
        BluetoothError::Io(e)
    }
}
