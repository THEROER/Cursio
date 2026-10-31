//! The mesh over Bluetooth: a second path, not a second protocol.
//!
//! A machine that has lost the network — Wi-Fi off, out of the house's range,
//! on a hotspot the others are not on, or a laptop whose card stopped passing
//! traffic while still claiming to be connected — is still the same peer. So
//! a Bluetooth link is just another byte stream under the same `Hello`,
//! pairing key, Noise handshake and priority classes that run over TCP, and
//! it joins the running session as one more path (`transport::resume`).
//! Nothing here speaks a Bluetooth profile's protocol.
//!
//! **Who calls whom.** A phone calls its computers and is never called:
//! Android does not let an app learn its own Bluetooth address, and keeps no
//! listener. Two computers call each other, using the RFCOMM channel each
//! listener advertises through SDP.
//! Which peer a Bluetooth address belongs to is known only once it has
//! greeted, so the registry remembers it ([`crate::registry::PeerRegistry::
//! note_bluetooth`]) and from then on an address is called only when *its*
//! peer has been off the network for [`NETWORK_GRACE`]. An address no peer
//! has answered on yet is called when any peer is missing, once, which is how
//! it gets learned.
//!
//! **Radio time is the cost.** Paging a device that is not there holds the
//! radio for seconds, and on a card that shares its antenna with Wi-Fi that is
//! seconds of Wi-Fi lost. So [`Dialer`] backs off: doubling up to
//! [`MAX_RETRY`] for a device that does not answer, and [`REFUSED_RETRY`] for
//! an unknown device with no mesh channel. Known peers retry after [`RETRY`],
//! and a new OS connection clears the backoff once. None of that applies to
//! a peer whose session is waiting with no path: it has a minute before it is
//! lost, so it is called within seconds ([`WAITING_GRACE`]). A machine whose card is known to lose
//! the network while Bluetooth is busy does not use Bluetooth at all unless
//! the person says so ([`decide`]).
//!
//! **Which path wins.** The network, whenever it is there: Bluetooth carries a
//! few hundred kilobytes a second, enough for input, notifications, messages,
//! calls' control and Opus audio — the priority classes keep those ahead —
//! but not for a screen. The side that placed a Bluetooth path lets it go
//! once the network carries the session again, unless the person asked for a
//! standby (`bluetooth.standby`).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use cursio_protocol::PeerId;
use cursio_transport::stream::{StreamTransport, BLUETOOTH};
use tokio::sync::mpsc;

use crate::config::BluetoothMesh;

/// How long a known peer must have been unreachable over the network before
/// Bluetooth is tried. Long enough that a reconnect after the Wi-Fi blinks
/// happens on Wi-Fi, short enough that walking out of range is felt as a
/// pause — the session itself waits a minute for a new path.
pub const NETWORK_GRACE: Duration = Duration::from_secs(20);

/// The least time between two attempts at the same device.
pub const RETRY: Duration = Duration::from_secs(30);

/// The most a device that does not answer waits between attempts.
pub const MAX_RETRY: Duration = Duration::from_secs(5 * 60);

/// How long a device that answered with no mesh channel is left alone.
pub const REFUSED_RETRY: Duration = Duration::from_secs(30 * 60);

/// How long the network must have been gone before Bluetooth is tried for a
/// peer whose session is waiting with no path at all. Its input is stopped
/// now, and the session is lost once the hold runs out — a minute — so the
/// ordinary grace and backoff, sized for a peer that is simply away, would
/// let it run out: two machines that called each other at the same moment
/// both got "busy", both waited the minute, and met again just after their
/// session had ended.
pub const WAITING_GRACE: Duration = Duration::from_secs(5);

/// The most time between attempts at a peer whose session is waiting, for
/// the one of the pair that calls first ([`crate::registry::should_dial`]).
/// The other waits [`WAITING_RETRY_SECOND`], so the two calls stop meeting
/// on the radio: two pages crossing leave both sides "busy".
pub const WAITING_RETRY_FIRST: Duration = Duration::from_secs(3);
/// See [`WAITING_RETRY_FIRST`].
pub const WAITING_RETRY_SECOND: Duration = Duration::from_secs(8);

/// A device paired with this one over Bluetooth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub address: String,
    pub name: String,
    pub connected: bool,
}

