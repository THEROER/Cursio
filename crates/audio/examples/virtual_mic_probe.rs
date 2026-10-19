//! Play a peer microphone's path end to end on this machine: feed a tone into
//! a virtual microphone (`PwSink::virtual_source`) in chunks paced like a
//! phone's capture, record that microphone back the way an application would,
//! and count the gaps in what was recorded. Linux only.
//!
//! A peer microphone that "cuts out" is either the sender or this end; this
//! takes the sender and the network out of the question.
//!
//! ```sh
//! cargo run -p cursio-audio --example virtual_mic_probe -- [seconds] [chunk_frames] [burst_ms]
//! # burst_ms > 0 holds chunks back and releases them together, like Wi-Fi does
//! ```

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use cursio_audio::pipewire_backend::{PwSink, PwSource};
    use cursio_protocol::AudioChunk;
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(5);
    let chunk_frames: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(768);
    let burst_ms: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);

    let name = "Cursio(probe): Microphone".to_string();
    let sink = PwSink::virtual_source(1, name.clone()).expect("open the virtual microphone");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut rec = PwSource::mic_tap(2, Some(name)).expect("record the virtual microphone");

    let chunk_time = Duration::from_secs_f64(chunk_frames as f64 / 48_000.0);
    let start = Instant::now();
    let mut sent_frames = 0usize;
    let mut held: Vec<AudioChunk> = Vec::new();
    let mut last_release = Instant::now();
    let mut recorded: Vec<f32> = Vec::new();
    let mut seq = 0u32;
    while start.elapsed() < Duration::from_secs(seconds) {
        while sent_frames < (start.elapsed().as_secs_f64() * 48_000.0) as usize {
            let data = (0..chunk_frames)
                .flat_map(|i| {
                    let t = (sent_frames + i) as f32 / 48_000.0;
                    let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
                    [s, s]
                })
                .flat_map(f32::to_le_bytes)
                .collect();
            held.push(AudioChunk {
                id: 1,
                seq,
                timestamp: sent_frames as u64,
                data,
            });
            seq += 1;
            sent_frames += chunk_frames;
        }
        if burst_ms == 0 || last_release.elapsed() >= Duration::from_millis(burst_ms) {
            for chunk in held.drain(..) {
                sink.play(&chunk).expect("play");
            }
            last_release = Instant::now();
        }
        while let Ok(chunk) = rec.chunks().try_recv() {
            recorded.extend(
                chunk
                    .data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .step_by(2)
                    .map(|b| f32::from_le_bytes(*b)),
            );
        }
        tokio::time::sleep(chunk_time / 4).await;
    }

    let Some(first) = recorded.iter().position(|s| s.abs() > 1e-3) else {
        println!(
            "recorded {} frames and none of them carried the tone",
            recorded.len()
        );
        return;
    };
    let body = &recorded[first..];
    let mut gaps = 0usize;
    let mut gap_frames = 0usize;
    let mut run = 0usize;
    for s in body {
        // A 440 Hz tone at 0.3 crosses zero, but never stays below 1e-3 for
        // a whole millisecond.
        if s.abs() < 1e-3 {
            run += 1;
        } else {
            if run >= 48 {
                gaps += 1;
                gap_frames += run;
            }
            run = 0;
        }
    }
    println!(
        "recorded {:.2} s of tone; {gaps} gaps totalling {:.1} ms",
        body.len() as f64 / 48_000.0,
        gap_frames as f64 / 48.0
    );
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("virtual_mic_probe is Linux-only");
}
