//! Open the mesh's Bluetooth channel for a while and report who connects.
//!
//! `cargo run -p cursio-bluetooth --example mesh_channel_probe -- [seconds]`
//!
//! Registers the same RFCOMM service a node does, so only run it while no
//! node on this machine holds it. Each paired device that opens the channel
//! is named, and the bytes it sends are counted (a phone's node greets with a
//! `Hello` frame within a second of connecting).

use std::io::Read;
use std::time::Duration;

use cursio_bluetooth::MeshChannel;

fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let (_channel, accepted) = match MeshChannel::start() {
        Ok(started) => started,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    println!("listening for {seconds}s; open the channel from a paired phone");
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok((peer, fd)) = accepted.recv_timeout(left) else {
            break;
        };
        let mut socket = std::os::unix::net::UnixStream::from(fd);
        let _ = socket.set_read_timeout(Some(Duration::from_secs(3)));
        let mut buf = [0u8; 4096];
        let n = socket.read(&mut buf).unwrap_or(0);
        println!("{peer} connected and sent {n} bytes first");
    }
}
