//! Describing a peer's file to Windows: the `FileInfo` it reads, the names in a
//! listing, and the directories that exist only here.

use crate::winmount::convert::{attributes_of, to_filetime, to_ntstatus};
use crate::winmount::fs::SECTOR_SIZE;
use cursio_protocol::{FileError, FileKind, FileMeta};
use widestring::U16CStr;
use winfsp::filesystem::{DirInfo, FileInfo, OpenFileInfo, WideNameInfo};

/// A directory that exists only in this presentation: the mount root, or a
/// volume's own folder.
pub(super) fn synthetic_dir(name: &str) -> FileMeta {
    FileMeta {
        name: name.to_string(),
        kind: FileKind::Dir,
        size: 0,
        modified_unix: 0,
        writable: true,
        allocated: None,
    }
}

/// Write a directory entry's name, giving its length without the terminator.
///
/// `DirInfo::set_name` appends a NUL and then reports the length *including*
/// it, while WinFsp takes that length as the name itself — so every entry went
/// out two bytes long, `a.o` described as `a.o\0`. A plain listing tolerated the
/// stray character, but a filtered one did not: the kernel matched the padded
/// name against the caller's pattern, `<.O` failed to match `a.o\0`, and a
/// search for `*.o` came back empty in a directory that held exactly that file.
/// That empty answer is what cargo reported as `ERROR_INVALID_NAME` while
/// clearing its `.temp-archive` directories.
pub(super) fn set_entry_name(info: &mut DirInfo, name: &str) -> winfsp::Result<()> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    info.set_name_raw(wide.as_slice())
        .map_err(|_| to_ntstatus(&FileError::Io("name too long".into())))
}

/// Tell Windows the spelling this filesystem actually uses for an opened path.
///
/// The volume declares itself case-insensitive but case-preserving, which is a
/// promise to answer *what the name really is* — not merely to tolerate any
/// casing on the way in. Leaving the normalized name unset broke that promise:
/// Windows then substituted its own upper-cased form, and it reached the peer
/// verbatim. A rename arrived as
/// `TMP/CURSIO-WRITE-TEST/DEPS/LIB.RMETA.TMP` → `.../lib.rmeta` — the source
/// spelled in capitals, the target as typed — and since the peer's filesystem
/// is case-*sensitive*, the source did not exist there and the rename failed
/// with `NotFound`. Cargo builds on the drive died this way: every `.rmeta` is
/// written to a temporary file and renamed into place.
///
/// Opening an existing file on a path already cached in the other casing failed
/// for the same reason.
///
/// The name handed back is the one WinFsp passed in, which is the caller's own
/// spelling and the one the peer stores.
pub(super) fn set_normalized_name(file_info: &mut OpenFileInfo, file_name: &U16CStr) {
    // Without the terminator: the length is taken from the slice, and a
    // trailing NUL would be counted as part of the name.
    file_info.set_normalized_name(file_name.as_slice(), None);
}

/// A stable file id for a path, in place of the inode number this filesystem
/// does not have.
///
/// Windows enumeration classes that carry a file id — `FileIdBothDirectoryInfo`
/// and its `Restart` and `ExtdDirectory` variants — are refused outright by the
/// kernel when the volume reports id zero for every entry. Rust's
/// `remove_dir_all` walks a directory with exactly those classes, so it failed
/// with `ERROR_INVALID_PARAMETER` before it deleted anything, while Node, which
/// enumerates with `FindFirstFile`, was unaffected. Measured directly: classes
/// 13 and 20 returned error 87, classes 10 and 15 — which carry no id —
/// succeeded on the same directory.
///
/// The id is derived from the path rather than tracked in a table: it must be
/// stable for as long as the name exists, which hashing gives for free, and
/// nothing here addresses a file by id the way FUSE does by inode. A renamed
/// file changes id, which is the same thing that happens when a file moves
/// between volumes on NTFS.
pub(super) fn file_id_of(volume: &str, path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    volume.hash(&mut h);
    path.hash(&mut h);
    // Zero means "no id", so it cannot be handed out as one.
    h.finish() | 1
}

/// Fill a `FileInfo` from wire metadata.
pub(super) fn fill_info_at(info: &mut FileInfo, meta: &FileMeta, volume: &str, path: &str) {
    fill_info(info, meta);
    info.index_number = file_id_of(volume, path);
}

/// Fill a `FileInfo` from wire metadata, leaving the file id unset.
pub(super) fn fill_info(info: &mut FileInfo, meta: &FileMeta) {
    let ft = to_filetime(meta.modified_unix);
    info.file_attributes = attributes_of(meta);
    info.reparse_tag = 0;
    info.file_size = meta.size;
    // Rounded up to a cluster, which is what Windows expects an allocation to
    // be; reporting the exact size makes some tools compute negative slack.
    info.allocation_size = meta.size.div_ceil(SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    info.creation_time = ft;
    info.last_access_time = ft;
    info.last_write_time = ft;
    info.change_time = ft;
    info.index_number = 0;
    info.hard_links = 0;
    info.ea_size = 0;
}
