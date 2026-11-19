//! Whether the user has the phone in hand rather than a computer.
//!
//! A phone's notification goes to the computer the user is at
//! (`cursio_protocol::notify`). That answer comes from the computers, and it
//! is slow on purpose: a desk counts as "here" for minutes after its last
//! input, so that reading a long page is not mistaken for leaving. Which is
//! wrong the moment the user gets up and takes the phone with them — every
//! message for the next five minutes lit up an empty desk.
//!
//! The phone knows something no computer can: that it is being used. A
//! screen that is on and unlocked is one somebody is looking at — the screen
//! times out within a minute without a touch, so "on and unlocked" is also
//! "touched recently", without reading anyone's input. That alone does not
//! settle it: people pick the phone up at the desk. What settles it is order.
//! The user is at the phone when it was picked up after the last input on any
//! computer; a computer that sees input after that has them back.
//!
//! Computers say when they are in use right now (`NotifyMsg::Activity`, input
//! in the last few seconds) — finer than presence. A computer too old to say
//! leaves the order unknown, and then only a phone moving away decides it:
//! its Wi-Fi signal falling steeply while in hand, which is what walking out
//! of the room with it looks like. On its own that proves nothing — signal
//! swings with a hand over the antenna — so it never overrides a computer
//! that is known to be in use.
//!
//! Not used, because nothing lawful offers it: Bluetooth signal strength
//! between phone and computer. Android has no public API for the strength of
//! a connected classic link, reading it on Linux needs a raw HCI socket
//! (`CAP_NET_RAW`), and Bluetooth Channel Sounding, which measures distance
//! properly, needs hardware the phones here do not have.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How far back the Wi-Fi trend looks.
const WIFI_WINDOW: Duration = Duration::from_secs(90);

/// A fall in Wi-Fi signal, in dB, over [`WIFI_WINDOW`] while the phone is in
/// hand, read as being carried away. Two rooms and a wall are about this; a
/// hand over the antenna is less.
const WIFI_FALL_DB: i16 = 10;

/// What one computer last said about being in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopActivity {
    /// The latest moment input is known to have happened there; `None` when
    /// the computer never said (an older build).
    pub last_input: Option<Instant>,
}

/// The phone's own side of the question.
#[derive(Debug, Default)]
pub struct AtPhone {
    /// Since when the screen has been on and unlocked; `None` while not.
    in_use_since: Option<Instant>,
    wifi: VecDeque<(Instant, i16)>,
}

impl AtPhone {
    pub fn set_in_use(&mut self, in_use: bool, now: Instant) {
        match (in_use, self.in_use_since) {
            (true, None) => self.in_use_since = Some(now),
            (false, Some(_)) => self.in_use_since = None,
            _ => {}
        }
    }

    pub fn note_wifi(&mut self, dbm: Option<i16>, now: Instant) {
        // A lost reading says nothing about distance; a gap in the samples is
        // the honest record of it.
        if let Some(dbm) = dbm {
            self.wifi.push_back((now, dbm));
        }
        while self
            .wifi
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > WIFI_WINDOW)
        {
            self.wifi.pop_front();
        }
    }

    /// The signal fell steeply while the phone was in hand.
    fn moving_away(&self, since: Instant) -> bool {
        let Some(strongest) = self.wifi.iter().map(|(_, d)| *d).max() else {
            return false;
        };
        let Some(latest) = self
            .wifi
            .iter()
            .rev()
            .find(|(at, _)| *at >= since)
            .map(|(_, d)| *d)
        else {
            return false;
        };
        strongest - latest >= WIFI_FALL_DB
    }

    /// Whether the user is at the phone rather than at any of `desktops`.
    pub fn user_at_phone(&self, desktops: &[DesktopActivity]) -> bool {
        let Some(since) = self.in_use_since else {
            return false;
        };
        // Input on any computer after the phone was picked up: they are
        // working there, glancing at the phone.
        if desktops
            .iter()
            .any(|d| d.last_input.is_some_and(|at| at >= since))
        {
            return false;
        }
        if desktops.iter().any(|d| d.last_input.is_none()) {
            return self.moving_away(since);
        }
        true
    }
}

