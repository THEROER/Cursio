//! `cargo xtask deploy` — put the current build on both machines and say which
//! commit each one is running.
//!
//! The Windows half exists because a running `.exe` is locked: it cannot be
//! overwritten, so every rebuild was copied beside it under a new hand-picked
//! name, and the peer's home directory filled with ~75 `cursio-*.exe` nobody
//! could match to a commit. Here each build gets a name derived from its
//! commit and content, a manifest records what deploy itself put there, and
//! only files that are both in the manifest and in deploy's naming scheme are
//! ever deleted — a hand-copied `cursio-v12.exe` is not ours to remove.
//!
//! Restarting goes through the task scheduler with an interactive logon,
//! because a process spawned from ssh lives in session 0: it has no desktop,
//! so input capture and injection silently do nothing.
//!
//! The Linux half never stops anything. The user runs the daemon in a
//! terminal under `cargo xtask run`; killing it from here would take down
//! their foreground session, so deploy rebuilds and says a restart is due.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::cross::WINDOWS_TARGET;
use crate::remote::{command_line_args, ps_quote, sh_quote, wsl_to_windows, Peer};

#[derive(Args, Debug)]
pub struct DeployArgs {
    /// Only build and ship the Windows .exe.
    #[arg(long, conflicts_with = "linux_only")]
    pub windows_only: bool,
    /// Only rebuild the Linux binary.
    #[arg(long)]
    pub linux_only: bool,
    /// Copy and verify, but leave the running Windows cursio alone.
    #[arg(long)]
    pub no_restart: bool,
    /// Ship what is already built instead of building first.
    #[arg(long)]
    pub no_build: bool,
    /// How many deployed Windows builds to keep.
    #[arg(long, default_value_t = 3)]
    pub keep: usize,
    #[command(flatten)]
    pub peer: Peer,
}

/// Manifest of the builds deploy placed on the peer, one per line:
/// `name<TAB>sha256<TAB>commit<TAB>utc`. Oldest first.
const MANIFEST: &str = "deployed.txt";

/// Name of the file holding the current build's name.
const CURRENT: &str = "current.txt";

/// A launcher that always runs the current build, so the user has one stable
/// thing to start by hand.
const LAUNCHER: &str = "cursio.cmd";

/// Scheduled task that restarts cursio in the logged-on user's session.
const RESTART_TASK: &str = "CursioDeployRestart";

/// The commit a build comes from, and whether uncommitted changes were in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub dirty: bool,
}

impl std::fmt::Display for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.sha, if self.dirty { "-dirty" } else { "" })
    }
}

pub fn current_commit() -> Result<Commit> {
    let sha = git(&["rev-parse", "--short=10", "HEAD"])?;
    // Untracked files are not built unless something references them, and
    // counting them would mark every build dirty over a stray scratch file.
    let dirty = !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    Ok(Commit { sha, dirty })
}

fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// sha256 of a local file, via coreutils rather than a hashing dependency.
pub fn sha256_file(path: &Path) -> Result<String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .context("running sha256sum")?;
    if !out.status.success() {
        bail!("sha256sum {} failed", path.display());
    }
    first_word(&String::from_utf8_lossy(&out.stdout))
}

fn first_word(text: &str) -> Result<String> {
    text.split_whitespace()
        .next()
        .map(str::to_string)
        .context("sha256sum printed nothing")
}

/// The deployed file name: commit, a dirty marker, and the content hash.
///
/// The hash is in the name because a dirty tree builds different bytes under
/// the same commit, and two such builds must not overwrite each other — one of
/// them may be the running, locked one.
pub fn deployed_name(commit: &Commit, sha256: &str) -> String {
    format!("cursio-{commit}-{}.exe", &sha256[..8.min(sha256.len())])
}

