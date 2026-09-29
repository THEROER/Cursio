//! Whether now is a moment the node can be restarted without anyone noticing.
//!
//! An update that installs whenever it arrives is fine; one that *switches*
//! whenever it arrives is not. Restarting the node drops every link at once,
//! so it must not happen while:
//!
//! - this machine is driving a peer's screen — the user's hand is on this
//!   mouse, and the cursor over there would freeze mid-move;
//! - a peer is driving *this* machine — the user is working here with the
//!   other machine's mouse and keyboard, and a switch throws the cursor back
//!   there mid-sentence. Not holding the cursor elsewhere says nothing about
//!   whether anyone is at this screen;
//! - a peer's disk is in use through a mount, or ours through theirs — a
//!   restart in the middle of a write is how Steam once wedged itself on a
//!   WinFsp volume badly enough to need a reboot;
//! - a USB device is carrying traffic across the link.
//!
//! The sessions and modules that know these things only report them here, in
//! atomics, because none of them should hold a lock the update path could
//! wait on.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long file, USB or a peer's input keeps a machine from being quiet after
/// it stops. Long enough to cover the gap between a program's reads, or a pause
/// to read what was just typed.
pub const SETTLE: Duration = Duration::from_secs(60);

static DRIVING: AtomicUsize = AtomicUsize::new(0);
static LAST_FILES: AtomicU64 = AtomicU64::new(0);
static LAST_USB: AtomicU64 = AtomicU64::new(0);
static LAST_INPUT: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Milliseconds since the epoch, never zero: zero means "never happened".
fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64 + 1
}

/// A file-sharing message crossed the link just now.
pub fn note_file_activity() {
    LAST_FILES.store(now_ms(), Ordering::Relaxed);
}

/// A USB message crossed the link just now.
pub fn note_usb_activity() {
    LAST_USB.store(now_ms(), Ordering::Relaxed);
}

/// A peer's input was injected here just now.
pub fn note_input_activity() {
    LAST_INPUT.store(now_ms(), Ordering::Relaxed);
}

/// Held by one session: counts it among those driving a peer while it is.
#[derive(Default)]
pub struct DrivingMark(bool);

impl DrivingMark {
    pub fn set(&mut self, driving: bool) {
        if driving != self.0 {
            match driving {
                true => DRIVING.fetch_add(1, Ordering::Relaxed),
                false => DRIVING.fetch_sub(1, Ordering::Relaxed),
            };
            self.0 = driving;
        }
    }
}

impl Drop for DrivingMark {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// What the machine is doing, as the reporters above last said.
#[derive(Debug, Clone, Copy, Default)]
struct Activity {
    driving: usize,
    last_files: u64,
    last_usb: u64,
    last_input: u64,
}

/// Why now is not a quiet moment, or `None` when it is.
pub fn busy() -> Option<String> {
    judge(
        Activity {
            driving: DRIVING.load(Ordering::Relaxed),
            last_files: LAST_FILES.load(Ordering::Relaxed),
            last_usb: LAST_USB.load(Ordering::Relaxed),
            last_input: LAST_INPUT.load(Ordering::Relaxed),
        },
        now_ms(),
    )
}

fn judge(activity: Activity, now: u64) -> Option<String> {
    let settle = SETTLE.as_millis() as u64;
    let recent = |at: u64| at != 0 && now.saturating_sub(at) < settle;
    if activity.driving > 0 {
        return Some("the cursor is on another machine".into());
    }
    if recent(activity.last_input) {
        return Some("a peer's mouse and keyboard are in use here".into());
    }
    if recent(activity.last_files) {
        return Some("a shared disk is in use".into());
    }
    if recent(activity.last_usb) {
        return Some("a shared USB device is in use".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_machine_is_quiet() {
        assert_eq!(judge(Activity::default(), 5), None);
    }

    #[test]
    fn driving_a_peer_holds_the_switch() {
        let busy = judge(
            Activity {
                driving: 1,
                ..Activity::default()
            },
            5,
        );
        assert!(busy.unwrap().contains("cursor"));
    }

    #[test]
    fn disk_and_usb_traffic_hold_the_switch_until_they_settle() {
        let settle = SETTLE.as_millis() as u64;
        let files = Activity {
            last_files: 1000,
            ..Activity::default()
        };
        assert!(judge(files, 1000 + settle - 1).unwrap().contains("disk"));
        assert_eq!(judge(files, 1000 + settle), None);
        let usb = Activity {
            last_usb: 1000,
            ..Activity::default()
        };
        assert!(judge(usb, 1500).unwrap().contains("USB"));
        assert_eq!(judge(usb, 1000 + settle), None);
    }

    #[test]
    fn being_driven_by_a_peer_holds_the_switch_until_it_settles() {
        let settle = SETTLE.as_millis() as u64;
        let driven = Activity {
            last_input: 1000,
            ..Activity::default()
        };
        assert!(judge(driven, 1500).unwrap().contains("in use here"));
        assert_eq!(judge(driven, 1000 + settle), None);
    }

    #[test]
    fn a_session_that_stops_driving_or_ends_releases_its_mark() {
        let mut mark = DrivingMark::default();
        mark.set(true);
        mark.set(true);
        assert!(mark.0);
        mark.set(false);
        assert!(!mark.0);
        mark.set(true);
        drop(mark);
    }
}
