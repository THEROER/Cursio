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
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for i in 0..N {
        tx.send(InputEvent::PointerMotion {
            dx: (i % 7) as i32 + 1,
            dy: 0,
        })
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
