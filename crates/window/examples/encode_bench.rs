//! Measures what the lossless H.264 encoder actually costs, per content type.
//!
//! The numbers that justify the codec choice (see
//! `docs/window-transfer-design.md`) were originally taken with `ffmpeg` on the
//! command line. This runs the same comparison through **our own encoder**, so
//! the figures reflect the code that will really ship — including the
//! BGRA→YUV420P conversion and the backend actually selected on this machine.
//!
//! ```sh
//! OPUS_TARGET=ubuntu-24.04_x86_64 \
//!   cargo run -p cursio-window --release --example encode_bench
//! ```

use cursio_window::video_encoder::{Fidelity, VideoEncoder};

const W: u32 = 1920;
const H: u32 = 1080;
const FRAMES: usize = 120;
const FPS: f64 = 60.0;

/// A window that never changes — an idle editor, a paused video.
fn static_frames() -> Vec<Vec<u8>> {
    let one = vec![0x30u8; (W as usize) * (H as usize) * 4];
    (0..FRAMES).map(|_| one.clone()).collect()
}

/// A window where a small region changes — a blinking caret, typing.
fn typing_frames() -> Vec<Vec<u8>> {
    let base = vec![0x30u8; (W as usize) * (H as usize) * 4];
    (0..FRAMES)
        .map(|i| {
            let mut f = base.clone();
            // A caret-sized block, toggling and drifting like typed text.
            let (cx, cy) = (200 + (i % 60) * 9, 400);
            for y in cy..cy + 20 {
                for x in cx..cx + 8 {
                    let p = (y * W as usize + x) * 4;
                    let v = if (i / 30) % 2 == 0 { 0xE0 } else { 0x30 };
                    f[p..p + 4].copy_from_slice(&[v, v, v, 0xFF]);
                }
            }
            f
        })
        .collect()
}

/// Full-screen motion — the pathological case (video playing, fast scroll).
fn motion_frames() -> Vec<Vec<u8>> {
    (0..FRAMES)
        .map(|i| {
            let mut f = vec![0u8; (W as usize) * (H as usize) * 4];
            // A moving diagonal gradient: every pixel changes every frame.
            for y in 0..H as usize {
                for x in 0..W as usize {
                    let p = (y * W as usize + x) * 4;
                    let v = ((x + y + i * 7) % 256) as u8;
                    f[p..p + 4].copy_from_slice(&[v, v.wrapping_mul(3), v.wrapping_add(90), 0xFF]);
                }
            }
            f
        })
        .collect()
}

fn bench(label: &str, frames: &[Vec<u8>]) {
    let mut enc = match VideoEncoder::open(W, H, Fidelity::Lossless) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{label}: encoder unavailable: {e}");
            return;
        }
    };
    let kind = enc.kind();

    let start = std::time::Instant::now();
    let mut bytes = 0usize;
    for f in frames {
        match enc.encode(f) {
            Ok(packets) => bytes += packets.iter().map(|p| p.len()).sum::<usize>(),
            Err(e) => {
                eprintln!("{label}: encode failed: {e}");
                return;
            }
        }
    }
    if let Ok(tail) = enc.finish() {
        bytes += tail.iter().map(|p| p.len()).sum::<usize>();
    }
    let elapsed = start.elapsed();

    let n = frames.len() as f64;
    let per_frame_kib = bytes as f64 / n / 1024.0;
    let mbit_s = bytes as f64 * 8.0 * FPS / n / 1e6;
    // Real-time budget: encoding must outrun playback or latency grows forever.
    let realtime = n / FPS;
    let speed = realtime / elapsed.as_secs_f64();

    println!(
        "  {label:<10} {per_frame_kib:>8.1} KiB/frame  {mbit_s:>7.2} Mbit/s   \
         encode {:.2}s for {realtime:.1}s of video ({speed:.1}x real-time)  [{kind:?}]",
        elapsed.as_secs_f64()
    );
}

fn main() {
    ffmpeg_next::init().expect("ffmpeg init");

    println!();
    println!(
        "Lossless H.264 through cursio's own encoder — {W}x{H}, {FRAMES} frames @ {FPS:.0}fps"
    );
    println!(
        "Raw BGRA for comparison: {:.1} KiB/frame, {:.0} Mbit/s",
        (W as f64 * H as f64 * 4.0) / 1024.0,
        W as f64 * H as f64 * 4.0 * 8.0 * FPS / 1e6
    );
    println!();

    bench("static", &static_frames());
    bench("typing", &typing_frames());
    bench("motion", &motion_frames());

    println!();
    println!("A gigabit LAN is 1000 Mbit/s; Wi-Fi realistically 100-400.");
    println!();
}
