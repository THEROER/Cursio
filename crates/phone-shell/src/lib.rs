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
use libadb::keys::store;
pub use libadb::keys::AdbKey;
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

/// The key this side presents, kept in `dir` as `adbkey` (generated there
/// the first time). The phone trusts it once paired; losing it means
/// pairing again.
///
/// Written here rather than by `libadb`'s store, which publishes a new key
/// with a hard link so as never to overwrite one; Android's SELinux refuses
/// an app hard links even in its own files, and pairing on the phone failed
/// with "Permission denied". A temporary file renamed into place is atomic
/// as well, and the one process holding this directory has nothing to race.
pub fn key(dir: &Path, name: &str) -> Result<AdbKey, ShellError> {
    let failed = |why: String| ShellError::Key {
        dir: dir.display().to_string(),
        why,
    };
    let path = dir.join(KEY_FILE);
    if !path.exists() {
        std::fs::create_dir_all(dir).map_err(|e| failed(e.to_string()))?;
        let key = AdbKey::generate(&mut OsRng, name).map_err(|e| failed(e.to_string()))?;
        let pem = key.to_pkcs8_pem().map_err(|e| failed(e.to_string()))?;
        let tmp = dir.join(format!("{KEY_FILE}.tmp"));
        write_private(&tmp, pem.as_bytes()).map_err(|e| failed(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| failed(e.to_string()))?;
        return Ok(key);
    }
    store::load(dir, &mut OsRng, name).map_err(|e| failed(e.to_string()))
}

/// The private key's file name, as `adb` names it.
const KEY_FILE: &str = "adbkey";

/// Write `bytes` readable by this user alone.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
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

/// Run `command` — shell source, reaching `sh -c` verbatim, so quoting what
/// goes into it is the caller's — as the phone's `shell` user through its
/// debugging port at `addr`, answering what it printed (stdout and stderr
/// together, at most [`MAX_OUTPUT`] bytes).
pub async fn exec(addr: &str, key: &mut AdbKey, command: &str) -> Result<Vec<u8>, ShellError> {
    let mut conn = connect(addr, key).await?;
    let mut out = vec![0u8; MAX_OUTPUT];
    let len = libadb::exec::run(&mut conn, command, &mut out)
        .await
        .map_err(|e| ShellError::Command {
            command: command.to_owned(),
            why: e.to_string(),
        })?;
    out.truncate(len);
    Ok(out)
}

/// Install (or replace) the APK at `apk` as the phone's `shell`, which no
/// person is asked to confirm. Answers the package manager's last word,
/// `Success` when it took the APK.
///
/// The bytes go down the command's stdin, the way `adb install` streams
/// them (`cmd package install -S <size>`): the `shell` user cannot read the
/// files of the app holding the APK, so the package manager could not be
/// handed a path.
pub async fn install(addr: &str, key: &mut AdbKey, apk: &Path) -> Result<String, ShellError> {
    use tokio::io::AsyncReadExt as _;
    let command = |why: String| ShellError::Command {
        command: format!("install {}", apk.display()),
        why,
    };
    let mut file = tokio::fs::File::open(apk)
        .await
        .map_err(|e| command(e.to_string()))?;
    let size = file
        .metadata()
        .await
        .map_err(|e| command(e.to_string()))?
        .len();
    let mut conn = connect(addr, key).await?;
    let mut dest = vec![0u8; 128];
    let line = format!("cmd package install -r -S {size}");
    let mut session = libadb::exec::open(&mut conn, &line, &mut dest)
        .await
        .map_err(|e| command(e.to_string()))?;
    let mut chunk = vec![0u8; INSTALL_CHUNK];
    loop {
        let read = file
            .read(&mut chunk)
            .await
            .map_err(|e| command(e.to_string()))?;
        if read == 0 {
            break;
        }
        session
            .write_stdin(&chunk[..read])
            .await
            .map_err(|e| command(e.to_string()))?;
    }
    let mut said = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        match session.read(&mut buf).await {
            Ok(n) => said.extend_from_slice(&buf[..n]),
            Err(libadb::Error::ChannelClosed) => break,
            Err(e) => return Err(command(e.to_string())),
        }
    }
    let said = String::from_utf8_lossy(&said).trim().to_owned();
    match said.lines().last() {
        Some(last) if last.starts_with("Success") => Ok(last.to_owned()),
        _ => Err(command(if said.is_empty() {
            "the package manager said nothing".into()
        } else {
            said
        })),
    }
}

/// Bytes sent at a time while streaming an APK.
const INSTALL_CHUNK: usize = 64 * 1024;

/// The ports wireless debugging picks from, for its pairing and its
/// debugging service alike.
pub const PORTS: std::ops::RangeInclusive<u16> = 30000..=49999;

/// Find the debugging port of the phone at `host` by trying every port of
/// [`PORTS`]: the ones that open are asked to run `true` with `key`, and the
/// first that does is the one.
///
/// What a program on the phone is left with: Android's own mDNS does not show
/// an app the services the same phone advertises (seen on HyperOS 3 /
/// Android 16), and the port changes every time debugging is turned on. A
/// connection to a closed port on the phone's own address is refused at once,
/// so the sweep takes a second or two.
pub async fn find_debugging_port(host: &str, key: &mut AdbKey) -> Option<u16> {
    use tokio::task::JoinSet;
    let mut open = Vec::new();
    let mut probes = JoinSet::new();
    for port in PORTS {
        if probes.len() >= SWEEP_AT_ONCE {
            if let Some(Ok(Some(found))) = probes.join_next().await {
                open.push(found);
            }
        }
        let addr = format!("{host}:{port}");
        probes.spawn(async move {
            let attempt = tokio::time::timeout(
                std::time::Duration::from_millis(300),
                tokio::net::TcpStream::connect(&addr),
            );
            matches!(attempt.await, Ok(Ok(_))).then_some(port)
        });
    }
    while let Some(done) = probes.join_next().await {
        if let Ok(Some(found)) = done {
            open.push(found);
        }
    }
    open.sort_unstable();
    for port in open {
        let addr = format!("{host}:{port}");
        let tried =
            tokio::time::timeout(std::time::Duration::from_secs(5), exec(&addr, key, "true"));
        if matches!(tried.await, Ok(Ok(_))) {
            return Some(port);
        }
    }
    None
}

/// Connections the port sweep opens at once.
const SWEEP_AT_ONCE: usize = 512;

/// Most a command's output may be; past it [`exec`] fails rather than cut.
pub const MAX_OUTPUT: usize = 1024 * 1024;

async fn connect(
    addr: &str,
    key: &mut AdbKey,
) -> Result<Connection<MaybeTls<libadb::TokioTcp>>, ShellError> {
    let refused = |why: String| ShellError::Refused {
        addr: addr.to_owned(),
        why,
    };
    let tls = tls_config(key).map_err(refused)?;
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

    #[test]
    fn a_key_is_kept_private_and_the_same_key_is_read_back() {
        let dir = std::env::temp_dir().join(format!("cursio-shell-keep-{}", std::process::id()));
        let first = key(&dir, "cursio@test").unwrap();
        let again = key(&dir, "cursio@test").unwrap();
        assert_eq!(first.public_key_wire(), again.public_key_wire());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "readable by others: {mode:o}");
        }
        assert!(!dir.join("adbkey.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
