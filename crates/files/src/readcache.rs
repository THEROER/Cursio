//! A peer's file contents, kept on this machine's disk across runs.
//!
//! A game launched from a peer's disk reads gigabytes, and over Wi-Fi that is
//! minutes on every launch. Keeping what was read lets the second launch read
//! this machine's disk instead. The page cache cannot do this — it does not
//! outlive the mount, and on Windows WinFsp's cache does not outlive a handle.
//!
//! ## Never the bytes the owner no longer has
//!
//! Contents are kept under the owner's version stamp
//! ([`FileMeta::stamp`](cursio_protocol::FileMeta::stamp)) and the length, not
//! under the modification time: whole seconds let a same-length rewrite keep
//! its key forever. A different stamp is a different file, so a stale one is
//! never served; the older copy is deleted the moment a newer stamp is stored.
//! Writes made through this same mount are known here before the owner's
//! metadata says so, and [`ReadCache::invalidate`] voids every key taken for
//! that path (or anything under it) before the write, so a handle opened
//! earlier stops using the cache for the rest of its life.
//!
//! ## One sparse file and a bitmap
//!
//! Each version is one data file of the file's length, written only where
//! blocks of [`BLOCK`] bytes were read, plus a map of which blocks are there.
//! A file per block would mean a hundred thousand files for one game archive;
//! a log of appended blocks would need its own index and compaction. Holes
//! cost nothing on ext4, btrfs or NTFS once the file is marked sparse.
//!
//! ## Crash ordering
//!
//! A block counts only once its bit is in the map on disk, and a bit reaches
//! the map only after the data file was synced: a flush snapshots the bits,
//! calls `sync_data`, then writes the map to a temporary file and renames it
//! over the old one. A crash in between leaves the previous map, which names
//! only data synced before it. A map that is torn, garbled or names a data file
//! of the wrong length is treated as empty and deleted.
//!
//! Everything here degrades to "not cached" with a warning: a read never fails
//! because the cache did.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cursio_protocol::{FileKind, FileMeta};
use serde::{Deserialize, Serialize};

use crate::fnv::Fnv64;

/// The unit stored and tracked: one wire chunk, so a block-aligned miss is
/// exactly one read request on the wire.
pub const BLOCK: u64 = cursio_protocol::FilesMsg::MAX_CHUNK as u64;

/// How long newly stored blocks may wait for a flush while a file is still
/// being read. They are served meanwhile; only their survival of a crash
/// waits.
const FLUSH_EVERY: Duration = Duration::from_secs(5);

/// Data files held open at once. Past this the least recently used is flushed
/// and closed; a game opens thousands of files, and a descriptor each for the
/// life of the process is not ours to take.
const MAX_LIVE: usize = 128;

/// Paths remembered as written through the mount. Past this the record is
/// cleared and every key taken before is voided at once, which costs only
/// caching on handles already open.
const MAX_INVALIDATED: usize = 100_000;

/// Eviction stops this far under the cap, so a cache at its limit does not
/// delete one entry per block stored.
const EVICT_TO: f64 = 0.9;

const MAGIC: &[u8; 8] = b"CURSIORC";
const FORMAT: u32 = 1;

/// Where the cache lives and how large it may grow; carried from the node to
/// whichever process presents the mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSettings {
    pub root: PathBuf,
    pub cap_bytes: u64,
}

/// One version of one file on one peer's volume, as a handle opened it.
#[derive(Debug, Clone)]
pub struct FileKey {
    name: String,
    dir: u64,
    /// The path's own hash and each ancestor's, for [`ReadCache::invalidate`]
    /// of a directory to reach it.
    lineage: Vec<u64>,
    stamp: u64,
    size: u64,
    /// The invalidation clock when the key was taken.
    taken: u64,
}

