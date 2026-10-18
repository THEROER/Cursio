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

use std::time::Duration;

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

/// How long since the last keyboard or pointer input, if the system says.
#[cfg(target_os = "linux")]
fn idle() -> Option<Duration> {
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
