//! Read this desktop's MPRIS players as the media module would send them, and
//! offer one made-up peer player for GNOME's media widget to show.
//!
//! `cargo run -p cursio-core --example media_probe` with something playing.
//! Needs a session bus, so it cannot be a unit test.

#[cfg(target_os = "linux")]
fn main() {
    use cursio_core::media_hub::{hub, MediaDisplay, RemotePlayer};
    use cursio_protocol::media::{PlaybackState, Player};
    use cursio_protocol::PeerId;

    tracing_subscriber::fmt().with_env_filter("debug").init();
    cursio_core::media_mpris::start();
    std::thread::sleep(std::time::Duration::from_secs(2));
    for player in hub().local_players() {
        println!(
            "{} — {} / {} [{:?}] {:?}/{:?} ms art={:?}",
            player.app,
            player.title,
            player.artist,
            player.state,
            player.position_ms,
            player.duration_ms,
            player.art
        );
    }
    let display = cursio_core::media_mpris::MprisDisplay::default();
    let peer = PeerId::from_bytes([9; 16]);
    display.show(
        peer,
        "Probe Peer",
        &[RemotePlayer {
            player: Player {
                id: "probe".into(),
                app: "Probe".into(),
                title: "A peer's track".into(),
                artist: "Cursio".into(),
                album: String::new(),
                state: PlaybackState::Playing,
                duration_ms: Some(200_000),
                position_ms: Some(30_000),
                volume: None,
                can_play: true,
                can_pause: true,
                can_next: true,
                can_previous: true,
                can_seek: true,
                art: None,
            },
            received: std::time::Instant::now(),
            art: None,
        }],
    );
    println!("offering a peer player for 20 s; look at GNOME's media widget");
    std::thread::sleep(std::time::Duration::from_secs(20));
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("MPRIS exists only on Linux");
}
