//! The app's entry in the desktop's application list, written by the install.
//!
//! `cargo xtask install` writes one for a checkout, but a machine that only
//! ever received builds over the mesh has no checkout and never runs it: the
//! app was installed and kept up to date there, and searching for Cursio found
//! nothing. So the node writes it whenever it runs from the install — at its
//! own start rather than in the apply helper, because the helper is the
//! *previous* version's code and would leave every change here one update late.
//!
//! The entry names the root's stable `cursio-app`, never a `versions/` path,
//! which a prune would leave pointing at nothing. The icon is compiled in, for
//! the same reason the entry is written here: there is no checkout to copy it
//! from. Files are rewritten only when they differ, so a node restarting does
//! not touch the desktop's caches each time.

use std::fs;
use std::path::{Path, PathBuf};

use super::layout::InstallLayout;
use super::UpdateError;

const APP: &str = "cursio-app";
/// The same file name `cargo xtask install` writes, so the two replace each
/// other instead of the list showing Cursio twice.
const ENTRY: &str = "dev.cursio.cursio-app.desktop";
const ICON: &str = "cursio";

const ICONS: &[(&str, &[u8])] = &[
    (
        "32x32",
        include_bytes!("../../../../app/icons/32x32.png").as_slice(),
    ),
    (
        "128x128",
        include_bytes!("../../../../app/icons/128x128.png").as_slice(),
    ),
    (
        "256x256",
        include_bytes!("../../../../app/icons/128x128@2x.png").as_slice(),
    ),
    (
        "512x512",
        include_bytes!("../../../../app/icons/icon.png").as_slice(),
    ),
];
const SCALABLE: &[u8] = include_bytes!("../../../../app/icons/logo.svg");

/// Register the app with the desktop when this process runs from the install
/// and the install has an app. Returns the entry written, `None` when there
/// was nothing to do.
pub fn register_from_install() -> Result<Option<PathBuf>, UpdateError> {
    let layout = InstallLayout::for_user()?;
    let exe = std::env::current_exe().map_err(|e| UpdateError::io(Path::new("current_exe"), e))?;
    if layout.version_of(&exe).is_none() {
        return Ok(None);
    }
    let Some(app) = super::layout::stable_executable(APP) else {
        return Ok(None);
    };
    let Some(data) = data_home() else {
        return Err(UpdateError::Refused(
            "neither XDG_DATA_HOME nor HOME is set; cannot add Cursio to the application list"
                .into(),
        ));
    };
    register(&data, &app).map(Some)
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

fn register(data: &Path, app: &Path) -> Result<PathBuf, UpdateError> {
    let theme = data.join("icons/hicolor");
    let mut icons_changed = false;
    for (size, bytes) in ICONS {
        icons_changed |= write_if_changed(&theme.join(size).join("apps/cursio.png"), bytes)?;
    }
    icons_changed |= write_if_changed(&theme.join("scalable/apps/cursio.svg"), SCALABLE)?;

    let apps = data.join("applications");
    let path = apps.join(ENTRY);
    if write_if_changed(&path, entry(app).as_bytes())? {
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&apps)
            .status();
    }
    // A stale cache in the theme hides icons added beside it; with no cache
    // the desktop scans the directory, so a missing tool costs nothing.
    if icons_changed && theme.join("icon-theme.cache").exists() {
        let _ = std::process::Command::new("gtk-update-icon-cache")
            .args(["-f", "-t"])
            .arg(&theme)
            .status();
    }
    Ok(path)
}

fn entry(app: &Path) -> String {
    // The window's Wayland app id is the binary's name, not this file's, so
    // without `StartupWMClass` the dock cannot tie the window to the entry.
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Cursio\n\
         Comment=LAN-first cross-machine workspace\n\
         Exec=\"{}\"\n\
         Icon={ICON}\n\
         StartupWMClass={APP}\n\
         Terminal=false\n\
         Categories=Utility;\n",
        app.display()
    )
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool, UpdateError> {
    if fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(false);
    }
    let dir = path.parent().expect("launcher paths have a parent");
    fs::create_dir_all(dir).map_err(|e| UpdateError::io(dir, e))?;
    fs::write(path, bytes).map_err(|e| UpdateError::io(path, e))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cursio-launcher-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_entry_names_the_stable_app_and_its_icon() {
        let data = scratch("entry");
        let path = register(&data, Path::new("/home/u/.local/lib/cursio/cursio-app")).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("Exec=\"/home/u/.local/lib/cursio/cursio-app\"\n"));
        assert!(text.contains("Icon=cursio\n"));
        assert!(text.contains("StartupWMClass=cursio-app\n"));
        assert!(data.join("icons/hicolor/128x128/apps/cursio.png").is_file());
        assert!(data
            .join("icons/hicolor/scalable/apps/cursio.svg")
            .is_file());
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn registering_again_leaves_unchanged_files_alone() {
        let data = scratch("again");
        let app = Path::new("/opt/cursio-app");
        let path = register(&data, app).unwrap();
        let written = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        register(&data, app).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), written);
        let _ = fs::remove_dir_all(&data);
    }
}
