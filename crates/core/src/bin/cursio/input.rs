//! This machine's input as the sessions share it: capture kept for the
//! life of the process, the ownership hotkey, injection and the cursor.

use cursio_core::input_hub::{InputHub, MachineInput};
use cursio_core::session::{self};

/// Narrow each edge to the part of it that crosses (`[input.crossing]`). Like
/// the modifier map, a property of this machine's desktop rather than of any
/// link, so it goes straight to capture.
pub(crate) fn apply_crossing(crossing: &cursio_core::config::CrossingConfig) {
    use cursio_protocol::Edge;
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let span = crossing.span(edge);
        cursio_input::set_crossing_span(
            edge,
            f32::from(span.from) / 100.0,
            f32::from(span.to) / 100.0,
        );
    }
}

/// How often the desktop's screens are read again. Nothing on GNOME or
/// Windows announces a monitor plugged in to a process with no window, and a
/// read is one D-Bus call or one enumeration.
const SCREENS_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Keep this machine's screens on the desk current, on a thread of its own:
/// the read blocks, and on Linux it is a D-Bus call that must not run on the
/// runtime's threads.
pub(crate) fn watch_screens(
    live: cursio_core::live::LiveConfig,
    control: cursio_core::control::NodeState,
) {
    let spawned = std::thread::Builder::new()
        .name("cursio-screens".into())
        .spawn(move || {
            let mut said = None;
            loop {
                match cursio_input::screens::screens() {
                    Ok(screens) => {
                        said = None;
                        control.set_local_screens(screens.clone());
                        live.set_screens(screens);
                    }
                    Err(e) => {
                        let message = e.to_string();
                        if said.as_ref() != Some(&message) {
                            tracing::warn!(error = %message, "cannot read this desktop's screens; it stays on the grid");
                            said = Some(message);
                        }
                    }
                }
                std::thread::sleep(SCREENS_EVERY);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start watching the screens; this machine stays on the grid");
    }
}

/// Read `--mac-modifiers` and hand it to the keymap, or refuse to start.
///
/// Refusing is deliberate. A mistyped map would otherwise be silently ignored
/// and the user would spend the session wondering why Command is still in the
/// wrong place — and the parse error already names what it could not read.
///
/// The map is stored in the keymap rather than threaded through the session
/// layers because it is not a property of any session: there is one keyboard
/// per machine, settled before the first peer connects, exactly like the
/// capture backend.
pub(crate) fn apply_modifier_map(spec: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let map = cursio_input::ModifierMap::parse(spec)
            .map_err(|e| anyhow::anyhow!("--mac-modifiers: {e}"))?;
        cursio_input::mac_keymap::configure(map);
        tracing::info!(
            modifiers = %map.describe(),
            hotkey = %map.hotkey_on_this_mac(),
            "keyboard mapping"
        );
    }
    // Elsewhere the flag is accepted and does nothing: the mapping happens
    // entirely on the Mac, in both directions. Saying so beats a user setting
    // it on the wrong machine and watching nothing change.
    #[cfg(not(target_os = "macos"))]
    if spec != "positional" {
        tracing::warn!(
            spec,
            "--mac-modifiers only applies on a Mac; set it there, on the machine being driven"
        );
    }
    Ok(())
}

// Ownership hotkey = Ctrl + Alt + G held together, in the wire's evdev codes
// (the neutral namespace every platform maps to); `G` for "grab". Either Ctrl
// and either Alt count, so laptops without a right Ctrl still work. Panic /
// emergency release is Ctrl+Alt+Esc: it slams control back to this machine no
// matter the session state.
use cursio_protocol::keys::{
    KEY_ESC, KEY_G, KEY_LEFTALT, KEY_LEFTCTRL, KEY_RIGHTALT, KEY_RIGHTCTRL,
};

/// Spawn a task that hands captured events to the hub, but watches for the
/// ownership hotkey (**Ctrl+Alt+G**) and emits an [`OwnershipCmd`] instead of
/// forwarding the trigger key. The hub decides which session each one reaches.
///
/// Combo (not a double-tap) so it can't fire by accident and doesn't depend on
/// tap timing. Modifier keys are still forwarded to the peer (they're needed in
/// normal use); only the `G` press that completes the combo is swallowed, so a
/// stray "g" doesn't reach the peer. Toggling alternates Take/Release; the
/// session ignores a redundant one, so we don't track remote state here.
pub(crate) fn spawn_hotkey_filter(
    mut upstream: cursio_input::EventReceiver,
    hub: InputHub,
    grab: cursio_input::GrabControl,
) {
    use cursio_protocol::InputEvent;

    tokio::spawn(async move {
        // Track whether each modifier side is currently held.
        let mut ctrl = false;
        let mut alt = false;
        // Set once the combo fires, so the modifier RELEASES that follow are
        // swallowed too.
        //
        // Without this the peer is left holding Ctrl+Alt forever: we forward
        // Ctrl↓ and Alt↓ (they look like ordinary keys until G arrives), then G
        // flips ownership to us — and from that moment we no longer forward
        // anything, so Ctrl↑/Alt↑ never reach the peer. The peer's injector
        // reports it as `releasing held keys ... keys=2` only at session end;
        // meanwhile every click there is a Ctrl+click and the mouse looks
        // broken. That was the "pressed a key and everything froze" report.
        let mut swallow_until_release = false;
        // Every change of mode forgets what is held. A hotkey that takes the
        // cursor back ends capture with Ctrl and Alt still down, and their
        // releases go to the local desktop — capture never sees them. Kept,
        // `ctrl` stayed true and the swallow never ended: from then on every
        // Alt press and release was eaten, so Alt+Tab on the peer arrived as
        // Tab, until the user happened to press Ctrl again.
        let mut last_mode = grab.mode();
        while let Some(event) = upstream.recv().await {
            let mode = grab.mode();
            if mode != last_mode {
                last_mode = mode;
                ctrl = false;
                alt = false;
                swallow_until_release = false;
            }
            if !mode.forwards_events() {
                continue;
            }
            if let InputEvent::Key { key, pressed } = event {
                match key {
                    KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                        ctrl = pressed;
                        // Tail of a fired combo: drop the release, and end the
                        // swallow once both modifiers are up. A press is never
                        // swallowed — it starts a new combo, and ends the old
                        // one's tail whatever was missed of it.
                        if swallow_until_release {
                            if pressed {
                                swallow_until_release = false;
                            } else {
                                if !alt {
                                    swallow_until_release = false;
                                }
                                continue;
                            }
                        }
                    }
                    KEY_LEFTALT | KEY_RIGHTALT => {
                        alt = pressed;
                        if swallow_until_release {
                            if pressed {
                                swallow_until_release = false;
                            } else {
                                if !ctrl {
                                    swallow_until_release = false;
                                }
                                continue;
                            }
                        }
                    }
                    KEY_G if pressed && ctrl && alt => {
                        // Combo complete: request a handoff and swallow this G.
                        // The session decides direction (only the active side
                        // hands off), so we just send a Toggle.
                        tracing::info!("ownership hotkey (Ctrl+Alt+G)");
                        hub.route_command(session::OwnershipCmd::Toggle);
                        // Swallow the modifier releases too — see the flag's
                        // declaration for why the peer would otherwise be stuck
                        // holding Ctrl+Alt.
                        swallow_until_release = true;
                        continue; // don't forward the trigger key to the peer
                    }
                    KEY_ESC if pressed && ctrl && alt => {
                        // PANIC: recover control locally RIGHT NOW, without
                        // waiting on the session. Dropping to Idle here ungrabs
                        // local input and (on Wayland) tears the portal barriers
                        // down, so this machine is usable even if the peer/link
                        // is wedged. Also ask every session to flip ownership +
                        // restore the cursor, which works if they're still alive.
                        tracing::warn!("PANIC hotkey (Ctrl+Alt+Esc): releasing locally");
                        grab.set_mode(cursio_input::InputMode::Idle);
                        cursio_input::set_watched_edges(hub.panic().edges);
                        swallow_until_release = true;
                        continue; // don't forward Esc to the peer
                    }
                    _ => {}
                }
            }
            hub.route_event(session::CapturedInput::from(event));
        }
        if upstream.stats().failed {
            grab.set_mode(cursio_input::InputMode::Idle);
            cursio_input::set_watched_edges(hub.panic().edges);
            cursio_core::health::capture_overloaded();
        }
    });
}

