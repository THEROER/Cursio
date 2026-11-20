//! What the kernel's TCP says about one connection: how much it sent and how
//! much of that it had to send again.
//!
//! Every lane is a TCP connection, and TCP delivers in order: a segment lost on
//! the air holds everything written after it until the resend arrives, and no
//! queue of ours can see that wait — the frame left our queue on time. The
//! queue delay, the round trip and the frame round trip each show the stall
//! but not its cause. Retransmissions are the cause, and only the kernel counts
//! them. They are what decides whether a lane would gain from a transport that
//! may skip a loss (datagrams, QUIC) or whether the time goes elsewhere; see
//! "Delivery" in `docs/transport-design.md`.
//!
//! Each platform counts in its own unit — Linux segments, Windows bytes — so a
//! reading carries its unit and is compared only with itself.

use std::io;
use std::time::Duration;

use tokio::net::TcpStream;

/// What a reading's counts are counted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountUnit {
    Segments,
    Bytes,
}

/// One reading of a connection's lifetime counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpCounters {
    pub sent: u64,
    pub retransmitted: u64,
    pub unit: CountUnit,
    /// The kernel's smoothed round trip, which is the transport's own and so
    /// below what a frame sees through our queue and the peer's.
    pub rtt: Duration,
}

impl TcpCounters {
    /// What happened between `earlier` and this reading, or `None` across a
    /// counter that went backwards (a different connection).
    pub fn since(&self, earlier: &TcpCounters) -> Option<TcpCounters> {
        Some(TcpCounters {
            sent: self.sent.checked_sub(earlier.sent)?,
            retransmitted: self.retransmitted.checked_sub(earlier.retransmitted)?,
            unit: self.unit,
            rtt: self.rtt,
        })
    }
}

/// Read `stream`'s counters.
pub fn read(stream: &TcpStream) -> io::Result<TcpCounters> {
    imp::read(stream)
}

/// How long a lane's retransmissions are gathered before they are logged.
const WINDOW: Duration = Duration::from_secs(10);

/// One path's retransmissions, logged a window at a time by the task that
/// writes to it.
#[derive(Default)]
pub(crate) struct RetransmitLog {
    since: Option<(std::time::Instant, TcpCounters)>,
    unreadable: bool,
}