#[derive(Debug, Clone, Copy)]
struct Attempt {
    at: Instant,
    wait: Duration,
}

/// When to open a Bluetooth link, kept apart from the radio so it is tested.
#[derive(Default)]
pub struct Dialer {
    tried: HashMap<String, Attempt>,
    linked: HashSet<String>,
    connected: HashSet<String>,
}

impl Dialer {
    /// A device coming back is fresh evidence, independent of a cached refusal.
    pub fn observe(&mut self, paired: &[Paired]) {
        let connected: HashSet<_> = paired
            .iter()
            .filter(|device| device.connected)
            .map(|device| device.address.clone())
            .collect();
        for address in connected.difference(&self.connected) {
            self.tried.remove(address);
        }
        self.connected = connected;
    }

    /// The devices to try now. `unreachable_for` is how long the longest-gone
    /// known peer has been off the network, `None` while all are on it, and
    /// `waiting` whether one of them has a session waiting for a path
    /// ([`WAITING_GRACE`]). `standby` asks for a Bluetooth path beside a
    /// working network one, so it does not wait for the network to fail.
    pub fn due(
        &mut self,
        paired: &[Paired],
        unreachable_for: Option<Duration>,
        waiting: bool,
        standby: bool,
        now: Instant,
    ) -> Vec<Paired> {
        let grace = if waiting {
            WAITING_GRACE
        } else {
            NETWORK_GRACE
        };
        if !standby && !unreachable_for.is_some_and(|gone| gone >= grace) {
            return Vec::new();
        }
        self.pace(paired, now)
    }

    /// Of `wanted`, the devices not linked and not tried too recently; each
    /// returned is counted as tried now.
    pub fn pace(&mut self, wanted: &[Paired], now: Instant) -> Vec<Paired> {
        let due: Vec<Paired> = wanted
            .iter()
            .filter(|device| !self.linked.contains(&device.address))
            .filter(|device| {
                self.tried
                    .get(&device.address)
                    .is_none_or(|tried| now.duration_since(tried.at) >= tried.wait)
            })
            .cloned()
            .collect();
        for device in &due {
            self.tried
                .entry(device.address.clone())
                .and_modify(|tried| tried.at = now)
                .or_insert(Attempt {
                    at: now,
                    wait: RETRY,
                });
        }
        due
    }

    /// The attempt at `address` failed: `refused` when the device answered
    /// with no mesh channel, otherwise it did not answer at all.
    pub fn failed(&mut self, address: &str, refused: bool, known_peer: bool) -> Duration {
        if let Some(tried) = self.tried.get_mut(address) {
            tried.wait = if refused && known_peer {
                RETRY
            } else if refused {
                REFUSED_RETRY
            } else {
                (tried.wait * 2).min(MAX_RETRY)
            };
            tried.wait
        } else {
            RETRY
        }
    }

    /// Try `address` again within `wait` of the last attempt, whatever its
    /// backoff had grown to; for a peer whose session is waiting for a path.
    pub fn hurry(&mut self, address: &str, wait: Duration) {
        if let Some(tried) = self.tried.get_mut(address) {
            tried.wait = tried.wait.min(wait);
        }
    }

    /// A link to `address` is up, or has ended. Up also forgets its backoff:
    /// the next time it is missing is a new absence.
    pub fn set_linked(&mut self, address: &str, up: bool) {
        if up {
            self.linked.insert(address.to_string());
            self.tried.remove(address);
        } else {
            self.linked.remove(address);
        }
    }
}

/// What a computer knows about one paired device, for [`select`].
pub struct Known {
    /// The peer that answered on it, once one has.
    pub peer: Option<PeerId>,
    /// That peer's session has a Bluetooth path already.
    pub bluetooth_up: bool,
    /// How long that peer has been off the network, `None` while it is on.
    pub gone_for: Option<Duration>,
    /// That peer's session is alive with no path at all; see
    /// [`WAITING_GRACE`].
    pub waiting: bool,
}

