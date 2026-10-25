//! Positioned reads and writes on an open file, and committing disk space for
//! a file ahead of the writes that fill it — each the way its platform does it.

use crate::server::to_file_error;
use cursio_protocol::FileError;
use std::fs::File;

/// Read `len` bytes from `offset`, without disturbing the file's cursor.
///
/// Short only at end of file: a short read reported as the whole answer would
/// silently truncate the file as the peer sees it, so this loops until the
/// buffer is full or the file really ends.
pub(super) fn read_at(file: &File, offset: u64, len: usize) -> Result<Vec<u8>, FileError> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;

    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    while filled < buf.len() {
        #[cfg(unix)]
        let got = file.read_at(&mut buf[filled..], offset + filled as u64);
        #[cfg(windows)]
        let got = file.seek_read(&mut buf[filled..], offset + filled as u64);
        match got {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(to_file_error(e)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Write `bytes` at `offset`, without disturbing the file's cursor.
///
/// Loops for the same reason a positioned read does: a partial write is not an
/// error, and stopping at one would leave a hole in the file the peer believes
/// it wrote.
pub(super) fn write_at(file: &File, offset: u64, bytes: &[u8]) -> Result<(), FileError> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;

    let mut written = 0usize;
    while written < bytes.len() {
        #[cfg(unix)]
        let put = file.write_at(&bytes[written..], offset + written as u64);
        #[cfg(windows)]
        let put = file.seek_write(&bytes[written..], offset + written as u64);
        match put {
            Ok(0) => {
                return Err(FileError::Io("the disk accepted no bytes".into()));
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(to_file_error(e)),
        }
    }
    Ok(())
}

/// Commit `size` bytes of storage to a file, without changing its length.
///
/// This is Windows' `AllocationSize`, relayed, and it keeps that contract:
/// `FileSize` stays where it was, and an allocation *below* the current length
/// truncates to it. Growing the length instead — which `posix_fallocate` does —
/// made every reservation look like a file already written to its full size,
/// so the mount reported gigabytes of zeroes as content and an append landed
/// after them rather than at the real end.
///
/// `fallocate(2)` with `FALLOC_FL_KEEP_SIZE` commits the blocks and leaves the
/// length alone. A filesystem that cannot do that (`EOPNOTSUPP`: FAT, some
/// network filesystems) keeps the file as it is: space not reserved in advance
/// is what every such filesystem gives a local program too, whereas faking the
/// reservation by moving the end of the file would corrupt what the caller
/// reads back.
///
/// `ENOSPC` is the answer that matters: an installer asking for room it cannot
/// have must be told so now rather than partway through a download.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn reserve_space(file: &File, size: u64) -> Result<(), FileError> {
    use std::os::fd::AsRawFd;

    let len = file.metadata().map_err(to_file_error)?.len();
    if size < len {
        return file.set_len(size).map_err(to_file_error);
    }
    if size == 0 {
        return Ok(());
    }
    // SAFETY: `fd` is owned by `file` and valid for this call; the mode, offset
    // and length are plain integers.
    let rc = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_KEEP_SIZE,
            0,
            size as libc::off_t,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EOPNOTSUPP) => {
            tracing::debug!(
                size,
                "this filesystem cannot reserve space; leaving the file as it is"
            );
            Ok(())
        }
        _ => Err(to_file_error(err)),
    }
}

/// Windows commits space through the file's own allocation size, which is
/// exactly the request being relayed — so it is passed through as that, not as
/// `set_len`, which would move the end of the file.
#[cfg(target_os = "windows")]
pub(super) fn reserve_space(file: &File, size: u64) -> Result<(), FileError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileAllocationInfo, SetFileInformationByHandle, FILE_ALLOCATION_INFO,
    };

    let info = FILE_ALLOCATION_INFO {
        AllocationSize: size as i64,
    };
    // SAFETY: the handle is owned by `file` and valid for this call; `info` is
    // a live, correctly sized `FILE_ALLOCATION_INFO` for the class named.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileAllocationInfo,
            (&info as *const FILE_ALLOCATION_INFO).cast(),
            std::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    };
    match ok {
        0 => Err(to_file_error(std::io::Error::last_os_error())),
        _ => Ok(()),
    }
}

/// No way to reserve without moving the end of the file here, so only the
/// truncating half of the contract is honoured.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "windows")))]
pub(super) fn reserve_space(file: &File, size: u64) -> Result<(), FileError> {
    let len = file.metadata().map_err(to_file_error)?.len();
    if size < len {
        return file.set_len(size).map_err(to_file_error);
    }
    Ok(())
}
