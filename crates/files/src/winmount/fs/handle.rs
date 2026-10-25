//! Mounting a peer's volume as a drive letter: finding WinFsp's DLL, choosing
//! the letter, and the handle that unmounts it.

use crate::bridge::PeerLink;
use crate::winmount::convert::dir_name_of;
use crate::winmount::fs::RemoteFs;
use crate::winmount::fs::SECTORS_PER_CLUSTER;
use crate::winmount::fs::SECTOR_SIZE;
use winfsp::host::{FileSystemHost, VolumeParams};

/// A drive letter nothing is using, counting down from `Z:`.
///
/// Downwards because the low letters belong to real hardware and to whatever the
/// user maps by habit; a share from another machine belongs at the far end,
/// which is also where Windows puts mapped network drives.
///
/// `GetLogicalDrives` returns a bitmask of the letters in use — one call, no
/// filesystem access, so a wedged mount elsewhere cannot make this hang.
pub(super) fn free_drive_letter() -> Option<char> {
    // Safe: no arguments, no out-params; returns a bitmask.
    let used = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    if used == 0 {
        // The call failed. `Z:` is the conventional choice and mounting fails
        // loudly if it is taken, which beats giving up here.
        return Some('Z');
    }
    ('D'..='Z')
        .rev()
        .find(|c| used & (1 << (*c as u8 - b'A')) == 0)
}

/// Load `winfsp-x64.dll` from the directory the installer recorded.
///
/// WinFsp deliberately does **not** put its DLL in `System32`: installs are
/// side-by-side, several versions may coexist, and each lives under
/// `…\WinFsp\SxS\sxs.<timestamp>\bin\`. An application is expected to read the
/// location out of the registry and load it itself.
///
/// The crate can do this — but only with its `system` feature, which builds
/// bindings against WinFsp's headers *on the build machine*. These binaries are
/// cross-compiled from Linux, where those headers do not exist, so that route is
/// closed and the lookup is done here instead.
///
/// Delay-loading (see `crates/core/build.rs`) is what makes this work: the
/// import is not resolved until the first call, so loading the DLL by full path
/// first means the loader finds the module already present and binds to it.
/// Without this the process starts fine and then fails at mount time with
/// `WIN32(1285) ERROR_DELAY_LOAD_FAILED`.
pub(super) fn load_winfsp_dll() -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let dir = winfsp_dll_dir().ok_or_else(|| {
        anyhow::anyhow!("WinFsp does not appear to be installed (no registry entry)")
    })?;
    let dll = dir.join("bin").join("winfsp-x64.dll");
    if !dll.exists() {
        anyhow::bail!("WinFsp is installed but {} is missing", dll.display());
    }

    let wide: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
    // Safe: `wide` is a valid NUL-terminated path that outlives the call. The
    // returned handle is intentionally leaked — the library must stay loaded for
    // the life of the process, since every later WinFsp call binds through it.
    let handle = unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
    if handle.is_null() {
        anyhow::bail!("cannot load {}", dll.display());
    }
    tracing::debug!(dll = %dll.display(), "loaded WinFsp");
    Ok(())
}

