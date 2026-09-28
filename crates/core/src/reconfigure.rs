//! Renegotiating a link that stays up.
//!
//! Modules used to be agreed once, in `Hello`, and fixed for the life of the
//! link; changing one meant restarting the node so every link greeted again.
//! Here the same negotiation reruns whenever either side's adverts change,
//! and the difference is applied to the running modules alone: what stopped
//! being agreed is stopped, what became agreed is started, and nothing else —
//! input, the cursor, a file transfer in another module — notices.
//!
//! ## Ordering across the two sides
//!
//! The side that changed sends [`Message::Reconfigure`] *before* it starts
//! anything, at control priority, so the peer hears about a module before the
//! module's first message arrives. The reader can still route that first
//! message before the session loop has read the announcement, which is why
//! the router holds messages for a module that is not running yet (see
//! `module_host::Router`).
//!
//! A module being restarted — stopped and built again on both sides because a
//! setting it read at build time changed — is not started again until its old
//! instance has finished shutting down. Two instances of the files module at
//! once would mount the peer's volumes twice; two of audio would publish every
//! node twice. The stop runs off the session loop, bounded like teardown, and
//! reports back when it is done.

use std::collections::HashSet;

use cursio_protocol::{Edge, Message, ModuleAdvert, ModuleId, Negotiated, Position, Reconfigure};
use tokio::sync::{mpsc, watch};

use crate::live::{LiveSettings, Restarts};
use crate::module::{ModuleRegistry, SessionCtx};
use crate::module_host::{self, LinkTx, ModuleFailure, ModuleHost, Router};
use crate::session::PeerHello;

/// What a running link needs to follow this node's settings.
pub struct LinkLive<'a> {
    pub registry: &'a ModuleRegistry,
    pub local: &'a crate::NodeIdentity,
    /// The peer as it greeted us.
    pub peer: PeerHello,
    /// What this side advertised in its own greeting.
    pub advertised: Vec<ModuleAdvert>,
    /// Where this side said it stood in its greeting.
    pub position: Position,
    pub settings: watch::Receiver<LiveSettings>,
    /// Whether this machine has capture. Input can be offered only with it,
    /// whatever the settings say.
    pub can_send_input: bool,
    /// Told what the link runs after every renegotiation, and where the peer
    /// now stands. `Send` so a session with no settings to follow can still
    /// be spawned onto the multi-threaded runtime.
    pub on_change: Box<dyn FnMut(&LinkState) + Send + 'a>,
}

/// A link's arrangement after a renegotiation.
#[derive(Debug, Clone)]
pub struct LinkState {
    pub negotiated: Negotiated,
    /// Where this machine stands, which a settings change can move too.
    pub position: Position,
    pub peer_position: Position,
    /// Which of this machine's edges borders the peer now.
    pub peer_edge: Option<Edge>,
    /// Whether that edge is different from the last one reported.
    pub edge_changed: bool,
}

/// One link's side of the renegotiation.
pub(crate) struct Reconf<'a> {
    registry: &'a ModuleRegistry,
    local: &'a crate::NodeIdentity,
    peer: PeerHello,
    on_change: Box<dyn FnMut(&LinkState) + Send + 'a>,
    can_send_input: bool,
    /// What the peer last heard this side offer.
    adverts: Vec<ModuleAdvert>,
    position: Position,
    input_on: bool,
    seen: Restarts,
    /// What should be running: agreed, less input, which is not a module.
    desired: Vec<ModuleId>,
    /// Modules whose old instance is still shutting down.
    stopping: HashSet<ModuleId>,
    peer_edge: Option<Edge>,
    stopped_tx: mpsc::UnboundedSender<Vec<ModuleId>>,
}

/// Where a reconfiguration puts modules it starts and stops.
pub(crate) struct Hosts<'h> {
    pub hosts: &'h mut Vec<ModuleHost>,
    pub router: &'h Router,
    pub tx: &'h LinkTx,
    pub failures: &'h mpsc::UnboundedSender<ModuleFailure>,
}

impl<'a> Reconf<'a> {
    /// Returns the state, the settings to watch, and where finished stops
    /// are reported.
    pub(crate) fn new(
        live: LinkLive<'a>,
    ) -> (
        Self,
        watch::Receiver<LiveSettings>,
        mpsc::UnboundedReceiver<Vec<ModuleId>>,
    ) {
        let LinkLive {
            registry,
            local,
            peer,
            advertised,
            position,
            mut settings,
            can_send_input,
            on_change,
        } = live;
        // Whatever moved between the greeting and now is compared against
        // what the greeting said, below; only restarts are taken as seen,
        // because every module on this link was just built.
        let seen = settings.borrow_and_update().restarts.clone();
        settings.mark_changed();
        let desired = agreed_modules(&cursio_protocol::negotiate(&advertised, &peer.modules));
        let (stopped_tx, stopped_rx) = mpsc::unbounded_channel();
        let reconf = Self {
            registry,
            local,
            input_on: advertised.iter().any(|a| a.id == ModuleId::INPUT),
            adverts: advertised,
            peer_edge: position.edge_toward(peer.position),
            position,
            peer,
            on_change,
            can_send_input,
            seen,
            desired,
            stopping: HashSet::new(),
            stopped_tx,
        };
        (reconf, settings, stopped_rx)
    }

