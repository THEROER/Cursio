//! Changing settings on a running node.
//!
//! A module toggle or a new grid position used to restart the whole process:
//! every link dropped, the peer's disks unmounted, and on Windows a fresh
//! process meant a fresh UAC prompt and no console. For a click in the app
//! that is the wrong price — the thing that changed is one module on each
//! link, or one edge, and everything else was fine.
//!
//! So a change is applied where it lands:
//!
//! - **Module switches** go into [`ModuleSwitches`], which every link reads
//!   when it advertises. Each running link then sends
//!   [`Message::Reconfigure`](cursio_protocol::Message::Reconfigure) with its
//!   new advert set; both sides renegotiate and start or stop only the modules
//!   whose answer changed.
//! - **Position** travels in the same message, and each side re-derives the
//!   edge between them. The window module reads that edge when it is built, so
//!   an edge that moved restarts it on both sides. **The desk layout** — the
//!   screens, read from the desktop every few seconds, and where the config
//!   stands them — travels the same way, and so does a seam closed.
//! - **A setting a module reads when it is built** (files' share policy,
//!   audio's render device, whether peers may view this screen) restarts that
//!   module on every link, both ends at once, through [`Restarts`].
//! - **Input** stops being forwarded, and the cursor comes home if it was
//!   away. Turning it back on works only if capture exists, which it does
//!   when input was on at startup: capture holds a portal permission that is
//!   deliberately taken once per process.
//!
//! - **A machine removed from the mesh** is added to
//!   [`LiveSettings::forgotten`]; its link, if one is up, says goodbye and
//!   closes, which shuts its modules down and unmounts its disks the way any
//!   ending does. The set only grows until a pairing succeeds with the same
//!   machine again ([`LiveConfig::unforget`]).
//!
//! What cannot change live is named by [`restart_needed`]: the listen port
//! (the listener owns it), the remote-screen portal backend (a D-Bus name
//! claimed at startup), AirPlay (a receiver bound and announced at startup),
//! and input turned on in a process that started without capture. The status
//! and every save reply carry that list, so the app can say "restart to
//! apply" rather than let a toggle silently do nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use cursio_protocol::{Arrange, Layout, ModuleId, PeerId, Position};

use crate::config::Config;
pub use crate::module::ModuleSwitches;

/// A value shared between the node's applier and the factories that read it
/// when they build a module.
#[derive(Debug, Default)]
pub struct Shared<T>(Arc<RwLock<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Clone> Shared<T> {
    pub fn new(value: T) -> Self {
        Self(Arc::new(RwLock::new(value)))
    }

    pub fn get(&self) -> T {
        self.0.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set(&self, value: T) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = value;
    }
}

/// How many times each module has been asked to restart.
///
/// A counter per module rather than a list of pending restarts: every link
/// watches the same value, and a link that wakes after two requests must still
/// see both. It restarts every module whose count moved since it last looked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restarts(BTreeMap<ModuleId, u64>);

impl Restarts {
    pub fn bump(&mut self, id: ModuleId) {
        *self.0.entry(id).or_default() += 1;
    }

    /// The modules asked to restart since `seen`, which is then brought up to
    /// date.
    pub fn since(&self, seen: &mut Restarts) -> Vec<ModuleId> {
        let moved = self
            .0
            .iter()
            .filter(|(id, n)| seen.0.get(id) != Some(n))
            .map(|(id, _)| *id)
            .collect();
        *seen = self.clone();
        moved
    }
}

/// What every running link follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSettings {
    pub position: Position,
    /// This machine's part of the shared desk: its screens as the desktop
    /// last reported them, and where the config stands it.
    pub layout: Layout,
    /// Arrangements the person made of other machines from here, for the
    /// link to each to carry; each link sends those numbered past the last it
    /// sent. Bounded to the latest [`ARRANGE_KEPT`].
    pub arrange_out: Vec<Outgoing>,
    /// Whether the user wants this machine's input shared. Only meaningful on
    /// a link that has capture.
    pub input: bool,
    pub restarts: Restarts,
    /// Machines the user removed from the mesh while this node ran. A link to
    /// one of them ends itself.
    pub forgotten: BTreeSet<PeerId>,
}

