//! Meeting a machine for the first time from the terminal: the password
//! prompt, one-time codes through the running node, and who is nearby.

use cursio_core::NodeIdentity;
use cursio_transport::discovery;
use std::sync::mpsc;
use std::time::Duration;
use zeroize::Zeroizing;

pub(crate) fn read_pairing_password() -> anyhow::Result<Zeroizing<String>> {
    let password = prompt_password("Pairing password: ")?;
    if password.is_empty() {
        anyhow::bail!("pairing password cannot be empty");
    }
    Ok(password)
}

/// Read a line from the terminal with echo off and nothing else changed.
///
/// Not rpassword: since 7.5 it puts the terminal in raw mode, ISIG included, and
/// answers Ctrl-C by raising SIGINT at itself before its guard restores the
/// terminal. The process dies with the terminal still raw, `cargo xtask run`
/// exits normally on seeing that, and the shell adopts the raw settings as its
/// own — so Ctrl-C stops interrupting anything in that terminal until `stty
/// sane`. With only ECHO cleared, Ctrl-C is the terminal's own SIGINT to the
/// whole foreground group, every parent dies by signal too, and the shell puts
/// back the settings it saved.
#[cfg(unix)]
pub(crate) fn prompt_password(prompt: &str) -> anyhow::Result<Zeroizing<String>> {
    use std::io::{BufRead, Write};
    use std::os::fd::AsRawFd;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| anyhow::anyhow!("--pair needs a terminal to read the password from: {e}"))?;
    let fd = tty.as_raw_fd();

    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `fd` is an open terminal for the life of `tty`, and tcgetattr
    // fully initialises the struct when it returns 0.
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
        anyhow::bail!(
            "cannot read the terminal settings: {}",
            std::io::Error::last_os_error()
        );
    }
    let original = unsafe { original.assume_init() };
    let mut silent = original;
    silent.c_lflag &= !libc::ECHO;
    silent.c_lflag |= libc::ECHONL;

    struct Restore(i32, libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the fd outlives this guard (declared after `tty`).
            unsafe { libc::tcsetattr(self.0, libc::TCSAFLUSH, &self.1) };
        }
    }

    (&tty).write_all(prompt.as_bytes())?;
    (&tty).flush()?;
    // SAFETY: as above; `silent` is a copy of settings the kernel just gave us.
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &silent) } != 0 {
        anyhow::bail!(
            "cannot turn off terminal echo: {}",
            std::io::Error::last_os_error()
        );
    }
    let _restore = Restore(fd, original);

    let mut line = Zeroizing::new(String::new());
    std::io::BufReader::new(&tty).read_line(&mut line)?;
    let trimmed = line.trim_end_matches(['\n', '\r']).len();
    line.truncate(trimmed);
    Ok(line)
}

#[cfg(not(unix))]
pub(crate) fn prompt_password(prompt: &str) -> anyhow::Result<Zeroizing<String>> {
    Ok(Zeroizing::new(rpassword::prompt_password(prompt)?))
}

pub(crate) async fn pair_with_code(code: Option<String>) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    use cursio_core::pairing_window::PairingOutcome;
    let client = control_client().await?;
    let command = match code {
        Some(code) => Command::PairEnter { code },
        None => Command::PairShow,
    };
    let mut shown_nearby = match client.request(command).await? {
        Reply::Pairing {
            state: Some(state),
            nearby,
            ..
        } => {
            match state.code {
                Some(code) => println!(
                    "Pairing code: {code}\nOn the other machine run `cursio pair {code}`, or enter it in the Cursio app."
                ),
                None => println!("Code accepted; waiting for the other machine…"),
            }
            print_nearby(&nearby);
            nearby_names(&nearby)
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected reply {other:?}"),
    };
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Reply::Pairing {
            state,
            last,
            nearby,
        } = client.request(Command::Pairing).await?
        else {
            anyhow::bail!("unexpected reply while waiting for pairing");
        };
        let names = nearby_names(&nearby);
        if state.is_some() && names != shown_nearby {
            print_nearby(&nearby);
            shown_nearby = names;
        }
        if state.is_some() {
            continue;
        }
        return match last {
            Some(PairingOutcome::Paired { peer }) => {
                println!("Paired with {peer}.");
                Ok(())
            }
            Some(PairingOutcome::Expired) => anyhow::bail!("the code expired before it was used"),
            Some(PairingOutcome::TooManyAttempts) => {
                anyhow::bail!("too many wrong codes; the window is closed — start again")
            }
            Some(PairingOutcome::Cancelled) | None => anyhow::bail!("pairing was cancelled"),
        };
    }
}

/// Who a pairing code could reach, compared by name and pairing state so a
/// repeated sighting does not print the list again.
pub(crate) fn nearby_names(nearby: &[cursio_core::control::NearbyMachine]) -> Vec<(String, bool)> {
    nearby.iter().map(|m| (m.name.clone(), m.paired)).collect()
}

/// Say who this node can see while a code waits, and say plainly when that is
/// nobody: a code shown to an empty network is never going to be used, and
/// without this the only sign is the window expiring.
pub(crate) fn print_nearby(nearby: &[cursio_core::control::NearbyMachine]) {
    if nearby.is_empty() {
        println!("{}", NOBODY_VISIBLE);
        return;
    }
    println!("Visible on this network:");
    for machine in nearby {
        let what = if machine.paired {
            "already paired"
        } else {
            "can pair"
        };
        println!("  {} ({}) — {what}", machine.name, machine.addr);
    }
}

pub(crate) const NOBODY_VISIBLE: &str =
    "No other Cursio machine is visible on this network, so nothing can \
use this code yet. Check that the other machine runs Cursio and is on the same network (not a \
guest network or a VPN, and not a hotspot that isolates its clients), and that the firewall lets \
Cursio through. Where the network blocks discovery, pair from a terminal with \
`cursio mesh --pair --pair-addr <address>:<port>`.";
use crate::client::control_client;

/// Browse the LAN and print discovered peers, then exit.
pub(crate) async fn list_peers(identity: NodeIdentity, timeout_secs: u64) -> anyhow::Result<()> {
    tracing::info!("browsing for {timeout_secs}s...");
    let self_id = identity.peer_id;
    let cfg = cursio_core::link::discovery_config();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = discovery::browse_with(self_id, &cfg, |peer| {
            let _ = tx.send(peer);
        });
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut found = 0;
    while std::time::Instant::now() < deadline {
        if let Ok(peer) = rx.recv_timeout(Duration::from_millis(200)) {
            found += 1;
            println!("  {} @ {} ({})", peer.display_name, peer.addr, peer.peer_id);
        }
    }
    tracing::info!("done; {found} peer(s) found");
    Ok(())
}
