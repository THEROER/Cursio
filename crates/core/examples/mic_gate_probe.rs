//! Record a real microphone and replay it through the silence gates, to hear
//! what a peer recording from it would get.
//!
//! ```sh
//! cargo run -p cursio-core --example mic_gate_probe -- [seconds] [device substring]
//! ```
//!
//! With no device it lists the inputs and records the first. Speak normally,
//! then quietly, then pause. It writes three WAVs into the current directory —
//! `mic-raw.wav`, `mic-gate-old.wav` (the -60 dBFS peak gate with no hangover
//! that cut quiet speakers off) and `mic-gate-auto.wav` (what is sent now) —
//! and prints, per gate, how much of the speech it dropped and how many times
//! it cut in. A dropped chunk is written as zeros, which is what the far side
//! plays on an underrun.

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    use cursio_core::audio_gate::{rms_db, Gate, GateKind, MicGate, SILENCE_THRESHOLD};

    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(30);
    let wanted = args.next();

    let inputs = cursio_audio::list_input_devices()?;
    for dev in &inputs {
        println!("input: {} ({})", dev.description, dev.id);
    }
    let device = match &wanted {
        Some(w) => inputs
            .iter()
            .find(|d| d.description.to_lowercase().contains(&w.to_lowercase()))
            .ok_or_else(|| anyhow::anyhow!("no input device matches {w:?}"))?,
        None => inputs
            .first()
            .ok_or_else(|| anyhow::anyhow!("this machine has no input devices"))?,
    };
    println!(
        "recording {} for {seconds} s — speak, then speak quietly, then pause",
        device.description
    );

    let mut capture = cursio_audio::Capture::mic_tap(1, Some(device.id.clone()))?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    loop {
        tokio::select! {
            chunk = capture.chunks().recv() => match chunk {
                Some(c) => chunks.push(c.data),
                None => anyhow::bail!("the capture ended early"),
            },
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    capture.stop();

    let old = |data: &[u8]| {
        data.as_chunks::<4>()
            .0
            .iter()
            .any(|b| f32::from_le_bytes(*b).abs() >= SILENCE_THRESHOLD)
    };
    let mut auto = Gate::default();
    let new = |data: &[u8]| auto.admit(GateKind::Mic(MicGate::Auto), data);

    let levels: Vec<f32> = chunks.iter().map(|c| rms_db(c)).collect();
    let mut sorted = levels.clone();
    sorted.sort_by(f32::total_cmp);
    let pct = |p: usize| {
        sorted
            .get(sorted.len() * p / 100)
            .copied()
            .unwrap_or(f32::NAN)
    };
    println!(
        "{} chunks; RMS dBFS p10 {:.1}, p50 {:.1}, p90 {:.1}, max {:.1}",
        chunks.len(),
        pct(10),
        pct(50),
        pct(90),
        sorted.last().copied().unwrap_or(f32::NAN)
    );
    write_wav("mic-raw.wav", &chunks.concat())?;
    report("old -60 dBFS peak", "mic-gate-old.wav", &chunks, old)?;
    report("auto", "mic-gate-auto.wav", &chunks, new)?;
    println!(
        "auto gate ended with floor {:?} dBFS, threshold {:.1} dBFS",
        auto.floor_db(),
        auto.auto_threshold()
    );
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
/// A gap shorter than this between two sent stretches is a cut inside speech:
/// nobody pauses that briefly on purpose, and the listener hears it as chopping.
#[cfg(any(target_os = "linux", target_os = "windows"))]
const CHOP_BYTES: usize = 48_000 * 2 * 4;

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn report(
    name: &str,
    path: &str,
    chunks: &[Vec<u8>],
    mut admit: impl FnMut(&[u8]) -> bool,
) -> anyhow::Result<()> {
    let mut out = Vec::new();
    let (mut sent, mut chops) = (0usize, 0usize);
    let mut ever_open = false;
    let mut gap: Option<usize> = None;
    for chunk in chunks {
        if admit(chunk) {
            sent += 1;
            out.extend_from_slice(chunk);
            if gap.is_some_and(|g| g < CHOP_BYTES) {
                chops += 1;
            }
            gap = None;
            ever_open = true;
        } else {
            out.extend(std::iter::repeat_n(0u8, chunk.len()));
            if ever_open {
                *gap.get_or_insert(0) += chunk.len();
            }
        }
    }
    write_wav(path, &out)?;
    println!(
        "{name:>18}: sent {sent}/{} chunks, {chops} gaps under 1 s inside sound -> {path}",
        chunks.len()
    );
    Ok(())
}

/// 48 kHz stereo IEEE float, the capture format.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn write_wav(path: &str, samples: &[u8]) -> std::io::Result<()> {
    let len = samples.len() as u32;
    let mut wav = Vec::with_capacity(44 + samples.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&3u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&48_000u32.to_le_bytes());
    wav.extend_from_slice(&(48_000u32 * 8).to_le_bytes());
    wav.extend_from_slice(&8u16.to_le_bytes());
    wav.extend_from_slice(&32u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&len.to_le_bytes());
    wav.extend_from_slice(samples);
    std::fs::write(path, wav)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn main() {
    eprintln!("mic_gate_probe needs an audio backend: Linux (PipeWire) or Windows (WASAPI)");
    std::process::exit(1);
}
