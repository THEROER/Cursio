//! FNV-1a, 64-bit: a hash whose value is the same in every process and build.
//!
//! What it hashes here outlives the process that computed it — a file's
//! version stamp crosses the wire and is compared by another machine, and a
//! cache directory's name must be found again after a restart. `DefaultHasher`
//! is seeded per process and its algorithm is explicitly unspecified, so it
//! cannot be used for either. FNV is not resistant to a chosen collision, and
//! nothing here needs that: the inputs are a file's own metadata and paths the
//! user already owns, and every consumer checks the full key it stored beside
//! the hash.

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Copy)]
pub(crate) struct Fnv64(u64);

impl Default for Fnv64 {
    fn default() -> Self {
        Self(OFFSET)
    }
}

impl Fnv64 {
    pub(crate) fn write(&mut self, bytes: &[u8]) -> &mut Self {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(PRIME);
        }
        self
    }

    pub(crate) fn write_u64(&mut self, v: u64) -> &mut Self {
        self.write(&v.to_le_bytes())
    }

    /// A string followed by a byte no UTF-8 string contains, so that
    /// `("ab", "c")` and `("a", "bc")` hash apart.
    pub(crate) fn write_str(&mut self, s: &str) -> &mut Self {
        self.write(s.as_bytes()).write(&[0xff])
    }

    pub(crate) fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::Fnv64;

    /// Pinned: a stamp computed by one build is compared by another, and a
    /// cache directory named by one run is looked up by the next.
    #[test]
    fn the_hash_of_a_known_input_never_changes() {
        assert_eq!(Fnv64::default().finish(), 0xcbf2_9ce4_8422_2325);
        assert_eq!(Fnv64::default().write(b"a").finish(), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(
            Fnv64::default().write(b"foobar").finish(),
            0x8594_4171_f739_67e8
        );
    }

    #[test]
    fn strings_hash_apart_however_they_are_split() {
        let a = Fnv64::default().write_str("ab").write_str("c").finish();
        let b = Fnv64::default().write_str("a").write_str("bc").finish();
        assert_ne!(a, b);
    }
}
