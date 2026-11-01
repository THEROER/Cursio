//! Starting an application's current build on this machine, asked from
//! another (`cursio apps launch`, `cursio apps ship --launch`).
//!
//! Only an application installed through the `apps` module can be started,
//! and only the file its own manifest names: the request carries an id and a
//! component name, never a path, so a peer can start nothing this machine's
//! person did not already accept builds of. Ending running copies is limited
//! the same way — processes whose executable lies inside that application's
//! install root, and nothing else.
//!
//! **Windows starts it as the person, not as the node.** The node elevates
//! itself for UIPI (`elevate.rs`), and a child inherits its token: an
//! application started from it ran elevated, could not be dragged onto, and
//! wrote its settings where the unelevated copy never looked. So an elevated
//! node starts it with the desktop shell's token — the person's own,
//! unelevated, in their session — through `CreateProcessWithTokenW`, which an
//! elevated administrator may call.

use std::path::Path;
use std::time::Duration;

use cursio_app_protocol::AppId;

use super::builds::builds;
use crate::config::{AdminConsent, ConfigEdit};
use crate::diagnostics::{Problem, ProblemAction, Severity};

/// How long running copies get to end before the new one starts anyway.
const END_WAIT: Duration = Duration::from_secs(5);

/// Start `app`'s current build, once it is `version` when one is named.
/// Answers what was started, for the person who asked.
pub fn launch(
    app: &str,
    version: Option<&str>,
    component: Option<&str>,
    restart: bool,
    start: bool,
    args: &[String],
) -> Result<String, String> {
    let id = AppId::parse(app).map_err(|e| e.to_string())?;
    let builds = builds().ok_or("this machine takes no applications' builds")?;
    let agent = builds.agent(id.as_str()).ok_or("not an application id")?;
    let layout = agent.layout();
    let current = layout.current();
    if let Some(wanted) = version {
        if current.as_deref() != Some(wanted) {
            let why = agent
                .status()
                .last
                .map(|last| format!("; last: {last}"))
                .unwrap_or_default();
            return Err(format!(
                "{id} {wanted} is not current here yet (current: {}){why}",
                current.as_deref().unwrap_or("none")
            ));
        }
    }
    // A phone replaces an application only through its installer, which asks
    // the person: being current there means the notification is up. Starting
    // it is opening the package its person allowed as this application —
    // which may have been installed some other way, so no build from the
    // mesh is needed for that.
    if builds.hands_to_installer() {
        if !start {
            let version = current.ok_or_else(|| format!("no build of {id} is installed here"))?;
            return Ok(format!(
                "{id} {version} is offered to this phone's installer; it runs once its person installs and opens it"
            ));
        }
        let opener = builds
            .opener()
            .ok_or("this phone cannot open applications")?;
        return super::phone_open::open(
            &builds.package_grants(),
            &id,
            version,
            restart,
            opener.as_ref(),
        );
    }
    let version = current.ok_or_else(|| format!("no build of {id} is installed here"))?;
    if !start {
        return Ok(format!("{id} {version} is installed and current"));
    }
    let manifest = layout.manifest(&version).map_err(|e| e.to_string())?;
    let name = pick(
        &id,
        &manifest.components.keys().cloned().collect::<Vec<_>>(),
        component,
    )?;
    let exe = layout
        .executable(&version, &name)
        .map_err(|e| e.to_string())?;
    let asks_admin = manifest.components.get(&name).is_some_and(|c| c.admin);
    let as_admin = match (asks_admin, builds.app_config(id.as_str()).admin) {
        (false, _) => false,
        (true, AdminConsent::Allow) => true,
        (true, AdminConsent::Deny) => false,
        (true, AdminConsent::Ask) => return Err(ask_for_admin(&id)),
    };
    crate::diagnostics::clear(&admin_problem(&id));
    let mut ended = 0;
    if restart {
        ended = end_running(layout.root());
    }
    let dir = layout.version_dir(&version);
    let pid = match as_admin {
        true => spawn_admin(&exe, &dir, args)?,
        false => Some(spawn(&exe, &dir, args)?),
    };
    let how = match (as_admin, pid) {
        (true, Some(pid)) => format!("as administrator, pid {pid}"),
        (true, None) => "as administrator, after this machine's own prompt".to_owned(),
        (false, Some(pid)) => format!("pid {pid}"),
        (false, None) => String::new(),
    };
    let how = match args.is_empty() {
        true => how,
        false => format!("{how}, args {args:?}"),
    };
    tracing::info!(%id, %version, component = %name, ?args, as_admin, ?pid, ended, "started an application asked for by a peer");
    Ok(match ended {
        0 => format!("started {id} {version} ({name}, {how})"),
        n => format!(
            "ended {n} running cop{} and started {id} {version} ({name}, {how})",
            if n == 1 { "y" } else { "ies" }
        ),
    })
}

