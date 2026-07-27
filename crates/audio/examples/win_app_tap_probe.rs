//! Probe Windows per-app capture (WASAPI process loopback): pick a playing app
//! by name and capture *only* that app's audio. Windows only.
//!
//! ```powershell
//! cargo run -p cursio-audio --example win_app_tap_probe -- steam
//! ```

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    let needle = std::env::args().nth(1).unwrap_or_else(|| "".into());

    let apps = cursio_audio::list_app_streams().expect("list apps");
    println!("playing apps: {apps:?}");

    let Some(app) = apps
        .iter()
        .find(|a| needle.is_empty() || a.app_name.to_lowercase().contains(&needle.to_lowercase()))
    else {
        eprintln!("no playing app matching {needle:?}");
        std::process::exit(2);
    };
    println!("tapping {:?} (pid {})", app.app_name, app.id);

    let mut cap = match cursio_audio::Capture::app_tap(1, app.id.clone(), app.app_name.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("app_tap FAILED: {e}");
            std::process::exit(3);
        }
    };
    println!("process loopback activated");

    let mut chunks = 0u64;
    let mut bytes = 0u64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            c = cap.chunks().recv() => match c {
                Some(chunk) => { chunks += 1; bytes += chunk.data.len() as u64; }
                None => break,
            }
        }
    }
    println!("captured {chunks} chunks / {bytes} bytes from {} alone", app.app_name);
    if chunks == 0 {
        eprintln!("NO AUDIO captured (is that app actually making sound?)");
        std::process::exit(4);
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("win_app_tap_probe is Windows-only");
}
