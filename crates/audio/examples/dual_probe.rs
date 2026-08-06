//! Repro: does capture from a virtual sink still deliver chunks when a Playback
//! node also exists in the same process (as in the bidirectional session)?
//! Linux only.

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use cursio_audio::{Capture, Playback};

    // Same objects the bidirectional session creates.
    let mut cap = Capture::virtual_sink(
        2,
        "cursio_dual_sink".to_string(),
        "Cursio: Dual Test".to_string(),
    )
    .expect("virtual sink");
    let _play = Playback::open(1, "Cursio: Dual Play".to_string()).expect("playback");

    println!(
        "both up. route audio to 'Cursio: Dual Test' (pactl set-default-sink cursio_dual_sink)"
    );

    let mut chunks = 0u64;
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            _ = ticker.tick() => {
                println!("tick: chunks so far = {chunks}");
            }
            c = cap.chunks().recv() => match c {
                Some(_) => chunks += 1,
                None => { println!("chunks channel closed!"); break; }
            }
        }
    }
    println!("captured {chunks} chunks with a Playback also alive");
}

#[cfg(not(target_os = "linux"))]
fn main() {}
