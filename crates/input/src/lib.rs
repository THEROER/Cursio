//! Cursio input injection (and, later, capture).
//!
//! The receiver side of input sharing: turn platform-neutral
//! [`cursio_protocol::InputEvent`]s from a peer into real pointer/keyboard
//! activity on this machine, via a persistent virtual device.
//!
//! Backends live behind [`InputInjector`] so the current uinput backend
//! (Linux) can be joined later by libei/portal (Wayland) and Windows
//! (SendInput) without changing callers. See `docs/input-design.md` for the
//! behaviour model (Mouse-Without-Borders, but relative motion).

use cursio_protocol::InputEvent;

#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod lin_cursor;
#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(target_os = "macos")]
pub mod mac_capture;
#[cfg(target_os = "macos")]
pub mod mac_cursor;
#[cfg(target_os = "macos")]
pub mod mac_keymap;
#[cfg(target_os = "linux")]
pub mod uinput;
#[cfg(target_os = "windows")]
pub mod win;
#[cfg(target_os = "windows")]
pub mod win_capture;
#[cfg(target_os = "windows")]
pub mod win_cursor;
#[cfg(target_os = "windows")]
mod win_keymap;
#[cfg(target_os = "linux")]
pub mod wl_capture;

#[cfg(target_os = "linux")]
pub use lin_cursor::LinuxCursor;
#[cfg(target_os = "macos")]
pub use mac::MacInjector;
#[cfg(target_os = "macos")]
pub use mac_cursor::MacCursor;
#[cfg(target_os = "macos")]
pub use mac_keymap::ModifierStyle;
#[cfg(target_os = "linux")]
pub use uinput::UinputInjector;
#[cfg(target_os = "windows")]
pub use win::WinInjector;
#[cfg(target_os = "windows")]
pub use win_cursor::WinCursor;

/// Start Windows RawInput capture, wiring edge-crossing detection to
/// `peer_edge`. Same signature as the Linux [`spawn_capture`] so callers are
/// platform-agnostic. Unlike Linux (where the Wayland portal watches all edges
/// and the caller filters), the Windows jump-zone watch lives in the mouse hook,
/// so the edge is pushed down into the backend here.
#[cfg(target_os = "windows")]
pub fn spawn_capture(
    peer_edge: Option<cursio_protocol::Edge>,
) -> Result<CaptureHandle, InputError> {
    win_capture::spawn_capture_with_edge(peer_edge)
}

/// Start macOS capture through a `CGEventTap`. Same signature as the other
/// platforms so callers stay platform-agnostic. `peer_edge` is ignored: macOS
/// has no pointer barriers, so the hotkey is what hands control over.
#[cfg(target_os = "macos")]
pub fn spawn_capture(
    peer_edge: Option<cursio_protocol::Edge>,
) -> Result<CaptureHandle, InputError> {
    mac_capture::spawn_capture(peer_edge)
}

/// Start local input capture, picking the backend for this Linux session.
///
/// On **Wayland** we can't grab `/dev/input` usefully, so we use the
/// `InputCapture` portal + libei ([`wl_capture`]) — which also gives us edge
/// crossing. On X11 / no display server we fall back to the evdev backend
/// ([`capture`]). The caller gets the same [`CaptureHandle`] either way.
///
/// `peer_edge` (which of our edges borders the peer) is passed to the Wayland
/// backend so it only arms a barrier on *that* edge — a barrier on any other
/// edge would still grab input on activation (locking the user), even though the
/// caller later filters the crossing out. With `None`, no barrier is armed, so
/// edge-crossing is off (only the hotkey hands off). The evdev backend has no
/// edge detection, so it ignores this.
#[cfg(target_os = "linux")]
pub fn spawn_capture(
    peer_edge: Option<cursio_protocol::Edge>,
) -> Result<CaptureHandle, InputError> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland");
    if wayland {
        tracing::info!("using Wayland InputCapture portal for capture");
        wl_capture::spawn_wayland_capture(peer_edge)
    } else {
        tracing::info!("using evdev for capture");
        capture::spawn_capture()
    }
}

/// Which of this machine's edges borders the peer the cursor would leave to.
///
/// Machine-wide rather than per session, and settable after capture has
/// started, because those are the two facts that decide where it lives. There
/// is one physical mouse, so there is one edge it can leave by; and the answer
/// comes from the peer's position in the greeting, which arrives seconds
/// *after* the barrier has to exist. Passing it at construction meant the user
/// had to state on the command line something both machines already knew.
///
/// Encoded as a number so the capture backends — which read it from a signal
/// handler, a Win32 hook and a `CFRunLoop` callback — can do so without a lock.
static PEER_EDGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Tell capture which edge borders the peer. `None` turns edge crossing off.
pub fn set_peer_edge(edge: Option<cursio_protocol::Edge>) {
    let code = match edge {
        None => 0,
        Some(cursio_protocol::Edge::Left) => 1,
        Some(cursio_protocol::Edge::Right) => 2,
        Some(cursio_protocol::Edge::Top) => 3,
        Some(cursio_protocol::Edge::Bottom) => 4,
    };
    if PEER_EDGE.swap(code, std::sync::atomic::Ordering::SeqCst) != code {
        tracing::info!(?edge, "watching this edge for a crossing toward the peer");
    }
    // The Windows hook keeps its own copy, because it is read from inside a
    // low-level hook where nothing may allocate or lock and the module's own
    // static is what that code already reaches for.
    #[cfg(target_os = "windows")]
    win_capture::set_watched_edge(edge);
    // Wayland does not read this value, it acts on it: a barrier is a D-Bus
    // object placed on one edge, so moving the watch means taking capture down
    // and putting a new list up. Harmless when the evdev backend is the one
    // running — nobody is listening.
    #[cfg(target_os = "linux")]
    wl_capture::set_watched_edge(edge);
}

