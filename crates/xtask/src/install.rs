//! Putting a build on this machine: the binaries, their desktop entries and
//! icons, and the GNOME Shell extension.

use crate::build;
use crate::checkout_dir;
use crate::cursio_log_dir;
use crate::exe_name;
use crate::out_dir;
use crate::select_for_host;
use crate::which;
use crate::BinArg;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Build release binaries (unless `no_build`) and copy them into ~/.local/bin,
/// installing `.desktop` launchers for the GUI ones on Linux.
pub(crate) fn install(only: Option<BinArg>, no_build: bool) -> Result<()> {
    if !no_build {
        build(true, only, None)?;
    }

    let bin_dir = local_bin_dir()?;
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;

    let src_dir = out_dir(true, None);
    let selected = select_for_host(only)?;
    for b in &selected {
        let src = src_dir.join(exe_name(b.bin, None));
        if !src.exists() {
            bail!(
                "{} not found — run without --no-build, or `cargo xtask build` first",
                src.display()
            );
        }
        let dst = bin_dir.join(exe_name(b.bin, None));
        install_binary(&src, &dst)
            .with_context(|| format!("installing {} → {}", src.display(), dst.display()))?;
        println!("✓ installed {} → {}", b.bin, dst.display());

        if b.gui && cfg!(target_os = "linux") {
            install_desktop_file(b.bin, &dst)?;
        }
    }

    if cfg!(target_os = "linux") {
        install_shell_extension()?;
    }

    warn_if_not_on_path(&bin_dir);
    println!("\nDone. Logs go to {}", cursio_log_dir().display());
    Ok(())
}

/// Copy `src` over `dst` atomically: write to a temp file in the same dir, then
/// `rename` it into place. `rename` swaps the directory entry to a new inode, so
/// a copy of the binary that's *currently running* (which holds the old inode)
/// keeps working — a plain `fs::copy` onto it fails with "Text file busy".
pub(crate) fn install_binary(src: &Path, dst: &Path) -> Result<()> {
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.new",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or("bin")
    ));
    // Clean up any leftover temp from a previous interrupted run.
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(src, &tmp).with_context(|| format!("staging {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, dst).with_context(|| format!("replacing {}", dst.display()))?;
    Ok(())
}

/// `~/.local/bin` via HOME. Kept simple: this is the user-install prefix chosen
/// for Cursio; `/usr/local/bin` and packaging are out of scope here.
pub(crate) fn local_bin_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

/// Write a `.desktop` launcher into ~/.local/share/applications for a GUI app.
pub(crate) fn install_desktop_file(bin: &str, exec: &Path) -> Result<()> {
    let apps_dir = {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .context("HOME not set")?;
        base.join("applications")
    };
    std::fs::create_dir_all(&apps_dir)?;

    let (name, comment) = match bin {
        "clipd" => ("Cursio Clipboard", "Clipboard history (Win+V for Wayland)"),
        "cursio-app" => ("Cursio", "LAN-first cross-machine workspace"),
        _ => (bin, "Cursio"),
    };
    // The window's Wayland app id is the binary's name, not the file name of
    // this launcher, so without `StartupWMClass` GNOME cannot tie the two
    // together and the dock shows "cursio-app" with a generic icon.
    let icon = if bin == "cursio-app" {
        install_app_icons(&base_data_dir()?)?;
        "Icon=cursio\n".to_string()
    } else {
        String::new()
    };
    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Comment={comment}\n\
         Exec={exec}\n\
         {icon}\
         StartupWMClass={bin}\n\
         Terminal=false\n\
         Categories=Utility;\n",
        exec = exec.display(),
    );
    let path = apps_dir.join(format!("dev.cursio.{bin}.desktop"));
    std::fs::write(&path, desktop).with_context(|| format!("writing {}", path.display()))?;
    println!("  ↳ launcher {}", path.display());
    // Best effort: both only refresh caches a desktop would also rebuild on
    // its own, and neither is installed everywhere.
    let _ = Command::new("update-desktop-database")
        .arg(&apps_dir)
        .status();
    Ok(())
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
pub(crate) fn base_data_dir() -> Result<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("HOME not set")
}

/// Put the logo into the user's hicolor theme as `cursio`, which is the name
/// the launcher asks for.
pub(crate) fn install_app_icons(data_dir: &Path) -> Result<()> {
    let src = checkout_dir().join("app/icons");
    let theme = data_dir.join("icons/hicolor");
    for (file, size) in [
        ("32x32.png", "32x32"),
        ("128x128.png", "128x128"),
        ("128x128@2x.png", "256x256"),
        ("icon.png", "512x512"),
    ] {
        let dir = theme.join(size).join("apps");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::copy(src.join(file), dir.join("cursio.png"))
            .with_context(|| format!("installing the {size} icon"))?;
    }
    let scalable = theme.join("scalable/apps");
    std::fs::create_dir_all(&scalable)?;
    std::fs::copy(src.join("logo.svg"), scalable.join("cursio.svg"))
        .context("installing the scalable icon")?;
    let _ = Command::new("gtk-update-icon-cache")
        .args(["-f", "-t"])
        .arg(&theme)
        .status();
    Ok(())
}

/// Copy the GNOME Shell extension (drag bridge and quick settings tile) into
/// the user's extensions directory.
///
/// Skipped without GNOME Shell. The running Shell does not reload an
/// extension's code on Wayland, so a changed copy only takes effect at the
/// next login — said here because nothing else would.
pub(crate) fn install_shell_extension() -> Result<()> {
    if which("gnome-shell").is_none() {
        return Ok(());
    }
    let src = checkout_dir().join("native/cursio-shell-extension");
    let dst = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("HOME not set")?
        .join("gnome-shell/extensions/cursio@cursio.dev");
    std::fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in std::fs::read_dir(&src).with_context(|| format!("reading {}", src.display()))? {
        let path = entry?.path();
        if path.is_file() {
            let to = dst.join(path.file_name().context("extension file has no name")?);
            std::fs::copy(&path, &to)
                .with_context(|| format!("copying {} → {}", path.display(), to.display()))?;
        }
    }
    println!(
        "✓ installed the GNOME Shell extension → {}\n  \
         enable it with `gnome-extensions enable cursio@cursio.dev`; a changed one \
         loads at the next login",
        dst.display()
    );
    Ok(())
}

/// Warn (don't fail) if the install dir isn't on PATH, with the fix.
pub(crate) fn warn_if_not_on_path(bin_dir: &Path) {
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == bin_dir))
        .unwrap_or(false);
    if !on_path {
        eprintln!(
            "\n! {} is not on your PATH. Add it, e.g.:\n  \
             echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc",
            bin_dir.display()
        );
    }
}
