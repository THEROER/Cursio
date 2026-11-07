//! Bounding the bytes the kernel holds unsent for a link.
//!
//! The priority drainer (see [`crate::priority`]) picks the most urgent frame
//! every time it writes, but that choice only reaches as far as the socket.
//! Whatever the kernel already holds goes out first, in the order it was
//! written, and left alone a TCP send buffer autotunes to megabytes: a video
//! IDR and a few file chunks written while the link was busy sat there, and a
//! pointer event written a moment later waited behind all of them — tens of
//! milliseconds on gigabit, far more on Wi-Fi, felt as a mouse that lags
//! whenever a window or a file is moving. Strict priority in our own queue
//! meant nothing once the kernel's queue was deep.
//!
//! So the kernel is only ever given a little more than it can put on the wire
//! right away, and everything else waits in our queue, where it can still be
//! overtaken. What "a little" means differs by platform, because what each one
//! lets us bound differs:
//!
//! - **Linux, Android, macOS**: `TCP_NOTSENT_LOWAT`. The socket reports
//!   writable only while fewer than [`UNSENT_LOW_WATER`] bytes are waiting that
//!   TCP has not yet sent. Bytes already sent and awaiting acknowledgement are
//!   not counted, which is exactly right: they are on the wire already, nothing
//!   could overtake them anyway, and the congestion window can grow as large as
//!   the path wants — so throughput is not capped, only the queue in front of
//!   the next write.
//! - **Windows** has no such option. There `SO_SNDBUF` counts every byte not
//!   yet acknowledged, since Windows TCP completes a send only on the ACK, so a
//!   fixed number would either cap throughput at `SO_SNDBUF / RTT` or bring the
//!   deep queue back. The stack does publish the number it wants —
//!   `SIO_IDEAL_SEND_BACKLOG_QUERY`, its own bandwidth-delay estimate for this
//!   connection — and Microsoft's guidance is to keep exactly that much
//!   outstanding. [`IdealBacklog`] follows it, re-reading as the estimate
//!   moves.
//! - **Elsewhere** a fixed `SO_SNDBUF`, which bounds in-flight and unsent
//!   together; looser than the options above, but a bound.
//!
//! A socket option that fails to apply is logged, not fatal: the link still
//! works, only with the old, deeper queue.

use std::io;

use tokio::net::TcpStream;

/// Unsent bytes the kernel may hold before the socket stops reporting
/// writable, on platforms with `TCP_NOTSENT_LOWAT`.
///
/// Two limits meet here. An input frame written now waits behind at most this
/// plus the one frame the drainer is part-way through (a Noise frame, at most
/// 64 KiB): at 32 KiB that is about 0.8 ms on gigabit and 3 ms on a
/// 300 Mbit/s Wi-Fi link, against the several megabytes, and tens of
/// milliseconds, of an autotuned buffer. From the other side, the drainer must
/// write again before this much drains or the link idles; 32 KiB lasts 260 µs
/// on gigabit, which a woken task meets with room to spare. Measured over
/// loopback in a release build, 16, 32, 64 and 128 KiB all carried bulk at
/// 130–180 MB/s against 120–180 MB/s unbounded — the cipher, not the socket,
/// is the limit there, and that is already past gigabit.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
const UNSENT_LOW_WATER: u32 = 32 * 1024;

/// `SO_SNDBUF` for platforms with neither `TCP_NOTSENT_LOWAT` nor an ideal
/// backlog to follow: a gigabit bandwidth-delay product at 2 ms, so a LAN link
/// still fills while a Wi-Fi one loses some headroom.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
)))]
const FIXED_SEND_BUFFER: usize = 256 * 1024;

/// Bound the kernel's unsent backlog for `stream`; see the module docs.
///
/// A failure is logged here and otherwise ignored, since the link is still
/// usable.
pub(crate) fn bound(stream: &TcpStream) {
    match apply(stream) {
        Ok(bound) => tracing::debug!(bound, "bounded the kernel send backlog"),
        Err(e) => tracing::warn!(
            error = %e,
            "could not bound the kernel send backlog; input may queue behind video and file \
             data already handed to the socket"
        ),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn apply(stream: &TcpStream) -> io::Result<&'static str> {
    set_unsent_low_water(stream, libc::TCP_NOTSENT_LOWAT)?;
    Ok("TCP_NOTSENT_LOWAT")
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn apply(stream: &TcpStream) -> io::Result<&'static str> {
    // `libc` does not export it for Apple targets; this is the value from
    // <netinet/tcp.h>, where Apple introduced the option.
    const TCP_NOTSENT_LOWAT: libc::c_int = 0x201;
    set_unsent_low_water(stream, TCP_NOTSENT_LOWAT)?;
    Ok("TCP_NOTSENT_LOWAT")
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
fn set_unsent_low_water(stream: &TcpStream, option: libc::c_int) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let value = UNSENT_LOW_WATER as libc::c_int;
    // SAFETY: the fd is a live TCP socket borrowed for the call, and the value
    // pointer and length describe one `c_int` on this stack frame.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            option,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
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
    // The first reading, before any data has flowed, is the stack's opening
    // estimate; `IdealBacklog` keeps it current from the write path.
    let ideal = windows::ideal_send_backlog(stream)?;
    windows::set_send_buffer(stream, windows::clamp_ideal(ideal))?;
    Ok("SO_SNDBUF from SIO_IDEAL_SEND_BACKLOG_QUERY")
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
)))]
fn apply(stream: &TcpStream) -> io::Result<&'static str> {
    use std::os::fd::AsRawFd;

    let value = FIXED_SEND_BUFFER as libc::c_int;
    // SAFETY: as in `set_unsent_low_water`.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok("fixed SO_SNDBUF")
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Keeps a Windows socket's `SO_SNDBUF` at the stack's current ideal send
/// backlog.
///
/// The estimate starts small and grows as the connection measures its path, so
/// a value read once at connect would cap a transfer at the opening guess.
/// Polling from the write path rather than waiting on
/// `SIO_IDEAL_SEND_BACKLOG_CHANGE` keeps this free of an overlapped request
/// that would need its own completion plumbing beside tokio's: the query is a
/// non-blocking ioctl, and a link that is not writing has no backlog to size.
#[cfg(target_os = "windows")]
pub(crate) struct IdealBacklog {
    last_query: Option<std::time::Instant>,
    applied: usize,
}

