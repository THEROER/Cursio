//! Drive Cursio's ScreenCast portal backend through a real
//! `xdg-desktop-portal`, on a private session bus, with a fake peer.
//!
//! What it proves: that the portal accepts this backend, routes a capture
//! app's `CreateSession`/`SelectSources`/`Start` to it, takes the peer node it
//! answers with, and lets the app open a PipeWire remote that reaches it —
//! the chain a capture app on GNOME walks. The chooser is this executable
//! too, answering "the fake peer's screen" without a dialog, so the probe
//! needs no hand on the mouse; GNOME's own backend is absent from a private
//! bus, so only the peer path is exercised.
//!
//! ```sh
//! cargo build -p cursio-window --features linux-probes --example portal_probe
//! P=target/debug/examples/portal_probe; D=$(mktemp -d)
//! printf '[portal]\nDBusName=org.freedesktop.impl.portal.desktop.cursio\nInterfaces=org.freedesktop.impl.portal.ScreenCast;\n' > $D/cursio.portal
//! printf '[preferred]\ndefault=cursio\n' > $D/portals.conf
//! dbus-run-session -- sh -c "$P serve & sleep 1; XDG_DESKTOP_PORTAL_DIR=$D /usr/libexec/xdg-desktop-portal & sleep 1; $P client"
//! ```

use std::sync::Arc;

use cursio_protocol::{
    Rect, ScreenSource, SourceId, SourceKind, TileCodec, WindowFrame, WindowMeta,
};
use cursio_window::linux_portal::{PeerScreens, RemoteScreens, ShownNode};
use cursio_window::{PipeWireScreenSource, Surrogate};

struct FakePeer;

impl RemoteScreens for FakePeer {
    fn peers(&self) -> Vec<PeerScreens> {
        vec![PeerScreens {
            peer: "fake".into(),
            name: "Fakebox".into(),
            sources: vec![ScreenSource {
                id: 1,
                kind: SourceKind::Monitor,
                title: "Screen 1".into(),
                width: 640,
                height: 360,
                picked_on_host: false,
            }],
        }]
    }

    fn show(
        &self,
        _peer: &str,
        _source: SourceId,
    ) -> tokio::sync::oneshot::Receiver<Result<ShownNode, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (_ended_tx, ended) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let (w, h) = (640u32, 360u32);
            let mut node = match PipeWireScreenSource::open(&WindowMeta {
                id: cursio_protocol::SCREEN_STREAM_ID,
                title: "Cursio — Fakebox · Screen 1".into(),
                width: w,
                height: h,
            }) {
                Ok(node) => node,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = tx.send(Ok(ShownNode {
                node_id: node.node_id(),
                width: w,
                height: h,
                kind: SourceKind::Monitor,
                token: 1,
                ended,
            }));
            for seq in 1..600u32 {
                let mut pixels = vec![0u8; (w * h * 4) as usize];
                for px in pixels.as_chunks_mut::<4>().0 {
                    *px = [0x00, 0xA0, 0x00, 0xFF];
                }
                let frame = WindowFrame {
                    id: cursio_protocol::SCREEN_STREAM_ID,
                    seq,
                    dirty: Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    },
                    codec: TileCodec::RawBgra8888,
                    data: pixels,
                };
                if node.apply_frame(&frame).is_err() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(33));
            }
            drop(_ended_tx);
        });
        rx
    }

    fn stop(&self, peer: &str, token: u64) {
        println!("serve: capture app stopped {peer}/{token}");
    }
}

async fn client() -> Result<(), String> {
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    let screencast = Screencast::new().await.map_err(|e| e.to_string())?;
    let session = screencast
        .create_session(Default::default())
        .await
        .map_err(|e| format!("create session: {e}"))?;
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor)),
        )
        .await
        .map_err(|e| format!("select sources: {e}"))?;
    let response = screencast
        .start(&session, None, Default::default())
        .await
        .map_err(|e| format!("start: {e}"))?
        .response()
        .map_err(|e| format!("start response: {e}"))?;
    let stream = response.streams().first().ok_or("no stream")?.clone();
    println!(
        "client: portal answered node {} size {:?}",
        stream.pipe_wire_node_id(),
        stream.size()
    );
    let fd = screencast
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| format!("open remote: {e}"))?;
    let out = std::env::temp_dir().join("cursio-portal-probe.png");
    let status = std::process::Command::new("gst-launch-1.0")
        .args([
            "-q",
            "pipewiresrc",
            "fd=0",
            &format!("path={}", stream.pipe_wire_node_id()),
            "num-buffers=10",
            "!",
            "videoconvert",
            "!",
            "pngenc",
            "snapshot=true",
            "!",
            "filesink",
            &format!("location={}", out.display()),
        ])
        .stdin(std::process::Stdio::from(fd))
        .status()
        .map_err(|e| format!("gst-launch: {e}"))?;
    println!(
        "client: consumer exited {status}, picture at {}",
        out.display()
    );
    Ok(())
}

#[tokio::main]
async fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == cursio_window::linux_chooser::CHOOSER_ARG {
        // Line 0 is "this computer"; line 1 is the fake peer's screen.
        println!("1");
        return;
    }
    tracing_subscriber::fmt().init();
    match mode.as_str() {
        "serve" => {
            let screens: Arc<dyn RemoteScreens> = Arc::new(FakePeer);
            if let Err(e) = cursio_window::linux_portal::serve(Some(screens)).await {
                eprintln!("serve: {e}");
            }
        }
        "client" => {
            if let Err(e) = client().await {
                eprintln!("client: {e}");
                std::process::exit(1);
            }
        }
        _ => eprintln!("usage: portal_probe serve|client"),
    }
}