/// The paired computers to call: those whose peer has been off the network
/// for [`NETWORK_GRACE`] (or, with `standby`, any whose session has no
/// Bluetooth path), and those no peer has answered on yet while any peer is
/// missing.
pub fn select(
    paired: &[Paired],
    known: impl Fn(&str) -> Known,
    anyone_missing: bool,
    standby: bool,
) -> Vec<Paired> {
    paired
        .iter()
        .filter(|device| {
            let known = known(&device.address);
            match known.peer {
                Some(_) if known.bluetooth_up => false,
                Some(_) => {
                    let grace = if known.waiting {
                        WAITING_GRACE
                    } else {
                        NETWORK_GRACE
                    };
                    standby || known.gone_for.is_some_and(|gone| gone >= grace)
                }
                None => anyone_missing || standby,
            }
        })
        .cloned()
        .collect()
}

/// Whether this machine carries the mesh over Bluetooth, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    On,
    /// The person turned it off.
    Off,
    /// Left off on the person's behalf: the Wi-Fi card, named, is one known
    /// to lose the network while Bluetooth is busy on the shared antenna.
    HeldBack(String),
}

impl Decision {
    pub fn is_on(&self) -> bool {
        matches!(self, Decision::On)
    }
}

/// Settle `mode` against what is known of this machine's radio.
pub fn decide(mode: BluetoothMesh, conflict: Option<&str>) -> Decision {
    match (mode, conflict) {
        (BluetoothMesh::On, _) | (BluetoothMesh::Auto, None) => Decision::On,
        (BluetoothMesh::Off, _) => Decision::Off,
        (BluetoothMesh::Auto, Some(driver)) => Decision::HeldBack(driver.to_string()),
    }
}

/// Wi-Fi drivers whose cards share an antenna with Bluetooth and lose the
/// network while Bluetooth is busy. b43 is Broadcom's BCM43xx on Linux: its
/// coexistence handling is known broken (some cards' SPROM claims support
/// they lack), and on a machine in this mesh beacons are lost with no
/// Bluetooth traffic at all.
const CONFLICTING_DRIVERS: &[&str] = &["b43", "b43legacy"];

/// The Wi-Fi driver this machine has that rules Bluetooth out by default.
#[cfg(target_os = "linux")]
pub fn radio_conflict() -> Option<String> {
    let interfaces = std::fs::read_dir("/sys/class/net").ok()?;
    interfaces.flatten().find_map(|interface| {
        let driver = std::fs::read_link(interface.path().join("device/driver")).ok()?;
        let name = driver.file_name()?.to_str()?.to_string();
        CONFLICTING_DRIVERS.contains(&name.as_str()).then_some(name)
    })
}

/// The Wi-Fi driver this machine has that rules Bluetooth out by default.
/// None known off Linux.
#[cfg(not(target_os = "linux"))]
pub fn radio_conflict() -> Option<String> {
    let _ = CONFLICTING_DRIVERS;
    None
}

const HELD_BACK: &str = "bluetooth.held-back";

/// Say so where the person looks when Bluetooth is held back, with both
/// answers as buttons; either answer is a setting, so the note ends with it.
pub fn report(decision: &Decision) {
    use crate::config::ConfigEdit;
    use crate::diagnostics::{self, Problem, ProblemAction, Severity};

    let Decision::HeldBack(driver) = decision else {
        diagnostics::clear(HELD_BACK);
        return;
    };
    diagnostics::raise(Problem {
        id: HELD_BACK.into(),
        severity: Severity::Info,
        summary: format!(
            "the mesh does not use bluetooth on this machine: its Wi-Fi card ({driver}) shares an antenna with bluetooth and drops off the network while both are busy"
        ),
        hint: Some(
            "other machines still reach it over the network; set [bluetooth] mesh = \"on\" to use bluetooth anyway, or \"off\" to keep it off without this note".into(),
        ),
        actions: vec![
            ProblemAction {
                label: "OK".into(),
                edit: ConfigEdit::BluetoothMesh(BluetoothMesh::Off),
            },
            ProblemAction {
                label: "Use bluetooth anyway".into(),
                edit: ConfigEdit::BluetoothMesh(BluetoothMesh::On),
            },
        ],
        since: std::time::SystemTime::now(),
    });
}

/// The most a Bluetooth link holds unsent below the session's own queues.
///
/// Whatever sits there goes out first in the order written, out of reach of
/// the lane order the path's writer keeps. Over TCP that queue is bounded by
/// `transport::send_backlog`; an RFCOMM socket keeps the kernel's default of
/// about 200 KiB, which at the radio's 100–200 KB/s is a second or more of
/// sound and file data queued in front of every pointer move written after
/// it — the mouse on a Bluetooth-only peer jumped in bursts. Linux doubles
/// the figure asked for.
const UNSENT: usize = 8 * 1024;