impl FileKey {
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// The on-disk read cache shared by every mount in this process.
pub struct ReadCache {
    root: PathBuf,
    cap: u64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<u64, Entry>,
    total: u64,
    clock: u64,
    /// Keys taken before this are void.
    floor: u64,
    invalidated: HashMap<u64, u64>,
    last_used: u64,
}

struct Entry {
    name: String,
    stamp: u64,
    size: u64,
    /// The clock of the key that stored this version; an older key holding a
    /// different stamp is the stale one and may not replace it.
    since: u64,
    used: u64,
    bytes: u64,
    live: Option<Arc<Live>>,
}

struct Live {
    name: String,
    stamp: u64,
    size: u64,
    data: File,
    data_path: PathBuf,
    map_path: PathBuf,
    blocks: Mutex<Blocks>,
    flushing: Mutex<()>,
}

struct Blocks {
    bits: Vec<u8>,
    dirty: bool,
    /// Served since the map was last written, so its time should move.
    touched: bool,
    flushed: Instant,
}

fn blocks_of(size: u64) -> u64 {
    size.div_ceil(BLOCK)
}

fn has(bits: &[u8], b: u64) -> bool {
    bits.get((b / 8) as usize)
        .is_some_and(|byte| byte & (1 << (b % 8)) != 0)
}

fn set(bits: &mut [u8], b: u64) {
    if let Some(byte) = bits.get_mut((b / 8) as usize) {
        *byte |= 1 << (b % 8);
    }
}

fn block_len(b: u64, size: u64) -> u64 {
    (size - b * BLOCK).min(BLOCK)
}

fn present_bytes(bits: &[u8], size: u64) -> u64 {
    (0..blocks_of(size))
        .filter(|b| has(bits, *b))
        .map(|b| block_len(b, size))
        .sum()
}

fn path_hash(peer: &str, volume: &str, path: &str) -> u64 {
    Fnv64::default()
        .write_str(peer)
        .write_str(volume)
        .write_str(path)
        .finish()
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn mtime_nanos(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// The cache for `settings`, opened once per process and shared by every
/// mount in it, so the cap holds across all of them. `None` when disabled or
/// when the directory cannot be used.
pub fn shared(settings: &CacheSettings) -> Option<Arc<ReadCache>> {
    static OPEN: OnceLock<Mutex<HashMap<PathBuf, Arc<ReadCache>>>> = OnceLock::new();
    if settings.cap_bytes == 0 {
        return None;
    }
    let mut open = OPEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(cache) = open.get(&settings.root) {
        return Some(cache.clone());
    }
    let cache = Arc::new(ReadCache::open(settings)?);
    open.insert(settings.root.clone(), cache.clone());
    Some(cache)
}

fn encode_map(name: &str, stamp: u64, size: u64, bits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(48 + name.len() + bits.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&stamp.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(bits);
    let sum = Fnv64::default().write(&out).finish();
    out.extend_from_slice(&sum.to_le_bytes());
    out
}

/// The map's name, stamp, size and bits, or `None` for anything not written
/// whole by [`encode_map`].
fn decode_map(raw: &[u8]) -> Option<(String, u64, u64, Vec<u8>)> {
    let (body, sum) = raw.split_at_checked(raw.len().checked_sub(8)?)?;
    if Fnv64::default().write(body).finish() != u64::from_le_bytes(sum.try_into().ok()?) {
        return None;
    }
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let piece = body.get(at..at.checked_add(n)?)?;
        at += n;
        Some(piece)
    };
    if take(8)? != MAGIC || u32::from_le_bytes(take(4)?.try_into().ok()?) != FORMAT {
        return None;
    }
    let name_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let name = String::from_utf8(take(name_len)?.to_vec()).ok()?;
    let stamp = u64::from_le_bytes(take(8)?.try_into().ok()?);
    let size = u64::from_le_bytes(take(8)?.try_into().ok()?);
    let bits = take(blocks_of(size).div_ceil(8) as usize)?.to_vec();
    (at == body.len()).then_some((name, stamp, size, bits))
}

fn name_of(peer: &str, volume: &str, path: &str) -> String {
    format!("{peer}\0{volume}\0{path}")
}

/// The directory hash a stored name belongs under.
fn hash_of_name(name: &str) -> Option<u64> {
    let mut parts = name.splitn(3, '\0');
    Some(path_hash(parts.next()?, parts.next()?, parts.next()?))
}

fn remove_quietly(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(file = %path.display(), error = %e, "cannot delete a read cache file");
        }
    }
}

/// Mark a fresh data file sparse, so a block stored far into it does not make
/// NTFS fill everything before it with zeros. Unix filesystems leave holes
/// without being asked.
#[cfg(windows)]
fn make_sparse(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let mut returned = 0u32;
    // SAFETY: the handle belongs to `file`, which outlives the call; no
    // buffers are passed and the call is synchronous.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    match ok {
        0 => Err(std::io::Error::last_os_error()),
        _ => Ok(()),
    }
}

#[cfg(not(windows))]
fn make_sparse(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset)? {
            0 => return Err(std::io::ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_at(_file: &File, _buf: &mut [u8], _offset: u64) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "positional reads are not available on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn write_at(_file: &File, _buf: &[u8], _offset: u64) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "positional writes are not available on this platform",
    ))
}

