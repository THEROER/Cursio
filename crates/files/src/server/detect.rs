//! What this machine has to offer: its disks and removable media on Linux,
//! its drive letters on Windows, and a phone's internal storage and cards.

use crate::server::capacity_of;
use crate::SharedVolume;
use cursio_protocol::Volume;
#[cfg(any(target_os = "linux", target_os = "android", test))]
use std::path::Path;
use std::path::PathBuf;

/// The root, plus every disk mounted below it.
///
/// The root alone reaches all of them — a second disk is mounted under `/mnt`
/// and so already hangs off `/` — but reaching is not presenting. Offered as
/// one volume, a machine's other disks arrive at the peer as unnamed folders
/// several levels down, with no capacity of their own, no label the user gave
/// them, and no entry in a file manager's sidebar. On this machine that hid
/// 1.2 TB across four disks inside a single `/`, while the Windows half
/// offered every drive letter separately — the same machine presented two
/// different ways depending on which end you looked from.
///
/// Each disk therefore becomes its own volume, which is also what makes a USB
/// stick appear as the disk the user plugged in. The medium stays mounted
/// exactly once, by this machine, and peers reach it through file operations —
/// so any number of them use it at once without the cache divergence two
/// kernels mounting one stick would cause. See `docs/usb-design.md`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn detect_volumes() -> Vec<SharedVolume> {
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
        confined: false,
    }];
    volumes.extend(detect_disks());
    volumes
}

/// Every disk mounted right now, as a volume of its own.
///
/// ## What counts as a disk
///
/// The same question a file manager answers when it decides what to put in its
/// sidebar, and answered the same way rather than by a rule of our own. udisks
/// distinguishes a volume backed by real hardware from one that is not by
/// whether the block device belongs to a *drive*: `/dev/sda1` reports a drive
/// with a model and a serial, while `/dev/loop0` reports none, because behind
/// it is a file. In sysfs that is exactly the difference between a device under
/// `/sys/devices/pci…` and one under `/sys/devices/virtual/`.
///
/// Reading it from sysfs rather than asking udisks over D-Bus keeps this
/// working on a machine with no desktop session and needs no second
/// implementation for Windows, which has no udisks at all. It is the same
/// answer from the same source udisks itself reads.
///
/// On this machine the test is decisive: 29 snap loop mounts and four real
/// disks, and a file manager shows the four. A rule based on the mount path
/// would have failed here — `/mnt` holds permanently mounted fixed disks, not
/// the removable media the name suggests.
#[cfg(target_os = "linux")]
pub(super) fn detect_disks() -> Vec<SharedVolume> {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(target)) = (fields.next(), fields.next()) else {
            continue;
        };
        // The root is already offered above, and a bind mount of it would
        // otherwise arrive a second time under another name.
        if target == "/" {
            continue;
        }
        let Some(disk) = backing_disk(source) else {
            continue;
        };
        // One volume per filesystem: a disk mounted twice (a bind mount, or the
        // same device mounted at two points) is one disk, and offering it twice
        // would show the peer two drives that are the same storage.
        if !seen.insert(source.to_string()) {
            continue;
        }
        // The mount table escapes spaces as an octal `\040`, so a stick
        // labelled "MY DRIVE" arrives with an escape sequence in its path.
        // Written raw: `"\040"` in Rust source is a NUL followed by "40",
        // which would match nothing this file ever contains.
        let target = target.replace(r"\040", " ");
        let root = PathBuf::from(&target);
        let (total_bytes, free_bytes) = capacity_of(&root);
        out.push(SharedVolume {
            info: Volume {
                // The mount path is the id: stable while the filesystem is
                // mounted, which is exactly as long as the volume is offered,
                // and unique because two filesystems cannot share a mount point.
                id: target.clone(),
                label: disk_label(&root, &target, total_bytes),
                writable: true,
                total_bytes,
                free_bytes,
                removable: is_removable_disk(&disk),
            },
            root,
            confined: false,
        });
    }
    out
}

/// The sysfs directory of the whole disk behind a block device, or `None` when
/// nothing physical is behind it.
///
/// A partition's sysfs entry is a child of its disk's, so the parent is taken
/// when the device is not itself a whole disk. Anything under
/// `/sys/devices/virtual` — loop, device-mapper, zram — has no drive behind it
/// and is not offered, which is what keeps a machine's snap mounts out of the
/// peer's file manager.
#[cfg(target_os = "linux")]
pub(super) fn backing_disk(source: &str) -> Option<PathBuf> {
    let name = source.strip_prefix("/dev/")?;
    let path = std::fs::canonicalize(format!("/sys/class/block/{name}")).ok()?;
    if path.starts_with("/sys/devices/virtual") {
        return None;
    }
    // `removable` sits on the whole disk; its presence is also what marks this
    // entry as the disk rather than one of its partitions.
    match path.join("removable").exists() {
        true => Some(path),
        false => Some(path.parent()?.to_path_buf()),
    }
}

