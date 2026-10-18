//! Letting Cursio, not PipeWire, be a paired phone's headset and speaker.
//!
//! PipeWire's Bluetooth backend registers every role by default, including
//! the two in which this computer is a *phone's* device: the hands-free unit
//! (`hfp_hf`, `hsp_hs`) and the media sink (`a2dp_sink`, `bap_sink`). BlueZ
//! allows one owner per role, and with PipeWire holding them the phone's
//! calls and music become a Bluetooth headset node on the desktop — a device
//! that appears, takes over the default output and microphone, and plays
//! wherever the desktop's own policy puts it.
//!
//! Cursio takes those roles instead, so the voice and the music enter the
//! same path as everything else the phone sends: one route, one volume, and
//! the same way onto another machine. PipeWire keeps the roles in which the
//! computer is the *source* (`a2dp_source`, `bap_source`) and the audio
//! gateway (`hfp_ag`, `hsp_ag`), so headphones and speakers paired with the
//! computer work exactly as before.
//!
//! One file, `~/.config/wireplumber/wireplumber.conf.d/90-cursio-bluetooth.conf`.
//! WirePlumber reads it when it starts; restarting it drops every Bluetooth
//! audio connection for a second, so that is done only by the explicit
//! command, never by a running node.

use std::path::PathBuf;

use anyhow::Context;

const FILE: &str = "90-cursio-bluetooth.conf";
const MARKER: &str =
    "# Written by `cursio bluetooth install`; `cursio bluetooth uninstall` removes it.";

/// The roles PipeWire keeps: the computer as a source and as a gateway.
const PIPEWIRE_ROLES: &str = "[ a2dp_source bap_source hfp_ag hsp_ag ]";

fn path() -> anyhow::Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(config.join("wireplumber/wireplumber.conf.d").join(FILE))
}

fn contents() -> String {
    format!(
        "{MARKER}\n\
         # Cursio is the hands-free unit and media sink for paired phones; PipeWire\n\
         # keeps the roles in which this computer plays to headphones and speakers.\n\
         monitor.bluez.properties = {{\n  bluez5.roles = {PIPEWIRE_ROLES}\n}}\n"
    )
}

/// Whether PipeWire has been told to leave the phone roles to Cursio.
pub fn installed() -> bool {
    path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|text| text.starts_with(MARKER))
}

pub fn install() -> anyhow::Result<()> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(&path, contents()).with_context(|| format!("write {}", path.display()))?;
    println!("wrote {}", path.display());
    restart_wireplumber()
}

pub fn uninstall() -> anyhow::Result<()> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) if text.starts_with(MARKER) => {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            println!("removed {}", path.display());
            restart_wireplumber()
        }
        Ok(_) => anyhow::bail!(
            "{} was not written by cursio; leaving it alone",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("nothing to remove");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn restart_wireplumber() -> anyhow::Result<()> {
    println!("restarting wireplumber (Bluetooth audio drops for a second)…");
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", "wireplumber"])
        .status()
        .context("run systemctl")?;
    anyhow::ensure!(
        status.success(),
        "systemctl --user restart wireplumber failed; restart it or log out and back in"
    );
    println!("done; paired phones can now use this computer for calls and sound through Cursio");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drop_in_leaves_pipewire_only_source_and_gateway_roles() {
        let text = contents();
        assert!(text.starts_with(MARKER));
        assert!(text.contains("bluez5.roles = [ a2dp_source bap_source hfp_ag hsp_ag ]"));
        assert!(!text.contains("hfp_hf") && !text.contains("a2dp_sink"));
    }
}
