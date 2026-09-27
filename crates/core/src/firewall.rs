//! Opening the mesh port in the Windows firewall.
//!
//! A node that cannot accept connections is not obviously broken: it starts,
//! it announces itself over mDNS, and every peer on the LAN lists it — because
//! discovery is UDP and the announcement leaves the machine fine. Only the TCP
//! connection back is dropped, and Windows drops it *silently*: the caller sees
//! a timeout rather than a refusal, which reads as a network problem rather
//! than a local rule. Seen live: a node visible in `cursio peers` at its own
//! address and port, with `listening for peers` in its log, that no peer could
//! reach.
//!
//! Windows does offer to create a rule the first time a program listens, and
//! accepting it is not enough here: the prompt covers the socket that triggered
//! it, and Cursio's first listener is the mDNS one. Answering "allow" opens
//! *UDP* — discovery keeps working, the node keeps appearing in `cursio peers`,
//! and the TCP port peers actually connect to stays shut. That is exactly the
//! failure this exists to prevent, and it looks like a network fault rather
//! than a firewall rule.
//!
//! Worse, the prompt is answered by whoever is at the machine: for a user who
//! is not an administrator, Windows writes a *blocking* rule whichever button
//! they press, and it then has to be found and deleted by hand.
//!
//! ## Why this is safe to do
//!
//! The rules name this executable and the ports it listens on: the mesh port
//! over TCP and mDNS over UDP. They apply on every network profile, because
//! Windows files many home LANs as "Public" and a private-only rule would
//! leave exactly those machines unreachable; the mesh port answers nobody who
//! has not paired. It is not a hole in the firewall: it is the same permission
//! the user would have granted by clicking "Allow access", made without
//! depending on them seeing a dialog.
//!
//! ## Profile any, but only from the local subnet
//!
//! "Every profile" answers *which networks* this machine may be on; it says
//! nothing about *who* may connect, and before this the answer was anyone who
//! could route a packet here. Cursio is a LAN mesh — peers find each other by
//! mDNS, which never leaves the link — so a connection from beyond the local
//! subnet is never a peer. Both rules therefore carry `remoteip=localsubnet`:
//! the addresses on the same subnet as one of this machine's interfaces.
//!
//! The case this closes is IPv6. A home router hands out global addresses and
//! NAT no longer hides the machine, so with an any-address rule the mesh port
//! was reachable from the internet wherever the router's own firewall was
//! permissive. Pairing still stood between that and a session, but a handshake
//! surface open to the world is not something a LAN tool should offer.
//!
//! The cost is a peer on a *routed* network — another VLAN, a VPN such as
//! Tailscale — which the firewall now refuses. Such a peer was never found by
//! discovery either, and `--pair-addr` to it would have to be paired with a
//! deliberately wider rule the user writes themselves.
//!
//! The rule name carries the scope so a machine with the older any-address
//! rule does not find "its" rule and stop: the new name is absent, so the
//! repair below runs once, purges the old rule by program and writes this one.
//!
//! ## Why a separate elevation, and not the process's own
//!
//! Adding the rule needs administrator rights, and Cursio does ask for them at
//! startup (see [`crate::elevate`]) — but running the whole node elevated is
//! not an option here: a filesystem mounted by an elevated process is invisible
//! to Explorer, which runs as the user. The peer's drives would stop appearing
//! in the very place the user looks for them.
//!
//! So only the rule is elevated: one `netsh` started with the `runas` verb,
//! which raises a UAC prompt once and leaves the node itself unelevated. Say no
//! and everything still works except being reachable, which is said plainly.

/// Make sure peers can reach this node's mesh port.
#[cfg(not(target_os = "windows"))]
pub fn allow_mesh_port(_port: u16) {
    // Nothing to do: Linux and macOS do not filter inbound connections by
    // default, and where a firewall is configured it is the administrator's,
    // not something an application should edit behind their back.
}