/// An arrangement of `peer` made here, waiting for that peer's link to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub seq: u64,
    pub peer: PeerId,
    pub request: Arrange,
}

/// How many outgoing arrangements are kept. A drag sends one at its end, so
/// this is many more than a link that was down for a moment can miss.
pub const ARRANGE_KEPT: usize = 32;

/// The node's live settings: switches for the registry, and a watch for the
/// links.
#[derive(Clone)]
pub struct LiveConfig {
    settings: Arc<tokio::sync::watch::Sender<LiveSettings>>,
    switches: ModuleSwitches,
}

impl LiveConfig {
    /// Start from the config the node was launched with.
    pub fn new(config: &Config) -> Self {
        let switches = ModuleSwitches::default();
        for (id, on) in switchable(config) {
            switches.set(id, on);
        }
        let (settings, _) = tokio::sync::watch::channel(LiveSettings {
            position: config.position,
            layout: Layout {
                screens: Vec::new(),
                place: config.layout.place,
                barriers: config.layout.barriers.clone(),
                offsets: config.layout.offsets.clone(),
            },
            arrange_out: Vec::new(),
            input: config.modules.input,
            restarts: Restarts::default(),
            forgotten: BTreeSet::new(),
        });
        Self {
            settings: Arc::new(settings),
            switches,
        }
    }

    pub fn switches(&self) -> ModuleSwitches {
        self.switches.clone()
    }

    pub fn current(&self) -> LiveSettings {
        self.settings.borrow().clone()
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<LiveSettings> {
        self.settings.subscribe()
    }

    /// End every link to `peer` and keep ending new ones, now that the user
    /// removed it from the mesh.
    pub fn forget(&self, peer: PeerId) {
        self.settings.send_modify(|s| {
            s.forgotten.insert(peer);
        });
    }

    /// `peer` was paired again, so its links may stay up.
    pub fn unforget(&self, peer: PeerId) {
        self.settings
            .send_if_modified(|s| s.forgotten.remove(&peer));
    }

    pub fn is_forgotten(&self, peer: &PeerId) -> bool {
        self.settings.borrow().forgotten.contains(peer)
    }

    /// The desktop's screens as just read. Every link hears of a change —
    /// a monitor plugged in, a resolution changed — and nothing otherwise.
    pub fn set_screens(&self, screens: Vec<cursio_protocol::Screen>) {
        self.settings.send_if_modified(|s| {
            let changed = s.layout.screens != screens;
            if changed {
                tracing::info!(screens = screens.len(), "this machine's screens changed");
                s.layout.screens = screens;
            }
            changed
        });
    }

    /// Ask `peer`, through its link, to change its own part of the desk.
    pub fn request_arrange(&self, peer: PeerId, request: Arrange) {
        self.settings.send_modify(|s| {
            let seq = s.arrange_out.last().map_or(1, |o| o.seq + 1);
            s.arrange_out.push(Outgoing { seq, peer, request });
            let excess = s.arrange_out.len().saturating_sub(ARRANGE_KEPT);
            s.arrange_out.drain(..excess);
        });
    }

    /// Carry out the part of `plan` every node shares. Switches are written
    /// before the links are woken, so a link that renegotiates reads the new
    /// answer; the wake itself is unconditional, because a switch alone
    /// changes nothing in the watched value.
    pub fn apply(&self, plan: &Plan) {
        for &(id, on) in &plan.switches {
            self.switches.set(id, on);
        }
        self.settings.send_modify(|s| {
            if let Some(position) = plan.position {
                s.position = position;
            }
            if let Some(layout) = &plan.layout {
                s.layout.place = layout.place;
                s.layout.barriers = layout.barriers.clone();
                s.layout.offsets = layout.offsets.clone();
            }
            if let Some(input) = plan.input {
                s.input = input;
            }
            for &id in &plan.restart {
                s.restarts.bump(id);
            }
        });
    }
}

/// The modules a config switch turns on and off, by id.
fn switchable(config: &Config) -> [(ModuleId, bool); 4] {
    let m = &config.modules;
    [
        (ModuleId::CLIPBOARD, m.clipboard),
        (ModuleId::AUDIO, m.audio),
        (ModuleId::FILES, m.files),
        (ModuleId::USB, m.usb),
    ]
}

/// How to get from one running config to the next without a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Module switches that changed.
    pub switches: Vec<(ModuleId, bool)>,
    pub position: Option<Position>,
    /// Where this machine stands on the desk, and its closed seams.
    pub layout: Option<crate::config::LayoutConfig>,
    pub input: Option<bool>,
    /// Modules to restart on every link because a setting they read at build
    /// time changed.
    pub restart: Vec<ModuleId>,
    /// New values for those settings, for the node to hand its factories
    /// before the restart happens.
    pub screen_host: Option<bool>,
    pub files_share_changed: bool,
    pub render_device: Option<Option<String>>,
    pub mac_modifiers: Option<String>,
    /// Handed to capture, which narrows its edges at once.
    pub crossing: Option<crate::config::CrossingConfig>,
    /// Read by a running audio module on every pump, so it needs no restart.
    pub mic_gate: Option<Option<String>>,
    /// Read at every call, so it needs no restart either.
    pub calls: Option<crate::config::CallsConfig>,
}

