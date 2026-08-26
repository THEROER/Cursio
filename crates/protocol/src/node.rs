//! What a node *is doing*, as opposed to what it can do.
//!
//! [`ModuleAdvert`](crate::ModuleAdvert) says a machine has audio; [`DeviceKind`]
//! says it is a laptop. Neither says the laptop is down to 9% and about to go
//! away, or that the desktop everything is being routed through just moved onto
//! a phone hotspot. That is state, it changes while a session is up, and until
//! now nothing on the wire carried it.
//!
//! ## Why this is not a module
//!
//! Every other subsystem here is a module because it is optional: a machine
//! without a clipboard backend simply doesn't advertise clipboard. But every
//! node has *some* answer to "are you on battery" — even if the answer is "no
//! battery, I am a desktop" — and the mesh wants that answer from peers it
//! shares nothing else with. Making it a module would mean a peer running only
//! files could not be asked, which is exactly backwards: the less a peer does
//! for us, the more useful it is to know whether it is about to vanish.
//!
//! So it rides the greeting (the state at the moment we met) plus
//! [`Message::NodeStatus`](crate::Message::NodeStatus) whenever it *changes*.
//!
//! ## Why it is sent on change, not polled
//!
//! A poll needs an interval, and no interval is right: battery percentage moves
//! once a minute, but the switch from charging to discharging is the moment a
//! peer actually wants to know, and it happens between any two polls. Sending
//! on change makes the interesting transitions immediate and costs nothing
//! while a machine sits plugged in.
//!
//! ## Everything is optional, and absence is not zero
//!
//! A desktop has no battery; a machine may not be able to read its Wi-Fi
//! signal; a phone knows its network type where a laptop on Ethernet does not
//! care. Every field is [`Option`], and `None` means "this node cannot say",
//! never "zero". A peer that renders a missing battery as 0% would show every
//! desktop in the mesh as flat.
//!
//! See `docs/mesh-design.md`.

use serde::{Deserialize, Serialize};

/// Where a node's power is coming from.
///
/// Separate from the charge level because the *level* is a number to display
/// and the *source* is what decides behaviour: a laptop at 80% and discharging
/// is a worse place to put an encode than one at 30% on mains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerSource {
    /// Running from mains. No time limit on anything this node is asked to do.
    Mains,
    /// Running from its own battery, which is draining.
    Battery,
    /// Has a battery, plugged in, and filling it.
    Charging,
}

impl PowerSource {
    /// Whether work placed here is competing with the user's remaining runtime.
    ///
    /// `Charging` counts as mains: the machine is plugged in, and the charge
    /// going in is not this node's concern.
    pub fn is_draining(self) -> bool {
        self == Self::Battery
    }
}

/// How a node reaches the mesh.
///
/// The distinction that matters is metered versus not: a peer on a phone
/// hotspot can carry a keystroke and should not be handed a file transfer, and
/// nothing else on the wire says so. Wired versus Wi-Fi is the weaker signal
/// but costs nothing to carry and is what a user reads first in `peers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkKind {
    /// Ethernet, or anything else with no radio and no meter.
    Wired,
    /// Wi-Fi. Assumed unmetered; [`NodeStatus::signal_dbm`] says how good.
    Wifi,
    /// A cellular connection, or a hotspot riding one. Assume every byte costs.
    Cellular,
    /// Connected by something this build has no name for. Deliberately distinct
    /// from `None`: "I looked and could not classify it" is not "I did not
    /// look", and a node that cannot classify its link should not be treated as
    /// wired by default.
    Unknown,
}

impl NetworkKind {
    /// Whether bulk traffic should avoid this node when there is a choice.
    ///
    /// Only `Cellular` is metered. `Unknown` is not: refusing to send files to
    /// every peer whose network this build cannot name would break file sharing
    /// on any platform whose detection we have not written yet, which is the
    /// opposite of degrading gracefully.
    pub fn is_metered(self) -> bool {
        self == Self::Cellular
    }
}

