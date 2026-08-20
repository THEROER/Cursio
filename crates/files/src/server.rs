//! Serving this machine's volumes to the peer.
//!
//! Turns [`FilesMsg`] requests into local filesystem calls. Everything here runs
//! on behalf of *the other machine*, so the guiding rule is that a request is
//! untrusted input: a path is a string that arrived over a socket, not something
//! this process constructed.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use cursio_protocol::{FileError, FileHandle, FileKind, FileMeta, OpenMode, Volume};

/// A volume this machine offers, and where it actually lives.
pub struct SharedVolume {
    pub info: Volume,
    /// Native path the volume's root maps to: `/` or `C:\`.
    pub root: PathBuf,
}

/// Serves the local filesystem to one peer.
///
/// Owns the open-file table: handles are indices into it, so a handle the peer
/// invents addresses nothing.
pub struct FileServer {
    volumes: Vec<SharedVolume>,
    open: HashMap<FileHandle, OpenFile>,
    next_handle: FileHandle,
}

struct OpenFile {
    file: File,
    writable: bool,
}

impl FileServer {
    /// Offer the volumes this machine has.
    pub fn new(volumes: Vec<SharedVolume>) -> Self {
        Self {
            volumes,
            open: HashMap::new(),
            next_handle: 1,
        }
    }

    /// Detect what this machine can share.
    ///
    /// Linux gets one volume — `/` — because everything, including other mounts
    /// and removable media, already hangs off it. Windows has no such single
    /// root, so each fixed drive letter is its own volume.
    pub fn detect() -> Self {
        Self::new(detect_volumes())
    }

    /// What to announce to the peer.
    pub fn volumes(&self) -> Vec<Volume> {
        self.volumes.iter().map(|v| v.info.clone()).collect()
    }

    /// Resolve a wire path (volume id + `/`-separated relative path) to a native
    /// path inside that volume.
    ///
    /// This is the security boundary. The peer's path is rejected unless it is
    /// strictly relative and free of `..`: a request for `../../etc/shadow`, an
    /// absolute path, or a Windows drive prefix must not be able to address
    /// anything outside the volume it names. `..` is refused outright rather
    /// than normalised away, because normalising is only equivalent when no
    /// component is a symlink — and the peer chooses the components.
    pub fn resolve(&self, volume: &str, path: &str) -> Result<PathBuf, FileError> {
        let vol = self
            .volumes
            .iter()
            .find(|v| v.info.id == volume)
            .ok_or(FileError::BadHandle)?;

        let mut out = vol.root.clone();
        for part in path.split('/') {
            // Empty parts come from leading, trailing and doubled separators and
            // mean "no component"; `.` is a no-op.
            if part.is_empty() || part == "." {
                continue;
            }
            if part == ".." {
                return Err(FileError::Denied);
            }
            // A component must be exactly one plain name. Anything the host OS
            // would read as a root, a drive or a separator is refused — on
            // Windows `C:` inside a component would otherwise re-root the path.
            let candidate = Path::new(part);
            let mut comps = candidate.components();
            match (comps.next(), comps.next()) {
                (Some(Component::Normal(name)), None) => out.push(name),
                _ => return Err(FileError::Denied),
            }
        }
        Ok(out)
    }

    /// Whether a volume accepts writes at all.
    fn writable(&self, volume: &str) -> bool {
        self.volumes
            .iter()
            .find(|v| v.info.id == volume)
            .is_some_and(|v| v.info.writable)
    }

    /// List a directory.
    pub fn read_dir(&self, volume: &str, path: &str) -> Result<Vec<FileMeta>, FileError> {
        let dir = self.resolve(volume, path)?;
        let writable = self.writable(volume);
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(to_file_error)? {
            let entry = entry.map_err(to_file_error)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // `symlink_metadata`, not `metadata`: a listing must describe the
            // link itself, or a link pointing outside the volume would be
            // reported as whatever it targets.
            let md = match entry.path().symlink_metadata() {
                Ok(md) => md,
                // A file can vanish between reading the directory and stat'ing
                // it. Skipping it is right — it is genuinely no longer there.
                Err(_) => continue,
            };
            out.push(meta_from(name, &md, writable));
        }
        Ok(out)
    }

