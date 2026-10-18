//! Become a paired phone's hands-free unit for a while and print what it says.
//!
//! `cargo run -p cursio-bluetooth --example hfp_probe -- 9C:9E:D5:78:FE:C4 [seconds]`
//!
//! Needs the phone paired with this computer and in range. It does not place
//! or answer calls; ring the phone from elsewhere to see one listed. Call
//! audio, if a call is up, is counted and dropped.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cursio_bluetooth::{CallAudio, CallAudioFactory, HandsFreeService};

static PLAYED: AtomicUsize = AtomicUsize::new(0);

struct Counting;

impl CallAudio for Counting {
    fn play(&mut self, pcm: &[i16]) {
        PLAYED.fetch_add(pcm.len(), Ordering::Relaxed);
    }

    fn record(&mut self, samples: usize, out: &mut Vec<i16>) {
        out.resize(samples, 0);
    }
}

struct Factory;

impl CallAudioFactory for Factory {
    fn open(&self, _phone: &str, _rate: u32) -> Result<Box<dyn CallAudio>, String> {
        Ok(Box::new(Counting))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cursio_bluetooth=trace,info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let phone = args
        .next()
        .ok_or("usage: hfp_probe <phone address> [seconds]")?;
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(20);

    let (service, events) = HandsFreeService::start(Arc::new(Factory))?;
    if let Err(e) = service.connect(&phone) {
        println!("connect: {e} (a phone already connected arrives on its own)");
    }
    let end = Instant::now() + Duration::from_secs(seconds);
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        if let Ok(event) = events.recv_timeout(left.min(Duration::from_millis(500))) {
            println!("{event:?}");
        }
    }
    println!(
        "call audio samples received: {}",
        PLAYED.load(Ordering::Relaxed)
    );
    Ok(())
}
