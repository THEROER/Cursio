//! Converting between Windows' vocabulary and the wire's.

use cursio_protocol::{FileError, FileKind, FileMeta};

/// `FILE_ATTRIBUTE_DIRECTORY`, `_READONLY`, `_NORMAL`, `_REPARSE_POINT`.
///
/// Spelled out rather than pulled from the `windows` crate: these four values
/// are fixed by the Win32 ABI and will not change, and depending on that crate
/// here would be a large dependency for four constants.
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// Kept for the test that pins down what we must *not* claim; see
/// [`attributes_of`].
#[cfg_attr(not(test), allow(dead_code))]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Windows counts 100-nanosecond ticks from 1601-01-01; Unix counts seconds from
/// 1970-01-01. This is the gap between those epochs, in seconds.
const EPOCH_GAP_SECS: i64 = 11_644_473_600;

/// Ticks per second in a FILETIME.
const TICKS_PER_SEC: i64 = 10_000_000;

/// Unix seconds to FILETIME.
///
/// Saturating, not wrapping: a timestamp far outside the representable range is
/// a corrupt or absurd value, and clamping shows it as very old or very new
/// rather than wrapping it around to a plausible-looking lie.
pub fn to_filetime(unix_secs: i64) -> u64 {
    unix_secs
        .saturating_add(EPOCH_GAP_SECS)
        .saturating_mul(TICKS_PER_SEC)
        .max(0) as u64
}

/// FILETIME back to Unix seconds.
///
/// Nothing converts this direction in production — the peer's timestamps only
/// ever travel outward — but it is what proves [`to_filetime`] round-trips, and
/// a one-way conversion with no inverse is exactly where an epoch bug hides.
#[cfg_attr(not(test), allow(dead_code))]
pub fn from_filetime(ft: u64) -> i64 {
    (ft as i64) / TICKS_PER_SEC - EPOCH_GAP_SECS
}

/// Windows file attributes for one wire entry.
pub fn attributes_of(meta: &FileMeta) -> u32 {
    let mut attrs = match meta.kind {
        FileKind::Dir => FILE_ATTRIBUTE_DIRECTORY,
        FileKind::File => FILE_ATTRIBUTE_NORMAL,
        // A symlink is reported as an ordinary file, not a reparse point.
        //
        // The reparse attribute promises Windows that the entry carries reparse
        // *data* it can follow, and this filesystem serves none — so Explorer
        // showed `/bin`, `/lib` and `/sbin` (symlinks to directories on any
        // modern Linux) as 1 KB files that open into nothing. Since the peer
        // does not send link targets either, the honest answer is to describe
        // what can actually be read.
        FileKind::Symlink => FILE_ATTRIBUTE_NORMAL,
    };
    if !meta.writable {
        attrs |= FILE_ATTRIBUTE_READONLY;
    }
    attrs
}

/// Directory name for a volume, when a peer offers more than one.
///
/// The rule is shared with the Linux mount — see [`crate::volume_dir_name`].
/// A colon cannot appear in a Windows filename at all, which is why this side
/// has always dropped it; the other side now does the same, for tools rather
/// than for the OS.
pub use crate::volume_dir_name as dir_name_of;