impl ReadCache {
    /// Open the cache at `settings.root`, taking stock of what a previous run
    /// left and evicting down to the cap.
    pub fn open(settings: &CacheSettings) -> Option<Self> {
        if let Err(e) = std::fs::create_dir_all(&settings.root) {
            tracing::warn!(root = %settings.root.display(), error = %e, "cannot create the read cache; reading peers' files without it");
            return None;
        }
        let cache = Self {
            root: settings.root.clone(),
            cap: settings.cap_bytes,
            state: Mutex::new(State::default()),
        };
        let mut state = cache.lock();
        cache.scan(&mut state);
        cache.evict(&mut state, None);
        tracing::info!(
            root = %cache.root.display(),
            entries = state.entries.len(),
            mib = state.total / (1024 * 1024),
            cap_mib = cache.cap / (1024 * 1024),
            "read cache opened"
        );
        drop(state);
        Some(cache)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn dir_of(&self, dir: u64) -> PathBuf {
        self.root.join(format!("{dir:016x}"))
    }

    fn files_of(&self, dir: u64, stamp: u64) -> (PathBuf, PathBuf) {
        let d = self.dir_of(dir);
        (
            d.join(format!("{stamp:016x}.data")),
            d.join(format!("{stamp:016x}.map")),
        )
    }

    fn scan(&self, state: &mut State) {
        let Ok(dirs) = std::fs::read_dir(&self.root) else {
            return;
        };
        for dir in dirs.flatten() {
            let path = dir.path();
            let Some(hash) = dir
                .file_name()
                .to_str()
                .filter(|n| n.len() == 16)
                .and_then(|n| u64::from_str_radix(n, 16).ok())
            else {
                continue;
            };
            if let Some(entry) = self.scan_dir(&path, hash) {
                state.total += entry.bytes;
                state.entries.insert(hash, entry);
            }
            let _ = std::fs::remove_dir(&path);
        }
    }

    /// The one valid version kept in `dir`, everything else in it deleted.
    fn scan_dir(&self, dir: &Path, hash: u64) -> Option<Entry> {
        let mut best: Option<Entry> = None;
        let mut keep: Vec<PathBuf> = Vec::new();
        let files: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|f| f.path())
            .collect();
        for map in files
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "map"))
        {
            let data = map.with_extension("data");
            let parsed = std::fs::read(map).ok().and_then(|raw| decode_map(&raw));
            let valid = parsed.filter(|(name, stamp, size, _)| {
                hash_of_name(name) == Some(hash)
                    && map.file_stem().and_then(|s| s.to_str()) == Some(&format!("{stamp:016x}"))
                    && std::fs::metadata(&data).is_ok_and(|m| m.len() == *size)
            });
            let Some((name, stamp, size, bits)) = valid else {
                tracing::warn!(map = %map.display(), "dropping a read cache entry whose map is torn or whose data does not match it");
                continue;
            };
            let entry = Entry {
                bytes: present_bytes(&bits, size),
                name,
                stamp,
                size,
                since: 0,
                used: mtime_nanos(map),
                live: None,
            };
            if best.as_ref().is_none_or(|b| entry.used > b.used) {
                best = Some(entry);
                keep = vec![map.clone(), data];
            }
        }
        for file in files.iter().filter(|f| !keep.contains(f)) {
            remove_quietly(file);
        }
        best
    }

    /// The key a handle reads `path` under, for a file the owner vouched for.
    /// `None` for anything but a regular file with a stamp.
    pub fn key(&self, peer: &str, volume: &str, path: &str, meta: &FileMeta) -> Option<FileKey> {
        let stamp = meta.stamp?;
        if meta.kind != FileKind::File || meta.size > self.cap {
            return None;
        }
        let mut lineage = vec![path_hash(peer, volume, "")];
        for (at, c) in path.char_indices() {
            if c == '/' {
                lineage.push(path_hash(peer, volume, &path[..at]));
            }
        }
        lineage.push(path_hash(peer, volume, path));
        // Every key its own tick, so two handles of one path always know
        // which opened later and only the later may replace a version.
        let taken = {
            let mut state = self.lock();
            state.clock += 1;
            state.clock
        };
        Some(FileKey {
            name: name_of(peer, volume, path),
            dir: path_hash(peer, volume, path),
            lineage,
            stamp,
            size: meta.size,
            taken,
        })
    }

    /// Whether anything is stored for exactly this key — for a mount deciding
    /// whether opening the owner's file can wait until a read misses.
    pub fn holds(&self, key: &FileKey) -> bool {
        let state = self.lock();
        Self::valid(&state, key)
            && state.entries.get(&key.dir).is_some_and(|e| {
                e.name == key.name && e.stamp == key.stamp && e.size == key.size && e.bytes > 0
            })
    }

    fn valid(state: &State, key: &FileKey) -> bool {
        key.taken >= state.floor
            && key
                .lineage
                .iter()
                .all(|h| state.invalidated.get(h).is_none_or(|at| *at <= key.taken))
    }

    /// Something here is changing the file at `path` on the owner: void every
    /// key taken for it so far, and delete what is stored. Cheap enough for
    /// every write a copy makes.
    pub fn invalidate(&self, peer: &str, volume: &str, path: &str) {
        let mut state = self.lock();
        Self::void(&mut state, path_hash(peer, volume, path));
        let dir = path_hash(peer, volume, path);
        if state
            .entries
            .get(&dir)
            .is_some_and(|e| e.name == name_of(peer, volume, path))
        {
            self.remove(&mut state, dir);
        }
    }

    fn void(state: &mut State, hash: u64) {
        state.clock += 1;
        let clock = state.clock;
        if state.invalidated.len() >= MAX_INVALIDATED {
            state.invalidated.clear();
            state.floor = clock;
        }
        state.invalidated.insert(hash, clock);
    }

    /// As [`invalidate`](Self::invalidate), for a path that may be a
    /// directory being renamed or removed: everything under it goes too.
    pub fn invalidate_tree(&self, peer: &str, volume: &str, path: &str) {
        let mut state = self.lock();
        Self::void(&mut state, path_hash(peer, volume, path));
        let name = name_of(peer, volume, path);
        let under = match path.is_empty() {
            true => name.clone(),
            false => format!("{name}/"),
        };
        let doomed: Vec<u64> = state
            .entries
            .iter()
            .filter(|(_, e)| e.name == name || e.name.starts_with(&under))
            .map(|(h, _)| *h)
            .collect();
        for dir in doomed {
            self.remove(&mut state, dir);
        }
    }

    fn remove(&self, state: &mut State, dir: u64) {
        if let Some(entry) = state.entries.remove(&dir) {
            state.total -= entry.bytes;
            let (data, map) = self.files_of(dir, entry.stamp);
            remove_quietly(&map);
            remove_quietly(&data);
            let _ = std::fs::remove_dir(self.dir_of(dir));
        }
    }

    fn tick(state: &mut State) -> u64 {
        state.last_used = now_nanos().max(state.last_used + 1);
        state.last_used
    }
}

