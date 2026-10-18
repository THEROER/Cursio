//! Whether the user is at this machine.
//!
//! A phone's notification should appear on the screen being looked at, and
//! nowhere else (see `cursio_protocol::notify`). Two things answer that:
//!
//! - **not driving a peer** — while this machine's mouse moves a cursor on
//!   another screen, the user is looking at that screen, not this one;
//! - **recent input** — the system's own idle time. Both Mutter's idle monitor
//!   and Windows' `GetLastInputInfo` count input injected by Cursio itself,
//!   so a laptop driven from the desktop beside it is correctly "here" while
//!   its own keyboard lies untouched.
//!
//! Where the idle time cannot be read the answer rests on the first test
//! alone: a machine claiming the user is present costs a duplicate
//! notification, while one wrongly claiming absence loses it.
//!
//! ## Which machine, for sound
//!
//! A call's voice carried here should play where the user is
//! ([`whereabouts`]). The same two tests say it, from this machine's side:
//! while its mouse drives a peer, the user is at that peer's screen — and its
//! speakers; otherwise, with recent input, here. A machine left alone says
//! nothing about where the user went, so the answer is then "away" and the
//! sound stays where it was.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use cursio_protocol::PeerId;

/// How long without input before the user is taken to have left.
pub const AWAY_AFTER: Duration = Duration::from_secs(5 * 60);

/// Whether the user is at this machine now.
pub fn user_here() -> bool {
    if crate::update::quiet::driving() {
        return false;
    }
    idle().is_none_or(|idle| idle < AWAY_AFTER)
}

/// How long a machine counts as in use after its last input, for
/// [`activity`]. Short on purpose: it is the answer to "is someone typing here
/// right now", which is what a phone just picked up is compared against —
/// `AWAY_AFTER` would keep a desk "in use" for minutes after the user left it.
pub const ACTIVE_WITHIN: Duration = Duration::from_secs(15);

/// Whether this machine is in use right now, and how long ago its last input
/// was; `None` when the system cannot say. A machine whose mouse drives a peer
/// is not in use itself: the input lands on the other screen.
pub fn activity() -> Option<(bool, Duration)> {
    if crate::update::quiet::driving() {
        return Some((false, ACTIVE_WITHIN));
    }
    let idle = idle()?;
    Some((idle < ACTIVE_WITHIN, idle))
}

/// Where the user is, as this machine can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whereabouts {
    Here,
    /// At the peer this machine's mouse is driving.
    Peer(PeerId),
    /// Not here, and nowhere this machine can see.
    Away,
}

pub fn whereabouts() -> Whereabouts {
    if let Some(peer) = driven_peer() {
        return Whereabouts::Peer(peer);
    }
    match idle() {
        Some(idle) if idle >= AWAY_AFTER => Whereabouts::Away,
        _ => Whereabouts::Here,
    }
}

/// Whether the user's hand was on this machine's keyboard or mouse just now
/// ([`ACTIVE_WITHIN`]), whichever screen it drove: a call taken then was
/// taken at this desk rather than on the phone in the user's hand.
pub fn at_desk() -> bool {
    driven_peer().is_some() || idle().is_some_and(|idle| idle < ACTIVE_WITHIN)
}

fn driven() -> &'static Mutex<Vec<(u64, PeerId)>> {
    static DRIVEN: OnceLock<Mutex<Vec<(u64, PeerId)>>> = OnceLock::new();
    DRIVEN.get_or_init(Default::default)
}

/// The peer this machine's mouse drives now, if any.
pub fn driven_peer() -> Option<PeerId> {
    driven()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .last()
        .map(|(_, peer)| *peer)
}

/// Held by one session: names its peer as driven while it is.
pub struct DrivenMark {
    token: u64,
    peer: Option<PeerId>,
    on: bool,
}

impl DrivenMark {
    pub fn new(peer: Option<PeerId>) -> Self {
        DrivenMark {
            token: rand::random(),
            peer,
            on: false,
        }
    }

    pub fn set(&mut self, driving: bool) {
        let Some(peer) = self.peer else { return };
        if driving == self.on {
            return;
        }
        self.on = driving;
        let mut list = driven().lock().unwrap_or_else(|p| p.into_inner());
        list.retain(|(token, _)| *token != self.token);
        if driving {
            list.push((self.token, peer));
        }
    }
}

impl Drop for DrivenMark {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// How long since the last keyboard or pointer input, if the system says.
///
/// Read by one thread for the whole process, once a second, and answered
/// here from that reading. Every link's notification module asked Mutter
/// itself, twice every two seconds, and Mutter answers from the Shell's main
/// thread: 120 ms a call on average and over a second while the Shell was
/// busy, all of it spent stalling that module's own traffic.
#[cfg(target_os = "linux")]
fn idle() -> Option<Duration> {
    static READING: OnceLock<Mutex<Option<(std::time::Instant, Duration)>>> = OnceLock::new();
    let reading = READING.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("idle-monitor".into())
            .spawn(|| loop {
                let idle = read_idle();
                if let Some(reading) = READING.get() {
                    *reading.lock().unwrap_or_else(|e| e.into_inner()) =
                        idle.map(|idle| (std::time::Instant::now(), idle));
                }
                std::thread::sleep(IDLE_POLL);
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot start the idle monitor; presence falls back to driving alone");
        }
        Mutex::new(None)
    });
    let (at, idle) = (*reading.lock().unwrap_or_else(|e| e.into_inner()))?;
    // Idle time grows by itself between readings; input in between is the
    // up-to-a-second error this costs.
    Some(idle + at.elapsed())
}

#[cfg(target_os = "linux")]
const IDLE_POLL: Duration = Duration::from_secs(1);

#[cfg(target_os = "linux")]
fn read_idle() -> Option<Duration> {
    let ms = crate::session_bus::with_bus(|bus| {
        let reply = bus
            .call_method(
                Some("org.gnome.Mutter.IdleMonitor"),
                "/org/gnome/Mutter/IdleMonitor/Core",
                Some("org.gnome.Mutter.IdleMonitor"),
                "GetIdletime",
                &(),
            )
            .map_err(|e| tracing::debug!(error = %e, "Mutter's idle monitor did not answer"))
            .ok()?;
        reply.body().deserialize::<u64>().ok()
    })??;
    Some(Duration::from_millis(ms))
}

#[cfg(target_os = "windows")]
fn idle() -> Option<Duration> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid LASTINPUTINFO with its size set, as required.
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return None;
    }
    // Both are milliseconds since boot in 32 bits; the subtraction wraps the
    // same way they do.
    let now = unsafe { GetTickCount() };
    Some(Duration::from_millis(u64::from(
        now.wrapping_sub(info.dwTime),
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn idle() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_driven_peer_is_named_while_driven_and_forgotten_after() {
        let peer = PeerId::from_u128(rand::random());
        let mut mark = DrivenMark::new(Some(peer));
        mark.set(true);
        assert_eq!(driven_peer(), Some(peer));
        mark.set(false);
        assert_ne!(driven_peer(), Some(peer));
        mark.set(true);
        drop(mark);
        assert_ne!(driven_peer(), Some(peer));
    }
}
