//! Reproduce the "input events don't arrive at the receiver" issue in a
//! controlled loopback setting: run a real encrypted session over TCP, have one
//! side stream many input events, and assert the other side receives them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_core::session::{self, SessionError};
use cursio_protocol::InputEvent;
use cursio_transport::pairing::Role;
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use cursio_transport::{EncryptedTransport, Transport};

/// Bring up a paired-key encrypted channel over TCP loopback and return both
/// ends (initiator, responder).
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
    let server = server.await.unwrap();
    (client, server)
}

/// Stream N motion events from the initiator via `run_session` while the
/// responder runs `run_session` and records what it injects. All N must arrive.
#[tokio::test]
async fn streamed_input_all_arrives_over_encrypted_tcp() {
    const N: usize = 2000;

    let (client, server) = encrypted_pair().await;

    // Responder: collect injected events until it has all N (or the link ends).
    let received = Arc::new(Mutex::new(Vec::<InputEvent>::new()));
    let received_in_task = received.clone();
    let server_task = tokio::spawn(async move {
        let server: Box<dyn Transport> = Box::new(server);
        // Server starts ACTIVE, so it injects inbound input from the dormant
        // client. No handoff needed.
        let res = session::run_session(
            server,
            session::CursorSetup {
                start_active: true,
                peer_takes_cursor: true,
            },
            None,
            |ev| received_in_task.lock().unwrap().push(ev),
            None,
            |_| {},
            Vec::new(),
            || None,
        )
        .await;
        // The client ends by dropping the link, so PeerLeft or a transport
        // close both count as a clean end here.
        matches!(
            res,
            Ok(()) | Err(SessionError::PeerLeft) | Err(SessionError::Transport(_))
        )
    });

    // Feed N motion events into the client's capture channel.
    let (tx, rx) = session::input_channel();
    for i in 0..N {
        tx.send(session::CapturedInput::from(InputEvent::PointerMotion {
            dx: (i % 7) as i32 + 1,
            dy: 0,
        }))
        .unwrap();
    }
    drop(tx); // capture channel closes after draining

    // The client holds the cursor and hands it over before sending anything,
    // which is the whole sequence a real handoff is: a machine forwards its
    // input only once the user *at it* has sent the cursor across, so a second
    // keyboard stays usable on its own machine rather than silently driving the
    // first. Sending the cursor away is also something only its holder can do —
    // an idle machine's pointer drifting into the edge is not a handoff, and
    // treating it as one left a Mac driving a peer nobody was sitting at.
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    cmd_tx
        .send(session::OwnershipCmd::HandoffToPeer {
            edge: cursio_protocol::Edge::Right,
            pos_norm: 0.5,
            at: None,
            entry: None,
        })
        .expect("the session has not started yet, so the channel is open");
    drop(cmd_tx);

    let client_task = tokio::spawn(async move {
        let client: Box<dyn Transport> = Box::new(client);
        // `run_session` borrows the capture channel now: it outlives a single
        // session in real use, so sessions read from it rather than own it.
        let mut rx = rx;
        let mut cmd_rx = cmd_rx;
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            session::run_session(
                client,
                session::CursorSetup {
                    start_active: true,
                    peer_takes_cursor: true,
                },
                Some(&mut rx),
                |_| {},
                Some(&mut cmd_rx),
                |_| {},
                Vec::new(),
                || None,
            ),
        )
        .await;
        // Dropping `client` here (end of task) closes the connection.
    });

    // Wait for the client to finish sending + the link to close.
    client_task.await.unwrap();
    let ok = tokio::time::timeout(Duration::from_secs(3), server_task)
        .await
        .expect("responder did not finish")
        .unwrap();
    assert!(ok, "session ended with an unexpected error");

    // The session coalesces consecutive PointerMotion, so the receiver may see
    // FEWER events than were sent — but the *total movement* must be identical
    // (relative deltas sum exactly). That's the real correctness property: no
    // motion is lost or duplicated, only merged.
    let events = received.lock().unwrap();
    let expected_sum: i64 = (0..N).map(|i| (i % 7) as i64 + 1).sum();
    let got_sum: i64 = events
        .iter()
        .map(|e| match e {
            InputEvent::PointerMotion { dx, .. } => *dx as i64,
            _ => 0,
        })
        .sum();
    assert_eq!(
        got_sum, expected_sum,
        "total motion mismatch: got {got_sum}, expected {expected_sum}"
    );
    assert!(
        events.len() <= N,
        "coalescing should not increase event count ({} > {N})",
        events.len()
    );
    // Sanity: coalescing actually did something under this flood.
    assert!(!events.is_empty(), "receiver got no events at all");
}

/// A module whose pump costs 2 ms, counting how often it runs.
struct SlowPump {
    pumps: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl cursio_core::module::SessionModule for SlowPump {
    fn name(&self) -> &'static str {
        "slow"
    }

    fn id(&self) -> cursio_protocol::ModuleId {
        cursio_protocol::ModuleId::FILES
    }

