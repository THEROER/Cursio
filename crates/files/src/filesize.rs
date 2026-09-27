//! How long a file open on a mount is, as far as that handle knows.
//!
//! Windows takes the size a filesystem reports after every write as the truth,
//! and acts on it: a size that went *down* is a truncation as far as the cache
//! and the application are concerned. With writes pipelined the peer's own
//! answer lags behind what has been sent, and the metadata cache forgets after
//! a second — so the mount used to fill the gap by inventing a size from the
//! write in hand, `offset + len`. An installer that preallocated a 4 GB file
//! and then wrote its first chunk was told the file was 1 MB long.
//!
//! So each handle keeps the one figure it can vouch for: the length the peer
//! reported when the file was opened, raised by every write this handle sent
//! and lowered only by an answer from the peer given when no write of ours
//! could still be on its way — after a resize, or once the pipeline is empty.

use std::sync::Mutex;

/// The best-known length of one open file.
pub struct KnownSize(Mutex<u64>);

impl KnownSize {
    /// Start from what the peer said when the file was opened.
    pub fn new(size: u64) -> Self {
        Self(Mutex::new(size))
    }

    pub fn get(&self) -> u64 {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Note a write this handle has sent, returning the length after it.
    ///
    /// Only ever raises the figure: a write inside the file leaves it alone,
    /// which is the whole point — a preallocated file stays its full size
    /// while its contents are filled in.
    pub fn wrote(&self, offset: u64, len: u64) -> u64 {
        let mut size = self.0.lock().unwrap_or_else(|p| p.into_inner());
        *size = (*size).max(offset.saturating_add(len));
        *size
    }

    /// Reconcile with a length the peer just reported, returning the length to
    /// tell Windows.
    ///
    /// With writes still outstanding the peer's answer may predate them, so
    /// it can only raise the figure. With none outstanding it is the truth —
    /// including a shrink someone else made on the owner's machine — and it
    /// replaces what this handle believed.
    pub fn observe(&self, reported: u64, writes_outstanding: bool) -> u64 {
        let mut size = self.0.lock().unwrap_or_else(|p| p.into_inner());
        *size = match writes_outstanding {
            true => (*size).max(reported),
            false => reported,
        };
        *size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this exists for: a write into a preallocated file must not be
    /// reported as the end of it.
    #[test]
    fn a_write_inside_a_preallocated_file_keeps_its_size() {
        let size = KnownSize::new(4 << 30);
        assert_eq!(size.wrote(0, 1 << 20), 4 << 30);
        assert_eq!(size.get(), 4 << 30);
    }

    #[test]
    fn a_write_past_the_end_extends_the_file() {
        let size = KnownSize::new(100);
        assert_eq!(size.wrote(90, 50), 140);
    }

    /// A stale answer from the peer, arriving while our writes are still on
    /// their way, must not shrink the file under them.
    #[test]
    fn a_stale_answer_cannot_shrink_a_file_with_writes_outstanding() {
        let size = KnownSize::new(0);
        size.wrote(0, 1 << 20);
        assert_eq!(size.observe(256 << 10, true), 1 << 20);
    }

    /// With nothing outstanding, the peer is the authority — including when
    /// the file shrank on the owner's machine.
    #[test]
    fn a_settled_answer_from_the_peer_is_believed() {
        let size = KnownSize::new(1000);
        assert_eq!(size.observe(10, false), 10);
        assert_eq!(size.get(), 10);
    }
}