impl ReadCache {
    /// The bytes at `offset`, when every block covering them is stored for
    /// exactly this key. Short only at the end of the file, as a read is.
    pub fn get(&self, key: &FileKey, offset: u64, len: usize) -> Option<Vec<u8>> {
        let end = offset.saturating_add(len as u64).min(key.size);
        let live = {
            let mut state = self.lock();
            if !Self::valid(&state, key) {
                return None;
            }
            if offset >= end {
                return Some(Vec::new());
            }
            let live = self.live(&mut state, key)?;
            {
                let mut blocks = live.blocks.lock().unwrap_or_else(|p| p.into_inner());
                if !(offset / BLOCK..=(end - 1) / BLOCK).all(|b| has(&blocks.bits, b)) {
                    return None;
                }
                blocks.touched = true;
            }
            let used = Self::tick(&mut state);
            if let Some(entry) = state.entries.get_mut(&key.dir) {
                entry.used = used;
            }
            live
        };
        let mut buf = vec![0u8; (end - offset) as usize];
        match read_at(&live.data, &mut buf, offset) {
            Ok(()) => Some(buf),
            Err(e) => {
                tracing::warn!(file = %live.data_path.display(), error = %e, "cannot read the read cache; dropping the entry");
                let mut state = self.lock();
                if self.is_current(&state, key, &live) {
                    self.remove(&mut state, key.dir);
                }
                None
            }
        }
    }

    /// Store the blocks `bytes` (read at `offset`) cover whole. A block cut
    /// short by the range is left out; the end of the file counts as whole.
    pub fn put(&self, key: &FileKey, offset: u64, bytes: &[u8]) {
        let end = offset + bytes.len() as u64;
        if end > key.size {
            return;
        }
        let first = offset.div_ceil(BLOCK);
        let whole: Vec<u64> = (first..blocks_of(key.size))
            .take_while(|b| b * BLOCK + block_len(*b, key.size) <= end)
            .collect();
        if whole.is_empty() {
            return;
        }
        let live = {
            let mut state = self.lock();
            if !Self::valid(&state, key) {
                return;
            }
            let Some(live) = self.live_or_create(&mut state, key) else {
                return;
            };
            live
        };
        let wanted: Vec<u64> = {
            let blocks = live.blocks.lock().unwrap_or_else(|p| p.into_inner());
            whole
                .into_iter()
                .filter(|b| !has(&blocks.bits, *b))
                .collect()
        };
        for b in &wanted {
            let at = b * BLOCK;
            let piece =
                &bytes[(at - offset) as usize..(at - offset + block_len(*b, key.size)) as usize];
            if let Err(e) = write_at(&live.data, piece, at) {
                tracing::warn!(file = %live.data_path.display(), error = %e, "cannot store in the read cache; dropping the entry");
                let mut state = self.lock();
                if self.is_current(&state, key, &live) {
                    self.remove(&mut state, key.dir);
                }
                return;
            }
        }
        let due = {
            let mut state = self.lock();
            // Voided or replaced while the bytes were written: they went to a
            // file nothing will read.
            if !Self::valid(&state, key) || !self.is_current(&state, key, &live) {
                return;
            }
            let mut added = 0;
            let due = {
                let mut blocks = live.blocks.lock().unwrap_or_else(|p| p.into_inner());
                for b in &wanted {
                    if !has(&blocks.bits, *b) {
                        set(&mut blocks.bits, *b);
                        added += block_len(*b, key.size);
                    }
                }
                blocks.dirty |= added > 0;
                blocks.dirty && blocks.flushed.elapsed() >= FLUSH_EVERY
            };
            let used = Self::tick(&mut state);
            if let Some(entry) = state.entries.get_mut(&key.dir) {
                entry.bytes += added;
                entry.used = used;
            }
            state.total += added;
            if state.total > self.cap {
                self.evict(&mut state, Some(key.dir));
            }
            due
        };
        if due {
            Self::flush(&live);
        }
    }

    /// The handle reading under `key` is done: make what it stored durable,
    /// and mark the entry as used for the next run's eviction order.
    pub fn close(&self, key: &FileKey) {
        let live = {
            let state = self.lock();
            state.entries.get(&key.dir).and_then(|e| e.live.clone())
        };
        if let Some(live) = live {
            Self::flush(&live);
        }
    }

