//! A byte buffer read through a cursor, for the decoders that frame a
//! connection's stream.
//!
//! Consumed bytes are released by advancing the cursor rather than by
//! draining the front of the buffer: a mirror stream at 60 fps delivers
//! thousands of packets a second, and pipelined requests on a control
//! connection arrive several at once, so re-copying the remainder after each
//! one would dominate the decode path. The buffer is compacted only once the
//! consumed prefix is at least as large as what is left, which keeps the
//! amortized cost per byte constant.

#[derive(Debug, Default)]
pub(crate) struct CursorBuffer {
    buffered: Vec<u8>,
    start: usize,
}

impl CursorBuffer {
    /// What has arrived and not yet been consumed.
    pub(crate) fn pending(&self) -> &[u8] {
        &self.buffered[self.start..]
    }

    /// Mark the first `len` pending bytes consumed.
    pub(crate) fn consume(&mut self, len: usize) {
        debug_assert!(len <= self.pending().len());
        self.start += len;
    }

    /// Append `bytes`, unless that would leave more than `limit` pending —
    /// answered `false` with nothing appended.
    pub(crate) fn push(&mut self, bytes: &[u8], limit: usize) -> bool {
        if self.pending().len().saturating_add(bytes.len()) > limit {
            return false;
        }
        self.compact();
        self.buffered.extend_from_slice(bytes);
        true
    }

    /// Bytes held, consumed or not: what compaction keeps bounded.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.buffered.len()
    }

    fn compact(&mut self) {
        if self.start == 0 {
            return;
        }
        if self.start == self.buffered.len() {
            self.buffered.clear();
            self.start = 0;
        } else if self.start >= self.buffered.len() - self.start {
            self.buffered.drain(..self.start);
            self.start = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumed_bytes_leave_the_pending_view_and_a_push_over_the_limit_is_refused() {
        let mut buf = CursorBuffer::default();
        assert!(buf.push(b"abcdef", 8));
        buf.consume(4);
        assert_eq!(buf.pending(), b"ef");
        assert!(!buf.push(b"1234567", 8));
        assert_eq!(buf.pending(), b"ef");
        assert!(buf.push(b"gh", 8));
        assert_eq!(buf.pending(), b"efgh");
    }
}