    async fn start(
        &mut self,
        _tx: &mut dyn cursio_transport::TransportTx,
    ) -> Result<(), cursio_transport::TransportError> {
        Ok(())
    }

    async fn pump(
        &mut self,
        _tx: &mut dyn cursio_transport::TransportTx,
    ) -> Result<(), cursio_transport::TransportError> {
        self.pumps
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(2));
        Ok(())
    }

    fn handle_message(&mut self, _msg: &cursio_protocol::Message) -> bool {
        false
    }
}

/// A receiver injects a flood of separate pointer messages without pumping its
/// modules once per message. 2000 messages behind a 2 ms pump each would take
/// four seconds to drain — the cursor replaying a movement long after the hand
/// stopped — where the pace alone pumps a few hundred times.
#[tokio::test]
async fn a_slow_module_does_not_hold_up_injecting_received_input() {
    const N: usize = 2000;
    let (mut client, server) = encrypted_pair().await;

    let pumps = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let received = Arc::new(Mutex::new(0usize));
    let received_in_task = received.clone();
    let modules: Vec<Box<dyn cursio_core::module::SessionModule>> = vec![Box::new(SlowPump {
        pumps: pumps.clone(),
    })];
    let server_task = tokio::spawn(async move {
        let server: Box<dyn Transport> = Box::new(server);
        let _ = session::run_session(
            server,
            session::CursorSetup {
                start_active: true,
                peer_takes_cursor: true,
            },
            None,
            // Motion that queued arrives merged, so what is counted is the
            // distance moved, not the calls.
            |ev| {
                if let InputEvent::PointerMotion { dx, .. } = ev {
                    *received_in_task.lock().unwrap() += dx as usize;
                }
            },
            None,
            |_| {},
            modules,
            || None,
        )
        .await;
    });

    let started = std::time::Instant::now();
    for _ in 0..N {
        client
            .send(&cursio_protocol::Message::Input(
                InputEvent::PointerMotion { dx: 1, dy: 0 },
            ))
            .await
            .unwrap();
    }
    while *received.lock().unwrap() < N {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "only {} of {N} injected after 2 s",
            *received.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        pumps.load(std::sync::atomic::Ordering::Relaxed) < N / 2,
        "modules were pumped once per input message"
    );
    drop(client);
    server_task.abort();
}

/// An injector as slow as `SendInput` on the Windows laptop (1.5 ms per real
/// pointer move) keeps up with a 1000 Hz mouse. Injected one message at a
/// time the queue grew for as long as the hand moved, and the cursor went on
/// replaying the backlog for half a second after it stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_slow_injector_does_not_let_received_motion_queue_behind_it() {
    const N: usize = 1000;
    const INJECT_COST: Duration = Duration::from_micros(1500);
    let (mut client, server) = encrypted_pair().await;

    // (distance moved so far, when) after every call into the injector.
    let landed = Arc::new(Mutex::new(Vec::<(usize, std::time::Instant)>::new()));
    let landed_in_task = landed.clone();
    let server_task = tokio::spawn(async move {
        let mut moved = 0usize;
        let _ = session::run_session(
            Box::new(server),
            session::CursorSetup {
                start_active: true,
                peer_takes_cursor: true,
            },
            None,
            move |ev| {
                let started = std::time::Instant::now();
                while started.elapsed() < INJECT_COST {
                    std::hint::spin_loop();
                }
                if let InputEvent::PointerMotion { dx, .. } = ev {
                    moved += dx as usize;
                }
                landed_in_task
                    .lock()
                    .unwrap()
                    .push((moved, std::time::Instant::now()));
            },
            None,
            |_| {},
            Vec::new(),
            || None,
        )
        .await;
    });

    let mut sent_at = Vec::with_capacity(N);
    let mut pace = tokio::time::interval(Duration::from_millis(1));
    for _ in 0..N {
        pace.tick().await;
        sent_at.push(std::time::Instant::now());
        client
            .send(&cursio_protocol::Message::Input(
                InputEvent::PointerMotion { dx: 1, dy: 0 },
            ))
            .await
            .unwrap();
    }
    let hand_stopped = std::time::Instant::now();
    while landed.lock().unwrap().last().map_or(0, |(moved, _)| *moved) < N {
        assert!(
            hand_stopped.elapsed() < Duration::from_secs(5),
            "motion never finished arriving"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // Each unit of motion, from being sent to the injector call that carried it.
    let landed = landed.lock().unwrap();
    let mut calls = landed.iter();
    let mut call = calls.next().unwrap();
    let mut lag: Vec<Duration> = sent_at
        .iter()
        .enumerate()
        .map(|(k, sent)| {
            while call.0 < k + 1 {
                call = calls.next().unwrap();
            }
            call.1 - *sent
        })
        .collect();
    lag.sort_unstable();
    let (p50, p99) = (lag[N / 2], lag[N * 99 / 100]);
    assert!(
        p99 < Duration::from_millis(20),
        "motion waited behind the injector: p50 {p50:?}, p99 {p99:?}"
    );
    assert_eq!(landed.last().unwrap().0, N, "motion was lost or duplicated");
    drop(client);
    server_task.abort();
}