/// Whether `name` is one deploy could have produced.
pub fn is_deployed_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("cursio-")
        .and_then(|n| n.strip_suffix(".exe"))
    else {
        return false;
    };
    let parts: Vec<&str> = stem.split('-').collect();
    let hex = |s: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
    };
    match parts.as_slice() {
        [sha, hash] => hex(sha, 7..=40) && hex(hash, 8..=8),
        [sha, "dirty", hash] => hex(sha, 7..=40) && hex(hash, 8..=8),
        _ => false,
    }
}

/// The names in a manifest, oldest first, each once at its latest position.
pub fn manifest_names(manifest: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in manifest.lines().filter_map(|l| l.split('\t').next()) {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        names.retain(|n| n != name);
        names.push(name.to_string());
    }
    names
}

/// Which deployed builds to delete: all but the newest `keep`, never one that
/// is running, and never a name outside deploy's own scheme.
pub fn prune_candidates(names: &[String], keep: usize, running: &[String]) -> Vec<String> {
    let cut = names.len().saturating_sub(keep);
    names[..cut]
        .iter()
        .filter(|n| is_deployed_name(n))
        .filter(|n| !running.iter().any(|r| r.eq_ignore_ascii_case(n)))
        .cloned()
        .collect()
}

/// A cursio process on the Windows peer.
#[derive(Debug, Clone, PartialEq)]
pub struct WinProcess {
    pub pid: u32,
    pub session: u32,
    pub path: String,
    pub command_line: String,
}

impl WinProcess {
    pub fn file_name(&self) -> &str {
        self.path.rsplit(['\\', '/']).next().unwrap_or(&self.path)
    }

    /// Whether this is a mesh daemon, as opposed to the Tauri UI or a
    /// one-off `cursio peers`.
    pub fn is_mesh(&self) -> bool {
        command_line_args(&self.command_line)
            .split_whitespace()
            .next()
            == Some("mesh")
    }
}

/// The running cursio processes on the peer.
pub fn windows_processes(peer: &Peer) -> Result<Vec<WinProcess>> {
    let out = peer.powershell(
        "Get-CimInstance Win32_Process | Where-Object { $_.Name -like 'cursio*.exe' -and $_.Name -ne 'cursio-app.exe' } | \
         ForEach-Object { \"$($_.ProcessId)`t$($_.SessionId)`t$($_.ExecutablePath)`t$($_.CommandLine)\" }",
    )?;
    Ok(parse_processes(&out))
}