/// Make sure peers can reach this node's mesh port.
///
/// Called once at startup with the port actually bound, so a node started on a
/// non-default port opens that one rather than the default.
#[cfg(target_os = "windows")]
pub fn allow_mesh_port(port: u16) {
    let name = rule_name(port);
    match rule_exists(&name) {
        Ok(true) => {
            tracing::debug!(port, "the firewall already lets peers in");
            return;
        }
        Ok(false) => {}
        Err(error) => {
            // Worth saying, but not worth stopping for: the add below will
            // report its own outcome.
            tracing::debug!(%error, "could not ask the firewall what it allows");
        }
    }

    match add_rule(&name, port) {
        Ok(()) => tracing::info!(port, "opened the mesh port in the firewall"),
        Err(error) => tracing::warn!(
            %error,
            port,
            "could not open the mesh port in the firewall; peers may see this \
             node without being able to reach it"
        ),
    }
}

/// The rule's name, carrying the port so two nodes on one machine — a second
/// instance on another port — do not fight over one rule.
#[cfg(target_os = "windows")]
fn rule_name(port: u16) -> String {
    format!("Cursio mesh (TCP {port}, local subnet)")
}

#[cfg(target_os = "windows")]
fn rule_exists(name: &str) -> Result<bool, String> {
    let out = netsh(&[
        "advfirewall",
        "firewall",
        "show",
        "rule",
        &format!("name={name}"),
    ])?;
    // `show rule` exits non-zero when the rule is absent, which `netsh` reports
    // through the status rather than the text, so a success here is the answer.
    Ok(out)
}

#[cfg(target_os = "windows")]
fn add_rule(name: &str, port: u16) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot name this executable: {e}"))?
        .display()
        .to_string();
    // The path is spliced into a `cmd.exe` line. Quotes keep `&` and spaces
    // literal, but cmd still expands `%VAR%` inside them, so a directory named
    // with a `%` would rewrite the command run as administrator.
    if exe.contains(['%', '"']) {
        return Err(format!(
            "the executable path {exe} contains a character cmd.exe would \
             expand; move cursio to a directory without '%' in its name"
        ));
    }
    // Deleting first, and by program rather than by our own rule name.
    //
    // Windows writes its own rules when a program starts listening, and for a
    // user who is not an administrator it writes *blocking* ones whichever
    // button they press — documented behaviour, and what happened here: two
    // rules, TCP and UDP, both `Block`, created by a user who chose "allow".
    // A block always beats an allow, so adding permission on top of them
    // changes nothing, and the node stays unreachable while the firewall
    // appears to have been asked nicely.
    // All in one elevation: several prompts for one repair is too many, and
    // stopping between them would leave the machine with neither rule.
    run_elevated("cmd.exe", &repair_command(name, port, &exe))
}

/// The one `cmd.exe` line that replaces whatever rules name `exe` with ours.
#[cfg(target_os = "windows")]
fn repair_command(name: &str, port: u16, exe: &str) -> String {
    let purge = format!("advfirewall firewall delete rule name=all program=\"{exe}\"");
    let add = format!(
        "advfirewall firewall add rule name=\"{name}\" dir=in action=allow \
         protocol=TCP localport={port} program=\"{exe}\" profile=any remoteip=localsubnet"
    );
    // The purge also removes the *allow* rule Windows may have written for
    // the mDNS socket, and inbound queries from peers are unsolicited UDP that
    // no connection state lets through. Without replacing it, fixing
    // reachability would cost discoverability.
    let discovery = format!(
        "advfirewall firewall add rule name=\"Cursio discovery (UDP 5353)\" dir=in \
         action=allow protocol=UDP localport=5353 program=\"{exe}\" profile=any \
         remoteip=localsubnet"
    );
    // `&&` so the exit status reports the first step that failed; the purge
    // finding nothing to delete is not a failure.
    format!("/c netsh {purge} & netsh {add} && netsh {discovery}")
}