/// A Bluetooth connection as a mesh transport.
#[cfg(unix)]
pub type Link = StreamTransport<tokio::net::UnixStream>;
/// A Bluetooth connection as a mesh transport.
#[cfg(windows)]
pub type Link = StreamTransport<tokio::io::DuplexStream>;

/// A connected Bluetooth socket as a mesh transport.
///
/// Any stream socket descriptor works: the RFCOMM socket BlueZ hands over or
/// the kernel connects, or the end of the socket pair the Android app pumps
/// an RFCOMM stream through. Needs the runtime, which registers it for
/// readiness.
#[cfg(unix)]
pub fn transport(fd: std::os::fd::OwnedFd) -> std::io::Result<Link> {
    use std::os::fd::AsRawFd;

    let size = UNSENT as libc::c_int;
    // SAFETY: fd is a live socket we own; size is a c_int for the length passed.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &size as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "could not bound a bluetooth link's send buffer; input may queue behind other traffic on it"
        );
    }
    let socket = std::os::unix::net::UnixStream::from(fd);
    socket.set_nonblocking(true)?;
    Ok(StreamTransport::new(
        tokio::net::UnixStream::from_std(socket)?,
        BLUETOOTH,
    ))
}

/// A connected Winsock RFCOMM socket as a mesh transport.
///
/// Pumped by two threads through an in-memory pipe rather than handed to
/// Tokio: whether Winsock's Bluetooth provider works under the readiness
/// polling Tokio uses is undocumented, and Chromium, the one large user of
/// these sockets, keeps them on blocking threads too. Either side closing
/// ends both threads: the node dropping its end ends the writer's read, and
/// the radio closing ends the reader's.
#[cfg(windows)]
pub fn transport(socket: std::os::windows::io::OwnedSocket) -> std::io::Result<Link> {
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let runtime = tokio::runtime::Handle::current();
    // Only read, write and shutdown are used, which Winsock serves for any
    // stream socket; nothing here asks it TCP's questions.
    let socket = Arc::new(std::net::TcpStream::from(socket));
    // The pipe is the queue below the session here, so it is the one bounded;
    // what Winsock buffers behind a blocking send is not ours to set. It
    // counts bare bytes where Linux charges each small packet about a
    // kilobyte, so it is smaller: 8 KiB here would hold a hundred pointer
    // moves the session could otherwise have merged into one.
    let (node, pump) = tokio::io::duplex(UNSENT / 4);
    let (mut from_node, mut to_node) = tokio::io::split(pump);

    let radio = Arc::clone(&socket);
    let handle = runtime.clone();
    std::thread::Builder::new()
        .name("bluetooth-rx".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            let mut reads = 0;
            loop {
                let n = match (&*radio).read(&mut buf) {
                    Ok(0) => break,
                    Err(error) => {
                        tracing::debug!(%error, "bluetooth socket read ended");
                        break;
                    }
                    Ok(n) => n,
                };
                reads += 1;
                if reads <= 4 {
                    tracing::debug!(
                        bytes = n,
                        reads,
                        "bluetooth socket received handshake bytes"
                    );
                }
                if handle.block_on(to_node.write_all(&buf[..n])).is_err() {
                    break;
                }
            }
            let _ = handle.block_on(to_node.shutdown());
            let _ = radio.shutdown(Shutdown::Both);
        })?;
    std::thread::Builder::new()
        .name("bluetooth-tx".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            let mut writes = 0;
            loop {
                let n = match runtime.block_on(from_node.read(&mut buf)) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                writes += 1;
                if writes <= 4 {
                    tracing::debug!(
                        bytes = n,
                        writes,
                        "bluetooth socket sending handshake bytes"
                    );
                }
                if let Err(error) = (&*socket).write_all(&buf[..n]) {
                    tracing::debug!(%error, "bluetooth socket write ended");
                    break;
                }
                if writes <= 4 {
                    tracing::debug!(bytes = n, writes, "bluetooth socket sent handshake bytes");
                }
            }
            let _ = socket.shutdown(Shutdown::Both);
        })?;
    Ok(StreamTransport::new(node, BLUETOOTH))
}

