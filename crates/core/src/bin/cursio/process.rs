//! The process itself: resource limits set at start, how an interrupt
//! ends it, and restarting into a new build or a changed setting.

use cursio_transport::tcp::TcpListenerHandle;
use std::net::SocketAddr;
use std::time::Duration;

/// Keep glibc to a few malloc arenas, before any thread exists.
///
/// glibc gives a thread that finds every arena busy a new one, up to eight per
/// core, and the first large block in a new arena is carved from that arena's
/// fresh heap and then cleared with `memset` by `calloc` — outside the main
/// arena it does not trust fresh memory to be zero. `fuser` allocates a zeroed
/// 16 MiB request buffer on every mount thread, and a mount runs dozens of
/// threads (`cursio_files::bridge::mount_threads`), so each buffer landed in
/// an arena of its own and every page of it became resident: 1.5 GB of zeros
/// on a node with four peer volumes mounted, measured page by page.
///
/// With the arenas capped, a new thread shares an existing one, its buffer
/// does not fit that arena's top and is served by a fresh `mmap`, which
/// `calloc` knows is zero and leaves untouched — resident only where a request
/// actually lands. Measured with sixteen threads each holding one such buffer:
/// 258 MB by default, 49 MB with four arenas. Four rather than one because the
/// input, audio and video threads allocate on their hot paths, and one lock
/// for all of them would trade memory for contention.
///
/// The cap alone held only until the first large block was freed. glibc's mmap
/// threshold is dynamic: freeing an mmapped block raises it to that block's
/// size, up to 32 MiB, and from then on a 16 MiB buffer is carved from an
/// arena heap — memory earlier mounts already dirtied, which `calloc` must
/// clear. Every peer reconnect rebuilds the mounts, so an idle node climbed to
/// 2.6 GB with 32 arena heaps each holding exactly three such buffers.
/// Pinning the threshold keeps it fixed: blocks of 16 MiB and up always come
/// from `mmap`, lazily zeroed and returned on free, while everything smaller —
/// video frames included — stays in the arenas as before. Measured over three
/// rebuilds of 48 mount threads: 261 MB resident unpinned, 2 MB pinned.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn limit_malloc_arenas() {
    // SAFETY: mallopt only adjusts allocator parameters; called before any
    // other thread is started, so nothing is allocating concurrently.
    if unsafe { libc::mallopt(libc::M_ARENA_MAX, 4) } == 0 {
        eprintln!("warning: glibc refused M_ARENA_MAX; mount threads may hold 16 MiB each");
    }
    // SAFETY: as above.
    if unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, 16 * 1024 * 1024) } == 0 {
        eprintln!("warning: glibc refused M_MMAP_THRESHOLD; mount buffers may stay resident after a reconnect");
    }
}

/// The desktop session's variables this process took from the user's
/// session manager, for the log once it is open; see
/// [`adopt_session_environment`].
#[cfg(target_os = "linux")]
static ADOPTED: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// The variables that place a process in the desktop session.
#[cfg(target_os = "linux")]
const SESSION_VARIABLES: [&str; 6] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Join the user's desktop session when started outside it, before any
/// thread exists.
///
/// A node started from SSH or a console — by hand, or by an update applied
/// from there, whose new node inherits that shell — had no `DISPLAY`: on X11
/// capture then cannot ask where the pointer is, so no edge ever crossed and
/// the cursor stayed stuck on the laptop with only the hotkey to bring it
/// back. GNOME exports its session's variables to the user's systemd manager,
/// so they are read from there; each one already set is kept.
#[cfg(target_os = "linux")]
pub(crate) fn adopt_session_environment() {
    if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return;
    }
    let Ok(output) = std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let found = session_variables(&text);
    if !found
        .iter()
        .any(|(name, _)| *name == "DISPLAY" || *name == "WAYLAND_DISPLAY")
    {
        return;
    }
    let mut adopted = Vec::new();
    for (name, value) in found {
        // A console login says `tty` here; the session it joins says what it is.
        let replace = name == "XDG_SESSION_TYPE";
        if replace || std::env::var_os(name).is_none() {
            adopted.push(format!("{name}={value}"));
            std::env::set_var(name, value);
        }
    }
    let _ = ADOPTED.set(adopted);
}

/// Log what [`adopt_session_environment`] took, now that there is a log.
#[cfg(target_os = "linux")]
pub(crate) fn report_adopted_session() {
    if let Some(adopted) = ADOPTED.get() {
        tracing::info!(
            ?adopted,
            "started outside the desktop session; joined it through the user's session manager"
        );
    }
}

/// The session variables in `systemctl --user show-environment` output.
#[cfg(target_os = "linux")]
fn session_variables(text: &str) -> Vec<(&'static str, String)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter_map(|(name, value)| {
            let name = SESSION_VARIABLES.iter().find(|v| **v == name)?;
            (!value.is_empty()).then(|| (*name, value.to_string()))
        })
        .collect()
}

/// Let SIGINT stop this process however it was started.
///
/// A program started in the background by a non-interactive shell inherits
/// SIGINT as ignored, and every node an update helper or `nohup … &` brings up
/// is such a program. The helper stops the old node with SIGINT, so a node
/// started that way could never be replaced: on one machine the switch timed
/// out, retried every 15 s, and the machine stayed on an old protocol for good.
#[cfg(unix)]
pub(crate) fn accept_interrupt() {
    // SAFETY: restoring a signal's default disposition before any thread or
    // handler exists has no memory effects.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
}