/// What a node is doing right now, as opposed to what it can do.
///
/// Carried in the greeting and re-sent whenever any field changes. Every field
/// is optional because the honest answer is often "this platform cannot tell
/// me", and inventing a value is how a laptop with an unreadable battery gets
/// ruled out of hosting something it would have handled fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NodeStatus {
    /// Battery charge, 0..=100. `None` on a machine with no battery *or* one
    /// that cannot read it — the two are indistinguishable to a peer and
    /// neither should be rendered as empty.
    pub battery_percent: Option<u8>,
    /// Where the power is coming from. `None` when the platform cannot say.
    pub power: Option<PowerSource>,
    /// How this node reaches the mesh.
    pub network: Option<NetworkKind>,
    /// Wi-Fi signal strength in dBm — always negative, roughly -30 (excellent)
    /// to -90 (unusable). Only meaningful with [`NetworkKind::Wifi`].
    ///
    /// `i16` rather than `i8`: the range fits in a byte, but a driver returning
    /// a sentinel outside it would wrap into a plausible-looking signal.
    pub signal_dbm: Option<i16>,
}

impl NodeStatus {
    /// A node that can say nothing about itself.
    ///
    /// What an older peer's greeting decodes to, and what a platform with no
    /// status backend reports. Distinct from every other value: all-`None`
    /// means "no information", which callers must not confuse with "on battery,
    /// flat, on cellular".
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Whether this node is running down its own battery.
    ///
    /// `false` when the power source is unknown: a machine that cannot report
    /// its power is not thereby in trouble, and treating silence as "on
    /// battery" would quietly exclude every platform whose backend is not
    /// written yet.
    pub fn on_battery(&self) -> bool {
        self.power.is_some_and(PowerSource::is_draining)
    }

    /// Whether this node's link should be kept clear of bulk traffic.
    pub fn is_metered(&self) -> bool {
        self.network.is_some_and(NetworkKind::is_metered)
    }

    /// Whether the node is low enough on power that the mesh should stop
    /// placing optional work on it.
    ///
    /// Only true when the node is *both* discharging and under the threshold: a
    /// machine at 5% and charging is about to be fine, and one at 5% on mains
    /// has a battery it is not using.
    pub fn is_power_critical(&self) -> bool {
        self.on_battery() && self.battery_percent.is_some_and(|p| p <= CRITICAL_PERCENT)
    }

    /// Whether two reports differ in anything worth telling peers about.
    ///
    /// Battery percentage moves constantly, and a node that announced every
    /// single point would send a frame a minute for the life of a session while
    /// saying nothing a peer acts on. The comparison is therefore deliberately
    /// coarse on the level and exact on everything else — a switch to
    /// discharging, or onto cellular, is the whole reason this exists and must
    /// never be rate-limited away.
    pub fn differs_meaningfully_from(&self, previous: &Self) -> bool {
        if self.power != previous.power
            || self.network != previous.network
            || self.is_power_critical() != previous.is_power_critical()
        {
            return true;
        }
        if percent_step(self.battery_percent) != percent_step(previous.battery_percent) {
            return true;
        }
        signal_bar(self.signal_dbm) != signal_bar(previous.signal_dbm)
    }
}

/// Battery level at or below which a discharging node is treated as critical.
pub const CRITICAL_PERCENT: u8 = 15;

/// Granularity at which a battery level is worth re-announcing, in percent.
const PERCENT_STEP: u8 = 5;

/// Which coarse bucket a battery level falls in, or `None` for no reading.
///
/// `Option` is carried through rather than flattened to a number, so that a
/// battery *disappearing* — an external one unplugged, or a read that starts
/// failing — is itself a change worth announcing.
fn percent_step(percent: Option<u8>) -> Option<u8> {
    percent.map(|p| p / PERCENT_STEP)
}

/// Which signal bucket a dBm reading falls in, roughly the bars a user sees.
///
/// Wi-Fi RSSI is noisy by nature — it moves a few dBm while nothing changes —
/// so the raw number is never the comparison. The thresholds are the
/// conventional ones: -50 excellent, -60 good, -70 fair, below that poor.
fn signal_bar(dbm: Option<i16>) -> Option<u8> {
    dbm.map(|d| match d {
        d if d >= -50 => 4,
        d if d >= -60 => 3,
        d if d >= -70 => 2,
        d if d >= -80 => 1,
        _ => 0,
    })
}