/// Whether the kernel calls this disk removable.
#[cfg(target_os = "linux")]
pub(super) fn is_removable_disk(disk: &Path) -> bool {
    std::fs::read_to_string(disk.join("removable"))
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// A name for a volume, in the order a person would recognise it.
///
/// The mount point's last component first, because udisks names a mount after
/// the filesystem label and that is the name the user gave the disk. A disk
/// with no label is mounted at a path that says nothing either — so rather than
/// offer the peer a volume called `sdd2`, it is named for its size, exactly as
/// a file manager shows "239 GB Volume" for the same disk.
#[cfg(target_os = "linux")]
pub(super) fn disk_label(root: &Path, target: &str, total_bytes: u64) -> String {
    let named = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.to_string());
    // A bare device name is not a label anybody chose.
    match named.starts_with("sd") || named.starts_with("nvme") || named.is_empty() {
        true => format!("{} Volume", human_size(total_bytes)),
        false => named,
    }
}

/// Round a capacity the way a file manager labels a disk: "239 GB", "1,0 TB".
///
/// Used on both platforms, for the same reason on each: a disk the user never
/// named has to be called something, and its size is what a file manager shows.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(super) fn human_size(bytes: u64) -> String {
    // Decimal units, as every disk vendor and file manager uses: a 240 GB SSD
    // labelled "223 GiB" reads as the wrong disk.
    const UNITS: [(&str, u64); 3] = [
        ("TB", 1_000_000_000_000),
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let whole = bytes / scale;
            return match whole >= 10 {
                true => format!("{whole} {unit}"),
                // One decimal below ten, so a 1.5 TB disk is not "1 TB".
                false => format!("{:.1} {unit}", bytes as f64 / scale as f64),
            };
        }
    }
    format!("{bytes} B")
}

/// macOS mounts removable media under `/Volumes`, but reporting which are
/// removable needs DiskArbitration — a framework this crate does not link for
/// the one machine that cannot host a mount anyway.
#[cfg(target_os = "macos")]
pub(super) fn detect_disks() -> Vec<SharedVolume> {
    Vec::new()
}

#[cfg(target_os = "windows")]
pub(super) fn detect_volumes() -> Vec<SharedVolume> {
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
        // Not a disk of ours: this letter is a peer's volume, mounted here by
        // Cursio itself. Offering it back sends the peer its own files across
        // the network to arrive home under a new letter — a machine with two
        // real disks announced six, four of them the asking machine's own,
        // labelled with that machine's name. Following one is a round trip for
        // a file the caller already has, and a walk into it recurses.
        if is_own_mount(letter) {
            continue;
        }
        let (total_bytes, free_bytes) = capacity_of(&root);
        out.push(SharedVolume {
            info: Volume {
                id: format!("{letter}:"),
                label: volume_label(letter).unwrap_or_else(|| human_size(total_bytes)),
                writable: true,
                total_bytes,
                free_bytes,
                removable: is_removable_drive(letter),
            },
            root,
            confined: false,
        });
    }
    out
}

