//! A settings change reaches a running link without ending it.
//!
//! Switching a module off used to restart the node, dropping every link to
//! apply one toggle. These run two real encrypted sessions, change one side's
//! settings the way the app does, and check that both sides start and stop
//! exactly that module while the session carries on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_core::config::Config;
use cursio_core::live::{LiveConfig, Plan};
use cursio_core::module::{ModuleFactory, ModuleRegistry, SessionCtx, SessionModule};
use cursio_core::reconfigure::{LinkLive, LinkState};
use cursio_core::session::{self, CursorSetup, PeerHello};
use cursio_core::NodeIdentity;
use cursio_protocol::{
    DeviceKind, Message, ModuleAdvert, ModuleId, NodeStatus, Position, ServiceOffer,
};
use cursio_transport::pairing::Role;
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use cursio_transport::{EncryptedTransport, TransportError, TransportTx};

async fn encrypted_pair() -> (
    EncryptedTransport<TcpTransport>,
    EncryptedTransport<TcpTransport>,
) {
    let listener = TcpListenerHandle::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr();
    let key = b"shared-key-for-test".to_vec();
    let server = tokio::spawn(async move {
        let t = listener.accept().await.unwrap();
        EncryptedTransport::establish(t, &key, Role::Responder)
            .await
            .unwrap()
    });
    let client_t = TcpTransport::connect(addr).await.unwrap();
    let client = EncryptedTransport::establish(client_t, b"shared-key-for-test", Role::Initiator)
        .await
        .unwrap();
    (client, server.await.unwrap())
}

/// How many instances of the files module exist on one side, and how many
/// were ever built there.
#[derive(Clone, Default)]
struct Census {
    alive: Arc<AtomicUsize>,
    built: Arc<AtomicUsize>,
    /// Greetings this side's instances heard from the peer's.
    heard: Arc<AtomicUsize>,
}

/// A files module that greets its peer on start and counts what it hears.
struct Counted {
    census: Census,
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.census.alive.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl SessionModule for Counted {
    fn name(&self) -> &'static str {
        "counted"
    }

    fn id(&self) -> ModuleId {
        ModuleId::FILES
    }

    async fn start(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        tx.send(&Message::module(ModuleId::FILES, cursio_protocol::Priority::Bulk, &1u8).unwrap())
            .await
    }

    async fn pump(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        if let Ok(Some(1u8)) = msg.decode_module::<u8>(ModuleId::FILES) {
            self.census.heard.fetch_add(1, Ordering::SeqCst);
        }
        true
    }
}

struct CountedFactory {
    census: Census,
}

impl ModuleFactory for CountedFactory {
    fn id(&self) -> ModuleId {
        ModuleId::FILES
    }

    fn advert(&self) -> Option<ModuleAdvert> {
        Some(ModuleAdvert::symmetric(ModuleId::FILES, 1))
    }

    fn build(&self, _ctx: &SessionCtx) -> anyhow::Result<Box<dyn SessionModule>> {
        self.census.alive.fetch_add(1, Ordering::SeqCst);
        self.census.built.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Counted {
            census: self.census.clone(),
        }))
    }
}

/// One side of the pair: its settings, what it counts, and what its link
/// reported after each renegotiation.
struct Side {
    live: LiveConfig,
    census: Census,
    states: Arc<Mutex<Vec<LinkState>>>,
    /// Every ownership this side was put in, latest last.
    active: Arc<Mutex<Vec<bool>>>,
}

fn config_at(position: Position, input: bool) -> Config {
    let defaults = Config::default();
    Config {
        position,
        modules: cursio_core::config::Modules {
            input,
            ..defaults.modules
        },
        ..defaults
    }
}

fn greeting(id: &NodeIdentity, position: Position) -> PeerHello {
    PeerHello {
        peer_id: id.peer_id,
        display_name: id.display_name.clone(),
        device_kind: DeviceKind::Desktop,
        modules: vec![ModuleAdvert::symmetric(ModuleId::FILES, 1)],
        position,
        service: ServiceOffer::none(),
        status: NodeStatus::unknown(),
    }
}

