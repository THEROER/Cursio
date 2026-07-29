//! A peer that dies *silently* — power cut, cable pulled, VM paused — never
//! sends a FIN, so the socket stays open and healthy-looking. Without a
//! liveness check the session hangs there until the OS TCP timeout, minutes
//! later, which is what "everything froze when the laptop died" actually was.
//!
//! Here the peer end is held open but deliberately never speaks, so only the
//! session's own timeout can end it.

use std::time::Duration;

use cursio_core::session::{self, SessionError};
use cursio_transport::pairing::Role;
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use cursio_transport::{EncryptedTransport, Transport};

#[tokio::test(start_paused = true)]
async fn silent_peer_times_out_instead_of_hanging() {
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

    // Hold the peer end open for the whole test but never send anything: a
    // machine that lost power, not one that hung up. Dropping it instead would
    // close the socket and prove nothing about the timeout.
    let _silent_peer = server;

    let boxed: Box<dyn Transport> = Box::new(client);
    let res = session::run_session(
        boxed,
        session::CursorSetup { start_active: false, peer_takes_cursor: true },
        None, |_| {}, None, |_| {}, Vec::new()).await;

    // `start_paused` auto-advances time when the runtime idles, so the 15s
    // timeout elapses in virtual time — the test itself is instant.
    match res {
        Err(SessionError::PeerTimeout(silent_for)) => {
            assert!(
                silent_for >= Duration::from_secs(15),
                "should not give up before the timeout, waited {silent_for:?}"
            );
        }
        other => panic!("expected PeerTimeout, got {other:?}"),
    }
}
