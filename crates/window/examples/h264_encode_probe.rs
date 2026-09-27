//! Push a moving picture through this machine's Media Foundation encoder,
//! change its bitrate halfway, and decode the result again.
//!
//! What a unit test cannot show: whether the encoder the machine actually
//! picks (usually an asynchronous hardware one) takes a frame per call, how
//! many bytes each half of the run cost, and whether a live bitrate change
//! keeps the stream going without a keyframe.
//!
//! ```sh
//! cargo run -p cursio-window --example h264_encode_probe
//! ```

#[cfg(target_os = "windows")]
fn main() {
    use cursio_window::video_decoder_win::VideoDecoder;
    use cursio_window::video_encoder_win::VideoEncoder;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let (w, h) = (1920u32, 1080u32);
    let mut encoder = VideoEncoder::open(w, h, 12_000_000).expect("open the H.264 encoder");
    let mut decoder = VideoDecoder::open().expect("open the H.264 decoder");
    let frame = |i: u32| {
        let mut f = vec![0x30u8; (w * h * 4) as usize];
        for y in 300..500 {
            let x0 = (i * 12) % (w - 200);
            let start = ((y * w + x0) * 4) as usize;
            f[start..start + 800].fill(0xE0);
        }
        f
    };
    let halves = [12_000_000u32, 3_000_000];
    let mut pictures = 0;
    for (half, bitrate) in halves.iter().enumerate() {
        encoder.set_bitrate(*bitrate);
        let (mut bytes, mut packets, mut keyframes, mut errors) = (0usize, 0, 0, 0);
        let started = std::time::Instant::now();
        for i in 0..120 {
            match encoder.encode(&frame(half as u32 * 120 + i)) {
                Ok(out) => {
                    for p in out {
                        packets += 1;
                        bytes += p.len();
                        if p.windows(4)
                            .any(|n| n[..3] == [0, 0, 1] && n[3] & 0x1F == 5)
                        {
                            keyframes += 1;
                        }
                        pictures += decoder.decode(&p).map(|d| d.len()).unwrap_or(0);
                    }
                }
                Err(e) => {
                    errors += 1;
                    if errors <= 3 {
                        println!("frame {i}: {e}");
                    }
                }
            }
        }
        println!(
            "at {} Mbit/s: 120 frames -> {packets} packets, {:.1} Mbit/s at 60 fps, {keyframes} keyframes, {errors} errors, {:.1} ms/frame",
            bitrate / 1_000_000,
            bytes as f64 * 8.0 / 2.0 / 1e6,
            started.elapsed().as_secs_f64() * 1000.0 / 120.0
        );
    }
    println!("decoded back: {pictures} pictures");
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("h264_encode_probe drives Media Foundation and only runs on Windows");
    std::process::exit(1);
}
