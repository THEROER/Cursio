//! Probe: open a WASAPI render stream on a *named* device (not default), to
//! verify device enumeration + selection. Windows only.
//!
//! ```powershell
//! cargo run -p cursio-audio --example wasapi_device_probe -- "VB-Audio Virtual Cable"
//! ```
//!
//! If it opens, the device was found by name and a render client initialized.

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    use cursio_audio::wasapi_backend::WasapiSink;

    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "CABLE".to_string());
    println!("opening render on a device matching: {name:?}");

    match WasapiSink::open_device(1, name.clone(), "Cursio probe".to_string()) {
        Ok(_sink) => {
            println!("OK: opened render on a device matching {name:?}");
            // Hold it briefly so the device is visibly in use.
            std::thread::sleep(std::time::Duration::from_secs(1));
            println!("done");
        }
        Err(e) => {
            eprintln!("FAILED to open device {name:?}: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("wasapi_device_probe is Windows-only");
}