/// The phone's one record.
fn state() -> &'static Mutex<AtPhone> {
    static STATE: OnceLock<Mutex<AtPhone>> = OnceLock::new();
    STATE.get_or_init(Mutex::default)
}

fn lock() -> std::sync::MutexGuard<'static, AtPhone> {
    state().lock().unwrap_or_else(|p| p.into_inner())
}

/// The screen came on unlocked, or went off or locked.
pub fn set_in_use(in_use: bool) {
    tracing::debug!(in_use, "the phone is in use");
    lock().set_in_use(in_use, Instant::now());
}

/// A Wi-Fi reading, for the trend; `None` is no reading.
pub fn note_wifi(dbm: Option<i16>) {
    lock().note_wifi(dbm, Instant::now());
}

/// Whether the user is at the phone, given what the computers said.
pub fn user_at_phone(desktops: &[DesktopActivity]) -> bool {
    lock().user_at_phone(desktops)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn known(at: Instant) -> DesktopActivity {
        DesktopActivity {
            last_input: Some(at),
        }
    }

    const UNKNOWN: DesktopActivity = DesktopActivity { last_input: None };

    #[test]
    fn a_phone_lying_unused_never_keeps_notifications() {
        let t0 = Instant::now();
        let phone = AtPhone::default();
        assert!(!phone.user_at_phone(&[known(t0)]));
        assert!(!phone.user_at_phone(&[]));
    }

    #[test]
    fn picking_the_phone_up_after_the_last_input_keeps_them_on_the_phone() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.set_in_use(true, t0 + secs(3));
        assert!(phone.user_at_phone(&[known(t0)]));
    }

    #[test]
    fn typing_on_a_computer_while_the_phone_is_unlocked_sends_them_there() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.set_in_use(true, t0);
        assert!(!phone.user_at_phone(&[known(t0 + secs(5))]));
    }

    #[test]
    fn any_computer_in_use_after_pickup_wins_over_the_phone() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.set_in_use(true, t0 + secs(10));
        assert!(!phone.user_at_phone(&[known(t0), known(t0 + secs(20))]));
    }

    #[test]
    fn locking_the_phone_hands_the_decision_back_to_the_computers() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.set_in_use(true, t0 + secs(3));
        phone.set_in_use(false, t0 + secs(30));
        assert!(!phone.user_at_phone(&[known(t0)]));
    }

    #[test]
    fn staying_in_use_keeps_the_moment_it_was_picked_up() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.set_in_use(true, t0 + secs(3));
        phone.set_in_use(true, t0 + secs(60));
        assert!(!phone.user_at_phone(&[known(t0 + secs(30))]));
    }

    #[test]
    fn a_computer_that_cannot_say_leaves_it_to_the_computers_without_movement() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.note_wifi(Some(-50), t0);
        phone.set_in_use(true, t0 + secs(1));
        phone.note_wifi(Some(-52), t0 + secs(20));
        assert!(!phone.user_at_phone(&[UNKNOWN]));
    }

    #[test]
    fn carrying_the_phone_away_settles_it_when_a_computer_cannot_say() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.note_wifi(Some(-48), t0);
        phone.set_in_use(true, t0 + secs(1));
        phone.note_wifi(Some(-63), t0 + secs(30));
        assert!(phone.user_at_phone(&[UNKNOWN]));
    }

    #[test]
    fn a_falling_signal_never_outweighs_a_computer_known_to_be_in_use() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.note_wifi(Some(-48), t0);
        phone.set_in_use(true, t0 + secs(1));
        phone.note_wifi(Some(-70), t0 + secs(30));
        assert!(!phone.user_at_phone(&[known(t0 + secs(40))]));
    }

    #[test]
    fn readings_older_than_the_window_are_forgotten() {
        let t0 = Instant::now();
        let mut phone = AtPhone::default();
        phone.note_wifi(Some(-40), t0);
        phone.set_in_use(true, t0 + secs(100));
        phone.note_wifi(Some(-60), t0 + secs(120));
        assert!(!phone.user_at_phone(&[UNKNOWN]));
    }
}
