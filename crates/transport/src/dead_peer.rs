//! Bound how long a TCP connection may hold unacknowledged data before the
//! kernel gives up on it.
//!
//! A peer whose Wi-Fi drops sends no FIN, and by default a socket with data in
//! flight retransmits for many minutes before it reports anything — while an
//! idle one reports nothing at all. The session's own silence check and the
//! resumable link's path checks catch most of that, but they act only on what
//! the application sees; a write that the kernel keeps retrying sits under
//! them, holding the drainer. So every link also asks the kernel to drop a
//! connection whose sent data stays unacknowledged for [`USER_TIMEOUT`] —
//! `TCP_USER_TIMEOUT` (RFC 5482) on Linux and Android,
//! `TCP_RXT_CONNDROPTIME` on Apple platforms, `TCP_MAXRT` on Windows.
//! Cloudflare's measurements of half-open sockets are why keepalive alone is
//! not enough: it never fires while data is queued, which is exactly when a
//! dead path hurts.
//!
//! Ten seconds: well past any stall a live LAN or Bluetooth link shows, and
//! short enough that a write into a vanished path ends while the session can
//! still resume on another one.

use std::io;
use std::time::Duration;

use tokio::net::TcpStream;

/// How long sent data may go unacknowledged before the connection is dropped.
pub const USER_TIMEOUT: Duration = Duration::from_secs(10);

/// Apply [`USER_TIMEOUT`] to `stream`, logging rather than failing: a link
/// without it still works, it only notices a dead peer later.
pub(crate) fn bound(stream: &TcpStream) {
    match apply(stream) {
        Ok(option) => {
            tracing::debug!(option, timeout = ?USER_TIMEOUT, "bounded unacknowledged data")
        }
        Err(e) => tracing::warn!(
            error = %e,
            "could not bound how long unacknowledged data may wait; a dead peer is noticed only by the session's own checks"
        ),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn apply(stream: &TcpStream) -> io::Result<&'static str> {
    let millis = USER_TIMEOUT.as_millis() as libc::c_uint;
    set_tcp_option(stream, libc::TCP_USER_TIMEOUT, millis)?;
    Ok("TCP_USER_TIMEOUT")
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn apply(stream: &TcpStream) -> io::Result<&'static str> {
    // From <netinet/tcp.h>; `libc` does not export it for Apple targets.
    const TCP_RXT_CONNDROPTIME: libc::c_int = 0x80;
    let seconds = USER_TIMEOUT.as_secs() as libc::c_uint;
    set_tcp_option(stream, TCP_RXT_CONNDROPTIME, seconds)?;
    Ok("TCP_RXT_CONNDROPTIME")
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
fn set_tcp_option(stream: &TcpStream, option: libc::c_int, value: libc::c_uint) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: the fd is a live TCP socket borrowed for the call, and the value
    // pointer and length describe one `c_uint` on this stack frame.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            option,
            (&value as *const libc::c_uint).cast(),
            std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn apply(stream: &TcpStream) -> io::Result<&'static str> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, WSAGetLastError, SOCKET, SOCKET_ERROR,
    };

    // From <ws2ipdef.h>: the option sets the retransmission time-out, in whole
    // seconds, after which the connection is dropped.
    const IPPROTO_TCP: i32 = 6;
    const TCP_MAXRT: i32 = 5;
    let seconds = USER_TIMEOUT.as_secs() as u32;
    // SAFETY: the value pointer and length describe one `u32` on this stack
    // frame, and the socket is borrowed live for the call.
    let rc = unsafe {
        setsockopt(
            stream.as_raw_socket() as SOCKET,
            IPPROTO_TCP,
            TCP_MAXRT,
            (&seconds as *const u32).cast(),
            std::mem::size_of::<u32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        // SAFETY: reads the calling thread's Winsock error; no arguments.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok("TCP_MAXRT")
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
)))]
fn apply(_stream: &TcpStream) -> io::Result<&'static str> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no option to bound unacknowledged data on this platform",
    ))
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_connection_drops_unacknowledged_data_after_the_user_timeout() {
        use std::os::fd::AsRawFd;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stream, _) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let stream = stream.unwrap();
        assert_eq!(apply(&stream).unwrap(), "TCP_USER_TIMEOUT");

        let mut value: libc::c_uint = 0;
        let mut len = std::mem::size_of::<libc::c_uint>() as libc::socklen_t;
        // SAFETY: reads one `c_uint` into this stack frame from a live socket.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_USER_TIMEOUT,
                (&mut value as *mut libc::c_uint).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(u128::from(value), USER_TIMEOUT.as_millis());
    }
}
