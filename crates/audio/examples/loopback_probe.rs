//! Live loopback: capture a sink's monitor and play it straight back through a
//! Cursio playback node — the full Linux capture→sink path, minus the network.
//! Linux only.
//!
//! ```sh
//! cargo run -p cursio-audio --example loopback_probe -- virtual_speaker.monitor
//! ```
//!
//! Verifies the playback node appears in the graph and consumes chunks. Play a
//! tone into the captured sink; this reports chunks moved capture→playback.

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use cursio_audio::pipewire_backend::{PwSink, PwSource};

    let target = std::env::args().nth(1);
    println!("capture target = {target:?}");

    let mut src = PwSource::start(1, target).expect("start capture");
    let sink = PwSink::open(1, "Cursio: Loopback".to_string()).expect("open playback");
    println!("playback node 'Cursio: Loopback' opened — check `pw-cli ls Node`");

    let mut moved = 0u64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            c = src.chunks().recv() => match c {
                Some(chunk) => {
                    if sink.play(&chunk).is_ok() {
                        moved += 1;
                    }
                }
                None => break,
            }
        }
    }

    println!("moved {moved} chunks capture -> playback in 4s");
    if moved == 0 {
        eprintln!("NO CHUNKS moved");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("loopback_probe is Linux-only");
}