/// Which edge capture is watching, if any.
pub fn peer_edge() -> Option<cursio_protocol::Edge> {
    match PEER_EDGE.load(std::sync::atomic::Ordering::SeqCst) {
        1 => Some(cursio_protocol::Edge::Left),
        2 => Some(cursio_protocol::Edge::Right),
        3 => Some(cursio_protocol::Edge::Top),
        4 => Some(cursio_protocol::Edge::Bottom),
        _ => None,
    }
}

/// A screen-edge crossing detected by the capture backend: the shared cursor
/// was shoved past `edge` at `pos_norm` along it (`0.0`..=`1.0`). The session
/// turns this into an edge-crossing ownership handoff (if that edge borders the
/// peer). Only the Wayland backend emits these today (via portal barriers);
/// evdev/Windows will grow their own edge detection later.
#[derive(Debug, Clone, Copy)]
pub struct EdgeCrossing {
    /// Which of this machine's edges the cursor left across.
    pub edge: cursio_protocol::Edge,
    /// Where along the edge, normalized: y-fraction for a vertical edge
    /// (left/right), x-fraction for a horizontal one (top/bottom).
    pub pos_norm: f32,
}

/// Handle to a running input capture: a channel of neutral events, the names of
/// the devices being read, and a [`GrabControl`] to freeze/unfreeze local input.
/// Both platform capture backends return this.
pub struct CaptureHandle {
    /// Receives neutral input events from all captured devices.
    pub events: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    /// Receives edge-crossing signals (cursor pushed off a screen edge). Empty
    /// on backends without edge detection.
    pub edges: tokio::sync::mpsc::UnboundedReceiver<EdgeCrossing>,
    /// Human-readable names of the sources we're capturing, for logging/UI.
    pub devices: Vec<String>,
    /// Freeze (grab) or release local input while still capturing it.
    pub grab: GrabControl,
}

/// What the capture backend should be doing with local input right now.
///
/// Two of these mirror the session's two ownership states; `Idle` is the
/// no-session baseline. The distinction matters on **Wayland**, where the
/// InputCapture portal has to be *armed* while we're active so a cursor shoved
/// off the peer-facing edge is detected (see [`wl_capture`]) — arming does **not**
/// grab input, only watches the edges. On evdev/Windows there are no edge
/// barriers, so `Active` and `Idle` are both ungrabbed; unlike `Active`, `Idle`
/// discards physical events instead of retaining them for a future session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// This machine holds the shared cursor. Local input acts normally (not
    /// grabbed). On Wayland the edge barriers are *armed* so an off-edge shove
    /// hands control to the peer.
    Active,
    /// This machine is driving the peer. Local input is grabbed and streamed;
    /// on Wayland the portal is capturing after a barrier activation.
    Dormant,
    /// No active session: input acts locally, nothing grabbed, barriers down.
    Idle,
    /// Tear the capture down for good: release everything and end the backend's
    /// thread/task.
    ///
    /// Distinct from `Idle`, which keeps the machinery alive and ready. A capture
    /// that merely goes idle still holds its Wayland portal session (or Windows
    /// hooks), so a reconnecting `serve` would stack a second capture on top of
    /// it — and every live capture reports the same barrier activation, which is
    /// how one edge crossing turned into four handoffs. Callers must send this
    /// when a session ends rather than relying on handles being dropped: the
    /// hotkey filter holds a `GrabControl` clone of its own, so the watch channel
    /// does not close on its own.
    Shutdown,
}

impl InputMode {
    /// Whether local input should be grabbed (frozen locally) in this mode.
    /// Only `Dormant` grabs; this is what evdev/Windows key off, ignoring the
    /// Wayland-only arm-while-`Active` distinction.
    pub fn grabbed(self) -> bool {
        matches!(self, InputMode::Dormant)
    }

    /// Whether physical events belong to an active session and should enter
    /// its queue. Idle capture stays installed for fast reconnects, but it must
    /// not retain hours of local input for the next peer.
    pub fn forwards_events(self) -> bool {
        matches!(self, InputMode::Active | InputMode::Dormant)
    }
}

