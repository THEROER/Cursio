//! Reaching the Windows peer: ssh into its WSL, and PowerShell through WSL's
//! interop.
//!
//! Scripts go over ssh's stdin to `bash -s` rather than as an argument: the
//! remote login shell would otherwise re-split and re-expand every quote, and a
//! PowerShell command inside a bash command inside an ssh argument has three
//! quoting layers nobody can read. PowerShell itself gets `-EncodedCommand`
//! (base64 UTF-16LE), which it takes verbatim whatever it contains.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use clap::Args;

/// Where the Windows peer is and how its paths map into WSL.
///
/// Defaults are the pair this tooling was built against (Lightmoon); every one
/// is a flag so another peer needs no code change.
#[derive(Args, Clone, Debug)]
pub struct Peer {
    /// ssh destination of the Windows peer (lands in its WSL).
    #[arg(long, default_value = "theroer@192.168.1.92")]
    pub ssh: String,
    /// ssh port of the Windows peer.
    #[arg(long, default_value_t = 2222)]
    pub ssh_port: u16,
    /// The Windows user's home, as WSL sees it.
    #[arg(long, default_value = "/mnt/c/Users/thero")]
    pub win_home: String,
    /// Name the peer goes by in the mesh (its mount lives at ~/Cursio/<name>).
    #[arg(long, default_value = "Lightmoon")]
    pub peer_name: String,
}

/// WSL's path to Windows PowerShell; interop runs it in the Windows user's
/// context, which is what `Win32_Process` and the task scheduler need.
const POWERSHELL: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";

impl Peer {
    /// The directory deploy and live-test own on the peer, in WSL form.
    pub fn cursio_dir(&self) -> String {
        format!("{}/Cursio", self.win_home)
    }

    /// Cursio's log directory on the peer (`%APPDATA%\cursio\cursio\data\logs`).
    pub fn log_dir(&self) -> String {
        format!("{}/AppData/Roaming/cursio/cursio/data/logs", self.win_home)
    }

    fn ssh_command(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
            .args(["-p", &self.ssh_port.to_string()])
            .arg(&self.ssh)
            .args(["bash", "-s"]);
        cmd
    }

    /// Run a bash script on the peer and return its stdout.
    pub fn bash(&self, script: &str) -> Result<String> {
        let mut child = self
            .ssh_command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ssh")?;
        child
            .stdin
            .take()
            .context("ssh stdin")?
            .write_all(script.as_bytes())
            .context("sending the script to ssh")?;
        let out = child.wait_with_output().context("waiting for ssh")?;
        if !out.status.success() {
            bail!(
                "ssh {} failed ({}): {}",
                self.ssh,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).replace('\r', ""))
    }

    /// Run a PowerShell script on the peer and return its stdout.
    pub fn powershell(&self, script: &str) -> Result<String> {
        // UTF-8 output: a Russian-locale console would otherwise hand back
        // cp866, and paths come back mangled.
        let full = format!("[Console]::OutputEncoding = [Text.Encoding]::UTF8\n{script}");
        self.bash(&format!(
            "'{POWERSHELL}' -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}\n",
            encode_powershell(&full)
        ))
    }

    /// Copy a local file to a WSL path on the peer.
    pub fn upload(&self, local: &std::path::Path, remote: &str) -> Result<()> {
        let status = Command::new("scp")
            .args([
                "-q",
                "-o",
                "BatchMode=yes",
                "-P",
                &self.ssh_port.to_string(),
            ])
            .arg(local)
            .arg(format!("{}:{remote}", self.ssh))
            .status()
            .context("spawning scp")?;
        if !status.success() {
            bail!("scp of {} to {remote} failed ({status})", local.display());
        }
        Ok(())
    }
}

/// `C:\...` for a `/mnt/c/...` WSL path.
pub fn wsl_to_windows(path: &str) -> String {
    match path.strip_prefix("/mnt/") {
        Some(rest) if rest.len() >= 2 && rest.as_bytes()[1] == b'/' => {
            let drive = rest[..1].to_ascii_uppercase();
            format!("{drive}:\\{}", rest[2..].replace('/', "\\"))
        }
        _ => path.replace('/', "\\"),
    }
}

/// A PowerShell literal string: single quotes, with embedded ones doubled.
pub fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A bash literal string: single quotes, with embedded ones closed and escaped.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The arguments of a Windows command line, with the program removed.
///
/// Windows hands a process one string, not a vector; the program is either a
/// quoted token or runs to the first whitespace, and everything after is kept
/// exactly as written so it can be passed back unchanged.
pub fn command_line_args(cmdline: &str) -> &str {
    let line = cmdline.trim_start();
    let rest = match line.strip_prefix('"') {
        Some(quoted) => quoted.find('"').map_or("", |end| &quoted[end + 1..]),
        None => line
            .find(char::is_whitespace)
            .map_or("", |end| &line[end..]),
    };
    rest.trim()
}

/// `-EncodedCommand` form: base64 of the UTF-16LE text.
fn encode_powershell(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_padding_cases() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn powershell_is_encoded_as_utf16le() {
        assert_eq!(encode_powershell("a"), "YQA=");
    }

    #[test]
    fn strips_a_quoted_or_bare_program_from_a_windows_command_line() {
        assert_eq!(
            command_line_args("cursio.new.exe  mesh --send-input --at 1,0"),
            "mesh --send-input --at 1,0"
        );
        assert_eq!(
            command_line_args(r#""C:\Users\a b\cursio.exe" mesh --files"#),
            "mesh --files"
        );
        assert_eq!(command_line_args("cursio.exe"), "");
    }

    #[test]
    fn maps_a_wsl_drive_path_to_windows() {
        assert_eq!(
            wsl_to_windows("/mnt/c/Users/thero/Cursio/x.exe"),
            r"C:\Users\thero\Cursio\x.exe"
        );
    }

    #[test]
    fn quotes_survive_both_shells() {
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }
}