fn admin_problem(id: &AppId) -> String {
    format!("app.{id}.admin")
}

/// Raise the question of `id` running as administrator here, with both
/// answers as buttons, and say what the asker waits for.
fn ask_for_admin(id: &AppId) -> String {
    let answer = |label: String, consent| ProblemAction {
        label,
        edit: ConfigEdit::AppAdmin {
            app: id.as_str().to_owned(),
            consent,
        },
    };
    crate::diagnostics::raise(Problem {
        id: admin_problem(id),
        severity: Severity::Warning,
        summary: format!("{id} asks to run as an administrator when started from another machine"),
        hint: Some(
            "the answer is kept for this application; `[apps.<id>] admin` in the config changes it"
                .into(),
        ),
        actions: vec![
            answer(
                format!("Let {id} run as administrator"),
                AdminConsent::Allow,
            ),
            answer("Run it as me".into(), AdminConsent::Deny),
        ],
        since: std::time::SystemTime::now(),
    });
    format!(
        "{id} asks to run as an administrator; this machine's person has to answer that \
         in Cursio's problems here first, once"
    )
}

/// The component to run: the one named, the only one, the one named after
/// the application, or the only one whose name starts with it.
fn pick(app: &AppId, names: &[String], asked: Option<&str>) -> Result<String, String> {
    if let Some(asked) = asked {
        return names
            .iter()
            .find(|n| n.as_str() == asked)
            .cloned()
            .ok_or_else(|| format!("the build has no {asked}; it has {}", names.join(", ")));
    }
    if let [only] = names {
        return Ok(only.clone());
    }
    if let Some(same) = names.iter().find(|n| n.as_str() == app.as_str()) {
        return Ok(same.clone());
    }
    let prefixed: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with(app.as_str()))
        .collect();
    match prefixed.as_slice() {
        [one] => Ok((*one).clone()),
        _ => Err(format!(
            "the build has several files ({}); name the one to run",
            names.join(", ")
        )),
    }
}

/// End every process running from inside `root`, answering how many there
/// were.
fn end_running(root: &Path) -> usize {
    let pids = running_from(root);
    for pid in &pids {
        terminate(*pid);
    }
    let start = std::time::Instant::now();
    while start.elapsed() < END_WAIT && !running_from(root).is_empty() {
        std::thread::sleep(Duration::from_millis(100));
    }
    pids.len()
}

#[cfg(target_os = "linux")]
fn running_from(root: &Path) -> Vec<u32> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
                .filter(|pid| {
                    std::fs::read_link(format!("/proc/{pid}/exe"))
                        .is_ok_and(|exe| exe.starts_with(&root))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn terminate(pid: u32) {
    // SAFETY: kill(2) on a pid read from /proc; a pid gone meanwhile is ESRCH.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

/// `args` as one Windows command line, the way the C runtime splits it back
/// apart: quoted where they hold a space, a tab or a quote (or are empty),
/// with backslashes before a quote doubled and the quote escaped. APIs that
/// take a single string — `CreateProcessWithTokenW`, `ShellExecuteExW` —
/// would otherwise hand `--name "a b"` over as two arguments.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_command_line(args: &[String]) -> String {
    let quote = |arg: &str| -> String {
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            return arg.to_owned();
        }
        let mut out = String::from('"');
        let mut backslashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    out.push_str(&"\\".repeat(backslashes * 2 + 1));
                    out.push('"');
                    backslashes = 0;
                }
                other => {
                    out.push_str(&"\\".repeat(backslashes));
                    out.push(other);
                    backslashes = 0;
                }
            }
        }
        out.push_str(&"\\".repeat(backslashes * 2));
        out.push('"');
        out
    };
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

