//! Bounded handoff into platform playback threads, with local arrival times.
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

pub(crate) struct Packet {
    pub data: Vec<u8>,
    pub arrived: Instant,
    pub timestamp: u64,
}

pub(crate) struct Sender {
    tx: mpsc::SyncSender<Packet>,
    queued: Arc<AtomicUsize>,
    limit: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    connected: Arc<AtomicBool>,
}

pub(crate) struct Receiver {
    rx: mpsc::Receiver<Packet>,
    dropped: Arc<AtomicU64>,
    connected: Arc<AtomicBool>,
    stale: std::cell::Cell<u64>,
    max_age: std::cell::Cell<Duration>,
    reported: std::cell::Cell<Instant>,
    limit: Arc<AtomicUsize>,
    base: usize,
    stale_after: std::cell::Cell<Duration>,
    queued: Arc<AtomicUsize>,
}

pub(crate) fn channel(limit: usize) -> (Sender, Receiver) {
    let base = limit;
    let limit = Arc::new(AtomicUsize::new(limit));
    let (tx, rx) = mpsc::sync_channel(128);
    let queued = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let connected = Arc::new(AtomicBool::new(true));
    (
        Sender {
            tx,
            queued: queued.clone(),
            limit: limit.clone(),
            dropped: dropped.clone(),
            connected: connected.clone(),
        },
        Receiver {
            rx,
            queued,
            dropped,
            connected,
            stale: std::cell::Cell::new(0),
            max_age: std::cell::Cell::new(Duration::ZERO),
            reported: std::cell::Cell::new(Instant::now()),
            limit,
            base,
            stale_after: std::cell::Cell::new(Duration::from_millis(250)),
        },
    )
}

impl Sender {
    pub fn send(&self, chunk: &cursio_protocol::AudioChunk) -> Result<(), crate::AudioError> {
        self.send_data(&chunk.data, chunk.timestamp)
    }

    fn send_data(&self, data: &[u8], timestamp: u64) -> Result<(), crate::AudioError> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(crate::AudioError::Backend("playback thread gone".into()));
        }
        if self
            .queued
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(data.len())
                    .filter(|&n| n <= self.limit.load(Ordering::Relaxed))
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let packet = Packet {
            data: data.to_vec(),
            arrived: Instant::now(),
            timestamp,
        };
        match self.tx.try_send(packet) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.queued.fetch_sub(data.len(), Ordering::Relaxed);
                match error {
                    mpsc::TrySendError::Full(_) => {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    }
                    mpsc::TrySendError::Disconnected(_) => {
                        Err(crate::AudioError::Backend("playback thread gone".into()))
                    }
                }
            }
        }
    }
}

impl Receiver {
    #[cfg(any(target_os = "linux", target_os = "android", test))]
    pub fn set_cycle(&self, wire_bytes: usize) {
        self.limit
            .store(self.base.saturating_add(wire_bytes), Ordering::Relaxed);
        let cycle = Duration::from_secs_f64(wire_bytes as f64 / (48000.0 * 8.0));
        self.stale_after
            .set(Duration::from_millis(250).max(cycle + Duration::from_millis(50)));
    }

    pub fn report(&self, stream: impl std::fmt::Display) {
        if self.reported.get().elapsed() < Duration::from_secs(10) {
            return;
        }
        tracing::debug!(%stream, queued_bytes = self.queued.load(Ordering::Relaxed),
            capacity_bytes = self.limit.load(Ordering::Relaxed),
            base_capacity_bytes = self.base,
            dropped_packets = self.dropped.swap(0, Ordering::Relaxed),
            stale_packets = self.stale.replace(0),
            max_age_ms = self.max_age.replace(Duration::ZERO).as_secs_f64() * 1000.0,
            "playback feed health");
        self.reported.set(Instant::now());
    }

    pub fn try_recv(&self) -> Result<Packet, mpsc::TryRecvError> {
        loop {
            let packet = self.rx.try_recv()?;
            self.queued.fetch_sub(packet.data.len(), Ordering::Relaxed);
            let age = packet.arrived.elapsed();
            self.max_age.set(self.max_age.get().max(age));
            if age <= self.stale_after.get() {
                return Ok(packet);
            }
            self.stale.set(self.stale.get() + 1);
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.connected.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_consumer_cannot_accumulate_audio_and_draining_reopens_capacity() {
        let (tx, rx) = channel(8);
        for _ in 0..10000 {
            tx.send_data(&[1; 4], 0).unwrap();
        }
        assert_eq!(tx.queued.load(Ordering::Relaxed), 8);
        assert_eq!(rx.try_recv().unwrap().data, [1; 4]);
        tx.send_data(&[2; 4], 0).unwrap();
        assert_eq!(rx.try_recv().unwrap().data, [1; 4]);
        assert_eq!(rx.try_recv().unwrap().data, [2; 4]);
        assert_eq!(tx.queued.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_long_device_cycle_has_enough_handoff_capacity_and_age_allowance() {
        let (tx, rx) = channel(48000 / 5 * 8);
        rx.set_cycle(12288 * 8);
        let payload = vec![0; 12288 * 8];
        tx.send_data(&payload, 0).unwrap();
        assert_eq!(rx.try_recv().unwrap().data.len(), payload.len());
        tx.queued.store(4, Ordering::Relaxed);
        tx.tx
            .try_send(Packet {
                data: vec![0; 4],
                arrived: Instant::now() - Duration::from_millis(260),
                timestamp: 0,
            })
            .unwrap();
        assert_eq!(rx.try_recv().unwrap().data.len(), 4);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn failed_delivery_releases_its_reservation() {
        let (tx, rx) = channel(8);
        drop(rx);
        assert!(tx.send_data(&[0; 4], 0).is_err());
        assert_eq!(tx.queued.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_full_disconnected_queue_reports_failure_instead_of_silently_dropping_forever() {
        let (tx, rx) = channel(4);
        tx.send_data(&[0; 4], 0).unwrap();
        drop(rx);
        assert!(tx.send_data(&[0; 4], 0).is_err());
    }

    #[test]
    fn stale_audio_is_never_replayed_after_a_callback_stall() {
        let (tx, rx) = channel(8);
        tx.queued.store(4, Ordering::Relaxed);
        tx.tx
            .try_send(Packet {
                data: vec![1; 4],
                arrived: Instant::now() - Duration::from_secs(1),
                timestamp: 0,
            })
            .unwrap();
        tx.send_data(&[2; 4], 0).unwrap();
        assert_eq!(rx.try_recv().unwrap().data, [2; 4]);
        assert_eq!(tx.queued.load(Ordering::Relaxed), 0);
    }
}