/// Controls what the capture backend does with local input — grab it, arm edge
/// barriers, or leave it alone — via an [`InputMode`].
///
/// While this machine drives the peer (`Dormant`) its own cursor/keyboard are
/// grabbed so they don't double-act; while it holds the cursor (`Active`) input
/// stays local but, on Wayland, edge barriers are armed to catch an off-edge
/// crossing. The backend may stay installed in every mode, but `Idle` events are
/// discarded so reconnecting cannot replay stale input.
/// Backed by a [`tokio::sync::watch`] the capture tasks observe (Linux evdev:
/// `EVIOCGRAB`; Wayland: portal enable/disable; Windows: low-level hooks).
#[derive(Clone)]
pub struct GrabControl {
    tx: tokio::sync::watch::Sender<InputMode>,
}

impl GrabControl {
    /// Create a control paired with the receiver the capture tasks watch.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", test))]
    pub(crate) fn new() -> (Self, tokio::sync::watch::Receiver<InputMode>) {
        let (tx, rx) = tokio::sync::watch::channel(InputMode::Idle);
        (Self { tx }, rx)
    }

    /// Set the capture mode (see [`InputMode`]). Idempotent.
    pub fn set_mode(&self, mode: InputMode) {
        // Ignore send errors: if all capture tasks have ended there's nothing
        // left to drive, which is fine.
        let _ = self.tx.send(mode);
    }

    /// Current capture mode.
    pub fn mode(&self) -> InputMode {
        *self.tx.borrow()
    }

    /// Convenience for the two-state callers that only care about grab: `true`
    /// = [`Dormant`](InputMode::Dormant), `false` = [`Active`](InputMode::Active).
    pub fn set_grabbed(&self, grabbed: bool) {
        self.set_mode(if grabbed {
            InputMode::Dormant
        } else {
            InputMode::Active
        });
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn idle_and_shutdown_do_not_forward_events() {
        assert!(!InputMode::Idle.forwards_events());
        assert!(!InputMode::Shutdown.forwards_events());
        assert!(InputMode::Active.forwards_events());
        assert!(InputMode::Dormant.forwards_events());
    }

    #[test]
    fn grab_control_reports_each_transition() {
        let (control, _receiver) = GrabControl::new();
        assert_eq!(control.mode(), InputMode::Idle);
        control.set_mode(InputMode::Active);
        assert_eq!(control.mode(), InputMode::Active);
        control.set_mode(InputMode::Dormant);
        assert_eq!(control.mode(), InputMode::Dormant);
    }
}

/// Errors from an input backend.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("input backend unavailable: {0}")]
    Unavailable(String),
}

/// Controls the visibility of this machine's own system cursor.
///
/// When this machine hands control to the peer (its input is now driving the
/// remote), its local cursor should disappear so a stale pointer doesn't sit on
/// screen. [`hide`](CursorControl::hide) is called then; [`show`] on regaining
/// control.
///
/// Backends are chosen at runtime by environment, because "hide the cursor" is
/// wildly platform-dependent:
/// - **Windows** — real hide via the OS cursor API.
/// - **Linux X11** — real hide via XFixes.
/// - **Linux Wayland** — **not possible** from a background evdev daemon (the
///   compositor only lets a focused client with its own surface touch the
///   cursor). The Wayland backend is an honest no-op until Cursio also runs as a
///   Wayland/libei client; callers don't change when that lands.
///
/// [`show`]: CursorControl::show
pub trait CursorControl {
    /// Hide this machine's system cursor. Idempotent; no-op on backends that
    /// can't hide (logs once).
    fn hide(&mut self) -> Result<(), InputError>;

    /// Restore this machine's system cursor. Idempotent.
    fn show(&mut self) -> Result<(), InputError>;

    /// Short label for logs/UI, e.g. `"windows"`, `"x11"`, `"wayland-noop"`.
    fn backend(&self) -> &'static str;
}

/// Injects peer input events into the local machine.
///
/// An injector owns a **persistent** virtual device for the whole session
/// (created once, not per transition) and an `active` flag:
///
/// - **active** → [`inject`](InputInjector::inject) replays events, so the
///   shared cursor/keyboard acts here.
/// - **inactive** → motion events are dropped and the local cursor freezes;
///   the device stays alive, ready to reactivate instantly.
///
/// See `docs/input-design.md` for why the device is kept alive and why full
/// cursor-hiding (parking + local grab) is deferred to the transition
/// milestone.
pub trait InputInjector {
    /// Replay one event on the local machine (no-op for motion while inactive).
    fn inject(&mut self, event: InputEvent) -> Result<(), InputError>;

    /// Whether events are currently being replayed here.
    fn is_active(&self) -> bool;

    /// Enable or disable replay. Toggling does not create/destroy the device.
    fn set_active(&mut self, active: bool);

    /// Release every key and mouse button we currently hold pressed via this
    /// injector, by injecting the matching key-up/button-up events.
    ///
    /// Called when a session ends (peer left, link dropped, shutdown) so a key
    /// that was down when the connection died doesn't **stick** on this machine:
    /// its key-up would otherwise never arrive. Idempotent — with nothing held
    /// it does nothing.
    fn release_all(&mut self) -> Result<(), InputError>;

    /// Short label for logs/UI, e.g. `"uinput"`.
    fn backend(&self) -> &'static str;
}