#[cfg(target_os = "linux")]
fn spawn(exe: &Path, dir: &Path, args: &[String]) -> Result<u32, String> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(exe)
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|child| child.id())
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))
}

#[cfg(windows)]
fn running_from(root: &Path) -> Vec<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let root = root.to_string_lossy().to_lowercase();
    let mut found = Vec::new();
    // SAFETY: a snapshot walked with a correctly sized entry, then closed.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return found;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let pid = entry.th32ProcessID;
            if image_path(pid).is_some_and(|p| p.to_lowercase().starts_with(&root)) {
                found.push(pid);
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    found
}

#[cfg(windows)]
fn image_path(pid: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is opened, queried into a sized buffer and closed.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(process);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

#[cfg(windows)]
fn terminate(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    // SAFETY: a handle opened for termination and closed.
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(process, 0);
            let _ = CloseHandle(process);
        }
    }
}

#[cfg(windows)]
fn spawn(exe: &Path, dir: &Path, args: &[String]) -> Result<u32, String> {
    if !crate::elevate::is_elevated() {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        return std::process::Command::new(exe)
            .args(args)
            .current_dir(dir)
            .creation_flags(DETACHED_PROCESS)
            .spawn()
            .map(|child| child.id())
            .map_err(|e| format!("cannot start {}: {e}", exe.display()));
    }
    as_the_desktop_user(exe, dir, args)
}

/// Start `exe` with the desktop shell's token: the person's, unelevated.
/// Also how the opener starts a link's handler from an elevated node.
#[cfg(windows)]
pub(crate) fn as_the_desktop_user(exe: &Path, dir: &Path, args: &[String]) -> Result<u32, String> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ADJUST_DEFAULT,
        TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{
        CreateProcessWithTokenW, OpenProcess, OpenProcessToken, CREATE_PROCESS_LOGON_FLAGS,
        PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, STARTUPINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

    let wide = |s: &std::ffi::OsStr| -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(std::iter::once(0)).collect()
    };
    let app = wide(exe.as_os_str());
    let mut line = format!("\"{}\"", exe.display());
    if !args.is_empty() {
        line.push(' ');
        line.push_str(&windows_command_line(args));
    }
    let mut command_line = wide(std::ffi::OsStr::new(&line));
    let cwd = wide(dir.as_os_str());
    // SAFETY: every handle is checked, used for its documented purpose and
    // closed; the strings outlive the call that reads them.
    unsafe {
        let shell = GetShellWindow();
        if shell.is_invalid() {
            return Err("no desktop shell is running to start it as the person".into());
        }
        let mut shell_pid = 0u32;
        GetWindowThreadProcessId(shell, Some(&mut shell_pid));
        let process = OpenProcess(PROCESS_QUERY_INFORMATION, false, shell_pid)
            .map_err(|e| format!("cannot open the desktop shell: {e}"))?;
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_DUPLICATE, &mut token);
        let _ = CloseHandle(process);
        opened.map_err(|e| format!("cannot read the desktop shell's token: {e}"))?;
        let mut primary = HANDLE::default();
        let duplicated = DuplicateTokenEx(
            token,
            TOKEN_QUERY
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_DUPLICATE
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        );
        let _ = CloseHandle(token);
        duplicated.map_err(|e| format!("cannot copy the desktop shell's token: {e}"))?;
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        let created = CreateProcessWithTokenW(
            primary,
            CREATE_PROCESS_LOGON_FLAGS(0),
            PCWSTR(app.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR(cwd.as_ptr()),
            &startup,
            &mut info,
        );
        let _ = CloseHandle(primary);
        created.map_err(|e| format!("cannot start {} as the person: {e}", exe.display()))?;
        let _ = CloseHandle(info.hThread);
        let _ = CloseHandle(info.hProcess);
        Ok(info.dwProcessId)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn running_from(_root: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(not(any(target_os = "linux", windows)))]
fn terminate(_pid: u32) {}

#[cfg(not(any(target_os = "linux", windows)))]
fn spawn(exe: &Path, _dir: &Path, _args: &[String]) -> Result<u32, String> {
    Err(format!(
        "starting {} for a peer is not built for this platform",
        exe.display()
    ))
}

/// Start `exe` as an administrator. `None` for the pid where the system's
/// own prompt starts it and does not say which process it became.
#[cfg(target_os = "linux")]
fn spawn_admin(exe: &Path, dir: &Path, args: &[String]) -> Result<Option<u32>, String> {
    use std::os::unix::process::CommandExt;
    // polkit asks on this machine's screen, as Windows' prompt does there.
    std::process::Command::new("pkexec")
        .arg(exe)
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|child| Some(child.id()))
        .map_err(|e| {
            format!(
                "cannot start {} through pkexec: {e}; install polkit",
                exe.display()
            )
        })
}