impl std::fmt::Display for NodeStatus {
    /// One line for `peers` and logs, naming only what the node actually
    /// reported. A node that knows nothing renders as nothing, not as a row of
    /// dashes standing in for values it never claimed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts: Vec<String> = Vec::new();

        if let Some(percent) = self.battery_percent {
            let suffix = match self.power {
                Some(PowerSource::Charging) => " charging",
                Some(PowerSource::Battery) => " discharging",
                _ => "",
            };
            parts.push(format!("battery {percent}%{suffix}"));
        } else if let Some(PowerSource::Mains) = self.power {
            parts.push("mains".to_string());
        }

        if let Some(network) = self.network {
            let name = match network {
                NetworkKind::Wired => "wired",
                NetworkKind::Wifi => "wifi",
                NetworkKind::Cellular => "cellular",
                NetworkKind::Unknown => "net",
            };
            match self.signal_dbm {
                Some(dbm) if network == NetworkKind::Wifi => parts.push(format!("{name} {dbm}dBm")),
                _ => parts.push(name.to_string()),
            }
        }

        f.write_str(&parts.join("  "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer built before this existed decodes to all-`None`, and every
    /// predicate must read that as "no information" rather than as bad news.
    #[test]
    fn an_unknown_node_is_not_mistaken_for_one_in_trouble() {
        let status = NodeStatus::unknown();
        assert!(!status.on_battery());
        assert!(!status.is_metered());
        assert!(!status.is_power_critical());
        assert_eq!(status.to_string(), "");
    }

    /// The whole point of separating source from level: a flat battery that is
    /// filling is not a machine to route work away from.
    #[test]
    fn a_charging_node_is_never_power_critical() {
        let charging = NodeStatus {
            battery_percent: Some(3),
            power: Some(PowerSource::Charging),
            ..NodeStatus::unknown()
        };
        assert!(!charging.is_power_critical());
        assert!(!charging.on_battery());

        let discharging = NodeStatus {
            power: Some(PowerSource::Battery),
            ..charging
        };
        assert!(discharging.is_power_critical());
        assert!(discharging.on_battery());
    }

    /// A machine on mains with a full battery is not critical, and one on
    /// battery above the threshold is not either.
    #[test]
    fn power_criticality_needs_both_a_low_level_and_a_draining_source() {
        let on_mains = NodeStatus {
            battery_percent: Some(2),
            power: Some(PowerSource::Mains),
            ..NodeStatus::unknown()
        };
        assert!(!on_mains.is_power_critical());

        let comfortable = NodeStatus {
            battery_percent: Some(90),
            power: Some(PowerSource::Battery),
            ..NodeStatus::unknown()
        };
        assert!(!comfortable.is_power_critical());
    }

    /// Only cellular is metered. An unclassifiable link must not be, or file
    /// sharing breaks on every platform whose detection isn't written yet.
    #[test]
    fn only_a_cellular_link_is_treated_as_metered() {
        for (kind, metered) in [
            (NetworkKind::Cellular, true),
            (NetworkKind::Wifi, false),
            (NetworkKind::Wired, false),
            (NetworkKind::Unknown, false),
        ] {
            let status = NodeStatus {
                network: Some(kind),
                ..NodeStatus::unknown()
            };
            assert_eq!(status.is_metered(), metered, "{kind:?}");
        }
    }

    /// Percentage drifts constantly; announcing every point would be a frame a
    /// minute saying nothing anyone acts on.
    #[test]
    fn a_one_point_battery_drift_is_not_worth_announcing() {
        let before = NodeStatus {
            battery_percent: Some(67),
            power: Some(PowerSource::Battery),
            ..NodeStatus::unknown()
        };
        let after = NodeStatus {
            battery_percent: Some(66),
            ..before
        };
        assert!(!after.differs_meaningfully_from(&before));

        let dropped = NodeStatus {
            battery_percent: Some(59),
            ..before
        };
        assert!(dropped.differs_meaningfully_from(&before));
    }

    /// Unplugging is the transition peers actually care about, and it must
    /// survive the rate limiting that suppresses level drift.
    #[test]
    fn losing_mains_power_is_announced_even_at_the_same_level() {
        let plugged = NodeStatus {
            battery_percent: Some(80),
            power: Some(PowerSource::Charging),
            ..NodeStatus::unknown()
        };
        let unplugged = NodeStatus {
            power: Some(PowerSource::Battery),
            ..plugged
        };
        assert!(unplugged.differs_meaningfully_from(&plugged));
    }

    /// Moving onto a metered link changes what the mesh may send here, so it
    /// cannot wait for the next level step.
    #[test]
    fn moving_onto_cellular_is_announced_immediately() {
        let wifi = NodeStatus {
            network: Some(NetworkKind::Wifi),
            ..NodeStatus::unknown()
        };
        let cellular = NodeStatus {
            network: Some(NetworkKind::Cellular),
            ..wifi
        };
        assert!(cellular.differs_meaningfully_from(&wifi));
    }

    /// Crossing the critical threshold matters even when the level step
    /// happens to be unchanged, because it is what stops work being placed.
    #[test]
    fn crossing_the_critical_threshold_is_always_announced() {
        let above = NodeStatus {
            battery_percent: Some(CRITICAL_PERCENT + 1),
            power: Some(PowerSource::Battery),
            ..NodeStatus::unknown()
        };
        let below = NodeStatus {
            battery_percent: Some(CRITICAL_PERCENT),
            ..above
        };
        assert!(below.is_power_critical());
        assert!(below.differs_meaningfully_from(&above));
    }

    /// RSSI moves a few dBm while nothing changes, so only a bar change counts.
    #[test]
    fn wifi_signal_is_compared_by_bars_not_raw_dbm() {
        let strong = NodeStatus {
            network: Some(NetworkKind::Wifi),
            signal_dbm: Some(-48),
            ..NodeStatus::unknown()
        };
        let jittered = NodeStatus {
            signal_dbm: Some(-45),
            ..strong
        };
        assert!(!jittered.differs_meaningfully_from(&strong));

        let weak = NodeStatus {
            signal_dbm: Some(-75),
            ..strong
        };
        assert!(weak.differs_meaningfully_from(&strong));
    }

    /// A battery that stops being readable is a change, not a non-event: the
    /// peer showing 67% would otherwise show it forever.
    #[test]
    fn a_battery_reading_disappearing_is_itself_a_change() {
        let reading = NodeStatus {
            battery_percent: Some(67),
            ..NodeStatus::unknown()
        };
        let gone = NodeStatus {
            battery_percent: None,
            ..reading
        };
        assert!(gone.differs_meaningfully_from(&reading));
    }

    /// The `peers` line names only what was actually reported.
    #[test]
    fn the_summary_line_omits_what_the_node_never_claimed() {
        let laptop = NodeStatus {
            battery_percent: Some(67),
            power: Some(PowerSource::Battery),
            network: Some(NetworkKind::Wifi),
            signal_dbm: Some(-48),
        };
        assert_eq!(laptop.to_string(), "battery 67% discharging  wifi -48dBm");

        let desktop = NodeStatus {
            power: Some(PowerSource::Mains),
            network: Some(NetworkKind::Wired),
            ..NodeStatus::unknown()
        };
        assert_eq!(desktop.to_string(), "mains  wired");
    }

    /// A signal reading without Wi-Fi is not rendered: dBm on a wired link is
    /// meaningless, and printing it invites the reader to act on it.
    #[test]
    fn a_signal_reading_is_only_shown_for_wifi() {
        let wired = NodeStatus {
            network: Some(NetworkKind::Wired),
            signal_dbm: Some(-48),
            ..NodeStatus::unknown()
        };
        assert_eq!(wired.to_string(), "wired");
    }
}