/// What applying `next` over `running` takes.
pub fn plan(running: &Config, next: &Config) -> Plan {
    let mut plan = Plan::default();
    for ((id, was), (_, now)) in switchable(running).into_iter().zip(switchable(next)) {
        if was != now {
            plan.switches.push((id, now));
        }
    }
    if running.position != next.position {
        plan.position = Some(next.position);
    }
    if running.layout != next.layout {
        plan.layout = Some(next.layout.clone());
    }
    if running.modules.input != next.modules.input {
        plan.input = Some(next.modules.input);
    }
    if running.modules.screen_host != next.modules.screen_host {
        plan.screen_host = Some(next.modules.screen_host);
        plan.restart.push(ModuleId::WINDOW);
    }
    if running.files.share != next.files.share {
        plan.files_share_changed = true;
        plan.restart.push(ModuleId::FILES);
    }
    if running.audio.render_device != next.audio.render_device {
        plan.render_device = Some(next.audio.render_device.clone());
        plan.restart.push(ModuleId::AUDIO);
    }
    if running.input.mac_modifiers != next.input.mac_modifiers {
        plan.mac_modifiers = Some(next.input.mac_modifiers.clone());
    }
    if running.input.crossing != next.input.crossing {
        plan.crossing = Some(next.input.crossing.clone());
    }
    if running.audio.mic_gate != next.audio.mic_gate {
        plan.mic_gate = Some(next.audio.mic_gate.clone());
    }
    if running.calls != next.calls {
        plan.calls = Some(next.calls.clone());
    }
    plan
}

