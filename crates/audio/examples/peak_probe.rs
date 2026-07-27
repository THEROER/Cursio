//! Measure the peak sample amplitude captured from a virtual sink, to see how
//! the OS volume slider (including >100% boost on Linux) affects what Cursio
//! actually sends. Linux only.
//!
//! ```sh
//! cargo run -p cursio-audio --example peak_probe
//! ```
//!
//! Values above 1.0 mean the samples are **clipping past full scale** — which a
//! >100% boost can produce, and which the far side must handle.

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use cursio_audio::pipewire_backend::PwSource;

    let mut src = PwSource::virtual_sink(
        1,
        "cursio_peak_probe".to_string(),
        "Cursio: Peak Probe".to_string(),
    )
    .expect("virtual sink");

    println!("sink 'Cursio: Peak Probe' up — route audio to cursio_peak_probe");

    let mut peak: f32 = 0.0;
    let mut over_full_scale = 0u64;
    let mut total = 0u64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            c = src.chunks().recv() => match c {
                Some(chunk) => {
                    // f32 stereo samples
                    for s in chunk.data.chunks_exact(4) {
                        let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]).abs();
                        if v > peak { peak = v; }
                        if v > 1.0 { over_full_scale += 1; }
                        total += 1;
                    }
                }
                None => break,
            }
        }
    }
    println!("peak amplitude: {peak:.4}  (1.0 = full scale)");
    println!("samples over full scale: {over_full_scale} / {total}");
}

#[cfg(not(target_os = "linux"))]
fn main() {}