/// Start one side's session on `transport`, greeted by `peer`. With `input`,
/// the side has capture and shares it.
fn run_side(
    transport: EncryptedTransport<TcpTransport>,
    me: NodeIdentity,
    peer: PeerHello,
    position: Position,
    start_active: bool,
    input: bool,
) -> (
    Side,
    tokio::task::JoinHandle<Result<(), session::SessionError>>,
) {
    let live = LiveConfig::new(&config_at(position, input));
    let census = Census::default();
    let states = Arc::new(Mutex::new(Vec::new()));
    let active = Arc::new(Mutex::new(Vec::new()));
    let side = Side {
        live: live.clone(),
        census: census.clone(),
        states: states.clone(),
        active: active.clone(),
    };
    let task = tokio::spawn(async move {
        let registry = ModuleRegistry::new()
            .with_switches(live.switches())
            .with(Box::new(CountedFactory { census }));
        let (_capture, mut captured) = tokio::sync::mpsc::unbounded_channel();
        let advertised = cursio_core::link::advertise(&registry, input);
        let negotiated = peer.negotiate(&advertised);
        let ctx = SessionCtx {
            local: &me,
            peer: &peer,
            peer_edge: position.edge_toward(peer.position),
        };
        let built = registry.build_agreed(&negotiated, &ctx);
        let link = LinkLive {
            registry: &registry,
            local: &me,
            peer: peer.clone(),
            advertised,
            position,
            settings: live.subscribe(),
            can_send_input: input,
            on_change: Box::new(move |state| states.lock().unwrap().push(state.clone())),
        };
        session::run_session_live(
            Box::new(transport),
            CursorSetup {
                start_active,
                peer_takes_cursor: true,
            },
            input.then_some(&mut captured),
            |_| {},
            None,
            move |ownership| active.lock().unwrap().push(ownership.active),
            built,
            || None,
            Some(link),
        )
        .await
    });
    (side, task)
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn pair_ids() -> (NodeIdentity, NodeIdentity) {
    (
        NodeIdentity {
            peer_id: cursio_protocol::PeerId::new_v4(),
            display_name: "desk".into(),
        },
        NodeIdentity {
            peer_id: cursio_protocol::PeerId::new_v4(),
            display_name: "laptop".into(),
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_a_module_off_and_on_stops_and_starts_it_on_both_sides_without_ending_the_link() {
    let (a_t, b_t) = encrypted_pair().await;
    let (a_id, b_id) = pair_ids();
    let (a_pos, b_pos) = (Position::new(0, 0), Position::new(1, 0));
    let (a, a_task) = run_side(
        a_t,
        a_id.clone(),
        greeting(&b_id, b_pos),
        a_pos,
        true,
        false,
    );
    let (b, b_task) = run_side(b_t, b_id, greeting(&a_id, a_pos), b_pos, false, false);

    eventually("both sides to greet each other's files module", || {
        a.census.heard.load(Ordering::SeqCst) == 1 && b.census.heard.load(Ordering::SeqCst) == 1
    })
    .await;

    a.live.apply(&Plan {
        switches: vec![(ModuleId::FILES, false)],
        ..Plan::default()
    });
    eventually("files to stop on both sides", || {
        a.census.alive.load(Ordering::SeqCst) == 0 && b.census.alive.load(Ordering::SeqCst) == 0
    })
    .await;
    assert!(
        !a_task.is_finished() && !b_task.is_finished(),
        "the link ended"
    );

    a.live.apply(&Plan {
        switches: vec![(ModuleId::FILES, true)],
        ..Plan::default()
    });
    eventually("files to start again on both sides", || {
        a.census.alive.load(Ordering::SeqCst) == 1 && b.census.alive.load(Ordering::SeqCst) == 1
    })
    .await;
    // The new instances greet each other too: the side that did not change
    // anything received its peer's greeting even though it may have arrived
    // before this side read the reconfiguration that announced the module.
    eventually("the restarted modules to greet each other", || {
        a.census.heard.load(Ordering::SeqCst) == 2 && b.census.heard.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(a.census.built.load(Ordering::SeqCst), 2);
    assert_eq!(b.census.built.load(Ordering::SeqCst), 2);
    assert!(
        !a_task.is_finished() && !b_task.is_finished(),
        "the link ended"
    );

    a_task.abort();
    b_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_request_rebuilds_the_module_on_both_sides_one_instance_at_a_time() {
    let (a_t, b_t) = encrypted_pair().await;
    let (a_id, b_id) = pair_ids();
    let (a_pos, b_pos) = (Position::new(0, 0), Position::new(1, 0));
    let (a, a_task) = run_side(
        a_t,
        a_id.clone(),
        greeting(&b_id, b_pos),
        a_pos,
        true,
        false,
    );
    let (b, b_task) = run_side(b_t, b_id, greeting(&a_id, a_pos), b_pos, false, false);
    eventually("both files modules up", || {
        a.census.heard.load(Ordering::SeqCst) == 1 && b.census.heard.load(Ordering::SeqCst) == 1
    })
    .await;

    a.live.apply(&Plan {
        restart: vec![ModuleId::FILES],
        ..Plan::default()
    });
    eventually("files rebuilt on both sides", || {
        a.census.built.load(Ordering::SeqCst) == 2 && b.census.built.load(Ordering::SeqCst) == 2
    })
    .await;
    assert!(a.census.alive.load(Ordering::SeqCst) <= 1);
    assert!(b.census.alive.load(Ordering::SeqCst) <= 1);
    eventually("one instance each, talking", || {
        a.census.alive.load(Ordering::SeqCst) == 1
            && b.census.alive.load(Ordering::SeqCst) == 1
            && a.census.heard.load(Ordering::SeqCst) == 2
            && b.census.heard.load(Ordering::SeqCst) == 2
    })
    .await;
    assert!(
        !a_task.is_finished() && !b_task.is_finished(),
        "the link ended"
    );
    a_task.abort();
    b_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_a_machine_tells_the_peer_which_edge_now_borders_it() {
    let (a_t, b_t) = encrypted_pair().await;
    let (a_id, b_id) = pair_ids();
    let (a_pos, b_pos) = (Position::new(0, 0), Position::new(1, 0));
    let (a, a_task) = run_side(
        a_t,
        a_id.clone(),
        greeting(&b_id, b_pos),
        a_pos,
        true,
        false,
    );
    let (b, b_task) = run_side(b_t, b_id, greeting(&a_id, a_pos), b_pos, false, false);
    eventually("both files modules up", || {
        a.census.heard.load(Ordering::SeqCst) == 1 && b.census.heard.load(Ordering::SeqCst) == 1
    })
    .await;

    // A moves from the left of B to the right of it.
    a.live.apply(&Plan {
        position: Some(Position::new(2, 0)),
        ..Plan::default()
    });
    eventually("B to hear where A went", || {
        b.states.lock().unwrap().iter().any(|s| {
            s.peer_position == Position::new(2, 0)
                && s.edge_changed
                && s.peer_edge == Some(cursio_protocol::Edge::Right)
        })
    })
    .await;
    assert!(a
        .states
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.edge_changed && s.peer_edge == Some(cursio_protocol::Edge::Left)));
    assert!(
        !a_task.is_finished() && !b_task.is_finished(),
        "the link ended"
    );
    a_task.abort();
    b_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_input_off_while_driving_the_peer_brings_the_cursor_home() {
    let (a_t, b_t) = encrypted_pair().await;
    let (a_id, b_id) = pair_ids();
    let (a_pos, b_pos) = (Position::new(0, 0), Position::new(1, 0));
    // A starts without the cursor: its input drives B. B shares input too, so
    // it can take the dormant side when A takes the cursor back.
    let (a, a_task) = run_side(
        a_t,
        a_id.clone(),
        greeting(&b_id, b_pos),
        a_pos,
        false,
        true,
    );
    let (b, b_task) = run_side(b_t, b_id, greeting(&a_id, a_pos), b_pos, true, true);
    eventually("both sessions up", || {
        a.census.heard.load(Ordering::SeqCst) == 1 && b.census.heard.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(a.active.lock().unwrap().last(), Some(&false));

    a.live.apply(&Plan {
        input: Some(false),
        ..Plan::default()
    });
    eventually("A to hold the cursor and B to hand it over", || {
        a.active.lock().unwrap().last() == Some(&true)
            && b.active.lock().unwrap().last() == Some(&false)
    })
    .await;
    assert!(
        !a_task.is_finished() && !b_task.is_finished(),
        "the link ended"
    );
    a_task.abort();
    b_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_machine_with_input_off_keeps_its_own_cursor_when_the_peer_takes_it() {
    let (a_t, b_t) = encrypted_pair().await;
    let (a_id, b_id) = pair_ids();
    let (a_pos, b_pos) = (Position::new(0, 0), Position::new(1, 0));
    // A drives B, and B shares nothing: made dormant it would grab its own
    // keyboard and send it nowhere.
    let (a, a_task) = run_side(
        a_t,
        a_id.clone(),
        greeting(&b_id, b_pos),
        a_pos,
        false,
        true,
    );
    let (b, b_task) = run_side(b_t, b_id, greeting(&a_id, a_pos), b_pos, true, false);
    eventually("both sessions up", || {
        a.census.heard.load(Ordering::SeqCst) == 1 && b.census.heard.load(Ordering::SeqCst) == 1
    })
    .await;

    a.live.apply(&Plan {
        input: Some(false),
        ..Plan::default()
    });
    eventually("A to hold the cursor", || {
        a.active.lock().unwrap().last() == Some(&true)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        b.active.lock().unwrap().last(),
        Some(&true),
        "B went dormant with nothing to forward"
    );
    a_task.abort();
    b_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_peer_ends_the_link_cleanly_and_the_peer_hears_goodbye() {
    let (a_t, b_t) = encrypted_pair().await;
    let (a_id, b_id) = pair_ids();
    let (a_pos, b_pos) = (Position::new(0, 0), Position::new(1, 0));
    let (a, a_task) = run_side(
        a_t,
        a_id.clone(),
        greeting(&b_id, b_pos),
        a_pos,
        true,
        false,
    );
    let (b, b_task) = run_side(
        b_t,
        b_id.clone(),
        greeting(&a_id, a_pos),
        b_pos,
        false,
        false,
    );
    eventually("both files modules up", || {
        a.census.heard.load(Ordering::SeqCst) == 1 && b.census.heard.load(Ordering::SeqCst) == 1
    })
    .await;

    a.live.forget(b_id.peer_id);

    let a_ended = tokio::time::timeout(Duration::from_secs(10), a_task)
        .await
        .expect("the removing side kept its link")
        .unwrap();
    assert!(a_ended.is_ok(), "removing ended in an error: {a_ended:?}");
    let b_ended = tokio::time::timeout(Duration::from_secs(10), b_task)
        .await
        .expect("the removed side kept its link")
        .unwrap();
    assert!(
        matches!(b_ended, Err(session::SessionError::PeerLeft)),
        "the peer did not hear goodbye: {b_ended:?}"
    );
    assert_eq!(a.census.alive.load(Ordering::SeqCst), 0);
    assert_eq!(b.census.alive.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_some_other_machine_leaves_this_link_up() {
    let (a_t, b_t) = encrypted_pair().await;
    let (a_id, b_id) = pair_ids();
    let (a_pos, b_pos) = (Position::new(0, 0), Position::new(1, 0));
    let (a, a_task) = run_side(
        a_t,
        a_id.clone(),
        greeting(&b_id, b_pos),
        a_pos,
        true,
        false,
    );
    let (b, b_task) = run_side(b_t, b_id, greeting(&a_id, a_pos), b_pos, false, false);
    eventually("both files modules up", || {
        a.census.heard.load(Ordering::SeqCst) == 1 && b.census.heard.load(Ordering::SeqCst) == 1
    })
    .await;

    a.live.forget(cursio_protocol::PeerId::new_v4());
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        !a_task.is_finished() && !b_task.is_finished(),
        "a link ended for a machine it does not reach"
    );
    assert_eq!(a.census.alive.load(Ordering::SeqCst), 1);
    a_task.abort();
    b_task.abort();
}
