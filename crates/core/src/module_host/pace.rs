//! How often a module is pumped: fast while it has traffic, slower once it has
//! been quiet a while, and slower still after a long quiet.

use std::time::Duration;

/// How often a module is pumped while it has traffic.
///
/// Sized just under the audio chunk cadence (~10 ms) so a steady stream is
/// pumped smoothly rather than in bursts. Without a pace a module was pumped
/// only when something else woke the loop, which turned continuous audio into
/// one clump every heartbeat.
pub(crate) const MODULE_PUMP_INTERVAL: Duration = Duration::from_millis(5);

/// The pace a module falls back to once it has been quiet for
/// [`IDLE_PUMP_AFTER`] consecutive pumps.
///
/// 200 wakeups a second buy nothing when nothing streams, but they keep the
/// core out of its deeper idle states — a steady draw on an always-on node, per
/// module now rather than per link. 50 ms still bounds how long a module that
/// starts producing between ticks waits for its first pump.
pub(crate) const IDLE_PUMP_INTERVAL: Duration = Duration::from_millis(50);

/// Consecutive silent pumps before dropping to [`IDLE_PUMP_INTERVAL`].
///
/// Several rather than one: audio chunks do not arrive on a perfect grid, so
/// one empty tick is normal mid-stream. At the fast pace this is 100 ms of
/// silence before the pace changes at all.
pub(crate) const IDLE_PUMP_AFTER: u32 = 20;

/// The pace of a module that has been quiet for long, after
/// [`DEEP_IDLE_AFTER`] pumps at [`IDLE_PUMP_INTERVAL`].
///
/// Even 50 ms is a wakeup per module per link twenty times a second: a phone
/// with eight modules on four links woke ~640 times a second with nothing
/// happening, and held the CPU awake to do it. What arrives from the peer
/// wakes a module through its inbox at once, whatever its pace; only a module
/// that has to notice something local — a capture that started producing, a
/// copy — waits, and a second is a fair price for that once it has been
/// quiet this long.
pub(crate) const DEEP_IDLE_INTERVAL: Duration = Duration::from_secs(1);

/// Pumps at [`IDLE_PUMP_INTERVAL`] before dropping to [`DEEP_IDLE_INTERVAL`]:
/// two seconds of nothing.
pub(crate) const DEEP_IDLE_AFTER: u32 = 40;

/// How many modules, across every link of this process, are at the fast pace
/// now — that is, have traffic. Zero means the node is idle, which a phone
/// uses to let its CPU and radio sleep; see [`modules_busy`].
pub(super) static FAST_MODULES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Whether any module of any link is moving data or owes an answer now.
pub fn modules_busy() -> bool {
    FAST_MODULES.load(std::sync::atomic::Ordering::Relaxed) > 0
}

/// Whether a module is pumped at the fast pace or the idle one.
///
/// Per module now, which is what the shared loop could never be: a file
/// transfer used to hold every module at 200 wakeups a second, and an idle
/// clipboard now sleeps beside a live audio stream.
pub(super) struct Pace {
    pub(super) interval: tokio::time::Interval,
    pub(super) fast: bool,
    /// Past [`DEEP_IDLE_AFTER`] at the idle pace.
    deep: bool,
    silent_pumps: u32,
    last_sent: u64,
}

impl Pace {
    pub(super) fn new() -> Self {
        FAST_MODULES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            interval: Self::interval(MODULE_PUMP_INTERVAL),
            fast: true,
            deep: false,
            silent_pumps: 0,
            last_sent: 0,
        }
    }

    fn interval(period: Duration) -> tokio::time::Interval {
        let mut interval = tokio::time::interval(period);
        // A missed tick fires once and carries on; replaying the backlog as a
        // burst is the very clumping the pace exists to prevent.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval
    }

    /// Follow the module's traffic after a pump.
    ///
    /// Sent anything, received anything, or owes an answer: stay fast. A mount
    /// serving a download sends little and receives a lot, and judging that
    /// idle put 50 ms between each request and the answer that had just
    /// arrived. Answers `true` when the pace changed.
    pub(super) fn follow(&mut self, sent: u64, busy: bool) -> bool {
        let active = sent != self.last_sent || busy;
        self.last_sent = sent;
        if active {
            self.silent_pumps = 0;
            self.deep = false;
            if !self.fast {
                self.interval = Self::interval(MODULE_PUMP_INTERVAL);
                self.fast = true;
                FAST_MODULES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return true;
            }
        } else if self.fast {
            self.silent_pumps = self.silent_pumps.saturating_add(1);
            if self.silent_pumps >= IDLE_PUMP_AFTER {
                self.interval = Self::interval(IDLE_PUMP_INTERVAL);
                self.fast = false;
                self.silent_pumps = 0;
                FAST_MODULES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                return true;
            }
        } else if !self.deep {
            self.silent_pumps = self.silent_pumps.saturating_add(1);
            if self.silent_pumps >= DEEP_IDLE_AFTER {
                self.interval = Self::interval(DEEP_IDLE_INTERVAL);
                self.deep = true;
                return true;
            }
        }
        false
    }
}

impl Drop for Pace {
    fn drop(&mut self) {
        if self.fast {
            FAST_MODULES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The idle pace is slower, and a stream with gaps cannot trip it.
    #[test]
    fn the_idle_pace_is_slower_and_slow_to_engage() {
        assert!(IDLE_PUMP_INTERVAL > MODULE_PUMP_INTERVAL);
        assert!(MODULE_PUMP_INTERVAL * IDLE_PUMP_AFTER >= Duration::from_millis(100));
    }

    /// A module that sends, receives or owes an answer stays fast; one that
    /// does none of those for long enough drops to the idle pace, and comes
    /// back the moment it sends again.
    #[tokio::test]
    async fn a_modules_pace_follows_its_own_traffic() {
        let mut pace = Pace::new();
        for _ in 0..IDLE_PUMP_AFTER - 1 {
            assert!(!pace.follow(0, false));
        }
        assert!(pace.fast, "one pump short of the threshold is still fast");
        assert!(pace.follow(0, false));
        assert!(!pace.fast, "quiet long enough drops to the idle pace");

        assert!(pace.follow(1, false), "sending restores the fast pace");
        assert!(pace.fast);

        for _ in 0..IDLE_PUMP_AFTER * 2 {
            pace.follow(1, true);
        }
        assert!(
            pace.fast,
            "a module owed an answer is busy however little it sends"
        );
    }

    /// Quiet at the idle pace for long, a module sleeps a second between
    /// pumps; anything it sends brings it straight back to the fast pace.
    #[tokio::test]
    async fn a_long_quiet_module_drops_to_the_deep_idle_pace() {
        let mut pace = Pace::new();
        for _ in 0..IDLE_PUMP_AFTER {
            pace.follow(0, false);
        }
        assert!(!pace.fast && !pace.deep);
        for _ in 0..DEEP_IDLE_AFTER - 1 {
            assert!(!pace.follow(0, false));
        }
        assert!(
            !pace.deep,
            "one pump short of the threshold is not deep yet"
        );
        assert!(pace.follow(0, false));
        assert!(pace.deep);
        assert_eq!(pace.interval.period(), DEEP_IDLE_INTERVAL);

        assert!(pace.follow(1, false), "sending wakes it");
        assert!(pace.fast && !pace.deep);
        assert_eq!(pace.interval.period(), MODULE_PUMP_INTERVAL);
    }
}
