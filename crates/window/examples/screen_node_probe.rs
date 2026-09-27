//! Publish a remote-screen PipeWire node fed with a moving test picture, with
//! no peer and no portal involved.
//!
//! Proves the half of the Linux remote-screen path that a unit test cannot:
//! that the node appears, negotiates with a real consumer and delivers the
//! picture. Consume it while it runs, e.g.
//!
//! ```sh
//! cargo run -p cursio-window --features linux-probes --example screen_node_probe -- 30
//! gst-launch-1.0 pipewiresrc path=<node id> num-buffers=30 ! videoconvert ! pngenc snapshot=true ! filesink location=node.png
//! ```

use cursio_protocol::{Rect, TileCodec, WindowFrame, WindowMeta, SCREEN_STREAM_ID};
use cursio_window::{PipeWireScreenSource, Surrogate};

fn main() {
    tracing_subscriber::fmt().init();
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let (w, h) = (640u32, 360u32);
    let mut node = PipeWireScreenSource::open(&WindowMeta {
        id: SCREEN_STREAM_ID,
        title: "Cursio — Probe: pattern".into(),
        width: w,
        height: h,
    })
    .expect("publish the node");
    println!("node id {}", node.node_id());

    let started = std::time::Instant::now();
    let mut seq = 0u32;
    while started.elapsed().as_secs() < seconds {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for px in pixels.as_chunks_mut::<4>().0 {
            *px = [0xFF, 0x00, 0xFF, 0xFF];
        }
        let x0 = (seq * 4) % (w - 40);
        for y in 0..40 {
            let start = ((y * w + x0) * 4) as usize;
            pixels[start..start + 160].fill(0xFF);
        }
        seq += 1;
        node.apply_frame(&WindowFrame {
            id: SCREEN_STREAM_ID,
            seq,
            dirty: Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            codec: TileCodec::RawBgra8888,
            data: pixels,
        })
        .expect("the node took the frame");
        if let Some(ack) = node.take_ack() {
            if seq.is_multiple_of(60) {
                println!("presented up to {}", ack.acked_seq);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(33));
    }
}