    /// Flush every open entry; for an orderly shutdown.
    pub fn flush_all(&self) {
        let live: Vec<Arc<Live>> = self
            .lock()
            .entries
            .values()
            .filter_map(|e| e.live.clone())
            .collect();
        for live in live {
            Self::flush(&live);
        }
    }

    fn is_current(&self, state: &State, key: &FileKey, live: &Arc<Live>) -> bool {
        state
            .entries
            .get(&key.dir)
            .and_then(|e| e.live.as_ref())
            .is_some_and(|l| Arc::ptr_eq(l, live))
    }

    /// The open entry for exactly this key, loading it from disk if needed.
    fn live(&self, state: &mut State, key: &FileKey) -> Option<Arc<Live>> {
        let entry = state.entries.get(&key.dir)?;
        if entry.name != key.name || entry.stamp != key.stamp || entry.size != key.size {
            return None;
        }
        if let Some(live) = &entry.live {
            return Some(live.clone());
        }
        let (data_path, map_path) = self.files_of(key.dir, key.stamp);
        let loaded = std::fs::read(&map_path)
            .ok()
            .and_then(|raw| decode_map(&raw))
            .filter(|(name, stamp, size, _)| {
                *name == key.name && *stamp == key.stamp && *size == key.size
            })
            .and_then(|(_, _, _, bits)| {
                let data = File::options()
                    .read(true)
                    .write(true)
                    .open(&data_path)
                    .ok()?;
                (data.metadata().ok()?.len() == key.size).then_some((data, bits))
            });
        let Some((data, bits)) = loaded else {
            tracing::warn!(map = %map_path.display(), "a read cache entry no longer matches its files; dropping it");
            self.remove(state, key.dir);
            return None;
        };
        self.adopt(state, key.dir, data, data_path, map_path, bits)
    }

    fn adopt(
        &self,
        state: &mut State,
        dir: u64,
        data: File,
        data_path: PathBuf,
        map_path: PathBuf,
        bits: Vec<u8>,
    ) -> Option<Arc<Live>> {
        self.close_spare(state, dir);
        let entry = state.entries.get(&dir)?;
        let live = Arc::new(Live {
            name: entry.name.clone(),
            stamp: entry.stamp,
            size: entry.size,
            data,
            data_path,
            map_path,
            blocks: Mutex::new(Blocks {
                bits,
                dirty: false,
                touched: false,
                flushed: Instant::now(),
            }),
            flushing: Mutex::new(()),
        });
        if let Some(entry) = state.entries.get_mut(&dir) {
            entry.live = Some(live.clone());
        }
        Some(live)
    }

    /// Keep the number of open data files under [`MAX_LIVE`], closing the
    /// least recently used other than `keep`.
    fn close_spare(&self, state: &mut State, keep: u64) {
        let open = state.entries.values().filter(|e| e.live.is_some()).count();
        if open < MAX_LIVE {
            return;
        }
        let oldest = state
            .entries
            .iter()
            .filter(|(h, e)| e.live.is_some() && **h != keep)
            .min_by_key(|(_, e)| e.used)
            .map(|(h, _)| *h);
        if let Some(live) = oldest
            .and_then(|h| state.entries.get_mut(&h))
            .and_then(|e| e.live.take())
        {
            Self::flush(&live);
        }
    }

    /// The open entry for this key, created when there is none or when the
    /// stored version is older than the key.
    fn live_or_create(&self, state: &mut State, key: &FileKey) -> Option<Arc<Live>> {
        if let Some(entry) = state.entries.get(&key.dir) {
            if entry.name != key.name {
                // Two paths with one hash: leave the one already stored.
                return None;
            }
            if entry.stamp == key.stamp && entry.size == key.size {
                return self.live(state, key);
            }
            if key.taken < entry.since {
                return None;
            }
            self.remove(state, key.dir);
        }
        let (data_path, map_path) = self.files_of(key.dir, key.stamp);
        let created = std::fs::create_dir_all(self.dir_of(key.dir))
            .and_then(|()| {
                File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&data_path)
            })
            .and_then(|data| {
                make_sparse(&data)?;
                data.set_len(key.size)?;
                Ok(data)
            });
        let data = match created {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(file = %data_path.display(), error = %e, "cannot create a read cache entry; reading this file without it");
                remove_quietly(&data_path);
                return None;
            }
        };
        let used = Self::tick(state);
        state.entries.insert(
            key.dir,
            Entry {
                name: key.name.clone(),
                stamp: key.stamp,
                size: key.size,
                since: key.taken,
                used,
                bytes: 0,
                live: None,
            },
        );
        let bits = vec![0u8; blocks_of(key.size).div_ceil(8) as usize];
        self.adopt(state, key.dir, data, data_path, map_path, bits)
    }

    /// Sync the data, then publish the bits that were set before the sync.
    fn flush(live: &Live) {
        let _one = live.flushing.lock().unwrap_or_else(|p| p.into_inner());
        let (bits, dirty, touched) = {
            let mut blocks = live.blocks.lock().unwrap_or_else(|p| p.into_inner());
            let snapshot = (blocks.bits.clone(), blocks.dirty, blocks.touched);
            blocks.dirty = false;
            blocks.touched = false;
            blocks.flushed = Instant::now();
            snapshot
        };
        if !dirty {
            if touched {
                let _ = File::options()
                    .write(true)
                    .open(&live.map_path)
                    .and_then(|m| m.set_modified(SystemTime::now()));
            }
            return;
        }
        let written = live.data.sync_data().and_then(|()| {
            let tmp = live.map_path.with_extension("map.tmp");
            let mut out = File::create(&tmp)?;
            out.write_all(&encode_map(&live.name, live.stamp, live.size, &bits))?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&tmp, &live.map_path)
        });
        if let Err(e) = written {
            tracing::warn!(map = %live.map_path.display(), error = %e, "cannot flush the read cache; its newest blocks will not survive a restart");
            live.blocks.lock().unwrap_or_else(|p| p.into_inner()).dirty = true;
        }
    }
}

