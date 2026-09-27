//! Feed an Annex-B H.264 file through this machine's Media Foundation decoder
//! and report how many pictures come out.
//!
//! Exists because "the decoder accepted every packet" and "the decoder produced
//! a picture" are different claims, and only the second one shows up on a
//! screen: a transform can take a stream whose profile it does not implement
//! and simply never emit anything. Produce the input with ffmpeg on any
//! machine, e.g. the encoder settings a host really uses:
//!
//! ```sh
//! ffmpeg -f lavfi -i testsrc=size=1920x1080 -pix_fmt yuv420p \
//!     -c:v libx264 -preset ultrafast -qp 0 -frames:v 30 lossless.h264
//! cargo run -p cursio-window --example h264_decode_probe -- lossless.h264
//! ```

#[cfg(target_os = "windows")]
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "debug".into()),
        )
        .init();
    let path = std::env::args()
        .nth(1)
        .expect("usage: h264_decode_probe <file.h264>");
    let stream = std::fs::read(&path).expect("read the input file");
    let mut decoder =
        cursio_window::video_decoder_win::VideoDecoder::open().expect("open the H.264 decoder");
    let (mut packets, mut pictures, mut errors) = (0, 0, 0);
    for access_unit in access_units(&stream) {
        packets += 1;
        match decoder.decode(access_unit) {
            Ok(frames) => {
                for f in &frames {
                    if pictures == 0 {
                        println!("first picture {}x{}", f.width, f.height);
                    }
                    pictures += 1;
                }
            }
            Err(e) => {
                errors += 1;
                if errors <= 3 {
                    println!("packet {packets}: {e}");
                }
            }
        }
    }
    println!("{path}: {packets} packets in, {pictures} pictures out, {errors} errors");
}

/// Split an Annex-B stream into access units: every NAL unit up to and
/// including a coded slice (types 1 and 5) belongs to one picture, which holds
/// for the single-slice streams every encoder here produces.
#[cfg(target_os = "windows")]
fn access_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            let at = if i > 0 && stream[i - 1] == 0 {
                i - 1
            } else {
                i
            };
            starts.push((at, stream[i + 3] & 0x1F));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::new();
    let mut begin = 0;
    for (n, &(_, kind)) in starts.iter().enumerate() {
        if kind == 1 || kind == 5 {
            let end = starts.get(n + 1).map_or(stream.len(), |&(at, _)| at);
            units.push(&stream[begin..end]);
            begin = end;
        }
    }
    units
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("h264_decode_probe drives the Media Foundation decoder and only runs on Windows");
    std::process::exit(1);
}
