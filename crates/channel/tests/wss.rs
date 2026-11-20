//! A session over the internet's carriers: WebSocket over TLS and TLS over
//! TCP, opened by a carrier-authenticated trust, bound to who it proved, and
//! resumed on a new connection that offers the tag bound to its own TLS.

#![cfg(all(feature = "websocket", feature = "tls"))]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use common::certs::{certs, Certs};
use common::*;
use cursio_channel::authenticated::PeerContext;
use cursio_channel::tls::TlsCarrier;
use cursio_channel::{
    ws, Carrier, CarrierAuthenticated, Opened, Path, PathKind, PeerIdentity, ResumableLink,
    ResumeHandle, Revoked, Role, SessionOptions, Trust, Verified, Verifier,
};
use tokio::net::TcpListener;

const NAME: &str = "panel.test";

/// A user is whoever its token names; the panel is whoever TLS found behind
/// the name the client dialled.
struct Rules;

#[async_trait]
impl Verifier for Rules {
    async fn verify(&self, peer: PeerContext<'_>) -> Result<Verified, Revoked> {
        if let Some(token) = peer.token {
            let sub = token
                .strip_prefix(b"user:")
                .ok_or_else(|| Revoked("not a token of ours".into()))?;
            return Ok(Verified::lasting(PeerIdentity::User {
                sub: String::from_utf8_lossy(sub).into(),
                aud: "panel".into(),
            }));
        }
        match peer.tls {
            Some(tls) if !tls.peer_certificates.is_empty() => {
                Ok(Verified::lasting(PeerIdentity::Service { id: NAME.into() }))
            }
            _ => Err(Revoked("neither a token nor a certificate".into())),
        }
    }

    fn check(&self, _: &Verified, _: SystemTime) -> Result<(), Revoked> {
        Ok(())
    }
}

fn panel_trust() -> CarrierAuthenticated {
    CarrierAuthenticated::new(Arc::new(Rules))
}

fn user_trust() -> CarrierAuthenticated {
    CarrierAuthenticated::new(Arc::new(Rules)).presenting(b"user:ann".to_vec())
}

struct Opening {
    path: Path,
    opened_tag: [u8; 16],
    resume: Option<[u8; 16]>,
    peer: Verified,
}

fn opening(carrier: Box<dyn Carrier>, opened: Opened) -> Opening {
    Opening {
        path: Path::new(carrier, opened.seal, PathKind::INTERACTIVE, false)
            .proving(opened.peer.clone()),
        opened_tag: opened.session_tag,
        resume: opened.resume,
        peer: opened.peer,
    }
}

/// One WSS connection, opened by both trusts: (the user's, the panel's).
async fn wss_pair(
    listener: &TcpListener,
    certs: &Certs,
    user: CarrierAuthenticated,
) -> (Opening, Opening) {
    let addr: SocketAddr = listener.local_addr().unwrap();
    let url = format!("wss://{NAME}:{}/channel", addr.port());
    let dial = async {
        let mut carrier = ws::connect_wss(addr, &url, NAME, certs.client.clone())
            .await
            .unwrap();
        let opened = user.open(&mut carrier, Role::Initiator).await.unwrap();
        opening(Box::new(carrier), opened)
    };
    let answer = async {
        let tcp = listener.accept().await.unwrap().0;
        let mut carrier = ws::accept_wss(tcp, certs.server.clone()).await.unwrap();
        let opened = panel_trust()
            .open(&mut carrier, Role::Responder)
            .await
            .unwrap();
        opening(Box::new(carrier), opened)
    };
    tokio::join!(dial, answer)
}

/// Fill every lane with WSS connections that ask to resume `tag`, the panel
/// placing each by the offer it was named by.
async fn join_every_lane(
    listener: &TcpListener,
    certs: &Certs,
    tag: [u8; 16],
    user: &ResumeHandle,
    panel: &ResumeHandle,
) {
    while !user.network_complete() {
        let (u, p) = wss_pair(listener, certs, user_trust().resuming(tag)).await;
        let offer = p.resume.expect("the user asked to resume");
        assert!(panel.named_by(&offer, &p.path));
        let (ru, rp) = tokio::join!(user.attach(u.path), panel.attach(p.path));
        ru.unwrap();
        rp.unwrap();
    }
}