fn parse_processes(out: &str) -> Vec<WinProcess> {
    out.lines()
        .filter_map(|line| {
            let mut cols = line.splitn(4, '\t');
            Some(WinProcess {
                pid: cols.next()?.trim().parse().ok()?,
                session: cols.next()?.trim().parse().ok()?,
                path: cols.next()?.to_string(),
                command_line: cols.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// The PowerShell that stops `old` and starts `exe` with `args`, run by the
/// scheduled task inside the user's session.
///
/// Ctrl-C first, delivered by attaching to the daemon's console: that is how
/// the user stops it by hand, and it lets the process leave through its own
/// exit path. `Stop-Process -Force` follows only if it is still there after a
/// few seconds, since a new daemon cannot start while the old one holds the
/// single-instance lock.
pub fn restart_script(old: &[u32], exe: &str, args: &str, log: &str) -> String {
    let pids = old.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let start = if args.is_empty() {
        "Start-Process -FilePath $exe -WorkingDirectory (Split-Path $exe)".to_string()
    } else {
        format!(
            "Start-Process -FilePath $exe -ArgumentList {} -WorkingDirectory (Split-Path $exe)",
            ps_quote(args)
        )
    };
    format!(
        r#"$log = {log}
$exe = {exe}
function Note($m) {{ Add-Content -Path $log -Value ("{{0:o}} {{1}}" -f (Get-Date), $m) }}
Add-Type -Namespace CursioDeploy -Name Con -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern bool AttachConsole(uint p);
[DllImport("kernel32.dll")] public static extern bool FreeConsole();
[DllImport("kernel32.dll")] public static extern bool SetConsoleCtrlHandler(System.IntPtr h, bool add);
[DllImport("kernel32.dll")] public static extern bool GenerateConsoleCtrlEvent(uint e, uint g);
'@
foreach ($p in @({pids})) {{
  [CursioDeploy.Con]::FreeConsole() | Out-Null
  if ([CursioDeploy.Con]::AttachConsole($p)) {{
    [CursioDeploy.Con]::SetConsoleCtrlHandler([System.IntPtr]::Zero, $true) | Out-Null
    [CursioDeploy.Con]::GenerateConsoleCtrlEvent(0, 0) | Out-Null
    Start-Sleep -Milliseconds 300
    [CursioDeploy.Con]::FreeConsole() | Out-Null
    [CursioDeploy.Con]::SetConsoleCtrlHandler([System.IntPtr]::Zero, $false) | Out-Null
    Note "sent Ctrl-C to $p"
  }} else {{ Note "could not attach to the console of $p" }}
  try {{ Wait-Process -Id $p -Timeout 8 -ErrorAction Stop; Note "$p exited" }}
  catch {{ Stop-Process -Id $p -Force -ErrorAction SilentlyContinue; Note "$p force-stopped" }}
}}
{start}
Note "started $exe"
"#,
        log = ps_quote(log),
        exe = ps_quote(exe),
    )
}

/// Register and fire the restart task as the logged-on user, interactively.
fn schedule_restart(peer: &Peer, script_win: &str) -> Result<()> {
    let action_args =
        format!("-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{script_win}\"");
    peer.powershell(&format!(
        "$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument {args}\n\
         $p = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive\n\
         Register-ScheduledTask -TaskName {task} -Action $a -Principal $p -Force | Out-Null\n\
         Start-ScheduledTask -TaskName {task}\n",
        args = ps_quote(&action_args),
        task = ps_quote(RESTART_TASK),
    ))?;
    Ok(())
}

pub fn deploy(args: DeployArgs) -> Result<()> {
    let commit = current_commit()?;
    if commit.dirty {
        println!("! the working tree has uncommitted changes; builds are labelled {commit}");
    }
    let mut report = Vec::new();
    if !args.windows_only {
        report.push(deploy_linux(&args, &commit)?);
    }
    if !args.linux_only {
        report.push(deploy_windows(&args, &commit)?);
    }
    println!("\n=== running where ===");
    for line in report {
        println!("{line}");
    }
    Ok(())
}

/// Sidecar recording which commit a Linux binary was built from.
fn commit_sidecar(exe: &Path) -> PathBuf {
    exe.with_extension("commit")
}

fn deploy_linux(args: &DeployArgs, commit: &Commit) -> Result<String> {
    let exe = crate::out_dir(true, None).join("cursio");
    let before = std::fs::read_to_string(commit_sidecar(&exe)).ok();
    if !args.no_build {
        crate::build(true, Some(crate::BinArg::Cursio), None)?;
        std::fs::write(commit_sidecar(&exe), commit.to_string())
            .with_context(|| format!("writing {}", commit_sidecar(&exe).display()))?;
    }
    let built = std::fs::read_to_string(commit_sidecar(&exe)).unwrap_or_else(|_| "unknown".into());
    let exe_abs =
        std::fs::canonicalize(&exe).with_context(|| format!("{} is missing", exe.display()))?;
    let running = linux_processes(&exe_abs);
    let host = hostname();
    Ok(match running.as_slice() {
        [] => format!("{host}: {} is {built}; not running", exe.display()),
        procs => {
            let stale: Vec<_> = procs.iter().filter(|p| p.1).map(|p| p.0).collect();
            if stale.is_empty() {
                format!("{host}: pid {} runs {built}", procs[0].0)
            } else {
                format!(
                    "{host}: pid {} runs the previous build ({}); {} is now {built} — \
                     stop your `cargo xtask run` terminal with Ctrl-C and start it again",
                    stale
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                    before.as_deref().unwrap_or("commit unknown"),
                    exe.display()
                )
            }
        }
    })
}

/// `(pid, replaced)` for each process running `exe`; `replaced` when the file
/// was rebuilt under it (Linux shows the old inode as `(deleted)`).
fn linux_processes(exe: &Path) -> Vec<(u32, bool)> {
    let deleted = PathBuf::from(format!("{} (deleted)", exe.display()));
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let target = std::fs::read_link(e.path().join("exe")).ok()?;
            if target == exe {
                Some((pid, false))
            } else if target == deleted {
                Some((pid, true))
            } else {
                None
            }
        })
        .collect()
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "this machine".into())
}

/// The Windows cross-build output directory. Deploy builds into the shared
/// `~/.cache/cursio-win-target` unless told otherwise, so its cross-build
/// cache is the one every other Windows build already warmed.
fn windows_target_dir() -> Result<()> {
    if std::env::var_os("CARGO_TARGET_DIR").is_none() {
        let home = std::env::var_os("HOME").context("HOME not set")?;
        std::env::set_var(
            "CARGO_TARGET_DIR",
            PathBuf::from(home).join(".cache/cursio-win-target"),
        );
    }
    Ok(())
}

fn deploy_windows(args: &DeployArgs, commit: &Commit) -> Result<String> {
    let peer = &args.peer;
    windows_target_dir()?;
    let dir = if args.no_build {
        crate::out_dir(true, Some(WINDOWS_TARGET))
    } else {
        crate::windows(true)?
    };
    let exe = dir.join("cursio.exe");
    if !exe.is_file() {
        bail!(
            "{} is missing — build it first or drop --no-build",
            exe.display()
        );
    }
    if args.no_build {
        warn_if_older_than_head(&exe);
    }
    let sha = sha256_file(&exe)?;
    let name = deployed_name(commit, &sha);
    let remote_dir = peer.cursio_dir();
    let remote_exe = format!("{remote_dir}/{name}");

    println!("→ copying {} to {}:{remote_exe}", exe.display(), peer.ssh);
    peer.bash(&format!("mkdir -p {}\n", sh_quote(&remote_dir)))?;
    let partial = format!("{remote_exe}.partial");
    peer.upload(&exe, &partial)?;
    let remote_sha = first_word(&peer.bash(&format!(
        "mv -f {p} {e} && sha256sum {e}\n",
        p = sh_quote(&partial),
        e = sh_quote(&remote_exe)
    ))?)?;
    if remote_sha != sha {
        bail!(
            "sha256 mismatch after copy: local {sha}, {} has {remote_sha}",
            peer.ssh
        );
    }
    println!("✓ sha256 {sha} on both ends");

    record_deployment(peer, &name, &sha, commit)?;
    let win_exe = wsl_to_windows(&remote_exe);

    let before = windows_processes(peer)?;
    let mesh: Vec<&WinProcess> = before.iter().filter(|p| p.is_mesh()).collect();
    if args.no_restart {
        println!("→ --no-restart: {win_exe} is ready; the running daemon was left alone");
    } else if let Some(first) = mesh.first() {
        let cmd_args = command_line_args(&first.command_line).to_string();
        if mesh
            .iter()
            .any(|p| command_line_args(&p.command_line) != cmd_args)
        {
            println!(
                "! several mesh daemons with different arguments; restarting with those of pid {}",
                first.pid
            );
        }
        let pids: Vec<u32> = mesh.iter().map(|p| p.pid).collect();
        restart(peer, &pids, &win_exe, &cmd_args)?;
    } else {
        println!(
            "! no cursio mesh is running on {}; start it with {}\\{LAUNCHER} mesh ...",
            peer.peer_name,
            wsl_to_windows(&remote_dir)
        );
    }

    let after = windows_processes(peer)?;
    prune(peer, args.keep, &after)?;
    Ok(describe_windows(peer, &after))
}

fn warn_if_older_than_head(exe: &Path) {
    let built = std::fs::metadata(exe).and_then(|m| m.modified()).ok();
    let head = git(&["log", "-1", "--format=%ct"])
        .ok()
        .and_then(|t| t.parse::<u64>().ok())
        .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
    if let (Some(built), Some(head)) = (built, head) {
        if built < head {
            println!(
                "! {} predates the HEAD commit; its label may not match its contents",
                exe.display()
            );
        }
    }
}

/// Append to the manifest and repoint `current.txt` and the launcher.
fn record_deployment(peer: &Peer, name: &str, sha: &str, commit: &Commit) -> Result<()> {
    let dir = peer.cursio_dir();
    let line = format!("{name}\t{sha}\t{commit}\t{}", utc_now());
    let launcher = format!("@\"%~dp0{name}\" %*\r\n");
    peer.bash(&format!(
        "cd {dir} && printf '%s\\n' {line} >> {MANIFEST} && printf '%s\\r\\n' {name} > {CURRENT} && printf '%s' {launcher} > {LAUNCHER}\n",
        dir = sh_quote(&dir),
        line = sh_quote(&line),
        name = sh_quote(name),
        launcher = sh_quote(&launcher),
    ))?;
    Ok(())
}

fn utc_now() -> String {
    Command::new("date")
        .args(["-u", "+%FT%TZ"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn restart(peer: &Peer, pids: &[u32], win_exe: &str, cmd_args: &str) -> Result<()> {
    let dir = peer.cursio_dir();
    let script = restart_script(
        pids,
        win_exe,
        cmd_args,
        &wsl_to_windows(&format!("{dir}/deploy-restart.log")),
    );
    let script_path = format!("{dir}/deploy-restart.ps1");
    peer.bash(&format!(
        "cat > {} <<'CURSIO_EOF'\n{script}CURSIO_EOF\n",
        sh_quote(&script_path)
    ))?;
    println!("→ restarting pid {pids:?} as {win_exe} {cmd_args}");
    schedule_restart(peer, &wsl_to_windows(&script_path))?;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let procs = windows_processes(peer)?;
        if let Some(p) = procs.iter().find(|p| p.path.eq_ignore_ascii_case(win_exe)) {
            if p.session == 0 {
                bail!(
                    "the new daemon (pid {}) landed in session 0, without a desktop",
                    p.pid
                );
            }
            println!("✓ pid {} running in session {}", p.pid, p.session);
            return Ok(());
        }
    }
    let log = peer
        .bash(&format!(
            "tail -n 5 {} 2>/dev/null\n",
            sh_quote(&format!("{dir}/deploy-restart.log"))
        ))
        .unwrap_or_default();
    bail!("the new daemon did not appear within 30 s; restart log:\n{log}")
}

fn prune(peer: &Peer, keep: usize, running: &[WinProcess]) -> Result<()> {
    let dir = peer.cursio_dir();
    let manifest = peer.bash(&format!(
        "cat {}/{MANIFEST} 2>/dev/null || true\n",
        sh_quote(&dir)
    ))?;
    let names = manifest_names(&manifest);
    let running: Vec<String> = running.iter().map(|p| p.file_name().to_string()).collect();
    let doomed = prune_candidates(&names, keep, &running);
    if doomed.is_empty() {
        return Ok(());
    }
    let mut script = format!("cd {} || exit 1\n", sh_quote(&dir));
    for name in &doomed {
        // A file that will not go (still locked) stays in the manifest, so the
        // next deploy tries again rather than forgetting it exists.
        script += &format!(
            "if rm -f {n}; then grep -v -F -- {tab} {MANIFEST} > {MANIFEST}.tmp; mv -f {MANIFEST}.tmp {MANIFEST}; echo removed {n}; fi\n",
            n = sh_quote(name),
            tab = sh_quote(&format!("{name}\t")),
        );
    }
    print!("{}", peer.bash(&script)?);
    Ok(())
}

fn describe_windows(peer: &Peer, procs: &[WinProcess]) -> String {
    let mesh: Vec<&WinProcess> = procs.iter().filter(|p| p.is_mesh()).collect();
    if mesh.is_empty() {
        return format!("{}: no cursio mesh running", peer.peer_name);
    }
    mesh.iter()
        .map(|p| {
            let label = if is_deployed_name(p.file_name()) {
                p.file_name()
                    .trim_start_matches("cursio-")
                    .trim_end_matches(".exe")
                    .to_string()
            } else {
                format!("{} (not a deployed build, commit unknown)", p.file_name())
            };
            format!(
                "{}: pid {} session {} runs {label}",
                peer.peer_name, p.pid, p.session
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(dirty: bool) -> Commit {
        Commit {
            sha: "c00af04abc".into(),
            dirty,
        }
    }

    #[test]
    fn deployed_names_carry_commit_dirtiness_and_content() {
        assert_eq!(
            deployed_name(&commit(false), "0123456789abcdef"),
            "cursio-c00af04abc-01234567.exe"
        );
        assert_eq!(
            deployed_name(&commit(true), "0123456789abcdef"),
            "cursio-c00af04abc-dirty-01234567.exe"
        );
        assert!(is_deployed_name(&deployed_name(
            &commit(true),
            "0123456789abcdef"
        )));
    }

    #[test]
    fn hand_copied_builds_are_never_taken_for_deployed_ones() {
        for name in [
            "cursio-v12.exe",
            "cursio.new.exe",
            "cursio-fix3.exe",
            "cursio-final.exe",
            "cursio-c00af04-xyz.exe",
            "cursio-app.exe",
        ] {
            assert!(!is_deployed_name(name), "{name}");
        }
    }

    #[test]
    fn prunes_all_but_the_newest_and_spares_the_running_one() {
        let names: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|h| format!("cursio-c00af04-{}.exe", h.repeat(8)))
            .collect();
        let running = vec![names[0].to_uppercase()];
        assert_eq!(
            prune_candidates(&names, 3, &running),
            vec![names[1].clone()]
        );
        assert!(prune_candidates(&names, 10, &[]).is_empty());
    }

    #[test]
    fn prune_ignores_foreign_names_even_when_listed() {
        let names = vec![
            "cursio-v12.exe".to_string(),
            "cursio-c00af04-aaaaaaaa.exe".to_string(),
        ];
        assert!(prune_candidates(&names, 0, &[])
            .iter()
            .all(|n| n != "cursio-v12.exe"));
    }

    #[test]
    fn a_redeployed_build_moves_to_the_newest_position() {
        let manifest = "x\t1\nb\t2\nx\t3\n";
        assert_eq!(
            manifest_names(manifest),
            vec!["b".to_string(), "x".to_string()]
        );
    }

    #[test]
    fn reads_processes_and_tells_the_mesh_daemon_from_others() {
        let out = "43684\t1\tC:\\Users\\thero\\cursio.new.exe\tcursio.new.exe  mesh --send-input\n\
                   99\t1\tC:\\x\\cursio.exe\t\"C:\\x\\cursio.exe\" peers\n";
        let procs = parse_processes(out);
        assert_eq!(procs.len(), 2);
        assert!(procs[0].is_mesh());
        assert!(!procs[1].is_mesh());
        assert_eq!(procs[0].file_name(), "cursio.new.exe");
    }

    #[test]
    fn restart_script_keeps_the_arguments_verbatim() {
        let s = restart_script(
            &[1, 2],
            r"C:\c\cursio-x.exe",
            "mesh --at 1,0 --name 'a'",
            r"C:\c\l.log",
        );
        assert!(s.contains("foreach ($p in @(1,2))"));
        assert!(s.contains("-ArgumentList 'mesh --at 1,0 --name ''a'''"));
        assert!(!restart_script(&[1], "e", "", "l").contains("-ArgumentList"));
    }
}
