//! A read buffer that fetches ahead of what was asked for.
//!
//! An application reading a file sequentially asks in whatever size suits it —
//! 4 KiB is common, and a program copying a file may ask for 64 KiB. Each such
//! request was a round trip, so a 9 ms link served a 4 KiB read at about
//! 440 KB/s no matter how much bandwidth was free: the link spent all its time
//! waiting rather than carrying.
//!
//! So a read fetches a whole chunk ([`CHUNK`]) and keeps it. The reads that
//! follow inside that chunk are answered from memory, and one round trip serves
//! however many small reads the caller makes of it.
//!
//! ## Why sequential access only
//!
//! Reading ahead is a bet: the bytes after the ones asked for are fetched on
//! the assumption they will be wanted. For sequential reading that bet always
//! pays. For random access — a database, or a game seeking around a texture
//! pack — it never does, and every fetch would carry a chunk to serve 4 KiB of
//! it.
//!
//! So the buffer watches: it reads ahead only while the caller keeps arriving
//! where the last read ended. One seek backwards, or a jump past the buffer,
//! and it fetches exactly what was asked for until the pattern returns.
//!
//! ## Consistency
//!
//! The buffer holds file *contents*, which makes it the one cache in this crate
//! that can turn a stale answer into corruption. It is therefore dropped the
//! moment anything writes through this handle, and it never outlives the handle
//! it belongs to — nothing here survives a `close`.

use std::sync::Mutex;

/// How much to fetch when reading ahead.
///
/// One wire chunk: the largest a single message carries, so a fetch of this
/// size costs exactly one round trip and no fragmentation. Larger would mean
/// several messages for one fill, which is the round-trip cost this exists to
/// avoid.
pub const CHUNK: usize = cursio_protocol::FilesMsg::MAX_CHUNK;

/// A file's read buffer.
#[derive(Default)]
struct Window {
    /// Where `data` starts in the file.
    start: u64,
    data: Vec<u8>,
    /// Where the previous read ended, for deciding whether access looks
    /// sequential.
    next_expected: Option<u64>,
}

/// Buffered reads for one open file.
pub struct ReadAhead {
    window: Mutex<Window>,
}

impl Default for ReadAhead {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadAhead {
    pub fn new() -> Self {
        Self {
            window: Mutex::new(Window::default()),
        }
    }

    /// Serve a read, fetching through `fetch` when the buffer cannot.
    ///
    /// `fetch(offset, len)` must return the bytes at that position, short only
    /// at end of file — exactly the contract of a `read` on the wire.
    pub fn read<E>(
        &self,
        offset: u64,
        want: usize,
        fetch: impl FnOnce(u64, u32) -> Result<Vec<u8>, E>,
    ) -> Result<Vec<u8>, E> {
        let mut w = self.window.lock().expect("read window");

        // Already held?
        if let Some(slice) = take_from(&w, offset, want) {
            w.next_expected = Some(offset + slice.len() as u64);
            return Ok(slice);
        }

        // Read ahead only where the caller is moving forwards through the file;
        // see the note on random access above.
        let sequential = w.next_expected.is_none_or(|at| at == offset);
        let fetch_len = match sequential {
            true => want.max(CHUNK),
            false => want,
        };

        let data = fetch(offset, fetch_len as u32)?;
        let answer_len = want.min(data.len());
        let answer = data[..answer_len].to_vec();

        // Keep the fetch only when it holds more than the answer; otherwise
        // there is nothing to serve a later read from and the memory is waste.
        if sequential && data.len() > answer_len {
            w.start = offset;
            w.data = data;
        } else {
            w.data.clear();
        }
        w.next_expected = Some(offset + answer_len as u64);
        Ok(answer)
    }

