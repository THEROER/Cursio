//! Reading this machine's own power and network state.
//!
//! Mechanism for the policy in [`cursio_protocol::node`]: that module decides
//! what a status *means* and when a change is worth announcing, this one only
//! answers "what is true right now".
//!
//! ## Every reading is optional, and a failure is not a zero
//!
//! A desktop has no battery. A laptop's battery may be behind a driver this
//! kernel does not expose. A container has no Wi-Fi to ask about. Each probe
//! returns `None` on anything it cannot answer, and `None` travels the wire as
//! "this node cannot say" — never as a flat battery or a dead link. The one
//! rule that matters here: **never substitute a plausible default for a failed
//! read**, because a peer cannot tell the difference and will act on it.
//!
//! ## Why polling, when the wire sends on change
//!
//! Neither platform offers one event source covering power *and* network.
//! Linux has udev and netlink; Windows has WM_POWERBROADCAST and WMI — four
//! subscriptions, three of which need a message loop or a COM apartment, to
//! learn a handful of integers. Polling a few sysfs files every 30 seconds
//! costs less than the machinery to avoid it, and
//! [`NodeStatus::differs_meaningfully_from`] means a poll that finds nothing
//! new sends nothing. The wire stays event-driven; only the reading is a poll.
//!
//! See `docs/mesh-design.md`.

use std::time::Duration;

use cursio_protocol::NodeStatus;

/// How often the local state is re-read.
///
/// Slow enough to be free, fast enough that unplugging a laptop reaches peers
/// while the user still associates the two. Battery level moves far slower than
/// this; the power *source* is the reading worth catching promptly.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Watches this machine's power and network state, reporting only changes.
///
/// Holds the last report so the caller can ask "is there anything to send?"
/// rather than tracking that itself — the answer depends on
/// [`NodeStatus::differs_meaningfully_from`], which is the protocol's rule, not
/// the caller's.
///
/// ## One monitor, several sessions
///
/// A node holds a link to every peer, and each session polls this same monitor.
/// A plain "changed since last poll" flag would therefore be consumed by
/// whichever session polled first, and every other peer would never hear about
/// it — the bug being that the *machine* changed, not the link.
///
/// So each reading carries a generation, and a session remembers the one it
/// last sent. Sessions poll independently, each learns about the change once,
/// and a link that comes up later simply greets with the current reading.
#[derive(Debug)]
pub struct NodeStatusMonitor {
    last: NodeStatus,
    /// Bumped on every meaningful change. Sessions compare against their own
    /// copy rather than being handed a one-shot.
    generation: u64,
}

/// One session's view of the shared monitor.
///
/// Created per link, and the only thing a session needs to hold: it knows which
/// generation this peer has been told about, so a change reaches every peer
/// exactly once.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusCursor {
    seen: u64,
}

impl NodeStatusMonitor {
    /// Start watching, reading the current state immediately.
    ///
    /// The first reading is returned by [`current`](Self::current) rather than
    /// announced: it belongs in the greeting, and a node that also sent it as a
    /// change would say the same thing twice on every link it brings up.
    pub fn new() -> Self {
        Self {
            last: read(),
            generation: 0,
        }
    }

    /// The most recent reading, for the greeting.
    pub fn current(&self) -> NodeStatus {
        self.last
    }

    /// A cursor for a link that has just greeted with [`current`](Self::current).
    ///
    /// Starts level with the present generation, so a session announces only
    /// what happens *after* it said hello.
    pub fn cursor(&self) -> StatusCursor {
        StatusCursor {
            seen: self.generation,
        }
    }

    /// Re-read the machine, recording a new generation if peers should hear it.
    ///
    /// Called by whichever session's timer fires; the reading is shared, so a
    /// second session polling a moment later re-reads rather than being told
    /// nothing happened. Cheap enough (a few small files) that this is simpler
    /// than coordinating a single reader.
    pub fn refresh(&mut self) {
        let fresh = read();
        if !fresh.differs_meaningfully_from(&self.last) {
            return;
        }
        self.last = fresh;
        self.generation += 1;
    }

