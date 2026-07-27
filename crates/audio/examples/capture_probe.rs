//! Live probe: capture a sink's monitor via the PipeWire backend and report the
//! chunk stream. Linux only. Run with a target monitor node name, e.g.:
//!
//! ```sh
//! cargo run -p cursio-audio --example capture_probe -- virtual_speaker.monitor
//! ```
//!
//! Play something (or a test tone) so the monitor produces audio, and this
//! prints how many chunks/bytes arrived — proving the capture path is live.

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use cursio_audio::pipewire_backend::PwSource;

    let target = std::env::args().nth(1);
    println!("starting capture (target = {target:?})");

    let mut src = match PwSource::start(1, target) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("capture failed to start: {e}");
            std::process::exit(1);
        }
    };

    if let Ok(meta) = src.advertisements().try_recv() {
        println!("advertised stream: {meta:?}");
    }

    let mut chunks = 0u64;
    let mut bytes = 0u64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            c = src.chunks().recv() => match c {
                Some(chunk) => {
                    chunks += 1;
                    bytes += chunk.data.len() as u64;
                    if chunks <= 3 {
                        println!(
                            "chunk seq={} ts={} bytes={}",
                            chunk.seq, chunk.timestamp, chunk.data.len()
                        );
                    }
                }
                None => break,
            }
        }
    }

    println!("captured {chunks} chunks, {bytes} bytes in 3s");
    if chunks == 0 {
        eprintln!("NO CHUNKS — is anything playing into the monitor?");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("capture_probe is Linux-only");
}
