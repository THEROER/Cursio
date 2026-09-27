//! `cargo xtask live-test` — the two real machines, measured over one window.
//!
//! Unit tests cannot see what broke today: a session loop stalling behind a
//! module, the mount hanging a caller, audio going quiet after a reconnect.
//! The daemons already log each of those as numbers; this command drives
//! some load, reads back both logs for exactly the window it ran, and judges
//! them against fixed limits (see `logscan`).
//!
//! Load it drives: synthetic pointer motion from this machine into the peer
//! (only with `--yes`, and only while the cursor is already on the peer), and
//! one file written, read back and removed through the peer's mount. Audio,
//! the remote screen and file traffic are judged from whatever the user is
//! doing meanwhile; a check with nothing to judge says SKIP, not PASS.
//!
//! The mount is touched only inside a child process with a deadline, and only
//! by exact path — never a listing. A FUSE call the peer never answers leaves
//! its caller in uninterruptible sleep, where not even SIGKILL reaches it; a
//! child stuck like that is reported and abandoned instead of taking this
//! process (and the terminal) with it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::logscan::{self, Machine, Probes};
use crate::remote::{sh_quote, Peer};

#[derive(Args, Debug)]
pub struct LiveTestArgs {
    /// Length of the measured window. The daemons report latency every 30 s,
    /// so anything shorter may see no report at all.
    #[arg(long, default_value_t = 65)]
    pub seconds: u64,
    /// Allow synthetic pointer motion (it moves the real cursor on the peer).
    #[arg(long)]
    pub yes: bool,
    /// Skip the file roundtrip through the peer's mount.
    #[arg(long)]
    pub no_files: bool,
    /// Mount point of the peer volume holding the Windows home (default: the
    /// `(C)` volume under ~/Cursio/<peer>).
    #[arg(long)]
    pub mount: Option<PathBuf>,
    #[command(flatten)]
    pub peer: Peer,
}

/// Size of the roundtrip file: several bulk chunks, small enough to finish in
/// well under a second on a healthy link.
const PROBE_BYTES: usize = 4 << 20;

/// Deadline for each child touching the mount.
const PROBE_DEADLINE: Duration = Duration::from_secs(20);

/// Rate of synthetic motion: a typical mouse's report rate.
const MOTION_HZ: u32 = 125;

/// Name of the folder the roundtrip may write in, beside deploy's builds.
const TEST_FOLDER: &str = "live-test";

/// Deterministic pseudo-random bytes, so the peer's copy can be checked by
/// hash without shipping the data twice.
pub fn probe_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

/// `\040`-style escapes in `/proc/mounts` fields.
pub fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).and_then(|d| {
            std::str::from_utf8(d)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 8).ok())
        });
        match (bytes[i], octal) {
            (b'\\', Some(b)) => {
                out.push(b);
                i += 4;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Cursio's FUSE mount points under `root`, from `/proc/mounts` text. Read
/// from the kernel's table rather than by listing `root`, which would enter
/// the mounts themselves.
pub fn cursio_mounts(proc_mounts: &str, root: &Path) -> Vec<PathBuf> {
    proc_mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_src, point, kind) = (f.next()?, f.next()?, f.next()?);
            (kind == "fuse.cursio").then(|| PathBuf::from(unescape_mount_field(point)))
        })
        .filter(|p| p.starts_with(root))
        .collect()
}