/// The name the user gave this drive, as Explorer shows it.
///
/// The volume label, not a name of our own. This used to answer
/// `format!("Disk ({letter}:)")`, which says only what the caller already knew
/// — and the letter is the peer's private naming: it means something on the
/// machine that assigned it and nothing on the machine reading it, where two
/// Windows peers each offer a `C` that are different disks. The mount that
/// presents this then added the letter back, giving the reader `Disk (C:) (C)`.
///
/// `None` when the drive has no label — plenty of disks are never named — and
/// the caller falls back to the size, exactly as a file manager writes
/// "239 GB Volume" for the same disk.
#[cfg(target_os = "windows")]
pub(super) fn volume_label(letter: char) -> Option<String> {
    let root: Vec<u16> = format!("{letter}:\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // `MAX_PATH + 1`, which is what `GetVolumeInformationW` documents as the
    // required size for the label buffer.
    let mut label = [0u16; 261];
    // Safe: `root` is a valid NUL-terminated wide string, and the buffer is
    // owned here and passed with its true length. The out-params this call does
    // not need are null, which the API accepts.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let end = label.iter().position(|&c| c == 0).unwrap_or(label.len());
    let name = String::from_utf16_lossy(&label[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Whether this drive letter is a peer's volume that Cursio mounted here.
///
/// Identified by the UNC name behind the letter. [`crate::winmount`] publishes
/// every mount as a network filesystem under `\cursio\<peer>-<volume>`, so that
/// prefix is exactly our own mounts and nothing else — a real network drive the
/// user mapped keeps its own server name and is offered to the peer like any
/// other disk, which is right: it is storage this machine can reach and the
/// peer may not.
///
/// Tested by the name rather than by `DRIVE_REMOTE`, which would also catch
/// those genuine mappings.
#[cfg(target_os = "windows")]
pub(super) fn is_own_mount(letter: char) -> bool {
    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

    let device: Vec<u16> = format!("{letter}:")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `device` is a NUL-terminated UTF-16 string that outlives the call.
    if unsafe { GetDriveTypeW(device.as_ptr()) } != DRIVE_REMOTE {
        return false;
    }

    let mut name = [0u16; 512];
    let mut len = name.len() as u32;
    // SAFETY: both buffers are owned here and passed with their true lengths;
    // `len` is in-out and the API writes no more than it says.
    let ok = unsafe { WNetGetConnectionW(device.as_ptr(), name.as_mut_ptr(), &mut len) };
    if ok != 0 {
        return false;
    }
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
        .trim_start_matches('\\')
        .starts_with("cursio\\")
}

/// Whether a drive letter is a removable medium.
///
/// `GetDriveTypeW` answers directly, and is the whole of what Windows needs
/// here — the drive-letter enumeration above already found the volume, so
/// unlike Linux there is no mount table to walk or parent device to resolve.
#[cfg(target_os = "windows")]
pub(super) fn is_removable_drive(letter: char) -> bool {
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

/// A phone's shared storage: what its own file manager calls "Internal
/// storage", plus each memory card.
///
/// Not `/`: an app sees almost nothing of the phone's real root, and what it
/// does see is the system's, not the user's. The storage a person thinks of as
/// "the phone's files" — Download, DCIM, Music — is the primary shared volume,
/// reached at `/storage/self/primary`, the current user's view of it whatever
/// that user's number. Cards are mounted beside it under `/storage/<uuid>`.
///
/// Read with plain file calls rather than through the Storage Access
/// Framework: the app holds "All files access" (or, on Android 10, the legacy
/// storage permission), which is what makes these real paths readable, so the
/// same server that answers for a desktop's disks answers here. SAF would mean
/// a JNI round trip and a content-provider query per entry, which turns a
/// directory of photos into seconds of waiting.
///
/// Until the user grants that access the directories are unreadable, and a
/// volume that answers `Denied` to everything is worse than none; so nothing
/// is offered, and the module's once-a-second rescan offers it the moment the
/// grant is made.
#[cfg(target_os = "android")]
pub(super) fn detect_volumes() -> Vec<SharedVolume> {
    storage_volumes(Path::new("/storage"), Path::new("/storage/self/primary"))
}

/// [`detect_volumes`] for a phone, against any directory standing in for
/// `/storage` — so it is testable on the host.
#[cfg(any(target_os = "android", test))]
pub(super) fn storage_volumes(storage: &Path, primary: &Path) -> Vec<SharedVolume> {
    let readable = |path: &Path| std::fs::read_dir(path).is_ok();
    let mut out = Vec::new();
    if readable(primary) {
        let (total_bytes, free_bytes) = capacity_of(primary);
        out.push(SharedVolume {
            info: Volume {
                // A name and not the path: the mount presents a volume under
                // the last component of its id, which for this path is `primary`
                // (or `0`), and neither says what the folder holds.
                id: INTERNAL_STORAGE.to_string(),
                label: INTERNAL_STORAGE.to_string(),
                writable: true,
                total_bytes,
                free_bytes,
                removable: false,
            },
            root: primary.to_path_buf(),
            confined: false,
        });
    }
    let Ok(entries) = std::fs::read_dir(storage) else {
        return out;
    };
    let mut cards: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            // `emulated` holds every user's primary storage and `self` is the
            // link to ours; both are the volume already offered above.
            name != "emulated" && name != "self"
        })
        .map(|e| e.path())
        .filter(|p| p.is_dir() && readable(p))
        .collect();
    cards.sort();
    let numbered = cards.len() > 1;
    for (i, root) in cards.into_iter().enumerate() {
        let name = match numbered {
            true => format!("{SD_CARD} {}", i + 1),
            false => SD_CARD.to_string(),
        };
        let (total_bytes, free_bytes) = capacity_of(&root);
        out.push(SharedVolume {
            info: Volume {
                id: name.clone(),
                label: name,
                writable: true,
                total_bytes,
                free_bytes,
                removable: true,
            },
            root,
            confined: false,
        });
    }
    out
}

/// What a phone's own file manager calls its primary shared storage.
#[cfg(any(target_os = "android", test))]
pub(super) const INTERNAL_STORAGE: &str = "Internal storage";

/// What a phone's memory card is called, numbered when there are several.
#[cfg(any(target_os = "android", test))]
pub(super) const SD_CARD: &str = "SD card";

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows",
    target_os = "android"
)))]
pub(super) fn detect_volumes() -> Vec<SharedVolume> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file manager labels a disk in decimal units — a 240 GB SSD shown as
    /// "223 GiB" reads as a different disk than the one on the box.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_capacity_reads_the_way_a_disk_is_sold() {
        assert_eq!(human_size(238_927_478_784), "238 GB");
        assert_eq!(human_size(1_000_200_994_816), "1.0 TB");
        assert_eq!(human_size(500_105_217_024), "500 GB");
    }

    /// Below ten the whole number alone loses too much: a 1.5 TB disk must not
    /// be labelled the same as a 1.0 TB one.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_small_multiple_keeps_one_decimal() {
        assert_eq!(human_size(1_500_000_000_000), "1.5 TB");
        assert_eq!(human_size(2_000_000_000), "2.0 GB");
    }

    /// udisks mounts a disk under its filesystem label, so the last path
    /// component is the name the user gave it.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_labelled_disk_keeps_its_name() {
        let target = "/mnt/Programing";
        assert_eq!(
            disk_label(Path::new(target), target, 200_000_000_000),
            "Programing"
        );
    }

    /// A disk with no label is mounted at a path naming the device, and
    /// `sdd2` is not a name anybody chose. Named for its size instead, exactly
    /// as a file manager does.
    #[test]
    #[cfg(target_os = "linux")]
    fn an_unlabelled_disk_is_named_for_its_size() {
        let target = "/media/theroer/sdd2";
        assert_eq!(
            disk_label(Path::new(target), target, 238_927_478_784),
            "238 GB Volume"
        );
    }

    /// Nothing physical backs a loop device, which is why a file manager does
    /// not show one — and why 29 snap mounts must not reach the peer.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_loop_device_has_no_backing_disk() {
        // Skipped rather than failed where the machine has no loop devices:
        // the assertion is about what sysfs reports, not about this file.
        if std::path::Path::new("/sys/class/block/loop0").exists() {
            assert!(backing_disk("/dev/loop0").is_none());
        }
    }

    /// Anything that is not a block device at all — tmpfs, cgroup, proc — has
    /// no disk behind it either.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_pseudo_filesystem_has_no_backing_disk() {
        assert!(backing_disk("tmpfs").is_none());
        assert!(backing_disk("proc").is_none());
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

    /// A phone that has not been granted storage access offers nothing,
    /// rather than a volume that answers "denied" to every request.
    #[test]
    fn a_phone_without_storage_access_offers_no_volume() {
        let storage = tempfile::tempdir().unwrap();
        let missing = storage.path().join("self/primary");
        assert!(storage_volumes(storage.path(), &missing).is_empty());
    }

    /// The primary storage is offered under the name a phone's file manager
    /// gives it, not under `primary` or `0`, the last part of its path.
    #[test]
    fn a_phones_primary_storage_is_offered_as_internal_storage() {
        let storage = tempfile::tempdir().unwrap();
        let primary = storage.path().join("emulated/0");
        std::fs::create_dir_all(primary.join("Download")).unwrap();
        std::fs::create_dir_all(storage.path().join("self")).unwrap();
        let volumes = storage_volumes(storage.path(), &primary);
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].info.id, INTERNAL_STORAGE);
        assert_eq!(volumes[0].root, primary);
        assert!(!volumes[0].info.removable);
        assert_eq!(
            crate::volume_dir_name(&volumes[0].info.id),
            INTERNAL_STORAGE
        );
    }

    /// A memory card mounted beside the primary storage is its own,
    /// removable volume; `emulated` and `self` are not cards.
    #[test]
    fn a_memory_card_is_offered_as_a_removable_volume_of_its_own() {
        let storage = tempfile::tempdir().unwrap();
        let primary = storage.path().join("emulated/0");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(storage.path().join("self")).unwrap();
        std::fs::create_dir_all(storage.path().join("1A2B-3C4D")).unwrap();
        let volumes = storage_volumes(storage.path(), &primary);
        let ids: Vec<&str> = volumes.iter().map(|v| v.info.id.as_str()).collect();
        assert_eq!(ids, [INTERNAL_STORAGE, SD_CARD]);
        assert!(volumes[1].info.removable);
    }

    /// Two cards are numbered, so neither hides the other under one name.
    #[test]
    fn two_memory_cards_are_numbered() {
        let storage = tempfile::tempdir().unwrap();
        let primary = storage.path().join("emulated/0");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(storage.path().join("1111-1111")).unwrap();
        std::fs::create_dir_all(storage.path().join("2222-2222")).unwrap();
        let ids: Vec<String> = storage_volumes(storage.path(), &primary)
            .into_iter()
            .map(|v| v.info.id)
            .collect();
        assert_eq!(ids, [INTERNAL_STORAGE, "SD card 1", "SD card 2"]);
    }
}