/// Run one command with administrator rights, raising a UAC prompt.
///
/// The node itself stays unelevated — see the module note on why that matters
/// for the drives in Explorer.
#[cfg(target_os = "windows")]
fn run_elevated(program: &str, args: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

    let verb = to_wide("runas");
    let file = to_wide(program);
    let params = to_wide(args);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        // Hidden: a console flashing past at startup says nothing the log does
        // not say better, and reads as a crash.
        nShow: 0,
        ..Default::default()
    };

    // SAFETY: `info` is fully initialised and the wide strings outlive the call.
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => {}
        Err(e) if e.code().0 as u32 & 0xFFFF == ERROR_CANCELLED.0 => {
            return Err("the elevation prompt was declined".into());
        }
        Err(e) => return Err(format!("ShellExecuteExW: {e}")),
    }

    // `ShellExecuteExW` returns once the child has *started*. Reporting that as
    // the rule being in place would claim success for a `netsh` that then
    // failed, so wait for it and read its exit code.
    let process = info.hProcess;
    if process.is_invalid() {
        return Err("the elevated command started without a process handle".into());
    }
    // SAFETY: `process` is the handle `SEE_MASK_NOCLOSEPROCESS` handed us; it
    // is closed exactly once below, on every path.
    unsafe {
        let waited = WaitForSingleObject(process, ELEVATED_TIMEOUT_MS);
        let mut code = 0u32;
        let read = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
        if waited != WAIT_OBJECT_0 {
            return Err(format!(
                "the elevated command did not finish within {} s",
                ELEVATED_TIMEOUT_MS / 1000
            ));
        }
        read.map_err(|e| format!("GetExitCodeProcess: {e}"))?;
        if code != 0 {
            return Err(format!("the elevated command exited with status {code}"));
        }
    }
    Ok(())
}

/// How long the elevated command may run. The UAC prompt has already been
/// answered by the time this starts counting, so this bounds `netsh` alone.
#[cfg(target_os = "windows")]
const ELEVATED_TIMEOUT_MS: u32 = 15_000;

/// Widen for the Win32 APIs, which take UTF-16 with a trailing NUL.
#[cfg(target_os = "windows")]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Run `netsh` and report whether it succeeded.
#[cfg(target_os = "windows")]
fn netsh(args: &[&str]) -> Result<bool, String> {
    use std::os::windows::process::CommandExt;
    /// Keep the console window from flashing up: this runs at startup, and a
    /// black rectangle appearing for a moment reads as a crash.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("netsh")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|out| out.status.success())
        .map_err(|e| format!("cannot run netsh: {e}"))
}

#[cfg(test)]
mod tests {
    /// The rule is named per port, so a second node on another port gets its
    /// own rule instead of overwriting the first one's.
    #[test]
    #[cfg(target_os = "windows")]
    fn each_port_gets_its_own_rule() {
        assert_ne!(super::rule_name(46073), super::rule_name(46077));
        assert!(super::rule_name(46077).contains("46077"));
    }

    /// Every profile, but only the local subnet: a Public-labelled home LAN
    /// still reaches the node, the internet over IPv6 does not.
    #[test]
    #[cfg(target_os = "windows")]
    fn both_rules_admit_only_the_local_subnet_on_every_profile() {
        let line = super::repair_command("Cursio mesh", 46073, r"C:\cursio\cursio.exe");
        let adds: Vec<&str> = line
            .split("netsh")
            .filter(|c| c.contains("add rule"))
            .collect();
        assert_eq!(adds.len(), 2, "mesh and discovery: {line}");
        for add in adds {
            assert!(add.contains("profile=any"), "{add}");
            assert!(add.contains("remoteip=localsubnet"), "{add}");
        }
    }

    /// A machine carrying the old any-address rule must not find it under the
    /// current name and skip the repair that narrows it.
    #[test]
    #[cfg(target_os = "windows")]
    fn the_rule_name_says_its_scope() {
        assert!(super::rule_name(46073).contains("local subnet"));
    }

    /// On platforms that do not filter inbound connections this does nothing
    /// and must not need a `#[cfg]` at the call site.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn opening_a_port_is_a_no_op_where_nothing_filters() {
        super::allow_mesh_port(46073);
    }
}