impl ReadCache {
    /// Delete the least recently used entries until the cache is back under
    /// [`EVICT_TO`] of its cap, sparing `current` (the entry being filled)
    /// while anything else can go.
    fn evict(&self, state: &mut State, current: Option<u64>) {
        if state.total <= self.cap {
            return;
        }
        let target = (self.cap as f64 * EVICT_TO) as u64;
        let mut order: Vec<(u64, u64)> = state
            .entries
            .iter()
            .map(|(h, e)| (Some(*h) == current, e.used, *h))
            .map(|(spared, used, h)| (if spared { u64::MAX } else { used }, h))
            .collect();
        order.sort_unstable();
        for (_, dir) in order {
            if state.total <= target {
                break;
            }
            self.remove(state, dir);
        }
    }

    /// Serve a read from the cache, or fetch the blocks covering it through
    /// `fetch(offset, len)` — a wire read, short only at end of file — and
    /// keep them.
    ///
    /// Fetched block-aligned so the bytes can be kept at all: a read of 4 KiB
    /// in the middle of a block stores nothing, and the next launch would ask
    /// the wire again for every such read. An answer shorter than the length
    /// the stamp promised is not kept: the file changed under its metadata.
    pub fn read_through<E>(
        &self,
        key: &FileKey,
        offset: u64,
        len: usize,
        fetch: impl FnOnce(u64, u64) -> Result<Vec<u8>, E>,
    ) -> Result<Vec<u8>, E> {
        if let Some(hit) = self.get(key, offset, len) {
            return Ok(hit);
        }
        let end = offset.saturating_add(len as u64).min(key.size);
        if offset >= end {
            return fetch(offset, len as u64);
        }
        let from = offset / BLOCK * BLOCK;
        let to = end.div_ceil(BLOCK).saturating_mul(BLOCK).min(key.size);
        let bytes = fetch(from, to - from)?;
        if bytes.len() as u64 == to - from {
            self.put(key, from, &bytes);
        }
        let start = ((offset - from) as usize).min(bytes.len());
        let stop = ((end - from) as usize).min(bytes.len());
        Ok(bytes[start..stop].to_vec())
    }
}

impl Drop for ReadCache {
    fn drop(&mut self) {
        self.flush_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(dir: &Path, cap: u64) -> CacheSettings {
        CacheSettings {
            root: dir.to_path_buf(),
            cap_bytes: cap,
        }
    }

    fn meta(size: u64, stamp: u64) -> FileMeta {
        FileMeta {
            name: "f".into(),
            kind: FileKind::File,
            size,
            modified_unix: 0,
            writable: true,
            allocated: None,
            stamp: Some(stamp),
        }
    }

    /// A file whose every byte names its own position, so a misplaced block
    /// shows.
    fn contents(size: u64) -> Vec<u8> {
        (0..size).map(|i| (i % 251) as u8).collect()
    }

    const BIG: u64 = 1 << 30;

    #[test]
    fn stored_blocks_are_served_and_a_read_spanning_them_is_whole() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
        let size = BLOCK * 2 + 1000;
        let file = contents(size);
        let key = cache
            .key("Nocturne", "/", "game/data.pak", &meta(size, 7))
            .expect("key");

        cache.put(&key, 0, &file);
        assert_eq!(cache.get(&key, 100, 10).as_deref(), Some(&file[100..110]));
        let across = cache.get(&key, BLOCK - 5, 10).expect("spans two blocks");
        assert_eq!(across, &file[(BLOCK - 5) as usize..(BLOCK + 5) as usize]);
        let tail = cache
            .get(&key, size - 10, 4096)
            .expect("the short last block");
        assert_eq!(tail, &file[(size - 10) as usize..], "short only at the end");
        assert_eq!(cache.get(&key, size, 10), Some(Vec::new()));
    }

    #[test]
    fn a_block_the_bytes_only_partly_cover_is_not_stored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
        let size = BLOCK * 3;
        let file = contents(size);
        let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");