    /// What this link has not yet been told, if anything.
    ///
    /// `None` is the common case — a plugged-in desktop never reports anything
    /// after its greeting.
    pub fn unsent(&self, cursor: &mut StatusCursor) -> Option<NodeStatus> {
        if cursor.seen == self.generation {
            return None;
        }
        cursor.seen = self.generation;
        Some(self.last)
    }

    /// Re-read and answer what this link still owes its peer, in one call.
    pub fn poll(&mut self, cursor: &mut StatusCursor) -> Option<NodeStatus> {
        self.refresh();
        self.unsent(cursor)
    }
}

impl Default for NodeStatusMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Read whatever this platform can tell us about itself.
fn read() -> NodeStatus {
    let (battery_percent, power) = platform::power();
    let (network, signal_dbm) = platform::network();
    NodeStatus {
        battery_percent,
        power,
        network,
        signal_dbm,
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::path::Path;

    use cursio_protocol::{NetworkKind, PowerSource};

    /// Where the kernel publishes battery and mains state.
    const POWER_SUPPLY: &str = "/sys/class/power_supply";

    /// Read one sysfs attribute, trimmed. `None` covers every failure —
    /// missing file, permission, a driver that vanished mid-read — because
    /// none of them is distinguishable to a peer and none means zero.
    fn attr(dir: &Path, name: &str) -> Option<String> {
        fs::read_to_string(dir.join(name))
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// Battery level and power source.
    ///
    /// Both come from `/sys/class/power_supply`, where each entry is either a
    /// battery or a mains supply. A machine can have several batteries; the
    /// first with a readable capacity is used rather than an average, because
    /// averaging two batteries at 100% and 0% reports 50% for a machine that
    /// is about to lose one of them.
    pub fn power() -> (Option<u8>, Option<PowerSource>) {
        let Ok(entries) = fs::read_dir(POWER_SUPPLY) else {
            return (None, None);
        };

        let mut battery: Option<(u8, String)> = None;
        let mut mains_online = false;
        let mut saw_mains = false;

        for entry in entries.flatten() {
            let dir = entry.path();
            match attr(&dir, "type").as_deref() {
                Some("Battery") => {
                    // `status` is the driver's own word: Charging, Discharging,
                    // Full, Not charging, Unknown. Preferred over inferring
                    // from the mains supply, which cannot tell "plugged in and
                    // full" from "plugged in and filling".
                    let status = attr(&dir, "status").unwrap_or_default();
                    if let Some(capacity) = attr(&dir, "capacity")
                        .and_then(|c| c.parse::<u8>().ok())
                        .filter(|c| *c <= 100)
                    {
                        battery.get_or_insert((capacity, status));
                    }
                }
                Some("Mains") => {
                    saw_mains = true;
                    mains_online |= attr(&dir, "online").as_deref() == Some("1");
                }
                _ => {}
            }
        }

        match battery {
            Some((capacity, status)) => {
                let source = match status.as_str() {
                    "Charging" => Some(PowerSource::Charging),
                    "Discharging" => Some(PowerSource::Battery),
                    // "Full" and "Not charging" both mean plugged in and not
                    // drawing down. Anything else is a driver we have no word
                    // for, and the mains reading is the honest fallback.
                    "Full" | "Not charging" => Some(PowerSource::Mains),
                    _ if saw_mains && mains_online => Some(PowerSource::Mains),
                    _ if saw_mains => Some(PowerSource::Battery),
                    _ => None,
                };
                (Some(capacity), source)
            }
            // No battery at all: a desktop, and mains is the whole story. With
            // no mains entry either this is a container or a board with no
            // power class, and claiming mains would be an invention.
            None if saw_mains => (None, Some(PowerSource::Mains)),
            None => (None, None),
        }
    }

    /// Network kind and Wi-Fi signal.
    ///
    /// `/proc/net/wireless` is the one place a plain read answers both "is
    /// there an associated Wi-Fi interface" and "how strong" without a netlink
    /// round trip. Its absence means no wireless interface is up, which on a
    /// machine that is talking to us means wired.
    pub fn network() -> (Option<NetworkKind>, Option<i16>) {
        let Ok(contents) = fs::read_to_string("/proc/net/wireless") else {
            return (Some(NetworkKind::Wired), None);
        };

        // Two header lines, then one per interface. The level column is the
        // fourth field and carries a trailing dot ("-48."), which is why it is
        // trimmed rather than parsed directly.
        let level = contents
            .lines()
            .skip(2)
            .filter_map(|line| line.split_whitespace().nth(3))
            .filter_map(|field| field.trim_end_matches('.').parse::<i16>().ok())
            // An unassociated interface reports 0, which is not a signal
            // strength — real RSSI is always negative.
            .find(|dbm| *dbm < 0);

        match level {
            Some(dbm) => (Some(NetworkKind::Wifi), Some(dbm)),
            // A wireless interface exists but none is associated: whatever is
            // carrying this session is not it.
            None => (Some(NetworkKind::Wired), None),
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use cursio_protocol::{NetworkKind, PowerSource};
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    /// `BatteryFlag` bit meaning the machine has no battery at all.
    const BATTERY_FLAG_NO_BATTERY: u8 = 128;
    /// `BatteryFlag` bit meaning the battery is charging.
    const BATTERY_FLAG_CHARGING: u8 = 8;
    /// What `ACLineStatus` reports when running on mains.
    const AC_LINE_ONLINE: u8 = 1;
    /// What both fields report when the driver cannot say.
    const UNKNOWN: u8 = 255;

    /// Battery level and power source, via `GetSystemPowerStatus`.
    ///
    /// One call answers both, which is why this needs neither WMI nor a COM
    /// apartment. The struct's "unknown" sentinel is 255 in every field, and it
    /// is passed through as `None` rather than clamped — 255% would render as a
    /// battery fuller than full.
    pub fn power() -> (Option<u8>, Option<PowerSource>) {
        let mut status = SYSTEM_POWER_STATUS::default();
        // SAFETY: the struct is a plain POD the call fills in; we pass a
        // pointer to our own stack copy and read it only on success.
        if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
            return (None, None);
        }

        let no_battery = status.BatteryFlag & BATTERY_FLAG_NO_BATTERY != 0;
        let percent = match status.BatteryLifePercent {
            UNKNOWN => None,
            p if p <= 100 && !no_battery => Some(p),
            _ => None,
        };

        let source = match status.ACLineStatus {
            AC_LINE_ONLINE if status.BatteryFlag & BATTERY_FLAG_CHARGING != 0 => {
                Some(PowerSource::Charging)
            }
            AC_LINE_ONLINE => Some(PowerSource::Mains),
            UNKNOWN => None,
            // Offline. With no battery this is a desktop whose UPS just took
            // over, which is genuinely running down a battery.
            _ => Some(PowerSource::Battery),
        };

        (percent, source)
    }

    /// Network kind and signal.
    ///
    /// Windows exposes both through the WLAN API, which needs a handle, an
    /// enumeration and a matching free for each query — a meaningful amount of
    /// unsafe code for two fields nothing yet acts on. Reporting `Unknown`
    /// rather than guessing `Wired` keeps the peer from treating a laptop on
    /// Wi-Fi as a wired node; `Unknown` is explicitly not metered, so nothing
    /// is withheld from this peer in the meantime.
    pub fn network() -> (Option<NetworkKind>, Option<i16>) {
        (Some(NetworkKind::Unknown), None)
    }
}

/// Android: the app reports, this answers what it last heard.
///
/// An app cannot read a phone's power or radio state from Rust — SELinux keeps
/// an untrusted app out of `/sys/class/power_supply`, and Wi-Fi signal lives
/// behind `WifiManager` — so the JVM side reads `BatteryManager` and
/// `ConnectivityManager` and hands the values in through [`report`]. Until the
/// first report everything is `None`, which is the truth: nothing is known yet.
#[cfg(target_os = "android")]
mod platform {
    use std::sync::Mutex;

    use cursio_protocol::{NetworkKind, PowerSource};

    #[derive(Clone, Copy, Default)]
    pub(super) struct Reported {
        pub battery: Option<u8>,
        pub source: Option<PowerSource>,
        pub network: Option<NetworkKind>,
        pub signal_dbm: Option<i16>,
    }

    pub(super) static REPORTED: Mutex<Reported> = Mutex::new(Reported {
        battery: None,
        source: None,
        network: None,
        signal_dbm: None,
    });

    fn reported() -> Reported {
        *REPORTED.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn power() -> (Option<u8>, Option<PowerSource>) {
        let r = reported();
        (r.battery, r.source)
    }

    pub fn network() -> (Option<NetworkKind>, Option<i16>) {
        let r = reported();
        (r.network, r.signal_dbm)
    }
}

/// What the Android app read from `BatteryManager` and `ConnectivityManager`;
/// each `None` is something it could not say. The next poll sends it on.
#[cfg(target_os = "android")]
pub fn report(
    battery: Option<u8>,
    source: Option<cursio_protocol::PowerSource>,
    network: Option<cursio_protocol::NetworkKind>,
    signal_dbm: Option<i16>,
) {
    *platform::REPORTED.lock().unwrap_or_else(|p| p.into_inner()) = platform::Reported {
        battery: battery.map(|b| b.min(100)),
        source,
        network,
        signal_dbm,
    };
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
mod platform {
    use cursio_protocol::{NetworkKind, PowerSource};

    /// No reading on this platform.
    ///
    /// An explicit `None` rather than a plausible default: a macOS laptop
    /// reported as mains-powered is worse than one reported as unknown, because
    /// the mesh would place work on it as if runtime were free.
    pub fn power() -> (Option<u8>, Option<PowerSource>) {
        (None, None)
    }

    pub fn network() -> (Option<NetworkKind>, Option<i16>) {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reading the local machine must never panic, whatever this host is —
    /// a laptop, a desktop, a container with no power class at all.
    #[test]
    fn reading_the_local_machine_answers_without_panicking() {
        let status = read();
        if let Some(percent) = status.battery_percent {
            assert!(percent <= 100, "battery over 100%: {percent}");
        }
        if let Some(dbm) = status.signal_dbm {
            assert!(dbm < 0, "wifi signal must be negative dBm: {dbm}");
        }
    }

    /// A monitor's first reading belongs in the greeting, so the first poll
    /// must not re-announce what has not changed.
    #[test]
    fn a_fresh_monitor_has_nothing_to_announce() {
        let monitor = NodeStatusMonitor::new();
        let mut cursor = monitor.cursor();
        assert_eq!(monitor.unsent(&mut cursor), None);
    }

    /// The reason generations exist rather than a "changed" flag: the machine
    /// changed, not one link, so every peer must hear it. A one-shot would be
    /// consumed by whichever session polled first.
    #[test]
    fn every_session_learns_about_a_change_not_just_the_first() {
        let mut monitor = NodeStatusMonitor::new();
        let mut first = monitor.cursor();
        let mut second = monitor.cursor();

        // Stand in for a real change: the platform read is whatever this host
        // happens to be, so the transition is forced rather than waited for.
        monitor.last = NodeStatus {
            battery_percent: Some(9),
            power: Some(cursio_protocol::PowerSource::Battery),
            ..NodeStatus::unknown()
        };
        monitor.generation += 1;

        assert_eq!(monitor.unsent(&mut first), Some(monitor.current()));
        assert_eq!(monitor.unsent(&mut second), Some(monitor.current()));
        // And neither repeats it.
        assert_eq!(monitor.unsent(&mut first), None);
        assert_eq!(monitor.unsent(&mut second), None);
    }

    /// A link brought up after a change greets with the current reading, so it
    /// must not then be told the same thing as news.
    #[test]
    fn a_link_that_greeted_after_a_change_does_not_re_announce_it() {
        let mut monitor = NodeStatusMonitor::new();
        monitor.generation += 1;

        let mut late = monitor.cursor();
        assert_eq!(monitor.unsent(&mut late), None);
    }
}
