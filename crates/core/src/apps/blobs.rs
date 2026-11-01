//! Files between copies of one application, by their content hash — the
//! node's side of `Cursio::fetch` and `App::on_blob` in `cursio-sdk`
//! (`docs/app-sdk-design.md`, "Files by their content").
//!
//! A copy that wants a modpack's mods, or a world, names each file by the
//! hash its own manifest already holds. Its node asks every connected machine
//! where the application runs which of those it has ([`AppsWire::BlobHave`]);
//! each answering node asks its running copy for the file that holds each
//! hash (`on_blob`) — the node never reads a path the application did not
//! name, and never one off the wire. Then the bytes come over in chunks, a
//! few outstanding at a time ([`WINDOW`]) on the bulk class so a pointer
//! moving meanwhile is not held up, several machines serving different blobs
//! at once.
//!
//! **Nothing counts until it hashes right.** Chunks land in a partial file
//! beside the destination; only once the whole file hashes to what was asked
//! is it renamed into place under [`BlobHash::file_name`]. A partial file left
//! by a dropped link is the start of the next attempt: it is kept, and asked
//! for from where it ends. A file whose bytes do not match is deleted — a
//! corrupted chunk cannot become a file the application trusts.
//!
//! **Whose files leave.** A machine gives its copy's files only to a machine
//! its person lets drive that application (`[apps.<id>] actions`,
//! [`super::actions::allowed`]) — the trust messages between copies use: it
//! is the same people's same devices, and a second question would have the
//! same answer.
//!
//! What no connected copy holds is reported back, not looked for elsewhere:
//! where else to get it — a CDN, the application's own cloud — is the
//! application's to know.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursio_app_protocol::{AppId, BlobAlgo, BlobEntry, BlobFailure, BlobHash, MAX_BLOB};
use cursio_protocol::apps::AppsWire;

/// Bytes one [`AppsWire::BlobWant`] asks for.
pub const CHUNK: u32 = 256 * 1024;

/// Chunks asked for and not yet answered, per blob: enough to keep a LAN
/// link full, few enough that a link's bulk queue never holds more than a
/// couple of megabytes ahead of everything else.
pub const WINDOW: usize = 8;

/// Most hashes one [`AppsWire::BlobHave`] names.
pub const HAVE_BATCH: usize = 1000;

/// How long a peer has to answer one question about blobs.
pub const ASK_WAIT: Duration = Duration::from_secs(30);

/// How the fetching side reaches the machines that may hold blobs: the mesh
/// in a node, the other half directly in a test.
#[async_trait::async_trait]
pub trait Peers: Send + Sync {
    /// Send `wire` about `app` to `peer` and wait for its answer.
    async fn ask(&self, peer: &str, app: &AppId, wire: AppsWire) -> Result<AppsWire, String>;
}

/// The serving side's application: which file holds a blob, as the running
/// copy names it.
pub trait Holder: Send + Sync {
    fn path(&self, app: &AppId, hash: &BlobHash) -> Option<PathBuf>;
}

/// How long the running copy has to name the file for a hash.
const PATH_WAIT: Duration = Duration::from_secs(5);

/// Most paths remembered for the chunks that follow a question.
const MAX_REMEMBERED: usize = 50_000;

/// This node's copies of an application: the first running process that
/// serves blobs names the file. Its answer is remembered, so the hundreds of
/// chunks of one file do not each ask the application again.
pub struct NodeHolder;

fn remembered() -> std::sync::MutexGuard<'static, HashMap<(AppId, BlobHash), PathBuf>> {
    static PATHS: std::sync::OnceLock<Mutex<HashMap<(AppId, BlobHash), PathBuf>>> =
        std::sync::OnceLock::new();
    PATHS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

