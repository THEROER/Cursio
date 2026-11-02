//! A phone's own `adb`, reached over its wireless debugging.
//!
//! An Android app is refused three things Cursio needs from a phone it is
//! already trusted with: another app's sound without the screen-recording
//! consent (playback capture is tied to a media projection), the screen
//! without asking every time, and an application's update without its person
//! tapping Install. The `shell` user — what `adb shell` runs as — has all
//! three: scrcpy records sound through `REMOTE_SUBMIX` and the screen through
//! a display of its own, and `pm install` asks nobody.
//!
//! Wireless debugging (Android 11+) gives that user to any program that
//! paired with the phone once: a six-digit code on the phone's own screen,
//! one SPAKE2 exchange, and from then on the phone accepts this program's
//! RSA key on its debugging port. The program can be on the phone itself, as
//! LADB and Shizuku are — which is what makes this Cursio's: paired once by
//! its person, the phone's node holds the key and reaches its own `shell`
//! without a computer or a cable.
//!
//! What it does not survive: the phone switches wireless debugging off on a
//! reboot and on a change of network. The key stays trusted; turning
//! debugging back on is the person's one tap, and until then whoever uses
//! this falls back to what an app may do on its own.
//!
//! The protocol is [`libadb`]'s: TLS 1.3, SPAKE2 bound to the TLS exporter,
//! the encrypted peer-info exchange, then ordinary ADB over TLS.

use std::path::Path;

use libadb::keys::rsa::rand_core::OsRng;
use libadb::keys::{store, AdbKey};
use libadb::tls::{TlsClientConfig, TlsIdentity};
use libadb::transport::tls::MaybeTls;
use libadb::{Connection, Feature};

/// Why the phone's shell could not be reached or used.
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("the pairing code is the six digits on the phone's \"Pair device with pairing code\" screen")]
    BadCode,
    #[error("cannot keep the debugging key in {dir}: {why}")]
    Key { dir: String, why: String },
    #[error("cannot reach the phone's debugging port {addr}: {why}; is wireless debugging on?")]
    Unreachable { addr: String, why: String },
    #[error("pairing with {addr} failed: {why}; a wrong code shows up here too")]
    Pairing { addr: String, why: String },
    #[error("the phone at {addr} would not let this key in: {why}; pair again")]
    Refused { addr: String, why: String },
    #[error("the phone's shell failed running {command}: {why}")]
    Command { command: String, why: String },
}

/// The key this side presents, kept in `dir` (generated there the first
/// time). The phone trusts it once paired; losing it means pairing again.
pub fn key(dir: &Path, name: &str) -> Result<AdbKey, ShellError> {
    store::load_or_generate(dir, &mut OsRng, name).map_err(|e| ShellError::Key {
        dir: dir.display().to_string(),
        why: e.to_string(),
    })
}

/// Pair with the phone's pairing service at `addr` (host:port, from the
/// "Pair device with pairing code" screen) using its six-digit `code`.
/// Answers the phone's GUID.
pub async fn pair(addr: &str, code: &str, key: &AdbKey) -> Result<String, ShellError> {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ShellError::BadCode);
    }
    let pairing = |why: String| ShellError::Pairing {
        addr: addr.to_owned(),
        why,
    };
    let tls = tls_config(key).map_err(pairing)?;
    let socket = tcp(addr).await?;
    let mut transport = MaybeTls::plain(socket);
    let paired = libadb::pairing::pair(&mut transport, &tls, key, code, &mut OsRng)
        .await
        .map_err(|e| pairing(e.to_string()))?;
    Ok(paired.guid.escape_debug().to_string())
}

/// Run `command` (its words) as the phone's `shell` user through its
/// debugging port at `addr`, answering what it printed.
pub async fn exec(addr: &str, key: AdbKey, command: &[&str]) -> Result<Vec<u8>, ShellError> {
    let mut conn = connect(addr, key).await?;
    let mut rx = vec![0u8; 64 * 1024];
    libadb::cmd::exec(&mut conn, command, &mut rx)
        .await
        .map(|out| out.to_vec())
        .map_err(|e| ShellError::Command {
            command: command.join(" "),
            why: e.to_string(),
        })
}

async fn connect(
    addr: &str,
    key: AdbKey,
) -> Result<Connection<MaybeTls<libadb::TokioTcp>>, ShellError> {
    let refused = |why: String| ShellError::Refused {
        addr: addr.to_owned(),
        why,
    };
    let tls = tls_config(&key).map_err(refused)?;
    let socket = tcp(addr).await?;
    Connection::connect_tls(
        MaybeTls::plain(socket),
        key,
        &[Feature::ShellV2, Feature::AbbExec, Feature::Abb],
        &tls,
    )
    .await
    .map_err(|e| refused(e.to_string()))
}

fn tls_config(key: &AdbKey) -> Result<TlsClientConfig, String> {
    let identity = TlsIdentity::from_key(key, &mut OsRng).map_err(|e| e.to_string())?;
    TlsClientConfig::adb(&identity).map_err(|e| e.to_string())
}

async fn tcp(addr: &str) -> Result<libadb::TokioTcp, ShellError> {
    let unreachable = |why: String| ShellError::Unreachable {
        addr: addr.to_owned(),
        why,
    };
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| unreachable(e.to_string()))?;
    stream
        .set_nodelay(true)
        .map_err(|e| unreachable(e.to_string()))?;
    Ok(libadb::TokioTcp::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_code_that_is_not_six_digits_never_reaches_the_phone() {
        let dir = std::env::temp_dir().join(format!("cursio-shell-key-{}", std::process::id()));
        let key = key(&dir, "cursio@test").unwrap();
        for code in ["12345", "1234567", "12a456"] {
            assert!(matches!(
                pair("127.0.0.1:1", code, &key).await,
                Err(ShellError::BadCode)
            ));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