    /// Metadata for one path.
    pub fn stat(&self, volume: &str, path: &str) -> Result<FileMeta, FileError> {
        let target = self.resolve(volume, path)?;
        let md = target.symlink_metadata().map_err(to_file_error)?;
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());
        Ok(meta_from(name, &md, self.writable(volume)))
    }

    /// Read a symlink's target, exactly as stored.
    ///
    /// Deliberately not resolved: following it here would let a link inside the
    /// volume address anything on this disk, which is the whole reason a listing
    /// reports links rather than what they point at. The target crosses the wire
    /// as written and the mount side interprets it against the mounted tree,
    /// where a link leading out of the volume simply dangles.
    ///
    /// Windows targets keep their backslashes. Rewriting them would be a guess
    /// about a path this side never resolves, and a dangling link that reads
    /// back what the owner stored is easier to explain than one silently
    /// rewritten into a different dangling path.
    pub fn read_link(&self, volume: &str, path: &str) -> Result<String, FileError> {
        let target = self.resolve(volume, path)?;
        // `symlink_metadata` first: `read_link` on a regular file reports
        // `InvalidInput`, which `to_file_error` cannot tell from a malformed
        // request. The caller asked about the wrong kind of thing, and
        // `WrongKind` says so.
        let md = target.symlink_metadata().map_err(to_file_error)?;
        if !md.file_type().is_symlink() {
            return Err(FileError::WrongKind);
        }
        let dest = std::fs::read_link(&target).map_err(to_file_error)?;
        dest.to_str()
            .map(str::to_owned)
            // Wire paths are UTF-8. A target that is not says so plainly rather
            // than arriving mangled by a lossy conversion.
            .ok_or(FileError::Io("link target is not valid utf-8".into()))
    }

    /// Open a file, returning a handle and its current size.
    pub fn open(
        &mut self,
        volume: &str,
        path: &str,
        mode: OpenMode,
    ) -> Result<(FileHandle, u64), FileError> {
        if mode.is_mutating() && !self.writable(volume) {
            return Err(FileError::ReadOnly);
        }
        let target = self.resolve(volume, path)?;
        let file = OpenOptions::new()
            .read(mode.read)
            .write(mode.write)
            .create(mode.create && !mode.exclusive)
            .create_new(mode.exclusive)
            .truncate(mode.truncate)
            .open(&target)
            .map_err(to_file_error)?;
        let size = file.metadata().map_err(to_file_error)?.len();

        let handle = self.next_handle;
        self.next_handle += 1;
        self.open.insert(
            handle,
            OpenFile {
                file,
                writable: mode.write,
            },
        );
        Ok((handle, size))
    }

    /// Read from an open handle. Returns fewer bytes than asked at end of file.
    pub fn read(
        &mut self,
        handle: FileHandle,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, FileError> {
        let open = self.open.get_mut(&handle).ok_or(FileError::BadHandle)?;
        open.file
            .seek(SeekFrom::Start(offset))
            .map_err(to_file_error)?;
        let mut buf = vec![0u8; len as usize];
        let mut filled = 0usize;
        // `read` may return short for reasons other than EOF, so loop until the
        // buffer is full or the file really ends — a short read reported as EOF
        // would silently truncate the file as the peer sees it.
        while filled < buf.len() {
            match open.file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(to_file_error(e)),
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    /// Write to an open handle.
    pub fn write(
        &mut self,
        handle: FileHandle,
        offset: u64,
        bytes: &[u8],
    ) -> Result<u64, FileError> {
        let open = self.open.get_mut(&handle).ok_or(FileError::BadHandle)?;
        if !open.writable {
            return Err(FileError::ReadOnly);
        }
        open.file
            .seek(SeekFrom::Start(offset))
            .map_err(to_file_error)?;
        open.file.write_all(bytes).map_err(to_file_error)?;
        Ok(bytes.len() as u64)
    }

    /// Set an open file's length.
    pub fn truncate(&mut self, handle: FileHandle, size: u64) -> Result<(), FileError> {
        let open = self.open.get_mut(&handle).ok_or(FileError::BadHandle)?;
        if !open.writable {
            return Err(FileError::ReadOnly);
        }
        open.file.set_len(size).map_err(to_file_error)
    }

    /// Release a handle. Unknown handles are ignored: a close is the peer saying
    /// it is finished, and there is nothing useful to report back.
    pub fn close(&mut self, handle: FileHandle) {
        self.open.remove(&handle);
    }

    /// Create a directory.
    pub fn mkdir(&self, volume: &str, path: &str) -> Result<(), FileError> {
        if !self.writable(volume) {
            return Err(FileError::ReadOnly);
        }
        std::fs::create_dir(self.resolve(volume, path)?).map_err(to_file_error)
    }

    /// Remove a file or an empty directory.
    pub fn remove(&self, volume: &str, path: &str) -> Result<(), FileError> {
        if !self.writable(volume) {
            return Err(FileError::ReadOnly);
        }
        let target = self.resolve(volume, path)?;
        let md = target.symlink_metadata().map_err(to_file_error)?;
        if md.is_dir() {
            std::fs::remove_dir(&target).map_err(to_file_error)
        } else {
            std::fs::remove_file(&target).map_err(to_file_error)
        }
    }

    /// Rename within one volume. Cross-volume moves are the caller's problem to
    /// decompose into copy+delete, exactly as they are locally.
    pub fn rename(&self, volume: &str, from: &str, to: &str) -> Result<(), FileError> {
        if !self.writable(volume) {
            return Err(FileError::ReadOnly);
        }
        let from = self.resolve(volume, from)?;
        let to = self.resolve(volume, to)?;
        std::fs::rename(from, to).map_err(to_file_error)
    }
}

fn meta_from(name: String, md: &std::fs::Metadata, volume_writable: bool) -> FileMeta {
    let kind = if md.file_type().is_symlink() {
        FileKind::Symlink
    } else if md.is_dir() {
        FileKind::Dir
    } else {
        FileKind::File
    };
    let modified_unix = md
        .modified()
        .ok()
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                // Before the epoch: report the negative offset rather than
                // clamping, which would date the file to 1970.
                .or_else(|e| Ok::<i64, std::convert::Infallible>(-(e.duration().as_secs() as i64)))
                .ok()
        })
        .unwrap_or(0);
    FileMeta {
        name,
        kind,
        size: if md.is_dir() { 0 } else { md.len() },
        modified_unix,
        writable: volume_writable && !md.permissions().readonly(),
    }
}

