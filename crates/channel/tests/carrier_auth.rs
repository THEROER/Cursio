//! A carrier-authenticated trust over the carriers that are not verified
//! TLS: refused before a byte of the opening exchange crosses, whatever the
//! verifier would have said.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use cursio_channel::authenticated::PeerContext;
use cursio_channel::tcp::TcpCarrier;
use cursio_channel::{
    Carrier, CarrierAuthenticated, PeerIdentity, Revoked, Role, Trust, Verified, Verifier,
};

/// Would trust anyone at all: the refusal must not depend on it.
struct Anyone;

#[async_trait]
impl Verifier for Anyone {
    async fn verify(&self, _: PeerContext<'_>) -> Result<Verified, Revoked> {
        Ok(Verified::lasting(PeerIdentity::Host { id: "any".into() }))
    }

    fn check(&self, _: &Verified, _: SystemTime) -> Result<(), Revoked> {
        Ok(())
    }
}

fn trust() -> CarrierAuthenticated {
    CarrierAuthenticated::new(Arc::new(Anyone)).presenting(b"a bearer token".to_vec())
}

/// Opens both ends of `pair`, asserting each is refused for want of TLS and
/// that nothing reached the far end first.
async fn assert_refused(mut client: Box<dyn Carrier>, mut server: Box<dyn Carrier>) {
    let (dialler, acceptor) = (trust(), trust());
    let (dialled, accepted) = tokio::join!(
        dialler.open(client.as_mut(), Role::Initiator),
        acceptor.open(server.as_mut(), Role::Responder)
    );
    for refused in [dialled.err(), accepted.err()] {
        let refused = refused.expect("refused");
        assert!(refused.to_string().contains("needs TLS"), "{refused}");
    }
    drop(client);
    let after = server.recv().await;
    assert!(after.is_err(), "the token never crossed: {after:?}");
}

#[tokio::test]
async fn carrier_authenticated_over_plain_tcp_is_refused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (client, accepted) = tokio::join!(TcpCarrier::connect(addr), listener.accept());
    let server = TcpCarrier::from_stream(accepted.unwrap().0).unwrap();
    assert_refused(Box::new(client.unwrap()), Box::new(server)).await;
}

#[cfg(feature = "websocket")]
#[tokio::test]
async fn carrier_authenticated_over_plain_ws_is_refused() {
    use cursio_channel::ws;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/channel");
    let (client, server) = tokio::join!(ws::connect(addr, &url), async {
        ws::accept(listener.accept().await.unwrap().0).await
    });
    let (client, server) = (client.unwrap(), server.unwrap());
    assert_eq!(client.kind(), "ws");
    assert_refused(Box::new(client), Box::new(server)).await;
}