/// Map a wire error onto the NTSTATUS WinFsp expects.
///
/// Values from `ntstatus.h`; like the attribute constants, these are ABI-fixed.
pub fn to_ntstatus(e: &FileError) -> winfsp::FspError {
    const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034u32 as i32;
    const STATUS_ACCESS_DENIED: i32 = 0xC0000022u32 as i32;
    const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC00000A2u32 as i32;
    const STATUS_NOT_A_DIRECTORY: i32 = 0xC0000103u32 as i32;
    const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC0000101u32 as i32;
    const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC0000035u32 as i32;
    const STATUS_INVALID_HANDLE: i32 = 0xC0000008u32 as i32;
    const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185u32 as i32;

    // `Io` is the one that reaches the user as a broken disk rather than a
    // missing file: Windows renders `STATUS_IO_DEVICE_ERROR` as "the device is
    // not ready", and Steam reports it as "Disk write failure". Every other
    // kind here is an ordinary answer an application knows how to handle, so
    // only this one is worth a line — and it needs one, because the message
    // explaining *why* is thrown away by the conversion.
    if let FileError::Io(why) = e {
        tracing::warn!(why, "answering Windows with a device error");
    }

    winfsp::FspError::NTSTATUS(match e {
        FileError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        FileError::Denied => STATUS_ACCESS_DENIED,
        FileError::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
        FileError::WrongKind => STATUS_NOT_A_DIRECTORY,
        FileError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        FileError::Exists => STATUS_OBJECT_NAME_COLLISION,
        FileError::BadHandle => STATUS_INVALID_HANDLE,
        FileError::Io(_) => STATUS_IO_DEVICE_ERROR,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The drive-letter search must skip letters in use.
    ///
    /// Asking WinFsp for "the next free drive" left the filesystem mounted but
    /// unlettered — `fsptool lsvol` showed the volume with a `-` where its drive
    /// should be, reachable by nothing. The letter is chosen here instead, so
    /// the bit arithmetic behind that choice is worth pinning down; mounting
    /// onto a letter already in use fails outright.
    #[test]
    fn the_drive_letter_search_reads_the_mask_correctly() {
        // Mirrors the predicate in `fs::free_drive_letter`, which cannot run on
        // this host: bit 0 is A:, bit 2 is C:.
        let taken = |used: u32, c: char| used & (1 << (c as u8 - b'A')) != 0;

        let used = 0b0000_0101; // A: and C:
        assert!(taken(used, 'A'));
        assert!(!taken(used, 'B'));
        assert!(taken(used, 'C'));
        assert!(!taken(used, 'Z'));

        // Everything but Z: taken — a downward search must land on it.
        let almost_full = !(1u32 << (b'Z' - b'A'));
        assert_eq!(
            ('D'..='Z').rev().find(|c| !taken(almost_full, *c)),
            Some('Z')
        );

        // Nothing free is an error, not a silent collision.
        assert_eq!(('D'..='Z').rev().find(|c| !taken(u32::MAX, *c)), None);

        // C: is never offered even when somehow free — the low letters belong
        // to real hardware.
        assert!(!('D'..='Z').contains(&'C'));
    }

    /// The epoch conversion must round-trip, or every file's timestamp is
    /// wrong by 369 years.
    #[test]
    fn filetime_round_trips() {
        for secs in [0i64, 1_700_000_000, -86_400] {
            assert_eq!(from_filetime(to_filetime(secs)), secs, "for {secs}");
        }
        // The Unix epoch is exactly the documented gap.
        assert_eq!(to_filetime(0), EPOCH_GAP_SECS as u64 * TICKS_PER_SEC as u64);
    }

    /// An absurd timestamp must clamp rather than wrap into something that
    /// looks plausible.
    #[test]
    fn extreme_timestamps_clamp() {
        assert_eq!(
            to_filetime(i64::MIN),
            0,
            "far past clamps to the FILETIME epoch"
        );
        // Far future saturates instead of wrapping to a small number.
        assert!(to_filetime(i64::MAX) > 0);
    }

    /// A colon is illegal in a Windows filename, so a drive id has to lose it.
    #[test]
    fn volume_directory_names_are_legal_on_windows() {
        assert_eq!(dir_name_of("C:"), "C");
        assert_eq!(dir_name_of("/"), "root");
        for id in ["C:", "/", "D:"] {
            let name = dir_name_of(id);
            assert!(
                !name.contains(':') && !name.contains('\\') && !name.contains('/'),
                "{name:?} is not a legal Windows filename"
            );
        }
    }

    /// Read-only and directory attributes must both survive; Explorer uses them
    /// to decide what it may offer to do with a file.
    #[test]
    fn attributes_reflect_kind_and_writability() {
        let dir = FileMeta {
            name: "d".into(),
            kind: FileKind::Dir,
            size: 0,
            modified_unix: 0,
            writable: true,
            allocated: None,
        };
        assert_ne!(attributes_of(&dir) & FILE_ATTRIBUTE_DIRECTORY, 0);
        assert_eq!(attributes_of(&dir) & FILE_ATTRIBUTE_READONLY, 0);

        let ro_file = FileMeta {
            name: "f".into(),
            kind: FileKind::File,
            size: 1,
            modified_unix: 0,
            writable: false,
            allocated: None,
        };
        assert_ne!(attributes_of(&ro_file) & FILE_ATTRIBUTE_READONLY, 0);
        assert_eq!(attributes_of(&ro_file) & FILE_ATTRIBUTE_DIRECTORY, 0);

        let link = FileMeta {
            name: "l".into(),
            kind: FileKind::Symlink,
            size: 0,
            modified_unix: 0,
            writable: true,
            allocated: None,
        };
        assert_eq!(
            attributes_of(&link) & FILE_ATTRIBUTE_REPARSE_POINT,
            0,
            "claiming a reparse point without reparse data to back it makes \
             Explorer show the link as an unopenable 1 KB file"
        );
    }
}