/// Map a local IO error onto the wire vocabulary.
fn to_file_error(e: std::io::Error) -> FileError {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => FileError::NotFound,
        K::PermissionDenied => FileError::Denied,
        K::AlreadyExists => FileError::Exists,
        K::NotADirectory | K::IsADirectory => FileError::WrongKind,
        K::DirectoryNotEmpty => FileError::NotEmpty,
        _ => FileError::Io(e.to_string()),
    }
}

/// Stand-in capacity for a volume whose real size cannot be read.
///
/// Never zero, and that is the whole point. A filesystem reporting a zero total
/// makes "percent used" a division by zero in every caller that computes it —
/// rclone's mount crashed programs with SIGFPE exactly this way — and a zero
/// free count makes others refuse to write, believing the disk full. A
/// plausible figure is wrong in a way nothing crashes on; zero is wrong in a way
/// that takes other programs down.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", test))]
pub(crate) const FALLBACK_TOTAL: u64 = 1 << 40; // 1 TiB
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", test))]
pub(crate) const FALLBACK_FREE: u64 = 1 << 39; // half of it

/// Total and free bytes for the filesystem holding `path`.
///
/// Falls back to [`FALLBACK_TOTAL`]/[`FALLBACK_FREE`] rather than zero when the
/// query fails.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", test))]
fn capacity_of(path: &Path) -> (u64, u64) {
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn raw_capacity(path: &Path) -> Option<(u64, u64)> {
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
fn raw_capacity(path: &Path) -> Option<(u64, u64)> {
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

#[cfg(all(
    not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
    test
))]
fn raw_capacity(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// The root, plus any removable medium mounted below it.
///
/// The root alone would be enough to *reach* a USB stick — it is mounted under
/// `/media` or `/mnt` and so already hangs off `/`. It is not enough to
/// *present* one: buried several levels inside the peer's root, a stick appears
/// as a folder nobody would look for, with no label, no capacity of its own and
/// no eject. Offering it as its own volume is what lets the mounting side dress
/// it as the disk the user plugged in.
///
/// The medium stays mounted exactly once, by this machine, and peers reach it
/// through file operations — which is why any number of them may use it at once
/// without the cache divergence that two kernels mounting one stick would
/// cause. See `docs/usb-design.md`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn detect_volumes() -> Vec<SharedVolume> {
    let root = PathBuf::from("/");
    let (total_bytes, free_bytes) = capacity_of(&root);
    let mut volumes = vec![SharedVolume {
        info: Volume {
            id: "/".to_string(),
            label: "Root".to_string(),
            writable: true,
            total_bytes,
            free_bytes,
            removable: false,
        },
        root,
    }];
    volumes.extend(detect_removable());
    volumes
}

