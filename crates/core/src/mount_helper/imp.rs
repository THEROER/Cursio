use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use cursio_files::bridge::PeerLink;
use cursio_files::relay::{self, MountRequest};
use tokio::net::windows::named_pipe::ClientOptions;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ADJUST_DEFAULT,
    TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Threading::{
    CreateProcessWithTokenW, OpenProcess, OpenProcessToken, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_PROCESS_LOGON_FLAGS, INFINITE, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

use super::HELPER_COMMAND;

/// How long the node waits for a helper it just started to open its pipe.
const HELPER_START: Duration = Duration::from_secs(10);

fn pipe_name(node_pid: u32) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\cursio-mount-{node_pid}"))
}

/// Closes a handle on drop, so every early return in the token dance below
/// releases what it opened.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Start `exe args…` with the desktop shell's token, which is this user's
/// unelevated one. Answers the child's process id and handle.
fn spawn_unelevated(exe: &std::path::Path, args: &[String]) -> anyhow::Result<(u32, HANDLE)> {
    // SAFETY: each handle is owned by an `Owned` and closed on every path; the
    // command line buffer is NUL-terminated, writable and outlives the call, as
    // CreateProcessWithTokenW requires.
    unsafe {
        let shell = GetShellWindow();
        if shell.is_invalid() {
            anyhow::bail!("no desktop shell is running to start the drive helper as");
        }
        let mut shell_pid = 0u32;
        GetWindowThreadProcessId(shell, Some(&mut shell_pid));
        let shell_process = Owned(
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, shell_pid)
                .context("cannot open the desktop shell's process")?,
        );
        let mut token = HANDLE::default();
        OpenProcessToken(shell_process.0, TOKEN_DUPLICATE, &mut token)
            .context("cannot read the desktop shell's token")?;
        let token = Owned(token);
        let mut primary = HANDLE::default();
        DuplicateTokenEx(
            token.0,
            TOKEN_QUERY
                | TOKEN_DUPLICATE
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
        .context("cannot copy the desktop shell's token")?;
        let primary = Owned(primary);

        let exe_wide: Vec<u16> = exe.as_os_str().encode_wide_nul();
        let line = std::iter::once(exe.display().to_string())
            .chain(args.iter().cloned())
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(" ");
        let mut line: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        CreateProcessWithTokenW(
            primary.0,
            CREATE_PROCESS_LOGON_FLAGS(0),
            PCWSTR(exe_wide.as_ptr()),
            Some(PWSTR(line.as_mut_ptr())),
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup,
            &mut info,
        )
        .context("cannot start the drive helper with the desktop's token")?;
        drop(Owned(info.hThread));
        Ok((info.dwProcessId, info.hProcess))
    }
}

trait EncodeWideNul {
    fn encode_wide_nul(&self) -> Vec<u16>;
}

impl EncodeWideNul for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}

/// The helper this node started, while it runs.
struct Helper {
    pid: u32,
    /// Kept as an integer: a `HANDLE` is a raw pointer and not `Send`.
    process: isize,
}

impl Helper {
    fn is_running(&self) -> bool {
        // SAFETY: the handle stays open for as long as `Helper` exists.
        unsafe { WaitForSingleObject(HANDLE(self.process as _), 0) == WAIT_TIMEOUT }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        drop(Owned(HANDLE(self.process as _)));
    }
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);

/// The running helper's process id, starting one if there is none.
fn helper_pid() -> anyhow::Result<u32> {
    let mut helper = HELPER.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(running) = helper.as_ref().filter(|h| h.is_running()) {
        return Ok(running.pid);
    }
    let exe = std::env::current_exe().context("cannot locate this executable")?;
    let args = vec![
        HELPER_COMMAND.to_string(),
        "--parent".to_string(),
        std::process::id().to_string(),
    ];
    let (pid, process) = spawn_unelevated(&exe, &args)?;
    tracing::info!(pid, "started the drive helper in the desktop's session");
    *helper = Some(Helper {
        pid,
        process: process.0 as isize,
    });
    Ok(pid)
}

pub(super) async fn run_helper(parent: u32) -> anyhow::Result<()> {
    // SAFETY: the handle is only waited on, and closed when the process exits.
    let parent_process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, parent) }
        .with_context(|| format!("cannot watch the node (process {parent}) this helper serves"))?;
    let parent_process = parent_process.0 as isize;
    std::thread::Builder::new()
        .name("watch-node".into())
        .spawn(move || {
            // SAFETY: the handle was opened above and is never closed before
            // this wait returns.
            unsafe { WaitForSingleObject(HANDLE(parent_process as _), INFINITE) };
            tracing::info!("the node this helper served has exited; unmounting");
            // The drives go with the process: WinFsp removes a mount whose
            // owner is gone.
            std::process::exit(0);
        })
        .context("cannot start the thread that watches the node")?;

    serve(pipe_name(parent)).await
}

