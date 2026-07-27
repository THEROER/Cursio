//! Live probe: capture the default render device's loopback via the WASAPI
//! backend and report the chunk stream. Windows only.
//!
//! ```powershell
//! cargo run -p cursio-audio --example wasapi_capture_probe
//! ```
//!
//! Play something (any audio) so the render device has output, and this prints
//! how many chunks/bytes arrived — proving the loopback capture path is live.

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    use cursio_audio::wasapi_backend::WasapiSource;

    println!("starting WASAPI loopback capture of default render device");
    let mut src = match WasapiSource::start(1) {
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
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            c = src.chunks().recv() => match c {
                Some(chunk) => {
                    chunks += 1;
                    bytes += chunk.data.len() as u64;
                    if chunks <= 3 {
                        println!("chunk seq={} ts={} bytes={}", chunk.seq, chunk.timestamp, chunk.data.len());
                    }
                }
                None => break,
            }
        }
    }

    println!("captured {chunks} chunks, {bytes} bytes in 4s");
    if chunks == 0 {
        eprintln!("NO CHUNKS — is anything playing on the default device?");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("wasapi_capture_probe is Windows-only");
}