impl Holder for NodeHolder {
    fn path(&self, app: &AppId, hash: &BlobHash) -> Option<PathBuf> {
        let key = (app.clone(), hash.clone());
        if let Some(path) = remembered().get(&key) {
            if path.is_file() {
                return Some(path.clone());
            }
        }
        let servers = super::registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .blob_servers(app);
        let path = servers
            .iter()
            .find_map(|server| super::serve::blob_path(server, hash, PATH_WAIT))?;
        let mut paths = remembered();
        if paths.len() >= MAX_REMEMBERED {
            paths.clear();
        }
        paths.insert(key, path.clone());
        Some(path)
    }
}

/// Whether `peer` may take this machine's copy's files of `app`.
pub type Allowed = dyn Fn(&str, &AppId) -> Result<(), String> + Send + Sync;

/// Told how far a blob is: `received` of `size` bytes.
pub type Progress = dyn Fn(&BlobHash, u64, u64) + Send + Sync;

/// What became of a fetch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub fetched: Vec<BlobHash>,
    pub not_found: Vec<BlobHash>,
    pub failed: Vec<BlobFailure>,
}

/// Answer `peer`'s question about this machine's copy of `app`'s blobs, or
/// `None` for a wire kind that is not a question about blobs.
pub fn serve(
    peer: &str,
    app: &str,
    wire: AppsWire,
    holder: &dyn Holder,
    allowed: &Allowed,
) -> Option<AppsWire> {
    let id_of = |app: &str| AppId::parse(app).map_err(|e| e.to_string());
    match wire {
        AppsWire::BlobHave { id, hashes } => {
            let refused = |why: String| AppsWire::BlobHas {
                id,
                have: Vec::new(),
                refused: Some(why),
            };
            let app = match id_of(app) {
                Ok(app) => app,
                Err(why) => return Some(refused(why)),
            };
            if let Err(why) = allowed(peer, &app) {
                return Some(refused(why));
            }
            let have = hashes
                .iter()
                .take(HAVE_BATCH)
                .filter_map(|text| {
                    let hash = BlobHash::parse(text).ok()?;
                    let size = std::fs::metadata(holder.path(&app, &hash)?).ok()?;
                    (size.is_file() && size.len() <= MAX_BLOB).then(|| (text.clone(), size.len()))
                })
                .collect();
            Some(AppsWire::BlobHas {
                id,
                have,
                refused: None,
            })
        }
        AppsWire::BlobWant {
            id,
            hash,
            offset,
            length,
        } => {
            let chunk = |result: Result<Vec<u8>, String>| match result {
                Ok(data) => AppsWire::BlobChunk {
                    id,
                    data,
                    error: None,
                },
                Err(why) => AppsWire::BlobChunk {
                    id,
                    data: Vec::new(),
                    error: Some(why),
                },
            };
            let read = || -> Result<Vec<u8>, String> {
                let app = id_of(app)?;
                allowed(peer, &app)?;
                let hash = BlobHash::parse(&hash).map_err(|e| e.to_string())?;
                let path = holder
                    .path(&app, &hash)
                    .ok_or_else(|| format!("{app} here no longer holds {hash}"))?;
                read_chunk(&path, offset, length.min(CHUNK))
            };
            Some(chunk(read()))
        }
        _ => None,
    }
}

fn read_chunk(path: &Path, offset: u64, length: u32) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut data = Vec::with_capacity(length as usize);
    file.take(u64::from(length))
        .read_to_end(&mut data)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(data)
}

