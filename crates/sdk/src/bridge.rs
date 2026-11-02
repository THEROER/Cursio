//! A DevTools socket the node may reach, in front of one it may not.
//!
//! Android's System WebView opens its DevTools on the abstract socket
//! `@webview_devtools_remote_<pid>` and accepts a connection only from its own
//! app's uid, `shell` or `root` — what `adb forward` arrives as. The Cursio
//! node is another app, so it is turned away however the person answered.
//! Inside the application the WebView's own uid is ours, so the SDK listens
//! on `@cursio-devtools-<pid>`, lets in only the node's uid (from the
//! connection to the node, `SO_PEERCRED`, the kernel's word) and joins each
//! connection to the declared socket. The node in turn checks that this
//! process is what listens on the bridge before it forwards anything.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// The uid of the node this process is connected to, once one answered.
#[derive(Debug)]
pub(crate) struct NodeUid(AtomicU32);

const UNKNOWN: u32 = u32::MAX;

impl Default for NodeUid {
    fn default() -> Self {
        Self(AtomicU32::new(UNKNOWN))
    }
}

impl NodeUid {
    #[cfg(unix)]
    pub(crate) fn learn(&self, node: &tokio::net::UnixStream) {
        if let Ok(credentials) = node.peer_cred() {
            self.0.store(credentials.uid(), Ordering::Relaxed);
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn learn<S>(&self, _node: &S) {}

    #[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
    fn is(&self, uid: u32) -> bool {
        let known = self.0.load(Ordering::Relaxed);
        known != UNKNOWN && known == uid
    }
}

/// The name of the bridge to declare in place of `target`. Without a bridge
/// — another platform, or the name already taken — `target` itself, which
/// the node then tries and says why it cannot.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn open(target: String, node: Arc<NodeUid>) -> String {
    #[cfg(target_os = "android")]
    use std::os::android::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    let name = format!("cursio-devtools-{}", std::process::id());
    let listener = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
        .and_then(|addr| std::os::unix::net::UnixListener::bind_addr(&addr))
        .and_then(|listener| {
            listener.set_nonblocking(true)?;
            tokio::net::UnixListener::from_std(listener)
        });
    let Ok(listener) = listener else {
        return target;
    };
    tokio::spawn(serve(listener, target, node));
    name
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn open(target: String, _node: Arc<NodeUid>) -> String {
    target
}

#[cfg(any(target_os = "linux", target_os = "android"))]
async fn serve(listener: tokio::net::UnixListener, target: String, node: Arc<NodeUid>) {
    loop {
        let Ok((mut client, _)) = listener.accept().await else {
            return;
        };
        let admitted = client
            .peer_cred()
            .is_ok_and(|credentials| node.is(credentials.uid()));
        if !admitted {
            continue;
        }
        let target = target.clone();
        tokio::spawn(async move {
            let Ok(mut upstream) = crate::connect_abstract(&target) else {
                return;
            };
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::linux::net::SocketAddrExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn the_node_reaches_the_socket_behind_the_bridge_and_no_one_else_does() {
        let target = format!("cursio-test-webview-{}", uuid_like());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(target.as_bytes()).unwrap();
        let behind = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
        behind.set_nonblocking(true).unwrap();
        let behind = tokio::net::UnixListener::from_std(behind).unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = behind.accept().await {
                let _ = s.write_all(b"devtools").await;
            }
        });

        let node = Arc::new(NodeUid::default());
        let name = open(target.clone(), node.clone());
        assert_ne!(name, target);

        let mut stranger = crate::connect_abstract(&name).unwrap();
        let mut got = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stranger.read_to_end(&mut got),
        )
        .await;
        assert!(
            got.is_empty(),
            "before the node is known nobody gets through"
        );

        node.0.store(own_uid(), Ordering::Relaxed);
        let mut known = crate::connect_abstract(&name).unwrap();
        let mut got = [0u8; 8];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            known.read_exact(&mut got),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&got, b"devtools");
    }

    /// This process's uid, as the kernel reports it on a socket pair.
    fn own_uid() -> u32 {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let a = tokio::net::UnixStream::from_std(a).unwrap();
        a.peer_cred().unwrap().uid()
    }

    fn uuid_like() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
}
