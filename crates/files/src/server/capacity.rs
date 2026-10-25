//! How large a volume is and how much of it is free, never answered with zero.

use std::path::Path;

/// Stand-in capacity for a volume whose real size cannot be read.
///
/// Never zero, and that is the whole point. A filesystem reporting a zero total
/// makes "percent used" a division by zero in every caller that computes it —
/// rclone's mount crashed programs with SIGFPE exactly this way — and a zero
/// free count makes others refuse to write, believing the disk full. A
/// plausible figure is wrong in a way nothing crashes on; zero is wrong in a way
/// that takes other programs down.
pub(crate) const FALLBACK_TOTAL: u64 = 1 << 40; // 1 TiB
pub(crate) const FALLBACK_FREE: u64 = 1 << 39; // half of it

/// Total and free bytes for the filesystem holding `path`.
///
/// Falls back to [`FALLBACK_TOTAL`]/[`FALLBACK_FREE`] rather than zero when the
/// query fails.
pub(crate) fn capacity_of(path: &Path) -> (u64, u64) {
    match raw_capacity(path) {
        // A total of zero is as dangerous as a failed query, so treat it the
        // same: some pseudo-filesystems really do report it.
        Some((total, free)) if total > 0 => (total, free),
        _ => (FALLBACK_TOTAL, FALLBACK_FREE),
    }
}

/// `statvfs` is POSIX, so one implementation covers both Unix hosts. The field
/// widths differ (macOS counts blocks in 32 bits, Linux in 64), which the casts
/// below absorb.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
pub(super) fn raw_capacity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // Safe: `c_path` is a valid NUL-terminated string and `stat` is owned here.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    // `f_frsize` is the fragment size the block counts are in — not `f_bsize`,
    // which is only a hint about efficient I/O size and can differ.
    let unit = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    // `f_bavail`, not `f_bfree`: the latter counts blocks reserved for root,
    // which an ordinary process cannot actually use.
    Some((stat.f_blocks as u64 * unit, stat.f_bavail as u64 * unit))
}

#[cfg(target_os = "windows")]
pub(super) fn raw_capacity(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_to_caller = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    // Safe: `wide` is a valid NUL-terminated wide string, and the out-params are
    // owned here. `GetDiskFreeSpaceExW` reports the caller's quota in the first
    // out-param, which is what a program can actually write.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        return None;
    }
    Some((total, free_to_caller))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows"
)))]
pub(super) fn raw_capacity(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capacity must never be reported as zero.
    ///
    /// A zero total makes "percent used" a division by zero in every caller that
    /// computes it — rclone's mount crashed programs with SIGFPE exactly this
    /// way — and a zero free count makes file managers refuse to copy, believing
    /// the disk full. A wrong-but-plausible figure breaks nothing; zero breaks
    /// other people's software.
    #[test]
    fn capacity_is_never_zero() {
        // A real path: whatever the system says, it must be usable.
        let (total, free) = capacity_of(Path::new("/"));
        assert!(total > 0, "a real filesystem must report a non-zero size");
        assert!(free <= total, "free space cannot exceed the total");

        // A path that cannot be queried falls back rather than answering zero.
        let (total, free) = capacity_of(Path::new("/nonexistent/path/for/this/test"));
        assert_eq!(total, FALLBACK_TOTAL);
        assert_eq!(free, FALLBACK_FREE);
        assert!(
            total > 0 && free > 0,
            "the fallback itself must be non-zero"
        );
    }
}
