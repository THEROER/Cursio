//! Open and close surrogate windows one after another in one process, the way a
//! viewer does as a peer's windows come and go.
//!
//! A surrogate needs a compositor, so no unit test can show that the second
//! window of a process opens at all, or that a closed one actually leaves the
//! screen. Each window here shows a solid colour for a few seconds and is then
//! closed as the host would close it; the last one stays until its own close
//! button is pressed.
//!
//! ```sh
//! cargo run -p cursio-window --features linux-probes --example surrogate_cycle -- 3
//! ```

#[cfg(target_os = "linux")]
fn main() {
    use cursio_protocol::{Rect, TileCodec, WindowCloseReason, WindowFrame, WindowMeta};
    use cursio_window::{GtkWindowRenderer, Surrogate, WindowRenderer};

    tracing_subscriber::fmt().init();
    let windows: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let (w, h) = (480u32, 320u32);
    for id in 1..=windows {
        let meta = WindowMeta {
            id,
            title: format!("Cursio — surrogate {id} of {windows}"),
            width: w,
            height: h,
        };
        let mut surrogate = match GtkWindowRenderer.open(&meta) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("window {id} did not open: {e}");
                std::process::exit(1);
            }
        };
        println!("window {id} open");
        let colour = [(id * 70 % 256) as u8, 0x40, 0xC0, 0xFF];
        let pixels: Vec<u8> = colour.repeat((w * h) as usize);
        let frame = WindowFrame {
            id,
            seq: 1,
            dirty: Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            codec: TileCodec::RawBgra8888,
            data: pixels,
        };
        if let Err(e) = surrogate.apply_frame(&frame) {
            eprintln!("window {id} refused a frame: {e}");
        }
        if id < windows {
            std::thread::sleep(std::time::Duration::from_secs(3));
            surrogate.close(WindowCloseReason::Returned);
            println!("window {id} closed by the host; it should be gone now");
            std::thread::sleep(std::time::Duration::from_secs(1));
        } else {
            println!("close window {id} with its own button");
            // The tick ending drops the input sender, which is how the session
            // learns the user closed the window.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build a runtime");
            runtime.block_on(async { while surrogate.input().recv().await.is_some() {} });
            println!("window {id} closed by the user");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("surrogate_cycle opens GTK surrogates and only runs on Linux");
    std::process::exit(1);
}
