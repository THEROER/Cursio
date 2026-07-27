//! Probe the real backend API: `PwSource::virtual_sink` should register a
//! switchable OUTPUT DEVICE and capture whatever the system routes to it.
//! Linux only.
//!
//! ```sh
//! cargo run -p cursio-audio --example virtual_sink_probe
//! # elsewhere: pactl list sinks short | grep -i cursio ; route audio to it
//! ```

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use cursio_audio::pipewire_backend::PwSource;

    let mut sink = PwSource::virtual_sink(
        1,
        "cursio_remote_sink".to_string(),
        "Cursio: Remote Output".to_string(),
    )
    .expect("create virtual sink");

    println!("virtual output device 'Cursio: Remote Output' up for 12s");
    println!("route audio to it: pactl set-default-sink cursio_remote_sink");

    let mut chunks = 0u64;
    let mut bytes = 0u64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            c = sink.chunks().recv() => match c {
                Some(chunk) => { chunks += 1; bytes += chunk.data.len() as u64; }
                None => break,
            }
        }
    }
    println!("captured {chunks} chunks / {bytes} bytes routed into the device");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("virtual_sink_probe is Linux-only");
}