async fn serve(path: PathBuf) -> anyhow::Result<()> {
    let create = |first| {
        crate::control::transport::windows::create(&path, first)
            .with_context(|| format!("cannot open the drive helper's pipe {}", path.display()))
    };
    let mut server = create(true)?;
    tracing::info!(pipe = %path.display(), "drive helper ready");
    loop {
        server
            .connect()
            .await
            .context("the drive helper's pipe failed")?;
        let connected = std::mem::replace(&mut server, create(false)?);
        tokio::spawn(async move {
            if let Err(e) = relay::present_mount(connected, mount_volume).await {
                tracing::warn!(error = %e, "a relayed mount ended with an error");
            }
        });
    }
}

fn mount_volume(
    link: PeerLink,
    request: MountRequest,
) -> Result<(cursio_files::winmount::MountHandle, String), String> {
    cursio_files::winmount::mount(link, &request.peer_name, &request.volume, &request.label)
        .map(|handle| {
            let at = handle.mountpoint().to_string();
            tracing::info!(peer = %request.peer_name, volume = %request.volume, at = %at, "mounted for the node");
            (handle, at)
        })
        .map_err(|e| format!("{e:#}"))
}

/// One of the peer's volumes, presented by the helper. Dropping it unmounts.
pub struct RelayedMount {
    task: tokio::task::JoinHandle<()>,
}

impl RelayedMount {
    /// Whether the drive is still up. A helper that exited, refused, or lost
    /// the pipe ends the task, and the engine's retry mounts the volume again.
    pub fn is_alive(&self) -> bool {
        !self.task.is_finished()
    }
}

impl Drop for RelayedMount {
    fn drop(&mut self) {
        // Closing the pipe is the unmount.
        self.task.abort();
    }
}

/// Have the helper present one of the peer's volumes, answering its calls
/// from `link`. Must be called inside the link's runtime.
pub fn mount(
    link: PeerLink,
    peer_name: &str,
    volume: &str,
    label: &str,
) -> anyhow::Result<RelayedMount> {
    let runtime = tokio::runtime::Handle::try_current()
        .context("a relayed mount needs the link's runtime")?;
    let helper = helper_pid()?;
    let request = MountRequest {
        peer_name: peer_name.to_string(),
        volume: volume.to_string(),
        label: label.to_string(),
    };
    let task = runtime.spawn(async move {
        let peer = request.peer_name.clone();
        let volume = request.volume.clone();
        let pipe = match connect(helper).await {
            Ok(pipe) => pipe,
            Err(e) => {
                tracing::warn!(peer = %peer, volume = %volume, error = %format!("{e:#}"), "cannot reach the drive helper; will retry");
                return;
            }
        };
        let (mounted_tx, mounted_rx) = tokio::sync::oneshot::channel();
        let report = {
            let (peer, volume) = (peer.clone(), volume.clone());
            async move {
                match mounted_rx.await {
                    Ok(Ok(at)) => tracing::info!(peer = %peer, volume = %volume, at = %at, "peer volume mounted where Explorer sees it"),
                    Ok(Err(reason)) => tracing::warn!(peer = %peer, volume = %volume, reason = %reason, "the drive helper could not mount this volume; will retry"),
                    Err(_) => {}
                }
            }
        };
        let (served, ()) = tokio::join!(relay::serve_mount(pipe, request, link, mounted_tx), report);
        if let Err(e) = served {
            tracing::warn!(peer = %peer, volume = %volume, error = %e, "lost the drive helper; will mount again");
        }
    });
    Ok(RelayedMount { task })
}

/// Open the helper's pipe, waiting out its start-up, and make sure the
/// process serving it is the helper this node started.
async fn connect(helper: u32) -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let path = pipe_name(std::process::id());
    let deadline = Instant::now() + HELPER_START;
    let pipe = loop {
        match ClientOptions::new().open(&path) {
            Ok(pipe) => break pipe,
            // Not created yet, or every instance busy with another volume's
            // connection: both clear up within moments.
            Err(e)
                if Instant::now() < deadline
                    && (e.kind() == std::io::ErrorKind::NotFound
                        || e.raw_os_error() == Some(231)) =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
        }
    };
    let mut server = 0u32;
    // SAFETY: the handle belongs to `pipe`, which outlives the call.
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server) }
        .context("cannot tell which process serves the drive helper's pipe")?;
    if server != helper {
        anyhow::bail!(
            "{} is served by process {server}, not the drive helper ({helper}) this node started",
            path.display()
        );
    }
    Ok(pipe)
}
