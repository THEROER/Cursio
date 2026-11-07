//! `CF_HDROP`'s payload, the `DROPFILES` block, built and read.
//!
//! Kept apart from the Windows backend because it is only bytes: the two
//! directions are each other's inverse, and that is tested on every platform,
//! not only where the clipboard that carries them exists.

use std::path::PathBuf;

/// Size of the `DROPFILES` header: `pFiles`, a point, `fNC`, `fWide`.
const HEADER: usize = 20;

/// A `DROPFILES` block: the 20-byte header (`pFiles`, a zero point, `fNC`,
/// `fWide`) then each path as NUL-terminated UTF-16, and one more NUL to end
/// the list. Explorer and every file dialog read pasted files from this.
pub(crate) fn drop_files(paths: &[PathBuf]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(HEADER as u32).to_le_bytes()); // pFiles: offset of the list
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    bytes.extend_from_slice(&0u32.to_le_bytes()); // fNC
    bytes.extend_from_slice(&1u32.to_le_bytes()); // fWide: UTF-16 paths
    for path in paths {
        for unit in path.as_os_str().to_string_lossy().encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

/// The paths a `DROPFILES` block lists — what Explorer puts on the clipboard
/// when files are copied.
///
/// The list starts where `pFiles` says, not at a fixed offset, since a writer
/// may append fields of its own after the header. A block with `fWide` unset
/// holds ANSI paths; nothing current writes one, and they are read as UTF-8,
/// which is right for every ASCII path and lossy past that.
pub(crate) fn dropped_paths(bytes: &[u8]) -> Vec<PathBuf> {
    let Some(header) = bytes.get(..HEADER) else {
        return Vec::new();
    };
    let field = |at: usize| {
        u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    let list = bytes.get(field(0) as usize..).unwrap_or_default();
    if field(16) != 0 {
        let units: Vec<u16> = list
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        units
            .split(|&u| u == 0)
            .take_while(|name| !name.is_empty())
            .map(|name| PathBuf::from(String::from_utf16_lossy(name)))
            .collect()
    } else {
        list.split(|&b| b == 0)
            .take_while(|name| !name.is_empty())
            .map(|name| PathBuf::from(String::from_utf8_lossy(name).into_owned()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Explorer reads back is the list that was written, in order.
    #[test]
    fn a_dropped_file_list_reads_back_as_written() {
        let paths = [
            PathBuf::from(r"\\cursio\Nocturne-root\home\me\фото 1.png"),
            PathBuf::from(r"C:\Users\me\Projects"),
        ];
        assert_eq!(dropped_paths(&drop_files(&paths)), paths);
    }

    /// `pFiles` names where the list begins; a writer with a longer header is
    /// read from there rather than from byte 20.
    #[test]
    fn the_list_is_read_from_where_the_header_says() {
        let mut bytes = drop_files(&[PathBuf::from("a.txt")]);
        bytes.splice(HEADER..HEADER, [0xAA; 8]);
        bytes[..4].copy_from_slice(&((HEADER + 8) as u32).to_le_bytes());
        assert_eq!(dropped_paths(&bytes), [PathBuf::from("a.txt")]);
    }

    #[test]
    fn a_truncated_block_lists_nothing() {
        assert!(dropped_paths(&[0; 10]).is_empty());
        assert!(dropped_paths(&drop_files(&[])).is_empty());
    }
}
