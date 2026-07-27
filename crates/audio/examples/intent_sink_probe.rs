//! Probe the Intent Sink: it should register a switchable OUTPUT DEVICE that
//! carries NO audio, and report which apps are routed into it. Linux only.
//!
//! ```sh
//! cargo run -p cursio-audio --example intent_sink_probe
//! # elsewhere: route an app (e.g. a browser tab, mpv) into "Cursio: Intent Probe"
//! #   pactl move-sink-input <id> cursio_intent_probe
//! # then move it away again and watch the Joined/Left lines.
//! ```

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use cursio_audio::IntentSink;

    let mut sink = IntentSink::create(
        1,
        "cursio_intent_probe".to_string(),
        "Cursio: Intent Probe".to_string(),
    )
    .expect("create intent sink");

    println!("Intent Sink 'Cursio: Intent Probe' up for 30s (carries no audio)");
    println!("route an app into it and watch membership:");
    println!("  pactl move-sink-input <id> cursio_intent_probe");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = sink.events().recv() => match ev {
                Some(e) => println!("membership: {e:?}"),
                None => break,
            }
        }
    }
    println!("done");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("intent_sink_probe is Linux-only");
}