/// Paired devices opening the mesh's channel on this machine.
pub struct Listener {
    accepted: mpsc::UnboundedReceiver<(String, RawSocket)>,
    /// Held where nothing can listen, so `accept` waits forever rather than
    /// reporting the channel closed on every turn of the caller's loop.
    _idle: Option<mpsc::UnboundedSender<(String, RawSocket)>>,
}

#[cfg(unix)]
type RawSocket = std::os::fd::OwnedFd;
#[cfg(windows)]
type RawSocket = std::os::windows::io::OwnedSocket;

impl Listener {
    /// A listener that never accepts, for a machine with Bluetooth off.
    pub fn idle() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Listener {
            accepted: rx,
            _idle: Some(tx),
        }
    }

    /// The next device to connect, by its Bluetooth address.
    pub async fn accept(&mut self) -> Option<(String, Link)> {
        loop {
            let (address, socket) = self.accepted.recv().await?;
            match transport(socket) {
                Ok(link) => return Some((address, link)),
                Err(e) => {
                    tracing::warn!(peer = %address, error = %e, "cannot use a bluetooth connection")
                }
            }
        }
    }
}

/// Start listening for paired devices on the mesh's Bluetooth channel.
///
/// Never fails the node: without Bluetooth, or with the stack refusing the
/// channel, the mesh simply has the network only, and the reason is logged.
/// Registered on a thread of its own, because zbus's blocking calls cannot
/// run inside the node's runtime.
#[cfg(any(target_os = "linux", windows))]
pub fn listen() -> Listener {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawned = std::thread::Builder::new()
        .name("bluetooth-mesh".into())
        .spawn(move || {
            let (channel, accepted) = match cursio_bluetooth::MeshChannel::start() {
                Ok(started) => started,
                Err(e) => {
                    tracing::warn!(error = %e, "peers off the network cannot reach this machine over bluetooth");
                    return;
                }
            };
            // The registration lives as long as this thread holds it.
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

/// Start listening for paired devices on the mesh's Bluetooth channel.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn listen() -> Listener {
    tracing::warn!(
        "the mesh's bluetooth channel is not implemented on this platform; peers reach it over the network only"
    );
    Listener::idle()
}

/// Why a call over Bluetooth did not connect.
#[cfg(any(target_os = "linux", windows))]
#[derive(Debug)]
pub struct DialError {
    /// The device answered with no mesh channel.
    pub refused: bool,
    pub reason: String,
}

/// Call `address`'s mesh channel. The radio pages for seconds, so the call
/// runs on the blocking pool.
#[cfg(any(target_os = "linux", windows))]
pub async fn dial(address: String) -> Result<Link, DialError> {
    let dialled = tokio::task::spawn_blocking(move || cursio_bluetooth::dial(&address)).await;
    let socket = match dialled {
        Ok(Ok(socket)) => socket,
        Ok(Err(e)) => {
            return Err(DialError {
                refused: matches!(e, cursio_bluetooth::BluetoothError::NoService(_)),
                reason: e.to_string(),
            })
        }
        Err(e) => {
            return Err(DialError {
                refused: false,
                reason: format!("the bluetooth dial did not finish: {e}"),
            })
        }
    };
    transport(socket).map_err(|e| DialError {
        refused: false,
        reason: format!("cannot use the bluetooth link: {e}"),
    })
}

/// The computers paired with this one, or none (with the reason logged once
/// per call) where the stack cannot say.
#[cfg(any(target_os = "linux", windows))]
pub async fn paired_computers() -> Vec<Paired> {
    let listed = tokio::task::spawn_blocking(cursio_bluetooth::paired_computers).await;
    match listed {
        Ok(Ok(devices)) => devices
            .into_iter()
            .map(|d| Paired {
                address: d.address,
                name: d.name,
                connected: d.connected,
            })
            .collect(),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "cannot list paired computers");
            Vec::new()
        }
        Err(e) => {
            tracing::debug!(error = %e, "listing paired computers did not finish");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(address: &str) -> Paired {
        Paired {
            address: address.into(),
            connected: false,
            name: "desk".into(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_socket_descriptor_becomes_a_bluetooth_transport() {
        use cursio_protocol::Message;
        use cursio_transport::Transport;

        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut near = transport(std::os::fd::OwnedFd::from(ours)).unwrap();
        let mut far = transport(std::os::fd::OwnedFd::from(theirs)).unwrap();
        assert_eq!(near.kind(), BLUETOOTH);
        near.send(&Message::Heartbeat { nonce: 7 }).await.unwrap();
        assert_eq!(far.recv().await.unwrap(), Message::Heartbeat { nonce: 7 });
    }

    #[test]
    fn leaves_bluetooth_alone_while_the_network_reaches_everyone() {
        let mut dialer = Dialer::default();
        assert!(dialer
            .due(&[pc("A")], None, false, false, Instant::now())
            .is_empty());
    }

    #[test]
    fn a_standby_is_dialled_while_the_network_still_works() {
        let mut dialer = Dialer::default();
        assert_eq!(
            dialer.due(&[pc("A")], None, false, true, Instant::now()),
            vec![pc("A")]
        );
    }

    #[test]
    fn waits_out_a_short_network_blip_before_trying_bluetooth() {
        let mut dialer = Dialer::default();
        let now = Instant::now();
        assert!(dialer
            .due(&[pc("A")], Some(Duration::from_secs(5)), false, false, now)
            .is_empty());
        assert_eq!(
            dialer.due(&[pc("A")], Some(NETWORK_GRACE), false, false, now),
            vec![pc("A")]
        );
    }

    #[test]
    fn a_waiting_session_does_not_wait_out_the_ordinary_grace() {
        let mut dialer = Dialer::default();
        assert_eq!(
            dialer.due(&[pc("A")], Some(WAITING_GRACE), true, false, Instant::now()),
            vec![pc("A")]
        );
    }

    #[test]
    fn does_not_retry_a_device_sooner_than_the_retry_interval() {
        let mut dialer = Dialer::default();
        let now = Instant::now();
        let gone = Some(NETWORK_GRACE);
        assert_eq!(dialer.due(&[pc("A")], gone, false, false, now).len(), 1);
        assert!(dialer
            .due(
                &[pc("A")],
                gone,
                false,
                false,
                now + Duration::from_secs(10)
            )
            .is_empty());
        assert_eq!(
            dialer
                .due(&[pc("A")], gone, false, false, now + RETRY)
                .len(),
            1
        );
    }

    #[test]
    fn does_not_dial_a_device_it_already_has_a_link_to() {
        let mut dialer = Dialer::default();
        dialer.set_linked("A", true);
        let due = dialer.due(
            &[pc("A"), pc("B")],
            Some(NETWORK_GRACE),
            false,
            false,
            Instant::now(),
        );
        assert_eq!(due, vec![pc("B")]);
    }

    #[test]
    fn a_device_that_does_not_answer_is_paged_ever_less_often() {
        let mut dialer = Dialer::default();
        let start = Instant::now();
        assert_eq!(dialer.pace(&[pc("A")], start).len(), 1);
        dialer.failed("A", false, false);
        assert!(dialer.pace(&[pc("A")], start + RETRY).is_empty());
        let second = start + RETRY * 2;
        assert_eq!(dialer.pace(&[pc("A")], second).len(), 1);
        for _ in 0..10 {
            dialer.failed("A", false, false);
        }
        assert!(dialer
            .pace(&[pc("A")], second + MAX_RETRY - Duration::from_secs(1))
            .is_empty());
        assert_eq!(dialer.pace(&[pc("A")], second + MAX_RETRY).len(), 1);
    }

    #[test]
    fn a_device_without_the_mesh_channel_is_left_alone_for_long() {
        let mut dialer = Dialer::default();
        let start = Instant::now();
        dialer.pace(&[pc("A")], start);
        dialer.failed("A", true, false);
        assert!(dialer.pace(&[pc("A")], start + MAX_RETRY).is_empty());
        assert_eq!(dialer.pace(&[pc("A")], start + REFUSED_RETRY).len(), 1);
    }

    #[test]
    fn a_known_peer_starting_its_listener_is_retried_without_a_thirty_minute_penalty() {
        let mut dialer = Dialer::default();
        let start = Instant::now();
        dialer.pace(&[pc("A")], start);
        assert_eq!(dialer.failed("A", true, true), RETRY);
        assert!(dialer
            .pace(&[pc("A")], start + RETRY - Duration::from_secs(1))
            .is_empty());
        assert_eq!(dialer.pace(&[pc("A")], start + RETRY).len(), 1);
    }

    #[test]
    fn a_radio_reconnection_clears_backoff_once_without_creating_a_dial_storm() {
        let mut dialer = Dialer::default();
        let start = Instant::now();
        let mut device = pc("A");
        dialer.observe(&[device.clone()]);
        dialer.pace(&[device.clone()], start);
        dialer.failed("A", true, false);
        device.connected = true;
        dialer.observe(&[device.clone()]);
        assert_eq!(
            dialer
                .pace(&[device.clone()], start + Duration::from_secs(1))
                .len(),
            1
        );
        dialer.failed("A", true, true);
        dialer.observe(&[device.clone()]);
        assert!(dialer
            .pace(&[device], start + Duration::from_secs(2))
            .is_empty());
    }

    #[test]
    fn a_link_coming_up_forgets_the_backoff() {
        let mut dialer = Dialer::default();
        let start = Instant::now();
        dialer.pace(&[pc("A")], start);
        dialer.failed("A", true, false);
        dialer.set_linked("A", true);
        dialer.set_linked("A", false);
        assert_eq!(dialer.pace(&[pc("A")], start).len(), 1);
    }

    fn peer(n: u128) -> PeerId {
        PeerId::from_u128(n)
    }

    #[test]
    fn calls_a_known_computer_only_once_its_own_peer_is_missing() {
        let paired = [pc("LIGHT"), pc("DARK")];
        let chosen = select(
            &paired,
            |address| Known {
                peer: Some(peer(if address == "LIGHT" { 1 } else { 2 })),
                bluetooth_up: false,
                gone_for: (address == "LIGHT").then_some(NETWORK_GRACE),
                waiting: false,
            },
            true,
            false,
        );
        assert_eq!(chosen, vec![pc("LIGHT")]);
    }

    #[test]
    fn calls_an_unknown_computer_only_while_someone_is_missing() {
        let unknown = |_: &str| Known {
            peer: None,
            bluetooth_up: false,
            gone_for: None,
            waiting: false,
        };
        assert!(select(&[pc("NEW")], unknown, false, false).is_empty());
        assert_eq!(select(&[pc("NEW")], unknown, true, false), vec![pc("NEW")]);
    }

    #[test]
    fn a_standby_calls_every_computer_without_a_bluetooth_path() {
        let paired = [pc("A"), pc("B")];
        let chosen = select(
            &paired,
            |address| Known {
                peer: Some(peer(1)),
                bluetooth_up: address == "A",
                gone_for: None,
                waiting: false,
            },
            false,
            true,
        );
        assert_eq!(chosen, vec![pc("B")]);
    }

    #[test]
    fn a_session_waiting_for_a_path_calls_well_before_the_ordinary_grace() {
        let known = |waiting| {
            move |_: &str| Known {
                peer: Some(peer(1)),
                bluetooth_up: false,
                gone_for: Some(WAITING_GRACE),
                waiting,
            }
        };
        assert!(select(&[pc("A")], known(false), true, false).is_empty());
        assert_eq!(select(&[pc("A")], known(true), true, false), vec![pc("A")]);
    }

    #[test]
    fn hurrying_caps_a_grown_backoff_for_a_waiting_session() {
        let mut dialer = Dialer::default();
        let start = Instant::now();
        dialer.pace(&[pc("A")], start);
        for _ in 0..4 {
            dialer.failed("A", false, true);
        }
        let later = start + WAITING_RETRY_SECOND;
        assert!(dialer.pace(&[pc("A")], later).is_empty());
        dialer.hurry("A", WAITING_RETRY_SECOND);
        assert_eq!(dialer.pace(&[pc("A")], later), vec![pc("A")]);
    }

    #[test]
    fn a_card_known_to_clash_holds_bluetooth_back_until_the_person_decides() {
        assert_eq!(decide(BluetoothMesh::Auto, None), Decision::On);
        assert_eq!(
            decide(BluetoothMesh::Auto, Some("b43")),
            Decision::HeldBack("b43".into())
        );
        assert_eq!(decide(BluetoothMesh::On, Some("b43")), Decision::On);
        assert_eq!(decide(BluetoothMesh::Off, None), Decision::Off);
    }
}
