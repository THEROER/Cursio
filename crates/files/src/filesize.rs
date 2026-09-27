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

/// Where a write lands, and how many of its bytes may be written, given the
/// file's length.
///
/// Two flags WinFsp passes change the answer, and ignoring either writes to
/// the wrong place:
///
/// - `write_to_eof` is an append: the offset the caller gave means nothing and
///   the bytes go at the current end.
/// - `constrained_io` marks paging I/O — the memory manager writing back a
///   mapped view — which must never move the end of the file. A page is a
///   whole 4 KiB even when the file ends partway through it, so an
///   unconstrained paging write pads the file out to a page boundary with
///   whatever the page held past the end; after a truncate it resurrects bytes
///   that were cut. It is clipped at the end, and dropped entirely past it.
pub fn place_write(
    size: u64,
    offset: u64,
    len: usize,
    write_to_eof: bool,
    constrained_io: bool,
) -> (u64, usize) {
    let at = match write_to_eof {
        true => size,
        false => offset,
    };
    if !constrained_io {
        return (at, len);
    }
    let room = size.saturating_sub(at);
    (at, len.min(usize::try_from(room).unwrap_or(usize::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_append_lands_at_the_end_whatever_offset_it_carries() {
        assert_eq!(place_write(100, 0, 10, true, false), (100, 10));
    }

    /// Paging I/O writes whole pages, and the part past the end of the file
    /// must not reach it.
    #[test]
    fn a_paging_write_is_clipped_at_the_end_of_the_file() {
        assert_eq!(place_write(5000, 4096, 4096, false, true), (4096, 904));
    }

    #[test]
    fn a_paging_write_past_the_end_writes_nothing() {
        assert_eq!(place_write(4096, 8192, 4096, false, true), (8192, 0));
    }

    #[test]
    fn an_ordinary_write_may_extend_the_file() {
        assert_eq!(place_write(10, 8, 100, false, false), (8, 100));
    }

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
