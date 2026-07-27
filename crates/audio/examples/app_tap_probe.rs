//! Probe per-app capture: find a playing app by name and tap *just* that app's
//! audio. Linux only.
//!
//! ```sh
//! # play something first, then:
//! cargo run -p cursio-audio --example app_tap_probe -- pw-play
//! ```
//!
//! Prints how many chunks came from that one application's stream.

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use cursio_audio::pipewire_backend::PwSource;

    let needle = std::env::args().nth(1).unwrap_or_else(|| "pw-play".into());

    let apps = cursio_audio::list_app_streams().expect("list apps");
    println!("playing apps: {apps:?}");

    let Some(app) = apps
        .iter()
        .find(|a| a.app_name.to_lowercase().contains(&needle.to_lowercase()))
    else {
        eprintln!("no playing app matching {needle:?}");
        std::process::exit(2);
    };
    println!("tapping app {:?} (node {})", app.app_name, app.id);

    let mut tap = PwSource::app_tap(1, app.id.clone(), app.app_name.clone()).expect("app tap");

    if let Ok(meta) = tap.advertisements().try_recv() {
        println!("advertised: app_name={:?}", meta.app_name);
    }

    let mut chunks = 0u64;
    let mut bytes = 0u64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            c = tap.chunks().recv() => match c {
                Some(chunk) => { chunks += 1; bytes += chunk.data.len() as u64; }
                None => break,
            }
        }
    }
    println!("captured {chunks} chunks / {bytes} bytes from that app alone");
    if chunks == 0 {
        eprintln!("NO AUDIO captured from the app tap");
        std::process::exit(3);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("app_tap_probe is Linux-only");
}
