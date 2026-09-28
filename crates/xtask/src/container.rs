//! The Ubuntu 24.04 build, made in a container from whatever Linux this is.
//!
//! A Linux build is only good where the FFmpeg it linked is installed (see
//! `crates/core/build.rs`), so a mesh with a 24.04 machine and a newer one
//! needs two Linux builds per version. The second is built here, in
//! `docker/ubuntu-24.04.Dockerfile`, with the tree and the cargo registry
//! mounted rather than copied: a build is incremental across runs, and the
//! binaries land in `target/ubuntu-24.04/` beside the host's.
//!
//! The image is rebuilt with `docker build` on every use. Docker's layer cache
//! makes that seconds when the Dockerfile is unchanged, and it means an edited
//! Dockerfile is never silently ignored in favour of a stale image.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

const IMAGE: &str = "cursio-build-ubuntu-24.04";
const DOCKERFILE: &str = include_str!("../docker/ubuntu-24.04.Dockerfile");
/// Under the mounted tree, so the output is the host's to read and stage.
const TARGET_DIR: &str = "target/ubuntu-24.04";

/// Whether this machine can build it at all.
pub fn available() -> bool {
    crate::which("docker").is_some()
}

/// Where the release binaries land.
pub fn out_dir() -> PathBuf {
    PathBuf::from(TARGET_DIR).join("release")
}

fn ensure_image() -> Result<()> {
    println!("→ preparing the Ubuntu 24.04 build image");
    let mut child = Command::new("docker")
        .args(["build", "-q", "-t", IMAGE, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("running docker; install it to build for Ubuntu 24.04")?;
    child
        .stdin
        .take()
        .context("docker build took no stdin")?
        .write_all(DOCKERFILE.as_bytes())
        .context("handing the Dockerfile to docker build")?;
    let status = child.wait().context("waiting for docker build")?;
    if !status.success() {
        bail!("docker build of {IMAGE} failed ({status})");
    }
    Ok(())
}

/// Build the node and the app for Ubuntu 24.04, release.
///
/// Two cargo invocations, as on the host: built together, feature
/// unification would hand the app a `cursio-core` with every platform module
/// in it, and its GTK4 cannot share a process with the webview's GTK3.
pub fn build() -> Result<()> {
    ensure_image()?;
    let root = std::env::current_dir().context("reading the working directory")?;
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .context("neither CARGO_HOME nor HOME is set; cannot share the cargo registry")?;
    // Root inside, so the toolchain under /root is usable; the output is then
    // handed back to whoever runs this, or `cargo clean` would need sudo.
    let owner = format!("{}:{}", id("-u")?, id("-g")?);
    let script = format!(
        "set -e; \
         cargo build --release -p cursio-core; \
         cargo build --release -p cursio-app; \
         chown -R {owner} {TARGET_DIR}"
    );
    println!("→ building cursio + cursio-app (release, Ubuntu 24.04)");
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "-w", "/src"])
        .arg("-v")
        .arg(format!("{}:/src", root.display()))
        .arg("-v")
        .arg(format!(
            "{}:/root/.cargo/registry",
            home.join("registry").display()
        ))
        .arg("-v")
        .arg(format!("{}:/root/.cargo/git", home.join("git").display()))
        .args(["-e", &format!("CARGO_TARGET_DIR=/src/{TARGET_DIR}")])
        .args(["-e", &format!("OPUS_TARGET={}", crate::OPUS_TARGET_LINUX)])
        .args([IMAGE, "bash", "-c", &script]);
    crate::run(cmd)
}

/// The FFmpeg ABI the image links against, as `crates/core/build.rs` names it.
pub fn abi() -> Result<String> {
    ensure_image()?;
    let out = Command::new("docker")
        .args([
            "run",
            "--rm",
            IMAGE,
            "pkg-config",
            "--modversion",
            "libavutil",
        ])
        .output()
        .context("running docker")?;
    if !out.status.success() {
        bail!(
            "pkg-config in {IMAGE} does not know libavutil: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    crate::stage::abi_from_version(&String::from_utf8_lossy(&out.stdout))
}

fn id(flag: &str) -> Result<String> {
    let out = Command::new("id")
        .arg(flag)
        .output()
        .context("running id")?;
    if !out.status.success() {
        bail!("id {flag} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