/// Removable media mounted right now, as volumes of their own.
///
/// Read from the kernel's mount table and its `removable` attribute rather than
/// guessed from the mount path. A path heuristic looks tempting — `/media` is
/// where udisks puts sticks — but `/mnt` holds permanently mounted fixed disks
/// on plenty of machines (this one included), and calling those removable would
/// offer the peer an eject button for a drive bolted into the case.
#[cfg(target_os = "linux")]
fn detect_removable() -> Vec<SharedVolume> {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(target)) = (fields.next(), fields.next()) else {
            continue;
        };
        // Only real block devices can be removable; skip tmpfs, cgroup and the
        // rest before touching the filesystem.
        if !source.starts_with("/dev/") || !is_removable_device(source) {
            continue;
        }
        // The mount table escapes spaces as an octal `\040`, so a stick
        // labelled "MY DRIVE" arrives with an escape sequence in its path.
        // Written raw: `"\040"` in Rust source is a NUL followed by "40",
        // which would match nothing this file ever contains.
        let target = target.replace(r"\040", " ");
        let root = PathBuf::from(&target);
        let (total_bytes, free_bytes) = capacity_of(&root);
        let label = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.clone());
        out.push(SharedVolume {
            info: Volume {
                // The mount path is the id: stable while the medium is mounted,
                // which is exactly as long as the volume is offered, and unique
                // because two filesystems cannot share a mount point.
                id: target,
                label,
                writable: true,
                total_bytes,
                free_bytes,
                removable: true,
            },
            root,
        });
    }
    out
}

/// Whether `/dev/sdb1` sits on a disk the kernel calls removable.
///
/// The flag lives on the whole disk (`sdb`), not the partition (`sdb1`), so the
/// partition's sysfs entry is resolved to its parent first.
#[cfg(target_os = "linux")]
fn is_removable_device(source: &str) -> bool {
    let Some(name) = source.strip_prefix("/dev/") else {
        return false;
    };
    let Ok(path) = std::fs::canonicalize(format!("/sys/class/block/{name}")) else {
        return false;
    };
    // A partition's sysfs directory is a child of its disk's; a whole device's
    // is not, and `removable` is already there.
    let disk = match path.join("removable").exists() {
        true => path,
        false => match path.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return false,
        },
    };
    std::fs::read_to_string(disk.join("removable"))
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// macOS mounts removable media under `/Volumes`, but reporting which are
/// removable needs DiskArbitration — a framework this crate does not link for
/// the one machine that cannot host a mount anyway.
#[cfg(target_os = "macos")]
fn detect_removable() -> Vec<SharedVolume> {
    Vec::new()
}

#[cfg(target_os = "windows")]
fn detect_volumes() -> Vec<SharedVolume> {
    // No single root on Windows, so each drive letter that responds is its own
    // volume. Probed by asking for the root's metadata: enumerating with
    // GetLogicalDrives would also list drives that are present but unreadable
    // (an empty card reader), which would show up as a folder that always fails.
    let mut out = Vec::new();
    for letter in 'A'..='Z' {
        let root = PathBuf::from(format!("{letter}:\\"));
        if std::fs::metadata(&root).is_err() {
            continue;
        }
        let (total_bytes, free_bytes) = capacity_of(&root);
        out.push(SharedVolume {
            info: Volume {
                id: format!("{letter}:"),
                label: format!("Disk ({letter}:)"),
                writable: true,
                total_bytes,
                free_bytes,
                removable: is_removable_drive(letter),
            },
            root,
        });
    }
    out
}