#[tokio::test]
async fn a_session_over_wss_is_bound_carries_both_lanes_and_resumes_on_new_connections() {
    let certs = certs(NAME);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (u, p) = wss_pair(&listener, &certs, user_trust()).await;
    assert_eq!(u.opened_tag, p.opened_tag, "the panel issued the tag");
    assert_eq!(
        p.peer.identity,
        PeerIdentity::User {
            sub: "ann".into(),
            aud: "panel".into()
        }
    );
    assert_eq!(u.peer.identity, PeerIdentity::Service { id: NAME.into() });
    let (ue, pe) = (u.path.tls.clone().unwrap(), p.path.tls.clone().unwrap());
    assert!(ue.exporter.is_some(), "TLS 1.3 offers the exporter");
    assert_eq!(
        ue.exporter, pe.exporter,
        "both ends derive the same binding"
    );

    let tag = u.opened_tag;
    let bound = |trust: CarrierAuthenticated| SessionOptions::bound_to(Arc::new(trust));
    let (user, user_handle) =
        ResumableLink::with_options(u.path, tag, classes(), bound(user_trust())).unwrap();
    let (panel, panel_handle) =
        ResumableLink::with_options(p.path, tag, classes(), bound(panel_trust())).unwrap();
    join_every_lane(&listener, &certs, tag, &user_handle, &panel_handle).await;
    let ((mut user_tx, mut user_rx), (mut panel_tx, mut panel_rx)) = (user.split(), panel.split());

    let file = Panel::Chunk(1, vec![9; 300_000]);
    user_tx.send(&file).await.unwrap();
    user_tx.send(&line("ls")).await.unwrap();
    let mut got = vec![
        panel_rx.recv().await.unwrap(),
        panel_rx.recv().await.unwrap(),
    ];
    got.sort_by_key(|m| matches!(m, Panel::Chunk(..)));
    assert_eq!(got, [line("ls"), file]);

    user_handle.network_lost();
    panel_handle.network_lost();
    user_tx.send(&line("while down")).await.unwrap();
    join_every_lane(&listener, &certs, tag, &user_handle, &panel_handle).await;
    let resumed = tokio::time::timeout(Duration::from_secs(5), panel_rx.recv()).await;
    assert_eq!(resumed.unwrap().unwrap(), line("while down"));
    panel_tx.send(&line("ok")).await.unwrap();
    assert_eq!(user_rx.recv().await.unwrap(), line("ok"));
}

#[tokio::test]
async fn an_offer_seen_on_one_connection_names_nothing_on_another() {
    let certs = certs(NAME);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (u, p) = wss_pair(&listener, &certs, user_trust()).await;
    let tag = u.opened_tag;
    let (_panel, panel_handle) = ResumableLink::<Panel>::with_options(
        p.path,
        tag,
        classes(),
        SessionOptions::bound_to(Arc::new(panel_trust())),
    )
    .unwrap();
    drop(u);

    let (_, first) = wss_pair(&listener, &certs, user_trust().resuming(tag)).await;
    let (_, second) = wss_pair(&listener, &certs, user_trust().resuming(tag)).await;
    let seen = first.resume.unwrap();
    assert_ne!(seen, tag, "the bare tag never crosses");
    assert!(panel_handle.named_by(&seen, &first.path));
    assert!(!panel_handle.named_by(&seen, &second.path));
}

#[tokio::test]
async fn tls_over_tcp_carries_frames_and_reports_what_it_verified() {
    let certs = certs(NAME);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (client, server) = tokio::join!(
        TlsCarrier::connect(addr, NAME, certs.client.clone()),
        async {
            TlsCarrier::accept(listener.accept().await.unwrap().0, certs.server.clone()).await
        }
    );
    let (mut client, mut server) = (client.unwrap(), server.unwrap());
    assert_eq!(client.kind(), "tls");
    assert_eq!(client.tls().unwrap().peer_certificates.len(), 1);
    assert!(server.tls().unwrap().peer_certificates.is_empty());
    assert_eq!(
        client.tls().unwrap().exporter,
        server.tls().unwrap().exporter
    );
    client.send(vec![1, 2, 3]).await.unwrap();
    assert_eq!(server.recv().await.unwrap(), [1, 2, 3]);

    let wrong = TlsCarrier::connect(addr, "someone-else.test", certs.client.clone());
    let (refused, _) = tokio::join!(wrong, async {
        let _ = TlsCarrier::accept(listener.accept().await.unwrap().0, certs.server.clone()).await;
    });
    assert!(
        refused.is_err(),
        "a certificate for another name fails closed"
    );
}
