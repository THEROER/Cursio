//! SCO sockets: the call's voice, as raw PCM.
//!
//! PipeWire listens for SCO on the wildcard address for its own audio
//! gateway role. The kernel hands an incoming SCO connection to the listener
//! bound to the *exact* local address before the wildcard one, so binding to
//! the adapter's own address takes the phone's call audio without touching
//! PipeWire. It would take a headset's too, so the listener exists only while
//! one of our phones has a call.
//!
//! A wideband call needs the link in transparent air mode, so the controller
//! passes mSBC through instead of converting CVSD. Outgoing, that is set
//! before connecting. Incoming, the listener defers accepting: the socket
//! comes back before the link is set up, takes the air mode the phone asked
//! for with `+BCS`, and is accepted by the first read ([`authorize`]).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_SCO: libc::c_int = 2;
const SOL_SCO: libc::c_int = 17;
const SCO_OPTIONS: libc::c_int = 1;
const SOL_BLUETOOTH: libc::c_int = 274;
const BT_DEFER_SETUP: libc::c_int = 7;
const BT_VOICE: libc::c_int = 11;
/// `BT_VOICE_CVSD_16BIT`, the kernel's default air mode.
const VOICE_CVSD: u16 = 0x0060;
const VOICE_TRANSPARENT: u16 = 0x0003;

#[repr(C)]
struct SockaddrSco {
    family: libc::sa_family_t,
    bdaddr: [u8; 6],
}

/// `"9C:9E:D5:78:FE:C4"` in the kernel's byte order, which is reversed.
pub fn bdaddr(address: &str) -> io::Result<[u8; 6]> {
    let parts: Vec<u8> = address
        .split(':')
        .map(|p| u8::from_str_radix(p, 16))
        .collect::<Result<_, _>>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad bluetooth address"))?;
    let mut out: [u8; 6] = parts
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad bluetooth address"))?;
    out.reverse();
    Ok(out)
}

pub fn address_of(bdaddr: &[u8; 6]) -> String {
    bdaddr
        .iter()
        .rev()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the result is checked before it is owned.
    let fd = unsafe {
        libc::socket(
            AF_BLUETOOTH,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            BTPROTO_SCO,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just returned by socket(2) and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn bind(fd: &OwnedFd, local: [u8; 6]) -> io::Result<()> {
    let addr = SockaddrSco {
        family: AF_BLUETOOTH as libc::sa_family_t,
        bdaddr: local,
    };
    // SAFETY: addr is a valid sockaddr_sco for the length passed.
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const SockaddrSco as *const libc::sockaddr,
            std::mem::size_of::<SockaddrSco>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_option<T>(fd: &OwnedFd, name: libc::c_int, value: &T) -> io::Result<()> {
    // SAFETY: value is a valid T for the length passed.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            SOL_BLUETOOTH,
            name,
            value as *const T as *const libc::c_void,
            std::mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Carry the link in transparent air mode (mSBC) or CVSD.
pub fn set_transparent(fd: &OwnedFd, transparent: bool) -> io::Result<()> {
    let setting = if transparent {
        VOICE_TRANSPARENT
    } else {
        VOICE_CVSD
    };
    set_option(fd, BT_VOICE, &setting)
}

/// Accept a connection taken from a deferring listener, in the air mode set
/// on it: the kernel completes the link on the first read.
pub fn authorize(fd: &OwnedFd) -> io::Result<()> {
    let mut byte = 0u8;
    // SAFETY: one writable byte.
    let r = unsafe { libc::read(fd.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::WouldBlock {
            return Err(e);
        }
    }
    Ok(())
}

/// A listener on the adapter's own address. With `defer`, a connection is
/// handed over before it is set up; see [`authorize`].
pub fn listen(local: [u8; 6], defer: bool) -> io::Result<OwnedFd> {
    let fd = socket()?;
    bind(&fd, local)?;
    if defer {
        set_option(&fd, BT_DEFER_SETUP, &1u32)?;
    }
    // SAFETY: listen(2) on a bound socket we own.
    if unsafe { libc::listen(fd.as_raw_fd(), 1) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Wait up to `timeout_ms` for a connection; `None` when none came.
pub fn accept(listener: &OwnedFd, timeout_ms: i32) -> io::Result<Option<(OwnedFd, String)>> {
    if !readable(listener, timeout_ms)? {
        return Ok(None);
    }
    let mut addr = SockaddrSco {
        family: 0,
        bdaddr: [0; 6],
    };
    let mut len = std::mem::size_of::<SockaddrSco>() as libc::socklen_t;
    // SAFETY: addr/len describe a writable sockaddr_sco.
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            &mut addr as *mut SockaddrSco as *mut libc::sockaddr,
            &mut len,
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just returned by accept4 and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(Some((fd, address_of(&addr.bdaddr))))
}

/// Open the audio link from this side, for bringing a call's sound back here.
pub fn connect(local: [u8; 6], remote: [u8; 6], transparent: bool) -> io::Result<OwnedFd> {
    let fd = socket()?;
    bind(&fd, local)?;
    if transparent {
        set_transparent(&fd, true)?;
    }
    let addr = SockaddrSco {
        family: AF_BLUETOOTH as libc::sa_family_t,
        bdaddr: remote,
    };
    // SAFETY: addr is a valid sockaddr_sco for the length passed.
    let r = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const SockaddrSco as *const libc::sockaddr,
            std::mem::size_of::<SockaddrSco>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// The packet size the controller wants; writing any other size is dropped
/// or garbled by some adapters.
pub fn mtu(fd: &OwnedFd) -> usize {
    let mut mtu: u16 = 0;
    let mut len = std::mem::size_of::<u16>() as libc::socklen_t;
    // SAFETY: SCO_OPTIONS fills a struct sco_options, whose first field is
    // the u16 mtu; len bounds the write.
    let r = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            SOL_SCO,
            SCO_OPTIONS,
            &mut mtu as *mut u16 as *mut libc::c_void,
            &mut len,
        )
    };
    if r < 0 || mtu == 0 {
        // 48 bytes is what USB controllers use for CVSD; the usual answer.
        48
    } else {
        mtu as usize
    }
}

pub fn readable(fd: &OwnedFd, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(e)
        };
    }
    if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 && pfd.revents & libc::POLLIN == 0 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "link closed",
        ));
    }
    Ok(r > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_reversed_for_the_kernel_and_back() {
        let b = bdaddr("9C:9E:D5:78:FE:C4").unwrap();
        assert_eq!(b, [0xC4, 0xFE, 0x78, 0xD5, 0x9E, 0x9C]);
        assert_eq!(address_of(&b), "9C:9E:D5:78:FE:C4");
    }
}