/// Where WinFsp says it lives.
///
/// `SxsDir` points at the versioned directory of the current install and is what
/// a modern install sets; `InstallDir` is the older layout. Both are under the
/// 32-bit view of the registry, which is where the (32-bit) installer writes
/// them regardless of the machine's architecture.
pub(super) fn winfsp_dll_dir() -> Option<std::path::PathBuf> {
    for key in [r"SOFTWARE\WOW6432Node\WinFsp", r"SOFTWARE\WinFsp"] {
        for value in ["SxsDir", "InstallDir"] {
            if let Some(dir) = read_registry_string(key, value) {
                let path = std::path::PathBuf::from(dir);
                if path.join("bin").join("winfsp-x64.dll").exists() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Read one `HKEY_LOCAL_MACHINE` string value.
pub(super) fn read_registry_string(subkey: &str, value: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    };
    let (subkey_w, value_w) = (wide(subkey), wide(value));

    // Ask for the size first: the value is a path of unknown length.
    let mut len: u32 = 0;
    // Safe: both strings are NUL-terminated and outlive the call; a null data
    // pointer with a valid length pointer is the documented way to size it.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }

    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    // Safe: `buf` is sized from the query above and `len` describes it in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    // Trim the trailing NUL the API includes in the length.
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// A live mount. Dropping it unmounts.
pub struct MountHandle {
    host: FileSystemHost<RemoteFs>,
    mountpoint: String,
}

impl MountHandle {
    pub fn mountpoint(&self) -> &str {
        &self.mountpoint
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        // The drive letter belongs to WinFsp, which drops it with the mount —
        // nothing to release separately.
        self.host.stop();
        self.host.unmount();
    }
}

/// Mount ONE of the peer's volumes on the next free drive letter.
///
/// Called once per volume the peer offers: `volume` is the wire id (`"C:"`,
/// `"/"`, a removable medium's mount path) and `volume_label` is what the drive
/// calls itself in Explorer.
pub fn mount(
    peer: PeerLink,
    peer_name: &str,
    volume: &str,
    volume_label: &str,
) -> anyhow::Result<MountHandle> {
    // Load WinFsp's DLL from where the installer actually put it, before
    // anything asks for a symbol from it.
    load_winfsp_dll()?;

    // WinFsp must be initialised once per process before any filesystem is
    // built; doing it here keeps the requirement with the code that needs it.
    winfsp::winfsp_init()
        .map_err(|e| anyhow::anyhow!("WinFsp is not available (is it installed?): {e:?}"))?;

    // The share this filesystem is published as. `prefix` is what WinFsp wants
    // (one leading backslash); `unc` is the same thing as a path a user or
    // `WNetAddConnection2` would name.
    // One share per volume, so the UNC path stays a working way in when the
    // drive letter is invisible (see the elevation note below). The volume's
    // directory name rather than its raw id: a share name cannot contain a
    // colon, which every Windows volume id ends with.
    let share = dir_name_of(volume);
    let prefix = format!(r"\cursio\{peer_name}-{share}");
    let unc = format!(r"\{prefix}");

    let mut params = VolumeParams::new();
    params
        .sector_size(SECTOR_SIZE)
        .sectors_per_allocation_unit(SECTORS_PER_CLUSTER)
        .filesystem_name(format!("cursio-{peer_name}"))
        // Declare this a NETWORK filesystem, by giving it a UNC prefix.
        //
        // The prefix is what makes WinFsp create the volume through
        // `WinFsp.Net` rather than `WinFsp.Disk`, and that distinction is not
        // cosmetic: as a "disk" the volume was created but no drive letter
        // could ever attach to it — `fsptool lsvol` showed it with a `-` where
        // the drive should be, invisible to Explorer and unreachable by path.
        // Windows expects a fixed disk to be a real device with a partition,
        // which this is not.
        //
        // It is also the honest description. Windows optimises fixed disks on
        // the assumption that they are fast and reliable, and network drives on
        // the assumption that they are neither — which is exactly right for a
        // filesystem whose every request crosses a Wi-Fi link to another
        // machine. sshfs-win and rclone mount this way for the same reason.
        .prefix(prefix.as_str())
        // No kernel cache: `FileInfoTimeout` is left at zero, which keeps the
        // volume off the NT cache manager.
        //
        // `u32::MAX` would put it on, and it was tried: the cache manager's lazy
        // writer coalesces an application's writes, which is what it is for.
        // But it accepts them at memory speed while this link drains at ~11
        // MB/s, so a download piled gigabytes of dirty pages into RAM, and
        // every `CloseHandle` — including the ones a dying process makes on
        // its way out — then had to flush its share through the link
        // synchronously. With the link stalled, Steam's download stopped at 0
        // B/s and the process could not be killed short of a reboot. Nothing a
        // user-mode filesystem does can interrupt that wait.
        //
        // The round-trip-per-write problem the cache was meant to solve is
        // solved without it, and without the unbounded buffer: `writeback`
        // keeps a bounded window of writes in flight and still reports every
        // failure to the application that caused it.
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        // Do the wildcard filtering here rather than leaving it to WinFsp.
        //
        // Left unset, the pattern never reaches `read_directory` and WinFsp
        // matches on its own — which on this volume found nothing: `*.o` came
        // back empty in a directory holding `a.o`, and cargo, which enumerates
        // with a pattern while clearing its `.temp-archive` directories, failed
        // with `ERROR_INVALID_NAME` on names that were perfectly valid.
        .pass_query_directory_pattern(true)
        // Accept POSIX-style unlink and rename.
        //
        // Rust's `remove_dir_all` deletes through `FileDispositionInformationEx`
        // with POSIX semantics, and without this declaration WinFsp refuses the
        // request outright: it never reached `set_delete` or `cleanup` — the
        // trace showed the directory opened and enumerated (empty), and then no
        // delete call at all — and Windows reported `ERROR_INVALID_PARAMETER`,
        // which cargo prints as "failed to remove temporary directory: the
        // parameter is incorrect (os error 87)" — localised, so it reads in
        // whatever language Windows runs in. Node deletes through the older
        // disposition class, which is why it never hit this.
        .supports_posix_unlink_rename(true);

    // Labelled with the peer and the disk, because several of these now sit
    // side by side in Explorer and "cursio" alone would not say which is which.
    let label = format!("{volume_label} ({peer_name})");
    let mut host: FileSystemHost<RemoteFs> = FileSystemHost::new(
        params,
        RemoteFs::new(peer, label, volume).of_peer(peer_name),
    )
    .map_err(|e| anyhow::anyhow!("cannot create the filesystem host: {e:?}"))?;

    // Name the drive letter explicitly.
    //
    // A standalone probe against this WinFsp install (scratchpad/winfsp-probe)
    // tried all four combinations: a named letter produces a working drive for
    // both a disk and a network filesystem, while `*:` silently picks some
    // *other* free letter that the caller has no way to learn — WinFsp does not
    // report the choice back. Passing the UNC path instead is refused outright,
    // as mount points with three or more backslashes come back as
    // `STATUS_NETWORK_ACCESS_DENIED`.
    // A letter belongs to the logon session that defines it. UAC gives an
    // administrator two linked sessions, so a drive mounted from the elevated
    // one is genuine — an elevated process lists `C: Y: Z:` — but absent from
    // Explorer, which runs in the other and sees only `C:`. An elevated node
    // therefore does not call this itself: it relays the mount to an
    // unelevated helper (`cursio_files::relay`, `cursio_core::mount_helper`),
    // and this runs there. The UNC path (`\\cursio\<peer>`) reaches the
    // drive from either session, which is what remains when the helper cannot
    // be started.
    //
    // Refused rather than folded in with another volume: there are 26 letters
    // for every disk on the machine and every peer's, and a volume quietly
    // sharing a letter with another is the ambiguity this change removes.
    let letter = free_drive_letter().ok_or_else(|| {
        anyhow::anyhow!("no free drive letter left for {peer_name}'s volume {volume}")
    })?;
    let mountpoint = format!("{letter}:");
    match host.mount(mountpoint.as_str()) {
        Ok(()) => tracing::debug!(drive = %mountpoint, "mounted"),
        Err(e) => return Err(anyhow::anyhow!("cannot mount on {mountpoint}: {e:?}")),
    }
    // Start the dispatcher with enough threads to keep requests in flight.
    //
    // `start()` asks WinFsp for its default, which is sized for a filesystem
    // whose work is local: a couple of threads per core, on the assumption that
    // a callback returns as soon as the disk does. Every callback here instead
    // blocks for a network round trip, so that default caps the mount at a
    // handful of concurrent operations and a copy of many small files spends
    // its time waiting rather than transferring — the same serialisation the
    // FUSE side had on its single event loop.
    //
    // WinFsp clamps whatever it is given to its own supported range, so an
    // unreasonable figure here degrades rather than failing to mount.
    let threads = crate::bridge::mount_threads();
    match host.start_with_threads(threads as u32) {
        Ok(()) => tracing::debug!(threads, "dispatcher started"),
        Err(e) => return Err(anyhow::anyhow!("cannot start the filesystem: {e:?}")),
    }

    tracing::info!(
        drive = %mountpoint,
        // Also reachable by path, which is worth recording: if the letter ever
        // fails to appear, this is the way in.
        unc = %unc,
        peer = peer_name,
        volume,
        "peer volume mounted"
    );
    Ok(MountHandle { host, mountpoint })
}