    /// Whether this side is offering its input on the link.
    pub(crate) fn input_on(&self) -> bool {
        self.input_on
    }

    /// Whether the user removed this link's peer from the mesh.
    pub(crate) fn peer_forgotten(&self, settings: &LiveSettings) -> bool {
        settings.forgotten.contains(&self.peer.peer_id)
    }

    /// This node's settings moved. Answers the message to send the peer —
    /// before anything starts — and the modules to restart, or `None` when
    /// nothing the peer can see changed.
    pub(crate) fn local(&mut self, settings: &LiveSettings) -> Option<(Message, Vec<ModuleId>)> {
        let input_on = self.can_send_input && settings.input;
        let adverts = crate::link::advertise(self.registry, input_on);
        let mut restart = settings.restarts.since(&mut self.seen);
        let edge = settings.position.edge_toward(self.peer.position);
        if edge != self.peer_edge && self.desired.contains(&ModuleId::WINDOW) {
            // The drag trigger compares the pointer against the edge it was
            // built with.
            restart.push(ModuleId::WINDOW);
        }
        restart.retain(|id| self.desired.contains(id));
        restart.dedup();
        if adverts == self.adverts && settings.position == self.position && restart.is_empty() {
            self.input_on = input_on;
            return None;
        }
        self.adverts = adverts;
        self.position = settings.position;
        self.input_on = input_on;
        let msg = Message::Reconfigure(Reconfigure {
            modules: self.adverts.clone(),
            position: self.position,
            restart: restart.clone(),
        });
        Some((msg, restart))
    }

    /// The peer's settings moved. Answers the modules it asked to restart.
    pub(crate) fn peer(&mut self, change: Reconfigure) -> Vec<ModuleId> {
        self.peer.modules = change.modules;
        self.peer.position = change.position;
        change.restart
    }

    /// Bring the running modules in line with what both sides now offer.
    ///
    /// `peer_started` says whose change this is: when the peer's, what it sent
    /// ahead of its announcement belongs to the modules started here; when
    /// ours, the peer has sent nothing for them yet.
    pub(crate) fn reconcile(&mut self, restart: &[ModuleId], peer_started: bool, to: Hosts<'_>) {
        let negotiated = cursio_protocol::negotiate(&self.adverts, &self.peer.modules);
        self.desired = agreed_modules(&negotiated);
        let edge = self.position.edge_toward(self.peer.position);
        let edge_changed = edge != self.peer_edge;
        self.peer_edge = edge;
        (self.on_change)(&LinkState {
            negotiated,
            position: self.position,
            peer_position: self.peer.position,
            peer_edge: edge,
            edge_changed,
        });

        let stop: Vec<ModuleId> = to
            .hosts
            .iter()
            .map(|h| h.id())
            .filter(|id| !self.desired.contains(id) || restart.contains(id))
            .collect();
        if !stop.is_empty() {
            let mut batch = Vec::new();
            let mut kept = Vec::new();
            for host in to.hosts.drain(..) {
                if stop.contains(&host.id()) {
                    module_host::detach(to.router, host.id());
                    batch.push(host);
                } else {
                    kept.push(host);
                }
            }
            *to.hosts = kept;
            for id in &stop {
                tracing::info!(module = %id, peer = %self.peer.display_name, "stopping module on a running link");
            }
            self.stopping.extend(stop.iter().copied());
            let done = self.stopped_tx.clone();
            tokio::spawn(async move {
                module_host::shut_down(batch, module_host::MODULE_SHUTDOWN_GRACE).await;
                let _ = done.send(stop);
            });
        }
        self.start_missing(peer_started, to);
    }

    /// Old instances of `ids` have finished shutting down; start whichever
    /// are still wanted.
    pub(crate) fn stopped(&mut self, ids: Vec<ModuleId>, to: Hosts<'_>) {
        for id in ids {
            self.stopping.remove(&id);
        }
        // A restart runs on both sides at once, so the peer's new instance
        // may already have spoken.
        self.start_missing(true, to);
    }

    fn start_missing(&mut self, keep_parked: bool, to: Hosts<'_>) {
        let running: Vec<ModuleId> = to.hosts.iter().map(|h| h.id()).collect();
        let start: Vec<ModuleId> = self
            .desired
            .iter()
            .copied()
            .filter(|id| !running.contains(id) && !self.stopping.contains(id))
            .collect();
        if start.is_empty() {
            return;
        }
        let ctx = SessionCtx {
            local: self.local,
            peer: &self.peer,
            peer_edge: self.peer_edge,
        };
        for module in self.registry.build(&start, &ctx) {
            let name = module.name();
            match ModuleHost::spawn(module, to.tx.fork(), to.failures.clone()) {
                Ok(host) => {
                    tracing::info!(module = name, peer = %self.peer.display_name, "started module on a running link");
                    host.attach(to.router, keep_parked);
                    to.hosts.push(host);
                }
                Err(e) => {
                    tracing::error!(module = name, error = %e, "could not start the module's thread; continuing without it");
                }
            }
        }
    }
}

/// The agreed modules that run on a host of their own: all but input, which
/// the session loop carries itself.
fn agreed_modules(negotiated: &Negotiated) -> Vec<ModuleId> {
    negotiated
        .agreed
        .iter()
        .copied()
        .filter(|id| *id != ModuleId::INPUT)
        .collect()
}