/// Settings that differ between the config this process `started` with and
/// the one it runs now, and that only a restart applies. Named as in the file.
///
/// Measured against the start rather than the last change, so turning a
/// setting back takes it off the list.
pub fn restart_needed(started: &Config, current: &Config) -> Vec<String> {
    let mut needed = Vec::new();
    let mut check = |name: &str, differs: bool| {
        if differs {
            needed.push(name.to_owned());
        }
    };
    check("port", started.port != current.port);
    check(
        "modules.remote_screen",
        started.modules.remote_screen != current.modules.remote_screen,
    );
    // Capture is created at startup when input is on, and kept; without it
    // there is nothing to forward from.
    check(
        "modules.input",
        current.modules.input && !started.modules.input,
    );
    check("airplay.mode", started.airplay.mode != current.airplay.mode);
    check(
        "airplay.window",
        started.airplay.window != current.airplay.window,
    );
    check(
        "airplay.share",
        started.airplay.share != current.airplay.share,
    );
    check("airplay.port", started.airplay.port != current.airplay.port);
    check(
        "airplay.interface",
        started.airplay.interface != current.airplay.interface,
    );
    needed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrowing_an_edge_applies_live_without_restarting_anything() {
        let running = Config::default();
        let mut next = running.clone();
        next.input.crossing.bottom = Some(crate::config::EdgeSpan { from: 66, to: 100 });
        let plan = plan(&running, &next);
        assert_eq!(plan.crossing, Some(next.input.crossing.clone()));
        assert!(plan.switches.is_empty() && plan.restart.is_empty());
        assert!(restart_needed(&running, &next).is_empty());
    }

    #[test]
    fn a_module_toggle_is_a_switch_and_nothing_else() {
        let running = Config::default();
        let mut next = running.clone();
        next.modules.files = false;
        let plan = plan(&running, &next);
        assert_eq!(plan.switches, vec![(ModuleId::FILES, false)]);
        assert!(plan.restart.is_empty());
        assert!(restart_needed(&running, &next).is_empty());
    }

    #[test]
    fn a_setting_read_at_build_time_restarts_its_module() {
        let running = Config::default();
        let mut next = running.clone();
        next.modules.screen_host = !running.modules.screen_host;
        next.audio.render_device = Some("Speakers".into());
        let plan = plan(&running, &next);
        assert_eq!(plan.restart, vec![ModuleId::WINDOW, ModuleId::AUDIO]);
        assert_eq!(plan.render_device, Some(Some("Speakers".into())));
    }

    #[test]
    fn the_mic_gate_changes_without_restarting_audio() {
        let running = Config::default();
        let mut next = running.clone();
        next.audio.mic_gate = Some("-55".into());
        let plan = plan(&running, &next);
        assert!(plan.restart.is_empty());
        assert_eq!(plan.mic_gate, Some(Some("-55".into())));
        assert!(restart_needed(&running, &next).is_empty());
    }

    #[test]
    fn call_settings_change_without_restarting_anything() {
        let running = Config::default();
        let mut next = running.clone();
        next.calls.voice = crate::config::CallVoice::Here;
        let plan = plan(&running, &next);
        assert!(plan.restart.is_empty() && plan.switches.is_empty());
        assert_eq!(plan.calls, Some(next.calls.clone()));
        assert!(restart_needed(&running, &next).is_empty());
    }

    #[test]
    fn only_what_cannot_change_live_asks_for_a_restart_and_undoing_it_clears_that() {
        let started = Config::default();
        let mut current = started.clone();
        current.port += 1;
        current.position = Position::new(1, 0);
        assert_eq!(restart_needed(&started, &current), vec!["port".to_owned()]);
        current.port = started.port;
        assert!(restart_needed(&started, &current).is_empty());
    }

    #[test]
    fn input_turned_on_without_capture_needs_a_restart_but_turned_off_does_not() {
        let mut started = Config::default();
        started.modules.input = false;
        let mut current = started.clone();
        current.modules.input = true;
        assert_eq!(
            restart_needed(&started, &current),
            vec!["modules.input".to_owned()]
        );
        assert!(restart_needed(&current, &started).is_empty());
    }

    #[test]
    fn every_link_sees_every_restart_however_late_it_looks() {
        let live = LiveConfig::new(&Config::default());
        let mut seen = live.current().restarts;
        live.apply(&Plan {
            restart: vec![ModuleId::FILES],
            ..Plan::default()
        });
        live.apply(&Plan {
            restart: vec![ModuleId::AUDIO],
            ..Plan::default()
        });
        let mut moved = live.current().restarts.since(&mut seen);
        moved.sort();
        assert_eq!(moved, vec![ModuleId::AUDIO, ModuleId::FILES]);
        assert!(live.current().restarts.since(&mut seen).is_empty());
    }

    #[test]
    fn a_switch_written_by_the_applier_is_what_the_registry_reads() {
        let live = LiveConfig::new(&Config::default());
        let registry = crate::module::ModuleRegistry::new().with_switches(live.switches());
        assert!(registry.switches().is_on(ModuleId::FILES));
        live.apply(&Plan {
            switches: vec![(ModuleId::FILES, false)],
            ..Plan::default()
        });
        assert!(!registry.switches().is_on(ModuleId::FILES));
    }
}