    /// Drop what is held, because the file has changed under it.
    pub fn invalidate(&self) {
        let mut w = self.window.lock().expect("read window");
        w.data.clear();
        w.next_expected = None;
    }
}

/// The requested range, if the buffer holds all of it.
fn take_from(w: &Window, offset: u64, want: usize) -> Option<Vec<u8>> {
    if w.data.is_empty() || offset < w.start {
        return None;
    }
    let from = (offset - w.start) as usize;
    if from >= w.data.len() {
        return None;
    }
    // A partial hit is still a hit: the caller gets what is held and asks again
    // for the rest, exactly as it would at end of file. Returning nothing would
    // throw away a fetch that has already crossed the wire.
    let to = (from + want).min(w.data.len());
    Some(w.data[from..to].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A file of `len` bytes whose contents are their own offsets, and a count
    /// of how many times the wire was touched.
    struct Fake {
        len: usize,
        fetches: RefCell<Vec<(u64, u32)>>,
    }

    impl Fake {
        fn new(len: usize) -> Self {
            Self {
                len,
                fetches: RefCell::new(Vec::new()),
            }
        }

        fn fetch(&self, offset: u64, want: u32) -> Result<Vec<u8>, ()> {
            self.fetches.borrow_mut().push((offset, want));
            let start = (offset as usize).min(self.len);
            let end = (start + want as usize).min(self.len);
            Ok((start..end).map(|i| i as u8).collect())
        }

        fn round_trips(&self) -> usize {
            self.fetches.borrow().len()
        }
    }

    /// The point of the whole file: many small sequential reads, one round trip.
    #[test]
    fn small_sequential_reads_share_one_fetch() {
        let f = Fake::new(CHUNK * 2);
        let ra = ReadAhead::new();

        for i in 0..16u64 {
            let got = ra
                .read(i * 4096, 4096, |o, n| f.fetch(o, n))
                .expect("reads");
            assert_eq!(got.len(), 4096);
            assert_eq!(got[0], (i * 4096) as u8, "wrong bytes at {}", i * 4096);
        }
        assert_eq!(
            f.round_trips(),
            1,
            "16 reads inside one chunk must cost one round trip, not 16"
        );
    }

    /// Random access must not drag a chunk across the wire for every 4 KiB.
    #[test]
    fn scattered_reads_fetch_only_what_was_asked_for() {
        let f = Fake::new(CHUNK * 8);
        let ra = ReadAhead::new();

        // The first read has nothing to go on and reads ahead; after that the
        // seeks are plainly not sequential.
        ra.read(0, 4096, |o, n| f.fetch(o, n)).expect("reads");
        for at in [CHUNK * 5, 4096, CHUNK * 7, 8192] {
            ra.read(at as u64, 4096, |o, n| f.fetch(o, n))
                .expect("reads");
        }

        let asked: Vec<u32> = f.fetches.borrow().iter().skip(1).map(|(_, n)| *n).collect();
        assert!(
            asked.iter().all(|&n| n == 4096),
            "random access must not read ahead; asked for {asked:?}"
        );
    }

    /// The bytes served must be the bytes at that offset, buffered or not.
    #[test]
    fn buffered_bytes_are_the_right_bytes() {
        let f = Fake::new(CHUNK * 2);
        let ra = ReadAhead::new();

        ra.read(0, 100, |o, n| f.fetch(o, n)).expect("reads");
        let got = ra.read(100, 50, |o, n| f.fetch(o, n)).expect("reads");
        let want: Vec<u8> = (100..150).map(|i| i as u8).collect();
        assert_eq!(got, want, "a buffered read returned the wrong bytes");
    }

    /// End of file: a read past the end comes back short, not wrong.
    #[test]
    fn a_read_past_the_end_comes_back_short() {
        let f = Fake::new(1000);
        let ra = ReadAhead::new();

        let got = ra.read(900, 4096, |o, n| f.fetch(o, n)).expect("reads");
        assert_eq!(got.len(), 100, "only 100 bytes remain");

        let none = ra.read(1000, 4096, |o, n| f.fetch(o, n)).expect("reads");
        assert!(none.is_empty(), "nothing remains at the end");
    }

    /// A write through the handle must not leave stale bytes readable.
    ///
    /// This is the one cache here whose staleness is corruption rather than a
    /// wrong number, so it is dropped rather than left to expire.
    #[test]
    fn invalidating_drops_what_was_held() {
        let f = Fake::new(CHUNK * 2);
        let ra = ReadAhead::new();

        ra.read(0, 4096, |o, n| f.fetch(o, n)).expect("reads");
        assert_eq!(f.round_trips(), 1);

        ra.invalidate();
        ra.read(4096, 4096, |o, n| f.fetch(o, n)).expect("reads");
        assert_eq!(
            f.round_trips(),
            2,
            "after invalidation the bytes must come from the peer again"
        );
    }
}