/// Whether a drive letter is a removable medium.
///
/// `GetDriveTypeW` answers directly, and is the whole of what Windows needs
/// here — the drive-letter enumeration above already found the volume, so
/// unlike Linux there is no mount table to walk or parent device to resolve.
#[cfg(target_os = "windows")]
fn is_removable_drive(letter: char) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    // `DRIVE_REMOVABLE` lives under `WindowsProgramming`, not beside the
    // function that returns it.
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOVABLE;

    let path: Vec<u16> = format!("{letter}:\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call.
    unsafe { GetDriveTypeW(path.as_ptr()) == DRIVE_REMOVABLE }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_volumes() -> Vec<SharedVolume> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_on(root: &Path) -> FileServer {
        FileServer::new(vec![SharedVolume {
            info: Volume {
                id: "T:".to_string(),
                label: "Test".to_string(),
                writable: true,
                total_bytes: FALLBACK_TOTAL,
                free_bytes: FALLBACK_FREE,
                removable: false,
            },
            root: root.to_path_buf(),
        }])
    }

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

    /// A host with no volumes advertises the module and then answers every
    /// listing with nothing, which reads as a broken link rather than an
    /// unsupported platform — so each host this builds for must find at least
    /// one.
    #[test]
    fn a_supported_host_finds_something_to_share() {
        assert!(
            !detect_volumes().is_empty(),
            "this platform serves file requests but offers no volume"
        );
    }

    /// Every detected volume carries a usable capacity, since that is what the
    /// mount turns into `statfs`.
    #[test]
    fn detected_volumes_report_capacity() {
        for v in detect_volumes() {
            assert!(
                v.info.total_bytes > 0,
                "volume {:?} reported a zero total",
                v.info.id
            );
            assert!(
                v.info.free_bytes <= v.info.total_bytes,
                "volume {:?} reports more free than total",
                v.info.id
            );
        }
    }

    /// A path from the peer must never address anything outside its volume.
    ///
    /// The request is a string from another machine, so this is the boundary
    /// that keeps "share my disk" from meaning "share everything above it too".
    #[test]
    fn a_path_cannot_escape_its_volume() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let srv = server_on(tmp.path());

        for escape in ["../etc/shadow", "a/../../etc/shadow", "..", "a/.."] {
            let got = srv.resolve("T:", escape);
            assert!(
                matches!(got, Err(FileError::Denied)),
                "{escape:?} must be denied, got {got:?}"
            );
        }

        // A leading separator is not an escape: wire paths are volume-relative,
        // so `/etc/shadow` means `etc/shadow` *within this volume*. What matters
        // is that it lands under the root, which is asserted here rather than
        // assumed.
        let rooted = srv.resolve("T:", "/etc/shadow").expect("resolves");
        assert!(
            rooted.starts_with(tmp.path()),
            "a rooted-looking path must still land inside the volume, got {rooted:?}"
        );

        // Every accepted path must stay under the root, whatever it looks like.
        for ok in ["a/b", "./a", "a/./b", "//a//b//"] {
            let got = srv.resolve("T:", ok).expect("resolves");
            assert!(
                got.starts_with(tmp.path()),
                "{ok:?} escaped the volume: {got:?}"
            );
        }
    }

    /// Ordinary paths resolve under the volume root, separators and all.
    #[test]
    fn ordinary_paths_resolve_under_the_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let srv = server_on(tmp.path());

        let got = srv.resolve("T:", "a/b/c.txt").expect("resolves");
        assert_eq!(got, tmp.path().join("a").join("b").join("c.txt"));

        // Leading, doubled and trailing separators are noise, not components.
        let same = srv.resolve("T:", "/a//b/c.txt/").expect("resolves");
        assert_eq!(same, got, "separator noise must not change the target");

        // The volume root itself.
        assert_eq!(srv.resolve("T:", "").expect("root"), tmp.path());
    }

    /// An unknown volume is not a path error — there is nothing to resolve
    /// against.
    #[test]
    fn an_unknown_volume_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let srv = server_on(tmp.path());
        assert!(matches!(
            srv.resolve("Z:", "anything"),
            Err(FileError::BadHandle)
        ));
    }

    /// Read/write/stat round-trip through the same API the peer drives.
    #[test]
    fn a_file_round_trips_through_the_server() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut srv = server_on(tmp.path());

        let (h, size) = srv
            .open(
                "T:",
                "hello.txt",
                OpenMode {
                    read: true,
                    write: true,
                    create: true,
                    truncate: false,
                    exclusive: false,
                },
            )
            .expect("creates");
        assert_eq!(size, 0, "a fresh file is empty");

        assert_eq!(srv.write(h, 0, b"hello world").expect("writes"), 11);
        assert_eq!(srv.read(h, 0, 11).expect("reads"), b"hello world");
        // Past the end returns short rather than failing.
        assert_eq!(srv.read(h, 6, 100).expect("reads"), b"world");

        srv.truncate(h, 5).expect("truncates");
        assert_eq!(srv.read(h, 0, 100).expect("reads"), b"hello");
        srv.close(h);

        let meta = srv.stat("T:", "hello.txt").expect("stats");
        assert_eq!(meta.size, 5);
        assert_eq!(meta.kind, FileKind::File);

        // A closed handle is no longer usable.
        assert!(matches!(srv.read(h, 0, 1), Err(FileError::BadHandle)));
    }

    /// A read-only volume refuses every mutating request, including the open.
    #[test]
    fn a_read_only_volume_refuses_writes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("f.txt"), b"data").expect("seed");
        let mut srv = FileServer::new(vec![SharedVolume {
            info: Volume {
                id: "R:".to_string(),
                label: "ReadOnly".to_string(),
                writable: false,
                total_bytes: FALLBACK_TOTAL,
                free_bytes: FALLBACK_FREE,
                removable: false,
            },
            root: tmp.path().to_path_buf(),
        }]);

        assert!(matches!(
            srv.open(
                "R:",
                "f.txt",
                OpenMode {
                    write: true,
                    ..OpenMode::read_only()
                }
            ),
            Err(FileError::ReadOnly)
        ));
        assert!(matches!(srv.mkdir("R:", "new"), Err(FileError::ReadOnly)));
        assert!(matches!(
            srv.remove("R:", "f.txt"),
            Err(FileError::ReadOnly)
        ));

        // Reading still works — that is the point of a read-only share.
        let (h, _) = srv
            .open("R:", "f.txt", OpenMode::read_only())
            .expect("opens");
        assert_eq!(srv.read(h, 0, 4).expect("reads"), b"data");
    }

    /// A listing describes links as links, so the mount side decides what to do
    /// with one rather than being handed its target silently.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_symlink_is_reported_as_a_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink("/etc/shadow", tmp.path().join("escape")).expect("symlink");
        let srv = server_on(tmp.path());

        let entries = srv.read_dir("T:", "").expect("lists");
        let link = entries
            .iter()
            .find(|e| e.name == "escape")
            .expect("present");
        assert_eq!(
            link.kind,
            FileKind::Symlink,
            "a link must not be reported as its target"
        );
    }

    /// A link's target crosses the wire exactly as stored, including one that
    /// leads out of the volume.
    ///
    /// Reporting a link without being able to answer for it is what made every
    /// symlink on the volume unreadable and untraversable. Resolving it here
    /// instead would be worse than useless: `/etc/shadow` is outside the shared
    /// tree, and the mount side can only recognise that if it receives the
    /// target unresolved.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_links_target_is_returned_unresolved() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink("/etc/shadow", tmp.path().join("escape")).expect("symlink");
        std::os::unix::fs::symlink("inside.txt", tmp.path().join("relative")).expect("symlink");
        let srv = server_on(tmp.path());

        assert_eq!(srv.read_link("T:", "escape").expect("reads"), "/etc/shadow");
        assert_eq!(
            srv.read_link("T:", "relative").expect("reads"),
            "inside.txt"
        );
    }

    /// Reading a link on something that is not one says so, rather than
    /// surfacing the `InvalidInput` the OS reports for it — which is
    /// indistinguishable from a malformed request.
    #[cfg(target_os = "linux")]
    #[test]
    fn reading_a_link_on_a_regular_file_reports_the_wrong_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("plain.txt"), b"data").expect("writes");
        let srv = server_on(tmp.path());

        assert!(matches!(
            srv.read_link("T:", "plain.txt"),
            Err(FileError::WrongKind)
        ));
        assert!(matches!(
            srv.read_link("T:", "missing"),
            Err(FileError::NotFound)
        ));
    }

    /// A link cannot be used to read outside the volume, the same as any other
    /// path: the escape is refused before the link is ever consulted.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_request_cannot_escape_its_volume() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let srv = server_on(tmp.path());

        assert!(matches!(
            srv.read_link("T:", "../outside"),
            Err(FileError::Denied)
        ));
    }
}