#[cfg(windows)]
fn spawn_admin(exe: &Path, dir: &Path, args: &[String]) -> Result<Option<u32>, String> {
    if crate::elevate::is_elevated() {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        return std::process::Command::new(exe)
            .args(args)
            .current_dir(dir)
            .creation_flags(DETACHED_PROCESS)
            .spawn()
            .map(|child| Some(child.id()))
            .map_err(|e| format!("cannot start {}: {e}", exe.display()));
    }
    match crate::elevate::run_as_admin(exe, &windows_command_line(args), Some(dir), true)? {
        true => Ok(None),
        false => Err("declined at this machine's administrator prompt".into()),
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn spawn_admin(exe: &Path, _dir: &Path, _args: &[String]) -> Result<Option<u32>, String> {
    Err(format!(
        "starting {} as an administrator is not built for this platform",
        exe.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn arguments_reach_a_windows_program_as_they_were_given() {
        assert_eq!(
            windows_command_line(&names(&["ui", "--no-daemon"])),
            "ui --no-daemon"
        );
        assert_eq!(
            windows_command_line(&names(&["--name", "a b", ""])),
            r#"--name "a b" """#
        );
        assert_eq!(
            windows_command_line(&names(&[r#"say "hi""#])),
            r#""say \"hi\"""#
        );
        assert_eq!(
            windows_command_line(&names(&[r"C:\dir with space\"])),
            r#""C:\dir with space\\""#
        );
    }

    #[test]
    fn an_application_asking_for_admin_waits_for_an_answer_kept_for_it() {
        let app = AppId::parse("needs-admin").unwrap();
        let why = ask_for_admin(&app);
        assert!(why.contains("has to answer"));
        let problem = crate::diagnostics::snapshot()
            .into_iter()
            .find(|p| p.id == "app.needs-admin.admin")
            .expect("the question is raised");
        let mut config = crate::config::Config::default();
        problem.actions[0].edit.clone().apply(&mut config);
        assert_eq!(config.apps["needs-admin"].admin, AdminConsent::Allow);
        problem.actions[1].edit.clone().apply(&mut config);
        assert_eq!(config.apps["needs-admin"].admin, AdminConsent::Deny);
        crate::diagnostics::clear("app.needs-admin.admin");
    }

    #[test]
    fn the_file_to_run_is_the_one_plainly_the_application() {
        let app = AppId::parse("leavepulse").unwrap();
        assert_eq!(pick(&app, &names(&["x"]), None).unwrap(), "x");
        assert!(
            pick(
                &app,
                &names(&["leavepulse-launcher", "leavepulse-launcher-updater-helper"]),
                None
            )
            .is_err(),
            "two start with the id: ambiguous"
        );
        assert_eq!(
            pick(&app, &names(&["leavepulse-launcher", "helper"]), None).unwrap(),
            "leavepulse-launcher"
        );
        assert_eq!(
            pick(&app, &names(&["leavepulse", "leavepulse-helper"]), None).unwrap(),
            "leavepulse"
        );
        assert_eq!(pick(&app, &names(&["a", "b"]), Some("b")).unwrap(), "b");
        assert!(pick(&app, &names(&["a", "b"]), Some("../c")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_outside_the_install_is_never_counted_as_its_copy() {
        let root = std::env::temp_dir().join(format!("cursio-launch-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(running_from(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