/// What `path` hashes to, by `algo`.
pub fn hash_file(path: &Path, algo: BlobAlgo) -> std::io::Result<BlobHash> {
    use sha2::Digest;
    fn digest<D: Digest>(mut file: std::fs::File) -> std::io::Result<String> {
        let mut hasher = D::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
    let file = std::fs::File::open(path)?;
    let hex = match algo {
        BlobAlgo::Sha1 => digest::<sha1::Sha1>(file)?,
        BlobAlgo::Sha256 => digest::<sha2::Sha256>(file)?,
        BlobAlgo::Sha512 => digest::<sha2::Sha512>(file)?,
    };
    BlobHash::new(algo, &hex).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Whether `path` is already the blob `hash` of `size` bytes.
fn already_there(path: &Path, hash: &BlobHash, size: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == size)
        && hash_file(path, hash.algo()).is_ok_and(|h| h == *hash)
}

fn next_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Fetch `entries` of `app` from `peers` into `dest`, telling `progress` how
/// far each blob is (`received`, `size`).
pub async fn fetch(
    asker: Arc<dyn Peers>,
    peers: Vec<String>,
    app: &AppId,
    entries: Vec<BlobEntry>,
    dest: &Path,
    progress: Arc<Progress>,
) -> Report {
    let mut report = Report::default();
    // One file per content: two entries with one hash are one fetch.
    let mut wanted: BTreeMap<BlobHash, u64> = BTreeMap::new();
    for entry in entries {
        wanted.entry(entry.hash).or_insert(entry.size);
    }
    if let Err(e) = tokio::fs::create_dir_all(dest).await {
        report.failed = wanted
            .into_keys()
            .map(|hash| BlobFailure {
                hash,
                reason: format!("cannot create {}: {e}", dest.display()),
            })
            .collect();
        return report;
    }
    let mut missing = BTreeMap::new();
    for (hash, size) in wanted {
        if size > MAX_BLOB {
            report.failed.push(BlobFailure {
                hash,
                reason: format!("a blob is at most {MAX_BLOB} bytes, not {size}"),
            });
            continue;
        }
        let path = dest.join(hash.file_name());
        let check = (path, hash.clone());
        let present = tokio::task::spawn_blocking(move || already_there(&check.0, &check.1, size))
            .await
            .unwrap_or(false);
        match present {
            true => {
                progress(&hash, size, size);
                report.fetched.push(hash);
            }
            false => {
                missing.insert(hash, size);
            }
        }
    }

    // Who holds what, by asking every machine about every missing blob.
    let mut holders: BTreeMap<BlobHash, Vec<String>> = BTreeMap::new();
    let hashes: Vec<String> = missing.keys().map(BlobHash::to_string).collect();
    for peer in &peers {
        for batch in hashes.chunks(HAVE_BATCH) {
            let ask = AppsWire::BlobHave {
                id: next_id(),
                hashes: batch.to_vec(),
            };
            match asker.ask(peer, app, ask).await {
                Ok(AppsWire::BlobHas {
                    have,
                    refused: None,
                    ..
                }) => {
                    for (text, size) in have {
                        let Ok(hash) = BlobHash::parse(&text) else {
                            continue;
                        };
                        // A holder whose file is another size has another file.
                        if missing.get(&hash) == Some(&size) {
                            holders.entry(hash).or_default().push(peer.clone());
                        }
                    }
                }
                Ok(AppsWire::BlobHas {
                    refused: Some(why), ..
                }) => {
                    tracing::info!(%peer, %app, %why, "a machine gives no files of this application to this one");
                }
                Ok(other) => {
                    tracing::debug!(%peer, ?other, "a blob question answered with something else");
                }
                Err(why) => {
                    tracing::debug!(%peer, %why, "a machine did not say which blobs it holds")
                }
            }
        }
    }

    // Each blob from one holder, spreading the bytes across them.
    let mut load: HashMap<String, u64> = HashMap::new();
    let mut plan: BTreeMap<String, Vec<(BlobHash, u64)>> = BTreeMap::new();
    for (hash, size) in missing {
        let Some(candidates) = holders.remove(&hash) else {
            report.not_found.push(hash);
            continue;
        };
        let peer = candidates
            .into_iter()
            .min_by_key(|p| load.get(p).copied().unwrap_or(0))
            .expect("a holder was found");
        *load.entry(peer.clone()).or_default() += size;
        plan.entry(peer).or_default().push((hash, size));
    }

    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut machines = tokio::task::JoinSet::new();
    for (peer, blobs) in plan {
        let (asker, app, dest, progress, outcomes) = (
            asker.clone(),
            app.clone(),
            dest.to_path_buf(),
            progress.clone(),
            outcomes.clone(),
        );
        machines.spawn(async move {
            for (hash, size) in blobs {
                let got =
                    download(asker.clone(), &peer, &app, &hash, size, &dest, &*progress).await;
                if let Err(why) = &got {
                    tracing::info!(%peer, %app, %hash, %why, "a blob did not arrive");
                }
                outcomes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((hash, got));
            }
        });
    }
    while machines.join_next().await.is_some() {}
    let outcomes = std::mem::take(&mut *outcomes.lock().unwrap_or_else(|p| p.into_inner()));
    for (hash, got) in outcomes {
        match got {
            Ok(()) => report.fetched.push(hash),
            Err(reason) => report.failed.push(BlobFailure { hash, reason }),
        }
    }
    report
}

/// The file a blob is gathered in until it hashes right.
fn partial_of(dest: &Path, hash: &BlobHash) -> PathBuf {
    dest.join(format!("{}.partial", hash.file_name()))
}

/// Fetch one blob from `peer` into `dest`, from where an earlier attempt's
/// partial file ends.
async fn download(
    asker: Arc<dyn Peers>,
    peer: &str,
    app: &AppId,
    hash: &BlobHash,
    size: u64,
    dest: &Path,
    progress: &Progress,
) -> Result<(), String> {
    let partial = partial_of(dest, hash);
    let done = dest.join(hash.file_name());
    let io = |e: std::io::Error| format!("{}: {e}", partial.display());
    let kept = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
    // Whole chunks only: a chunk cut off mid-write is asked for again. A
    // partial file already whole is only checked.
    let from = match kept.min(size) {
        whole if whole == size => size,
        kept => (kept / u64::from(CHUNK)) * u64::from(CHUNK),
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&partial)
        .map_err(io)?;
    file.set_len(from).map_err(io)?;
    if from > 0 {
        tracing::info!(%peer, %hash, from, size, "resuming a blob where an earlier attempt stopped");
    }
    let mut received = from;
    progress(hash, received, size);

    let mut next = from;
    let mut asked = tokio::task::JoinSet::new();
    let text = hash.to_string();
    while next < size || !asked.is_empty() {
        while asked.len() < WINDOW && next < size {
            let length = u64::from(CHUNK).min(size - next) as u32;
            let want = AppsWire::BlobWant {
                id: next_id(),
                hash: text.clone(),
                offset: next,
                length,
            };
            let (peer, app, asker) = (peer.to_owned(), app.clone(), asker.clone());
            asked.spawn(async move { (next, length, asker.ask(&peer, &app, want).await) });
            next += u64::from(length);
        }
        let Some(joined) = asked.join_next().await else {
            break;
        };
        let (offset, length, answer) = joined.map_err(|e| e.to_string())?;
        let data = match answer {
            Ok(AppsWire::BlobChunk {
                data, error: None, ..
            }) => data,
            Ok(AppsWire::BlobChunk {
                error: Some(why), ..
            }) => return Err(format!("{peer}: {why}")),
            Ok(other) => return Err(format!("{peer} answered {other:?}")),
            Err(why) => return Err(why),
        };
        if data.len() != length as usize {
            return Err(format!(
                "{peer} sent {} bytes at {offset} where {length} were asked",
                data.len()
            ));
        }
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        file.write_all(&data).map_err(io)?;
        received += u64::from(length);
        progress(hash, received.min(size), size);
    }
    file.sync_all().map_err(io)?;
    drop(file);

    let (check, algo) = (partial.clone(), hash.algo());
    let got = tokio::task::spawn_blocking(move || hash_file(&check, algo))
        .await
        .map_err(|e| e.to_string())?
        .map_err(io)?;
    if got != *hash {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "the bytes from {peer} hash to {got}, not {hash}; discarded"
        ));
    }
    std::fs::rename(&partial, &done).map_err(|e| format!("{}: {e}", done.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch(name: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cursio-blobs-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A file of `len` bytes that are not all the same, and its sha256.
    fn blob(dir: &Path, len: usize) -> (PathBuf, BlobHash) {
        let path = dir.join(format!("source-{len}"));
        let bytes: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&path, bytes).unwrap();
        let hash = hash_file(&path, BlobAlgo::Sha256).unwrap();
        (path, hash)
    }

    struct Files(HashMap<BlobHash, PathBuf>);

    impl Holder for Files {
        fn path(&self, _app: &AppId, hash: &BlobHash) -> Option<PathBuf> {
            self.0.get(hash).cloned()
        }
    }

    /// The serving half, reached directly: what a peer's node answers.
    struct Direct {
        files: Files,
        allow: bool,
        /// Flip one byte of the chunk at this offset on its way.
        corrupt_at: Option<u64>,
        asked_offsets: Mutex<Vec<u64>>,
    }

    #[async_trait::async_trait]
    impl Peers for Direct {
        async fn ask(&self, _peer: &str, app: &AppId, wire: AppsWire) -> Result<AppsWire, String> {
            let offset = match &wire {
                AppsWire::BlobWant { offset, .. } => Some(*offset),
                _ => None,
            };
            if let Some(offset) = offset {
                self.asked_offsets.lock().unwrap().push(offset);
            }
            let allow = self.allow;
            let allowed = move |_: &str, _: &AppId| match allow {
                true => Ok(()),
                false => Err("not allowed here".to_owned()),
            };
            let mut answer = serve("Nocturne", app.as_str(), wire, &self.files, &allowed)
                .ok_or("not a question")?;
            if let (Some(at), Some(offset), AppsWire::BlobChunk { data, .. }) =
                (self.corrupt_at, offset, &mut answer)
            {
                if at == offset && !data.is_empty() {
                    data[0] ^= 0xff;
                }
            }
            Ok(answer)
        }
    }

    fn direct(files: Vec<(BlobHash, PathBuf)>) -> Direct {
        Direct {
            files: Files(files.into_iter().collect()),
            allow: true,
            corrupt_at: None,
            asked_offsets: Mutex::new(Vec::new()),
        }
    }

    fn entry(hash: &BlobHash, size: u64) -> BlobEntry {
        BlobEntry {
            path: "mods/x.jar".into(),
            hash: hash.clone(),
            size,
        }
    }

    async fn run(peer: Arc<Direct>, entries: Vec<BlobEntry>, dest: &Path) -> (Report, u64) {
        let last = Arc::new(Mutex::new(0u64));
        let seen = last.clone();
        let progress = Arc::new(move |_: &BlobHash, received: u64, _: u64| {
            *seen.lock().unwrap() = received;
        });
        let app = AppId::parse("leavepulse").unwrap();
        let report = fetch(
            peer,
            vec!["Lightmoon".into()],
            &app,
            entries,
            dest,
            progress,
        )
        .await;
        let last = *last.lock().unwrap();
        (report, last)
    }

    #[tokio::test]
    async fn a_blob_of_several_chunks_arrives_whole_and_is_kept_under_its_hash() {
        let (src, dest) = (scratch("src"), scratch("dest"));
        let len = CHUNK as usize * 2 + 1000;
        let (path, hash) = blob(&src, len);
        let peer = Arc::new(direct(vec![(hash.clone(), path.clone())]));
        let (report, last) = run(peer, vec![entry(&hash, len as u64)], &dest).await;
        assert_eq!(report.fetched, vec![hash.clone()]);
        assert_eq!(last, len as u64, "progress reached the end");
        assert_eq!(
            std::fs::read(dest.join(hash.file_name())).unwrap(),
            std::fs::read(&path).unwrap()
        );
        assert!(!partial_of(&dest, &hash).exists());
    }

    #[tokio::test]
    async fn a_corrupted_chunk_never_becomes_a_file() {
        let (src, dest) = (scratch("src"), scratch("dest"));
        let len = CHUNK as usize * 2;
        let (path, hash) = blob(&src, len);
        let mut peer = direct(vec![(hash.clone(), path)]);
        peer.corrupt_at = Some(u64::from(CHUNK));
        let (report, _) = run(Arc::new(peer), vec![entry(&hash, len as u64)], &dest).await;
        assert!(report.fetched.is_empty());
        assert!(
            report.failed[0].reason.contains("discarded"),
            "{:?}",
            report.failed
        );
        assert!(!dest.join(hash.file_name()).exists());
        assert!(!partial_of(&dest, &hash).exists(), "the bad bytes are gone");
    }

    #[tokio::test]
    async fn a_fetch_cut_short_goes_on_from_where_it_stopped() {
        let (src, dest) = (scratch("src"), scratch("dest"));
        let len = CHUNK as usize * 3;
        let (path, hash) = blob(&src, len);
        // What an earlier attempt left: the first chunk and part of the next.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(partial_of(&dest, &hash), &bytes[..CHUNK as usize + 100]).unwrap();
        let peer = Arc::new(direct(vec![(hash.clone(), path)]));
        let (report, _) = run(peer.clone(), vec![entry(&hash, len as u64)], &dest).await;
        assert_eq!(report.fetched, vec![hash.clone()]);
        let mut asked = peer.asked_offsets.lock().unwrap().clone();
        asked.sort_unstable();
        assert_eq!(
            asked,
            vec![u64::from(CHUNK), u64::from(CHUNK) * 2],
            "the whole first chunk was kept; the cut one asked again"
        );
        assert_eq!(std::fs::read(dest.join(hash.file_name())).unwrap(), bytes);
    }

    #[tokio::test]
    async fn what_no_copy_holds_comes_back_as_not_found() {
        let (src, dest) = (scratch("src"), scratch("dest"));
        let (path, held) = blob(&src, 10);
        let (_, nobody) = blob(&src, 11);
        let peer = Arc::new(direct(vec![(held.clone(), path)]));
        let (report, _) = run(peer, vec![entry(&held, 10), entry(&nobody, 11)], &dest).await;
        assert_eq!(report.fetched, vec![held]);
        assert_eq!(report.not_found, vec![nobody]);
    }

    #[tokio::test]
    async fn a_machine_that_does_not_allow_the_asker_gives_nothing() {
        let (src, dest) = (scratch("src"), scratch("dest"));
        let (path, hash) = blob(&src, 10);
        let mut peer = direct(vec![(hash.clone(), path)]);
        peer.allow = false;
        let (report, _) = run(Arc::new(peer), vec![entry(&hash, 10)], &dest).await;
        assert_eq!(report.not_found, vec![hash.clone()]);
        assert!(!dest.join(hash.file_name()).exists());
    }

    #[tokio::test]
    async fn a_blob_already_there_is_not_asked_for() {
        let (src, dest) = (scratch("src"), scratch("dest"));
        let (path, hash) = blob(&src, 500);
        std::fs::copy(&path, dest.join(hash.file_name())).unwrap();
        let peer = Arc::new(direct(Vec::new()));
        let (report, _) = run(peer.clone(), vec![entry(&hash, 500)], &dest).await;
        assert_eq!(report.fetched, vec![hash]);
        assert!(peer.asked_offsets.lock().unwrap().is_empty());
    }

    #[test]
    fn a_peer_gets_no_bytes_of_a_file_the_application_did_not_name() {
        let files = Files(HashMap::new());
        let want = AppsWire::BlobWant {
            id: 1,
            hash: format!("sha1:{}", "a".repeat(40)),
            offset: 0,
            length: 10,
        };
        let allow = |_: &str, _: &AppId| Ok(());
        let answer = serve("Nocturne", "leavepulse", want, &files, &allow).unwrap();
        assert!(matches!(
            answer,
            AppsWire::BlobChunk { error: Some(_), ref data, .. } if data.is_empty()
        ));
    }
}