/// Spawn a task that turns capture-reported edge crossings into ownership
/// handoffs, each to the session of the peer beyond the crossed edge. A
/// crossing with no peer beyond it is dropped by the hub, so the cursor just
/// stops there as usual — though capture arms only edges that border a peer,
/// so that is a peer that left between the crossing and now.
///
/// This is where the screen topology lives; the session stays topology-agnostic
/// (it only sees "hand off to the peer").
pub(crate) fn spawn_edge_handoff(
    mut edges: tokio::sync::mpsc::UnboundedReceiver<cursio_input::EdgeCrossing>,
    hub: InputHub,
) {
    tokio::spawn(async move {
        while let Some(crossing) = edges.recv().await {
            tracing::info!(edge = ?crossing.edge, pos = crossing.pos_norm, "edge crossing toward peer");
            hub.route_command(session::OwnershipCmd::HandoffToPeer {
                edge: crossing.edge,
                pos_norm: crossing.pos_norm,
                at: crossing.at,
                entry: None,
            });
        }
    });
}

/// Create the platform input injector, or `None` if unavailable / unsupported.
///
/// Linux uses a uinput virtual pointer, Windows `SendInput`, macOS a `CGEvent`
/// posted at the HID tap. On any other platform there's no backend yet, so
/// injection falls back to logging (see [`inject`]).
pub(crate) fn make_injector() -> Option<Box<dyn cursio_input::InputInjector>> {
    #[cfg(target_os = "linux")]
    {
        use cursio_input::InputInjector as _;
        match cursio_input::UinputInjector::new() {
            Ok(inj) => {
                tracing::info!(backend = inj.backend(), "input injector ready");
                Some(Box::new(inj))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no input injector; peer input will be logged only");
                None
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use cursio_input::InputInjector as _;
        match cursio_input::WinInjector::new() {
            Ok(inj) => {
                tracing::info!(backend = inj.backend(), "input injector ready");
                Some(Box::new(inj))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no input injector; peer input will be logged only");
                None
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        use cursio_input::InputInjector as _;
        match cursio_input::MacInjector::new() {
            Ok(inj) => {
                tracing::info!(backend = inj.backend(), "input injector ready");
                Some(Box::new(inj))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no input injector; peer input will be logged only");
                None
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        tracing::warn!("no input backend on this platform yet; peer input will be logged only");
        None
    }
}

/// Create the platform cursor controller (hide/show our own cursor), or `None`
/// if unsupported. Linux is currently a no-op backend (Wayland can't hide; X11
/// XFixes not wired yet); Windows really hides the cursor, and macOS hides it
/// for as long as Cursio is the frontmost application.
pub(crate) fn make_cursor() -> Option<Box<dyn cursio_input::CursorControl>> {
    #[cfg(target_os = "linux")]
    {
        use cursio_input::CursorControl as _;
        match cursio_input::LinuxCursor::new() {
            Ok(c) => {
                tracing::info!(backend = c.backend(), "cursor control ready");
                Some(Box::new(c))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no cursor control");
                None
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use cursio_input::CursorControl as _;
        match cursio_input::WinCursor::new() {
            Ok(c) => {
                tracing::info!(backend = c.backend(), "cursor control ready");
                Some(Box::new(c))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no cursor control");
                None
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        use cursio_input::CursorControl as _;
        match cursio_input::MacCursor::new() {
            Ok(c) => {
                tracing::info!(backend = c.backend(), "cursor control ready");
                Some(Box::new(c))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no cursor control");
                None
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Tell the injector whether peer events should act on this machine.
///
/// Only true while this machine holds the shared cursor — which is also the
/// only time a peer sends any. Setting it matters beyond dropping stray events:
/// a backend uses the transition to false→true to re-read state that went stale
/// while it was idle, and the macOS one in particular re-seeds the shadow
/// cursor it has to keep because `CGEvent` has no relative motion. Left
/// permanently active, that shadow drifts from the real pointer every time the
/// user moves this machine's own mouse, and the peer's first movement afterwards
/// teleports the cursor to wherever the shadow had got to.
pub(crate) fn set_injecting(
    injector: Option<&mut Box<dyn cursio_input::InputInjector>>,
    active: bool,
) {
    if let Some(inj) = injector {
        inj.set_active(active);
    }
}

/// Hide (`true`) or show (`false`) the local cursor through the controller,
/// logging any backend error.
pub(crate) fn set_cursor_hidden(
    cursor: Option<&mut Box<dyn cursio_input::CursorControl>>,
    hidden: bool,
) {
    if let Some(c) = cursor {
        let res = if hidden { c.hide() } else { c.show() };
        if let Err(e) = res {
            tracing::warn!(error = %e, hidden, "cursor hide/show failed");
        }
    }
}

/// Local input capture, set up once and reused across peer sessions.
///
/// Capture is a property of this *machine*, not of any one peer link: on Wayland
/// starting it means asking the portal for an InputCapture session, which
/// prompts the user for permission. Rebuilding it per connection re-prompted on
/// every reconnect, and the new request raced the old session's teardown — the
/// portal still saw the previous session holding capture, so the request hung
/// and edge-crossing stopped working until the process was restarted.
///
/// So the capture task, the hotkey filter and the edge-crossing forwarder all
/// live here, outliving individual sessions. They feed the [`InputHub`], and
/// every session attaches to it for as long as it runs — all of them at once,
/// each reached only by what concerns its own peer.
pub(crate) struct PersistentCapture {
    /// Which session each captured event and ownership request reaches.
    pub(crate) hub: InputHub,
    /// Freeze/release local input.
    pub(crate) grab: cursio_input::GrabControl,
    /// Hides this machine's pointer while it drives a peer. One per machine,
    /// like the pointer: two sessions each holding one wrote over each other,
    /// and macOS counts hides, so two hides needed two shows.
    pub(crate) cursor: std::cell::RefCell<Option<Box<dyn cursio_input::CursorControl>>>,
    /// Whether the pointer is hidden now, so a session reporting a change that
    /// changes nothing machine-wide does not hide or show it again.
    pub(crate) hidden: std::cell::Cell<bool>,
}

/// One session's attachment to the hub, and its two ports.
///
/// Dropping it detaches — on every way out of a session, an error included —
/// and puts grab, barriers and pointer back in line with the links that
/// remain. A session that left without detaching would keep its edge armed and
/// could keep receiving this machine's input.
pub(crate) struct HubLink<'c> {
    pub(crate) capture: &'c PersistentCapture,
    pub(crate) token: cursio_core::input_hub::LinkToken,
    pub(crate) events: session::InputReceiver,
    pub(crate) commands: tokio::sync::mpsc::UnboundedReceiver<session::OwnershipCmd>,
}

impl Drop for HubLink<'_> {
    fn drop(&mut self) {
        self.capture.apply(self.capture.hub.detach(self.token));
    }
}

impl PersistentCapture {
    /// Bring grab, barriers and pointer in line with what the hub says this
    /// machine's input is doing.
    ///
    /// Each is written only when it changes. Every session reports its own
    /// ownership, and most reports change nothing machine-wide; setting the
    /// grab mode again is not free — on Wayland re-entering Active releases
    /// the portal grab and nudges the pointer.
    pub(crate) fn apply(&self, machine: MachineInput) {
        arm(&machine);
        let mode = if machine.driving {
            cursio_input::InputMode::Dormant
        } else if machine.linked {
            cursio_input::InputMode::Active
        } else {
            cursio_input::InputMode::Idle
        };
        if self.grab.mode() != mode {
            self.grab.set_mode(mode);
        }
        if self.hidden.get() != machine.driving {
            self.hidden.set(machine.driving);
            set_cursor_hidden(self.cursor.borrow_mut().as_mut(), machine.driving);
        }
    }
}

/// Arm the edges the hub says lead somewhere, and only the pixels of each
/// where a peer's screen meets it. Callable from any thread: capture keeps
/// both behind its own atomics and locks.
pub(crate) fn arm(machine: &MachineInput) {
    use cursio_core::input_hub::edge_slot;
    use cursio_protocol::Edge;
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        cursio_input::set_crossing_windows(edge, machine.windows[edge_slot(edge)].clone());
    }
    cursio_input::set_watched_edges(machine.edges);
}

/// Capture shared between concurrent mesh sessions.
///
/// `Rc` rather than a lock because mesh runs single-threaded on a `LocalSet`.
/// `None` inside means capture is unavailable/disabled. Every session uses it
/// at once: which of them a keystroke or a crossing reaches is the hub's
/// decision, not a question of who borrowed capture first. Lending it whole to
/// the first session left every other peer without local input.
pub(crate) type SharedCapture = std::rc::Rc<Option<PersistentCapture>>;

/// Bring up input capture for the whole process lifetime.
///
/// Returns `None` when capture is disabled or unavailable, in which case
/// sessions simply run without local input sharing.
pub(crate) fn make_persistent_capture(enabled: bool) -> Option<PersistentCapture> {
    if !enabled {
        return None;
    }

    let handle = make_capture()?;
    let grab = handle.grab.clone();

    // Both forwarders are permanent too: they own the raw receivers and hand
    // what they read to the hub, which passes each to the session it concerns.
    let hub = InputHub::new();
    spawn_hotkey_filter(handle.events, hub.clone(), grab.clone());
    spawn_edge_handoff(handle.edges, hub.clone());

    Some(PersistentCapture {
        hub,
        grab,
        cursor: std::cell::RefCell::new(make_cursor()),
        hidden: std::cell::Cell::new(false),
    })
}

/// Start capturing local input, returning the full [`CaptureHandle`] (event
/// channel + grab control), or `None` if unavailable / unsupported.
///
/// On Linux this reads `/dev/input/event*` via evdev (needs `input`-group
/// access) or the Wayland portal; on Windows it installs RawInput hooks; on
/// macOS it opens a `CGEventTap` (needs the Accessibility permission).
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub(crate) fn make_capture() -> Option<cursio_input::CaptureHandle> {
    match cursio_input::spawn_capture() {
        Ok(handle) => {
            tracing::info!(devices = ?handle.devices, "capturing local input");
            cursio_core::health::input_capture_started();
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start input capture");
            cursio_core::health::input_capture_unavailable(&e.to_string());
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) fn make_capture() -> Option<cursio_input::CaptureHandle> {
    tracing::warn!("no input capture backend on this platform yet");
    None
}

/// Inject one event through the injector, or log it if there's no backend.
pub(crate) fn inject(
    injector: Option<&mut Box<dyn cursio_input::InputInjector>>,
    event: cursio_protocol::InputEvent,
) {
    match injector {
        Some(inj) => {
            if let Err(e) = inj.inject(event) {
                tracing::warn!(error = %e, "failed to inject input event");
            }
        }
        None => tracing::info!(?event, "peer input (no backend to inject)"),
    }
}