impl RetransmitLog {
    /// Read `tx`'s counters if a window has run, and log what it sent and
    /// resent in it.
    pub(crate) fn sample(&mut self, tx: &dyn crate::TransportTx, lane: crate::lane::Lane) {
        if self.unreadable {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .since
            .is_some_and(|(at, _)| now.saturating_duration_since(at) < WINDOW)
        {
            return;
        }
        let reading = match tx.tcp_counters() {
            Some(Ok(reading)) => reading,
            // A platform with no reader says so on every path of every
            // connection, four at a time; that is a fact about the build, not
            // a problem with this link.
            Some(Err(error)) if error.kind() == io::ErrorKind::Unsupported => {
                self.unreadable = true;
                tracing::debug!(?lane, %error, "this lane's TCP retransmissions go unmeasured");
                return;
            }
            Some(Err(error)) => {
                self.unreadable = true;
                tracing::warn!(?lane, %error, "cannot read this lane's TCP retransmissions; it goes unmeasured");
                return;
            }
            None => {
                self.unreadable = true;
                return;
            }
        };
        if let Some(window) = self
            .since
            .replace((now, reading))
            .and_then(|(_, earlier)| reading.since(&earlier))
            .filter(|window| window.sent > 0)
        {
            tracing::debug!(
                ?lane,
                sent = window.sent,
                retransmitted = window.retransmitted,
                unit = ?window.unit,
                retransmitted_pct = window.retransmitted as f64 * 100.0 / window.sent as f64,
                rtt_ms = window.rtt.as_secs_f64() * 1000.0,
                "TCP resent part of what this lane sent; everything behind each resend waited for it"
            );
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    use tokio::net::TcpStream;

    use super::{CountUnit, TcpCounters};

    /// Offsets into the kernel's `struct tcp_info` (`<linux/tcp.h>`), which only
    /// ever grows at its end. Read as bytes rather than through `libc`'s
    /// struct, which the Android target does not define.
    const RTT_US: usize = 68;
    const TOTAL_RETRANS: usize = 100;
    /// `tcpi_segs_out`, there since Linux 4.2.
    const SEGS_OUT: usize = 136;
    const NEEDED: usize = SEGS_OUT + 4;

    pub(super) fn read(stream: &TcpStream) -> io::Result<TcpCounters> {
        let mut info = [0u8; NEEDED];
        let mut len = NEEDED as libc::socklen_t;
        // SAFETY: the buffer and its length describe `info` on this stack
        // frame; the kernel writes at most `len` bytes and reports how many.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                info.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if (len as usize) < NEEDED {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this kernel's TCP_INFO has no sent-segment count (Linux 4.2 added it)",
            ));
        }
        let u32_at =
            |at: usize| u32::from_ne_bytes(info[at..at + 4].try_into().expect("four bytes"));
        Ok(TcpCounters {
            sent: u64::from(u32_at(SEGS_OUT)),
            retransmitted: u64::from(u32_at(TOTAL_RETRANS)),
            unit: CountUnit::Segments,
            rtt: Duration::from_micros(u64::from(u32_at(RTT_US))),
        })
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::io;
    use std::os::windows::io::AsRawSocket;
    use std::time::Duration;

    use tokio::net::TcpStream;
    use windows_sys::Win32::Networking::WinSock::{
        TCP_INFO_v0, WSAGetLastError, WSAIoctl, SIO_TCP_INFO, SOCKET, SOCKET_ERROR,
    };

    use super::{CountUnit, TcpCounters};

    pub(super) fn read(stream: &TcpStream) -> io::Result<TcpCounters> {
        let version: u32 = 0;
        let mut info = TCP_INFO_v0::default();
        let mut returned: u32 = 0;
        // SAFETY: a synchronous query on a live socket; input and output
        // pointers describe `version` and `info` on this stack frame, and no
        // overlapped structure is passed, so nothing outlives the call.
        let rc = unsafe {
            WSAIoctl(
                stream.as_raw_socket() as SOCKET,
                SIO_TCP_INFO,
                (&version as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
                (&mut info as *mut TCP_INFO_v0).cast(),
                std::mem::size_of::<TCP_INFO_v0>() as u32,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc == SOCKET_ERROR {
            // SAFETY: reads the calling thread's Winsock error; no arguments.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(TcpCounters {
            sent: info.BytesOut,
            retransmitted: u64::from(info.BytesRetrans),
            unit: CountUnit::Bytes,
            rtt: Duration::from_micros(u64::from(info.RttUs)),
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "windows")))]
mod imp {
    use std::io;

    use tokio::net::TcpStream;

    use super::TcpCounters;

    pub(super) fn read(_stream: &TcpStream) -> io::Result<TcpCounters> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reading TCP retransmissions is implemented for Linux, Android and Windows only",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reading_since_another_is_their_difference() {
        let earlier = TcpCounters {
            sent: 100,
            retransmitted: 2,
            unit: CountUnit::Segments,
            rtt: Duration::from_millis(3),
        };
        let later = TcpCounters {
            sent: 250,
            retransmitted: 5,
            rtt: Duration::from_millis(4),
            ..earlier
        };
        let between = later.since(&earlier).unwrap();
        assert_eq!((between.sent, between.retransmitted), (150, 3));
        assert_eq!(earlier.since(&later), None);
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[tokio::test]
    async fn a_loopback_connection_counts_what_it_sent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let (mut client, (mut server, _)) = (client.unwrap(), accepted.unwrap());
        let before = read(&client).unwrap();
        client.write_all(&[7u8; 64 * 1024]).await.unwrap();
        let mut received = vec![0u8; 64 * 1024];
        server.read_exact(&mut received).await.unwrap();
        let after = read(&client).unwrap();
        let between = after.since(&before).unwrap();
        assert!(between.sent > 0);
        assert_eq!(between.retransmitted, 0, "loopback loses nothing");
    }
}
