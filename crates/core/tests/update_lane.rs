//! A build crosses between two machines that no longer speak the same
//! protocol.
//!
//! The case the update lane exists for: the machine that is behind cannot
//! hold a session with the one that has the new build, and still has to get
//! it. These greet over real sockets with different protocol versions, see
//! each other through the frozen preamble, fall back to the lane over an
//! encrypted channel, and hand the build across.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cursio_core::config::{TrustedBuilder, UpdateChannel, UpdateConfig, UpdatePolicy};
use cursio_core::session::{self, SessionError};
use cursio_core::update::agent::Agent;
use cursio_core::update::lane;
use cursio_core::update::layout::InstallLayout;
use cursio_core::update::manifest::Manifest;
use cursio_core::update::sign::{hex, BuilderKey};
use cursio_core::NodeIdentity;
use cursio_protocol::update::Channel;
use cursio_protocol::{DeviceKind, NodeStatus, PeerId, Position, ServiceOffer, PROTOCOL_VERSION};
use cursio_transport::pairing::Role;
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use cursio_transport::EncryptedTransport;

const PLATFORM: &str = "linux-x86_64";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cursio-lane-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn agent(root: &Path, trusts: Option<&BuilderKey>) -> Arc<Agent> {
    let trusted: Vec<TrustedBuilder> = trusts
        .map(|k| TrustedBuilder {
            peer: PeerId::nil(),
            name: "nocturne".into(),
            key: hex(&k.public()),
        })
        .into_iter()
        .collect();
    Arc::new(Agent::new(
        InstallLayout::at(root),
        PLATFORM.into(),
        move || UpdateConfig {
            policy: UpdatePolicy::Ask,
            channel: UpdateChannel::Dev,
            trusted_builders: trusted.clone(),
        },
    ))
}

fn identity(name: &str) -> NodeIdentity {
    NodeIdentity {
        peer_id: PeerId::new_v4(),
        display_name: name.into(),
    }
}

async fn greet(
    transport: &mut TcpTransport,
    who: &NodeIdentity,
    version: u16,
) -> Result<session::PeerHello, SessionError> {
    session::handshake_as(
        version,
        transport,
        who,
        DeviceKind::Desktop,
        &[],
        Position::new(0, 0),
        ServiceOffer::none(),
        NodeStatus::unknown(),
        &[],
    )
    .await
}

#[tokio::test]
async fn a_newer_build_reaches_a_machine_that_speaks_an_older_protocol() {
    let root = scratch("skew");
    let key = BuilderKey::from_seed([3; 32]);

    let stage = root.join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("cursio"), vec![5u8; 700_000]).unwrap();
    Manifest::describe(&stage, "0.9.0+later", &["cursio"])
        .unwrap()
        .write(&stage)
        .unwrap();
    let newer = agent(&root.join("newer"), None);
    newer
        .store()
        .publish(&key, Channel::Dev, &[(PLATFORM.into(), stage)], 50)
        .unwrap();
    let older = agent(&root.join("older"), Some(&key));

    let listener = TcpListenerHandle::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr();
    let shared = b"stored-pair-key".to_vec();

    let server_agent = older.clone();
    let server_key = shared.clone();
    let server = tokio::spawn(async move {
        let mut t = listener.accept().await.unwrap();
        let err = greet(&mut t, &identity("lightmoon"), PROTOCOL_VERSION)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SessionError::UpdateOnly(ref p) if p.protocol_version == PROTOCOL_VERSION + 1)
        );
        let secure = EncryptedTransport::establish(t, &server_key, Role::Responder)
            .await
            .unwrap();
        lane::run_alone_for(
            secure,
            server_agent,
            "nocturne".into(),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    });

    let mut t = TcpTransport::connect(addr).await.unwrap();
    let err = greet(&mut t, &identity("nocturne"), PROTOCOL_VERSION + 1)
        .await
        .unwrap_err();
    assert!(
        matches!(err, SessionError::UpdateOnly(ref p) if p.protocol_version == PROTOCOL_VERSION)
    );
    let secure = EncryptedTransport::establish(t, &shared, Role::Initiator)
        .await
        .unwrap();
    lane::run_alone_for(secure, newer, "lightmoon".into(), Duration::from_secs(2))
        .await
        .unwrap();
    server.await.unwrap();

    assert!(older
        .layout()
        .installed()
        .contains(&"0.9.0+later".to_owned()));
    assert_eq!(older.status().ready.unwrap().version, "0.9.0+later");
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn equal_versions_greet_as_before() {
    let listener = TcpListenerHandle::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr();
    let server = tokio::spawn(async move {
        let mut t = listener.accept().await.unwrap();
        greet(&mut t, &identity("a"), PROTOCOL_VERSION)
            .await
            .unwrap()
    });
    let mut t = TcpTransport::connect(addr).await.unwrap();
    let peer = greet(&mut t, &identity("b"), PROTOCOL_VERSION)
        .await
        .unwrap();
    assert_eq!(peer.display_name, "a");
    assert_eq!(server.await.unwrap().display_name, "b");
}