/// Lift the open-descriptor limit from the soft default to the hard one.
///
/// A node's descriptors scale with what it serves, not with what it does: each
/// peer volume mounted here runs up to 64 FUSE threads, each with its own
/// `/dev/fuse` clone and the eventfd/epoll pair of its loop, so two peers'
/// volumes alone take ~700 of the 1024 a desktop session hands out. Every file
/// a peer holds open on *our* disks costs one more, and a program walking a
/// shared volume used the remaining ~300 in minutes: from then on every file
/// request failed with EMFILE and the peer's mounts went away. The hard limit
/// (524288 under systemd) is what the kernel will actually allow; the soft
/// 1024 exists for programs still using `select`, which nothing here does.
#[cfg(unix)]
pub(crate) fn raise_descriptor_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit read and write a struct we own.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            eprintln!("warning: cannot read the open-file limit; serving shared disks may run out of descriptors");
            return;
        }
        if limit.rlim_cur >= limit.rlim_max {
            return;
        }
        // macOS reports an unlimited hard limit it then refuses above
        // OPEN_MAX (10240, `sys/syslimits.h`; the libc crate does not carry it).
        #[cfg(target_os = "macos")]
        let target = limit.rlim_max.min(10240);
        #[cfg(not(target_os = "macos"))]
        let target = limit.rlim_max;
        let raised = libc::rlimit {
            rlim_cur: target,
            rlim_max: limit.rlim_max,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) != 0 {
            eprintln!(
                "warning: cannot raise the open-file limit above {}; serving shared disks may run out of descriptors",
                limit.rlim_cur
            );
        }
    }
}

/// Set in the environment of a node started by [`restart_process`], so it
/// waits out the port its predecessor is still releasing.
pub(crate) const RESTARTED_ENV: &str = "CURSIO_RESTARTED";

/// Start this node again from the top, with the same command line.
///
/// By the time this runs the node future has been dropped, which drops every
/// session and with them the modules' mounts, streams and capture handles.
/// The pause lets the module threads notice their channels closing and
/// unmount before the image is replaced; a mount that still outlives it is
/// cleared by the next start, as after a crash.
///
/// Unix replaces the process image: same PID, so a supervisor (`cargo xtask
/// run`, systemd) sees one continuous process. Every descriptor is made
/// close-on-exec first — the listener's, so the port is free at once, and the
/// ones libraries opened without it, which would otherwise pile up across
/// restarts (see `update::apply::close_inherited_on_exec`).
///
/// Windows has no exec: a child is started with the same arguments and this
/// process exits; the child retries its bind while the port is released.
pub(crate) async fn restart_process(reason: &str) -> anyhow::Result<()> {
    tokio::time::sleep(Duration::from_secs(1)).await;
    let exe = match cursio_core::update::layout::restart_target("cursio") {
        Some(current) => current,
        None => std::env::current_exe()?,
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    tracing::info!(reason, exe = %exe.display(), "restarting");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cursio_core::update::apply::close_inherited_on_exec();
        let error = std::process::Command::new(&exe)
            .args(&args)
            .env(RESTARTED_ENV, "1")
            .exec();
        Err(anyhow::anyhow!(
            "could not restart {}: {error}",
            exe.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let mut command = std::process::Command::new(&exe);
        command.args(&args).env(RESTARTED_ENV, "1");
        #[cfg(windows)]
        if !cursio_core::elevate::has_console() {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
            .spawn()
            .map_err(|e| anyhow::anyhow!("could not restart {}: {e}", exe.display()))?;
        std::process::exit(0)
    }
}

/// Bind the mesh port, waiting a few seconds for it after a restart.
///
/// Only after a restart: then the previous process is known to be letting go
/// of this port. Otherwise a busy port is another node, and saying so at once
/// beats making the user wait to hear it.
pub(crate) async fn bind_mesh_port(port: u16) -> anyhow::Result<TcpListenerHandle> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let mut attempts = match std::env::var_os(RESTARTED_ENV) {
        Some(_) => 20,
        None => 1,
    };
    loop {
        attempts -= 1;
        match TcpListenerHandle::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if attempts > 0 => {
                tracing::debug!(error = %e, "mesh port still held by the previous run; retrying");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// What a laptop's GNOME on X11 exported, from a console login.
    #[test]
    fn reads_the_desktop_sessions_variables_from_the_session_manager() {
        let text = "HOME=/home/theroer\nDISPLAY=:0\nXAUTHORITY=/run/user/1000/gdm/Xauthority\n\
                    XDG_SESSION_TYPE=x11\nWAYLAND_DISPLAY=\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\n";
        assert_eq!(
            session_variables(text),
            vec![
                ("DISPLAY", ":0".to_string()),
                ("XAUTHORITY", "/run/user/1000/gdm/Xauthority".to_string()),
                ("XDG_SESSION_TYPE", "x11".to_string()),
                (
                    "DBUS_SESSION_BUS_ADDRESS",
                    "unix:path=/run/user/1000/bus".to_string()
                ),
            ]
        );
    }
}