#[cfg(target_os = "windows")]
impl IdealBacklog {
    /// Often enough to follow a connection ramping up within a few round
    /// trips of it happening; rare enough that the ioctl never shows up
    /// beside the write it precedes.
    const REQUERY: std::time::Duration = std::time::Duration::from_millis(100);

    pub(crate) fn new() -> Self {
        Self {
            last_query: None,
            applied: 0,
        }
    }

    /// Re-read the ideal backlog if it is due, and apply it if it moved.
    pub(crate) fn retune(&mut self, stream: &TcpStream) {
        let now = std::time::Instant::now();
        if self
            .last_query
            .is_some_and(|at| now.duration_since(at) < Self::REQUERY)
        {
            return;
        }
        self.last_query = Some(now);
        let wanted = match windows::ideal_send_backlog(stream) {
            Ok(ideal) => windows::clamp_ideal(ideal),
            Err(e) => {
                tracing::debug!(error = %e, "ideal send backlog query failed; keeping the last size");
                return;
            }
        };
        if wanted == self.applied {
            return;
        }
        match windows::set_send_buffer(stream, wanted) {
            Ok(()) => self.applied = wanted,
            Err(e) => tracing::debug!(error = %e, wanted, "could not resize the send buffer"),
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::io;
    use std::os::windows::io::AsRawSocket;

    use tokio::net::TcpStream;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, WSAGetLastError, WSAIoctl, SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_SNDBUF,
    };

    /// `_IOR('t', 123, ULONG)` from <ws2def.h>; `windows-sys` does not carry it.
    const SIO_IDEAL_SEND_BACKLOG_QUERY: u32 = 0x4004_747B;

    /// Never below one Noise frame plus change: a buffer smaller than the frame
    /// the drainer writes turns every write into several wake-ups.
    const MIN_SEND_BUFFER: usize = 128 * 1024;

    /// The estimate is trusted up to here. It is a bandwidth-delay product, and
    /// past this the path is not a LAN; a larger buffer would only rebuild the
    /// queue this module exists to remove.
    const MAX_SEND_BUFFER: usize = 4 * 1024 * 1024;

    pub(super) fn clamp_ideal(ideal: usize) -> usize {
        ideal.clamp(MIN_SEND_BUFFER, MAX_SEND_BUFFER)
    }

    fn raw(stream: &TcpStream) -> SOCKET {
        stream.as_raw_socket() as SOCKET
    }

    pub(super) fn ideal_send_backlog(stream: &TcpStream) -> io::Result<usize> {
        let mut ideal: u32 = 0;
        let mut returned: u32 = 0;
        // SAFETY: a synchronous query on a live socket; the output pointer and
        // length describe one `u32` on this stack frame, and no overlapped
        // structure is passed, so nothing outlives the call.
        let rc = unsafe {
            WSAIoctl(
                raw(stream),
                SIO_IDEAL_SEND_BACKLOG_QUERY,
                std::ptr::null(),
                0,
                (&mut ideal as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc == SOCKET_ERROR {
            // SAFETY: reads the calling thread's Winsock error; no arguments.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(ideal as usize)
    }

    pub(super) fn set_send_buffer(stream: &TcpStream, bytes: usize) -> io::Result<()> {
        let value = i32::try_from(bytes).unwrap_or(i32::MAX);
        // SAFETY: the value pointer and length describe one `i32` on this
        // stack frame, and the socket is borrowed live for the call.
        let rc = unsafe {
            setsockopt(
                raw(stream),
                SOL_SOCKET,
                SO_SNDBUF,
                (&value as *const i32).cast(),
                std::mem::size_of::<i32>() as i32,
            )
        };
        if rc == SOCKET_ERROR {
            // SAFETY: as above.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(())
    }
}