fn utc_date() -> Result<String> {
    let out = Command::new("date")
        .args(["-u", "+%F"])
        .output()
        .context("running date")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Where one machine's log stood when the window opened. Logs rotate at UTC
/// midnight, so a window may span two files.
struct LogMark {
    date: String,
    offset: u64,
}

fn log_name(date: &str) -> String {
    format!("cursio.log.{date}")
}

fn local_mark() -> Result<LogMark> {
    let date = utc_date()?;
    let path = crate::cursio_log_dir().join(log_name(&date));
    let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(LogMark { date, offset })
}

fn read_from(path: &Path, offset: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Ok(String::new());
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn local_window(mark: &LogMark) -> Result<String> {
    let dir = crate::cursio_log_dir();
    let mut text = read_from(&dir.join(log_name(&mark.date)), mark.offset)?;
    let today = utc_date()?;
    if today != mark.date {
        text += &read_from(&dir.join(log_name(&today)), 0)?;
    }
    Ok(text)
}

fn remote_mark(peer: &Peer) -> Result<LogMark> {
    let out = peer.bash(&format!(
        "d=$(date -u +%F); f={}/cursio.log.$d; echo $d; stat -c %s \"$f\" 2>/dev/null || echo 0\n",
        sh_quote(&peer.log_dir())
    ))?;
    let mut lines = out.lines();
    let date = lines
        .next()
        .context("peer printed no date")?
        .trim()
        .to_string();
    let offset = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    Ok(LogMark { date, offset })
}

fn remote_window(peer: &Peer, mark: &LogMark) -> Result<String> {
    let dir = sh_quote(&peer.log_dir());
    peer.bash(&format!(
        "tail -c +{start} {dir}/{first} 2>/dev/null\n\
         d=$(date -u +%F); [ \"$d\" != {date} ] && cat {dir}/cursio.log.$d 2>/dev/null; true\n",
        start = mark.offset + 1,
        first = log_name(&mark.date),
        date = sh_quote(&mark.date),
    ))
}

/// Whether this machine last handed the cursor away, i.e. it is on the peer.
/// Read from the tail of today's log: `ownership changed active=false` is
/// written when the cursor crosses to the peer.
fn cursor_on_peer() -> Option<bool> {
    let path = crate::cursio_log_dir().join(log_name(&utc_date().ok()?));
    let len = std::fs::metadata(&path).ok()?.len();
    let tail = read_from(&path, len.saturating_sub(8 << 20)).ok()?;
    let last = tail
        .lines()
        .rev()
        .find(|l| l.contains("ownership changed"))?;
    Some(logscan::field(last, "active")? == "false")
}

/// `(memory, io)` full-stall totals from `/proc/pressure`, in µs.
fn pressure_totals() -> Option<(u64, u64)> {
    let read = |r: &str| {
        std::fs::read_to_string(format!("/proc/pressure/{r}"))
            .ok()
            .and_then(|t| logscan::pressure_full_total_us(&t))
    };
    Some((read("memory")?, read("io")?))
}

/// The hidden `file-probe` subcommand, run as a child: write, sync and read
/// back one file, or remove it. Prints timings on success.
pub fn file_probe(path: &Path, seed: u64, remove: bool) -> Result<()> {
    if remove {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        println!("removed");
        return Ok(());
    }
    let data = probe_bytes(seed, PROBE_BYTES);
    let t0 = Instant::now();
    {
        use std::io::Write;
        let mut f =
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        f.write_all(&data).context("writing")?;
        f.sync_all().context("syncing")?;
    }
    let wrote = t0.elapsed();
    let t1 = Instant::now();
    let back = std::fs::read(path).context("reading back")?;
    let read = t1.elapsed();
    if back != data {
        bail!(
            "read back {} bytes that differ from the {} written",
            back.len(),
            data.len()
        );
    }
    println!(
        "write {} ms, read {} ms",
        wrote.as_millis(),
        read.as_millis()
    );
    Ok(())
}

/// Run `file-probe` in a child and wait at most [`PROBE_DEADLINE`].
fn run_probe(path: &Path, seed: u64, remove: bool) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locating xtask: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("file-probe")
        .arg(path)
        .args(["--seed", &seed.to_string()]);
    if remove {
        cmd.arg("--remove");
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning the probe: {e}"))?;
    let deadline = Instant::now() + PROBE_DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                return Err(format!(
                    "no answer in {}s; probe pid {} may be stuck in the mount (check \
                     /sys/fs/fuse/connections/*/waiting) — abandoned, not killed",
                    PROBE_DEADLINE.as_secs(),
                    child.id()
                ))
            }
            Err(e) => return Err(format!("waiting for the probe: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    if out.status.success() {
        Ok(text(&out.stdout))
    } else {
        Err(text(&out.stderr))
    }
}

/// The peer's volume holding its Windows home, as mounted here.
fn home_volume(args: &LiveTestArgs) -> Result<PathBuf> {
    if let Some(m) = &args.mount {
        return Ok(m.clone());
    }
    let home = std::env::var_os("HOME").context("HOME not set")?;
    let root = PathBuf::from(home)
        .join("Cursio")
        .join(&args.peer.peer_name);
    let table = std::fs::read_to_string("/proc/mounts").context("reading /proc/mounts")?;
    let mounts = cursio_mounts(&table, &root);
    let drive = args
        .peer
        .win_home
        .strip_prefix("/mnt/")
        .and_then(|r| r.chars().next())
        .map(|c| format!("({})", c.to_ascii_uppercase()))
        .context("--win-home is not a /mnt/<drive>/ path")?;
    mounts
        .iter()
        .find(|m| m.to_string_lossy().ends_with(&drive))
        .cloned()
        .with_context(|| {
            format!(
                "no cursio mount of the {drive} volume under {} (is --files on?)",
                root.display()
            )
        })
}

/// Write through the mount, confirm the bytes on the peer's own disk, remove.
fn file_roundtrip(args: &LiveTestArgs) -> Result<String, String> {
    let peer = &args.peer;
    let volume = home_volume(args).map_err(|e| e.to_string())?;
    // The folder is made on the peer's side, so the mount is asked only to
    // look up names that exist and never to list or create a directory.
    let remote_dir = format!("{}/{TEST_FOLDER}", peer.cursio_dir());
    peer.bash(&format!("mkdir -p {}\n", sh_quote(&remote_dir)))
        .map_err(|e| format!("preparing the folder on the peer: {e}"))?;
    let relative = remote_dir
        .strip_prefix("/mnt/")
        .and_then(|r| r.get(2..))
        .ok_or("--win-home is not a /mnt/<drive>/ path")?;
    let seed = std::process::id() as u64 ^ 0x5eed_c0de;
    let name = format!("probe-{}.bin", std::process::id());
    let local = volume.join(relative).join(&name);

    let timings = run_probe(&local, seed, false)?;
    let expected = sha256_of(&probe_bytes(seed, PROBE_BYTES)).map_err(|e| e.to_string())?;
    let on_peer = peer
        .bash(&format!(
            "sha256sum {}\n",
            sh_quote(&format!("{remote_dir}/{name}"))
        ))
        .map_err(|e| format!("hashing on the peer: {e}"))?;
    let removed = run_probe(&local, seed, true);
    if !on_peer.starts_with(&expected) {
        return Err(format!("the peer's disk holds different bytes ({timings})"));
    }
    removed.map_err(|e| format!("verified, but removing failed: {e}"))?;
    Ok(format!("4 MiB {timings}, sha256 matches on peer"))
}

fn sha256_of(data: &[u8]) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running sha256sum")?;
    child
        .stdin
        .take()
        .context("sha256sum stdin")?
        .write_all(data)?;
    let out = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

pub fn live_test(args: LiveTestArgs) -> Result<()> {
    if args.seconds < 31 {
        bail!("--seconds must be at least 31: latency is only reported every 30 s");
    }
    let peer = &args.peer;
    let local_name = crate::deploy::hostname();
    let peer_name = peer.peer_name.as_str();

    let on_peer = cursor_on_peer();
    let inject = match (args.yes, on_peer) {
        (true, Some(true)) => true,
        (true, _) => {
            println!(
                "! the cursor is not on {peer_name} (last ownership line says so or is missing);"
            );
            println!("  move it there first — injecting now would move your own pointer here. Input is judged passively.");
            false
        }
        (false, _) => {
            println!("! synthetic pointer motion is off. With --yes, while the cursor is on {peer_name},");
            println!("  this traces small circles on {peer_name}'s screen for {}s to measure input latency.", args.seconds);
            false
        }
    };

    let local_start = local_mark()?;
    let remote_start = remote_mark(peer).context("reading the peer's log position")?;
    let pressure_start = pressure_totals();
    let started = Instant::now();
    let until = started + Duration::from_secs(args.seconds);
    println!("→ measuring for {}s", args.seconds);

    let motion = inject.then(|| {
        std::thread::spawn(move || -> Result<u64> {
            let mut pointer = crate::uinput::Pointer::create()?;
            pointer.circles(until, MOTION_HZ)
        })
    });

    let file_roundtrip = (!args.no_files).then(|| file_roundtrip(&args));

    while Instant::now() < until {
        std::thread::sleep(Duration::from_secs(1));
    }
    if let Some(handle) = motion {
        match handle.join() {
            Ok(Ok(n)) => println!("→ injected {n} motion events"),
            Ok(Err(e)) => println!("! synthetic motion failed: {e:#}"),
            Err(_) => println!("! synthetic motion thread panicked"),
        }
    }
    let elapsed_us = started.elapsed().as_micros() as f64;
    let pressure = pressure_start
        .zip(pressure_totals())
        .map(|((m0, i0), (m1, i1))| {
            let pct = |a: u64, b: u64| b.saturating_sub(a) as f64 / elapsed_us * 100.0;
            (pct(m0, m1), pct(i0, i1))
        });

    let local_log = local_window(&local_start)?;
    let remote_log = remote_window(peer, &remote_start).context("reading the peer's log")?;
    let local = logscan::summarize(local_log.lines());
    let remote = logscan::summarize(remote_log.lines());
    println!(
        "→ read {} lines here, {} on {peer_name}",
        local.lines, remote.lines
    );

    let rows = logscan::evaluate(
        &[
            Machine {
                name: &local_name,
                summary: &local,
                expects_input: inject.then_some("captured->sent"),
            },
            Machine {
                name: peer_name,
                summary: &remote,
                expects_input: inject.then_some("received->injected"),
            },
        ],
        &Probes {
            file_roundtrip,
            pressure,
        },
    );
    print!("\n{}", logscan::render(&rows));
    if logscan::failed(&rows) {
        bail!("live test FAILED");
    }
    println!("\nlive test PASSED");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_bytes_are_reproducible_and_not_constant() {
        let a = probe_bytes(7, 64);
        assert_eq!(a, probe_bytes(7, 64));
        assert_ne!(a, probe_bytes(8, 64));
        assert!(a.iter().any(|b| *b != a[0]));
    }

    #[test]
    fn finds_cursio_mounts_with_escaped_spaces_and_ignores_others() {
        let table =
            "cursio-Lightmoon /home/u/Cursio/Lightmoon/Windows-SSD\\040(C) fuse.cursio rw 0 0\n\
                     /dev/sda1 /home/u/Cursio/other ext4 rw 0 0\n\
                     cursio-X /elsewhere fuse.cursio rw 0 0\n";
        let mounts = cursio_mounts(table, Path::new("/home/u/Cursio/Lightmoon"));
        assert_eq!(
            mounts,
            vec![PathBuf::from("/home/u/Cursio/Lightmoon/Windows-SSD (C)")]
        );
    }
}
