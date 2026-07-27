//! Live loopback: capture the default render device and play it straight back
//! through a WASAPI render node — the full Windows capture→sink path minus the
//! network. Windows only.
//!
//! ```powershell
//! cargo run -p cursio-audio --example wasapi_loopback_probe
//! ```
//!
//! Play a tone into the default device; this reports chunks moved
//! capture→render (and you should hear the echo doubling).

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    use cursio_audio::wasapi_backend::{WasapiSink, WasapiSource};

    let mut src = WasapiSource::start(1).expect("start capture");
    let sink = WasapiSink::open(1, "Cursio probe".to_string()).expect("open render");
    println!("capture + render open; moving audio through for 4s");

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

    println!("moved {moved} chunks capture -> render in 4s");
    if moved == 0 {
        eprintln!("NO CHUNKS moved — is anything playing?");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("wasapi_loopback_probe is Windows-only");
}
