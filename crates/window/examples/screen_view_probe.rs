//! Open a remote-screen view exactly as the daemon does and feed it a moving
//! picture for a while, with no peer involved.
//!
//! The view is meant to be invisible to the person at the machine and visible
//! to capture apps; neither half can be seen from a unit test. Run this, then
//! inspect the window from outside while it lives — for instance
//! `wgc_probe --inspect "Cursio — Probe"` — or pick it in Discord or OBS.
//!
//! ```sh
//! cargo run -p cursio-window --example screen_view_probe -- 60
//! ```

#[cfg(target_os = "windows")]
fn main() {
    use cursio_protocol::{Rect, ScreenSource, SourceKind, TileCodec, WindowFrame, WindowMeta};
    use cursio_window::{Surrogate, Win32WindowRenderer, WindowRenderer};

    tracing_subscriber::fmt().init();
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let (w, h) = (1280u32, 720u32);
    let meta = WindowMeta {
        id: cursio_protocol::SCREEN_STREAM_ID,
        title: "Cursio — Probe".into(),
        width: w,
        height: h,
    };
    let mut view = Win32WindowRenderer
        .open_screen_view(&meta)
        .expect("open the remote-screen view");
    view.set_sources(&[ScreenSource {
        id: 1,
        kind: SourceKind::Monitor,
        title: "Probe pattern".into(),
        width: w,
        height: h,
        picked_on_host: false,
    }]);
    view.restart_stream("Probe pattern");
    let started = std::time::Instant::now();
    let mut seq = 0u32;
    while started.elapsed().as_secs() < seconds {
        // Magenta with a white bar sweeping across the top.
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for px in pixels.as_chunks_mut::<4>().0 {
            *px = [0xFF, 0x00, 0xFF, 0xFF];
        }
        let x0 = (seq * 8) % (w - 40);
        for y in 0..40 {
            let start = ((y * w + x0) * 4) as usize;
            pixels[start..start + 160].fill(0xFF);
        }
        seq += 1;
        let frame = WindowFrame {
            id: meta.id,
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
        if let Err(e) = view.apply_frame(&frame) {
            eprintln!("view refused a frame: {e}");
            break;
        }
        while let Some(pick) = view.take_source_pick() {
            println!("picked from the icon's menu: {pick:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(33));
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("screen_view_probe opens the Windows view and only runs on Windows");
    std::process::exit(1);
}
