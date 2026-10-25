//! The NTP-style timing channel the sender expects answered beside the stream.

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;

pub(super) const TIMING_PACKET_BYTES: usize = 32;
pub(super) const NTP_UNIX_EPOCH_OFFSET: u64 = 2_208_988_800;
pub(super) async fn run_timing(socket: UdpSocket) {
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut response = [0_u8; 128];
    loop {
        interval.tick().await;
        let request = timing_request();
        if socket.send(&request).await.is_err() {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_millis(500), socket.recv(&mut response)).await;
    }
}

pub(super) fn timing_request() -> [u8; TIMING_PACKET_BYTES] {
    let mut request = [0_u8; TIMING_PACKET_BYTES];
    request[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs().saturating_add(NTP_UNIX_EPOCH_OFFSET);
    let fraction = (u128::from(elapsed.subsec_nanos()) << 32) / 1_000_000_000_u128;
    let timestamp = (seconds << 32) | fraction as u64;
    request[24..].copy_from_slice(&timestamp.to_be_bytes());
    request
}