        // Half of block 0, all of block 1, a quarter of block 2.
        let from = BLOCK / 2;
        let to = BLOCK * 2 + BLOCK / 4;
        cache.put(&key, from, &file[from as usize..to as usize]);
        assert_eq!(cache.get(&key, 0, 10), None, "block 0 was cut short");
        assert_eq!(
            cache.get(&key, BLOCK * 2, 10),
            None,
            "block 2 was cut short"
        );
        assert_eq!(
            cache.get(&key, BLOCK, BLOCK as usize).as_deref(),
            Some(&file[BLOCK as usize..(BLOCK * 2) as usize])
        );
        assert_eq!(
            cache.get(&key, BLOCK - 1, 2),
            None,
            "one missing block misses the read"
        );
    }

    #[test]
    fn a_stale_stamp_is_never_served_and_a_newer_one_replaces_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
        let old = cache.key("p", "v", "a", &meta(BLOCK, 1)).expect("key");
        cache.put(&old, 0, &vec![1u8; BLOCK as usize]);

        let new = cache.key("p", "v", "a", &meta(BLOCK, 2)).expect("key");
        assert_eq!(
            cache.get(&new, 0, 4),
            None,
            "another version is another file"
        );
        cache.put(&new, 0, &vec![2u8; BLOCK as usize]);
        assert_eq!(cache.get(&new, 0, 4), Some(vec![2u8; 4]));
        assert_eq!(cache.get(&old, 0, 4), None, "the old version is gone");

        // A handle still holding the old version may not bring it back.
        cache.put(&old, 0, &vec![1u8; BLOCK as usize]);
        assert_eq!(cache.get(&new, 0, 4), Some(vec![2u8; 4]));
        cache.close(&new);
        let files = std::fs::read_dir(cache.dir_of(new.dir))
            .expect("dir")
            .count();
        assert_eq!(files, 2, "one version on disk: its data and its map");
    }

    #[test]
    fn blocks_survive_a_restart_only_once_flushed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let size = BLOCK * 2;
        let file = contents(size);
        {
            let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
            let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");
            cache.put(&key, 0, &file[..BLOCK as usize]);
            cache.close(&key);
            cache.put(&key, BLOCK, &file[BLOCK as usize..]);
            // The process dies before the second block is flushed.
            std::mem::forget(cache);
        }
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("reopens");
        let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");
        assert_eq!(
            cache.get(&key, 0, 8).as_deref(),
            Some(&file[..8]),
            "flushed before the crash"
        );
        assert_eq!(
            cache.get(&key, BLOCK, 8),
            None,
            "never flushed, so never claimed"
        );
    }

    #[test]
    fn a_map_naming_data_that_is_not_there_is_treated_as_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let size = BLOCK;
        let (data, map) = {
            let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
            let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");
            cache.put(&key, 0, &contents(size));
            cache.close(&key);
            cache.files_of(key.dir, key.stamp)
        };
        // The map reached the disk; the data file did not (or lost its length).
        std::fs::File::options()
            .write(true)
            .open(&data)
            .expect("data")
            .set_len(10)
            .expect("cut");
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("reopens");
        let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");
        assert_eq!(cache.get(&key, 0, 8), None);
        assert!(
            !map.exists() && !data.exists(),
            "the mismatched entry was deleted"
        );
    }

    #[test]
    fn a_torn_map_is_treated_as_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let size = BLOCK;
        let map = {
            let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
            let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");
            cache.put(&key, 0, &contents(size));
            cache.close(&key);
            cache.files_of(key.dir, key.stamp).1
        };
        let raw = std::fs::read(&map).expect("map");
        std::fs::write(&map, &raw[..raw.len() - 3]).expect("tear");
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("reopens");
        let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");
        assert_eq!(cache.get(&key, 0, 8), None);

        let mut garbled = raw.clone();
        garbled[20] ^= 0xff;
        assert_eq!(
            decode_map(&garbled),
            None,
            "a flipped byte fails the checksum"
        );
    }

    #[test]
    fn the_least_recently_used_entry_goes_first_when_over_the_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = ReadCache::open(&settings(tmp.path(), BLOCK * 3)).expect("opens");
        let block = vec![9u8; BLOCK as usize];
        let a = cache.key("p", "v", "a", &meta(BLOCK, 1)).expect("key");
        let b = cache.key("p", "v", "b", &meta(BLOCK, 1)).expect("key");
        let c = cache.key("p", "v", "c", &meta(BLOCK * 2, 1)).expect("key");
        cache.put(&a, 0, &block);
        cache.put(&b, 0, &block);
        assert!(cache.get(&a, 0, 1).is_some(), "a is now more recent than b");
        cache.put(&c, 0, &[block.clone(), block.clone()].concat());
        assert_eq!(cache.get(&b, 0, 1), None, "b was used least recently");
        assert!(cache.get(&c, 0, 1).is_some());
        assert!(cache.lock().total <= BLOCK * 3);

        let huge = meta(BLOCK * 4, 1);
        assert!(
            cache.key("p", "v", "huge", &huge).is_none(),
            "a file over the cap is never kept"
        );
    }

    #[test]
    fn eviction_order_survives_a_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let block = vec![9u8; BLOCK as usize];
        {
            let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
            for (name, pause) in [("old", true), ("new", false)] {
                let key = cache.key("p", "v", name, &meta(BLOCK, 1)).expect("key");
                cache.put(&key, 0, &block);
                cache.close(&key);
                if pause {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let cache = ReadCache::open(&settings(tmp.path(), BLOCK * 3 / 2))
            .expect("reopens under a cap that holds one of them");
        let old = cache.key("p", "v", "old", &meta(BLOCK, 1)).expect("key");
        let new = cache.key("p", "v", "new", &meta(BLOCK, 1)).expect("key");
        assert_eq!(cache.get(&old, 0, 1), None);
        assert!(cache.get(&new, 0, 1).is_some());
    }

    #[test]
    fn a_write_through_the_mount_voids_handles_opened_before_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
        let block = vec![3u8; BLOCK as usize];
        let before = cache.key("p", "v", "dir/a", &meta(BLOCK, 1)).expect("key");
        cache.put(&before, 0, &block);
        assert!(cache.get(&before, 0, 1).is_some());

        cache.invalidate("p", "v", "dir/a");
        assert_eq!(cache.get(&before, 0, 1), None, "the stored copy is gone");
        cache.put(&before, 0, &block);
        assert_eq!(
            cache.get(&before, 0, 1),
            None,
            "and the old handle cannot store again"
        );

        // The metadata after the write may still carry the old stamp; a handle
        // opened from now on reads and stores normally.
        let after = cache.key("p", "v", "dir/a", &meta(BLOCK, 1)).expect("key");
        cache.put(&after, 0, &block);
        assert!(cache.get(&after, 0, 1).is_some());

        // A directory renamed or removed takes everything under it.
        cache.invalidate_tree("p", "v", "dir");
        assert_eq!(cache.get(&after, 0, 1), None);
        let fresh = cache.key("p", "v", "dir/a", &meta(BLOCK, 1)).expect("key");
        assert_eq!(
            cache.get(&fresh, 0, 1),
            None,
            "and what was stored under it is gone"
        );
        let sibling = cache.key("p", "v", "dirt", &meta(BLOCK, 1)).expect("key");
        cache.put(&sibling, 0, &block);
        cache.invalidate_tree("p", "v", "dir");
        assert!(
            cache.get(&sibling, 0, 1).is_some(),
            "a name sharing the prefix is another path"
        );
    }

    #[test]
    fn a_file_without_a_stamp_is_not_cached() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
        let mut m = meta(10, 1);
        m.stamp = None;
        assert!(cache.key("p", "v", "a", &m).is_none());
        m.stamp = Some(1);
        m.kind = FileKind::Dir;
        assert!(cache.key("p", "v", "a", &m).is_none());
    }

    /// A small read in the middle of a block fetches the whole block once;
    /// every later read inside it, this run or the next, stays off the wire.
    #[test]
    fn a_miss_fetches_whole_blocks_and_the_next_read_stays_local() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let size = BLOCK * 3 + 77;
        let file = contents(size);
        let fetches = std::cell::RefCell::new(Vec::new());
        let fetch = |at: u64, len: u64| -> Result<Vec<u8>, ()> {
            fetches.borrow_mut().push((at, len));
            let end = (at + len).min(size);
            Ok(file[at as usize..end as usize].to_vec())
        };
        {
            let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
            let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");
            let got = cache
                .read_through(&key, BLOCK + 10, 4096, fetch)
                .expect("reads");
            assert_eq!(
                got,
                &file[(BLOCK + 10) as usize..(BLOCK + 10 + 4096) as usize]
            );
            let tail = cache
                .read_through(&key, size - 5, 4096, fetch)
                .expect("reads the end");
            assert_eq!(tail, &file[(size - 5) as usize..]);
            cache.close(&key);
        }
        assert_eq!(
            *fetches.borrow(),
            vec![(BLOCK, BLOCK), (BLOCK * 3, 77)],
            "aligned to blocks, clamped to the file"
        );
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("reopens");
        let key = cache.key("p", "v", "a", &meta(size, 1)).expect("key");
        let again = cache
            .read_through(&key, BLOCK + 100, 1000, fetch)
            .expect("reads");
        assert_eq!(
            again,
            &file[(BLOCK + 100) as usize..(BLOCK + 1100) as usize]
        );
        assert_eq!(
            fetches.borrow().len(),
            2,
            "the second launch read nothing over the wire"
        );
    }

    #[test]
    fn a_fetch_shorter_than_the_stamp_promised_is_not_kept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = ReadCache::open(&settings(tmp.path(), BIG)).expect("opens");
        let key = cache.key("p", "v", "a", &meta(BLOCK * 2, 1)).expect("key");
        let got = cache
            .read_through(&key, 0, 10, |_, _| Ok::<_, ()>(vec![5u8; 100]))
            .expect("reads");
        assert_eq!(got, vec![5u8; 10], "the caller gets what the owner has");
        assert_eq!(
            cache.get(&key, 0, 10),
            None,
            "but the file shrank, so none is kept"
        );
    }
}
