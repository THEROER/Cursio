//! What the gallery does with a file once it is on this computer: open it in
//! its own application, show it in the file manager, put it on the clipboard
//! as a file.
//!
//! None of it goes through a shell. A gallery item's name comes from another
//! machine, so it reaches a program only as one argument of its own, never as
//! part of a command line something could split or expand. The programs are
//! the desktop's own openers — `xdg-open` and the file manager's
//! `org.freedesktop.FileManager1` interface on Linux, Explorer on Windows —
//! rather than an opener plugin: they are what every other application on
//! the machine already uses, so a file opens where the user set it to.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// How to open `path` in its default application.
fn open_invocation(path: &Path) -> (&'static str, Vec<OsString>) {
    #[cfg(windows)]
    {
        // Explorer given a file opens it with its registered handler.
        ("explorer.exe", vec![path.as_os_str().to_owned()])
    }
    #[cfg(not(windows))]
    {
        ("xdg-open", vec![path.as_os_str().to_owned()])
    }
}

/// How to show `path` selected in the file manager.
fn reveal_invocation(path: &Path) -> (&'static str, Vec<OsString>) {
    #[cfg(windows)]
    {
        // `/select,` and the path are one argument: Explorer splits it itself.
        let mut arg = OsString::from("/select,");
        arg.push(path.as_os_str());
        ("explorer.exe", vec![arg])
    }
    #[cfg(not(windows))]
    {
        // Nautilus, Dolphin, Nemo and Thunar all answer ShowItems, which
        // opens the folder with the file selected rather than just the folder.
        let uri = file_uri(path);
        (
            "dbus-send",
            vec![
                "--session".into(),
                "--dest=org.freedesktop.FileManager1".into(),
                "--type=method_call".into(),
                "/org/freedesktop/FileManager1".into(),
                "org.freedesktop.FileManager1.ShowItems".into(),
                format!("array:string:{uri}").into(),
                "string:".into(),
            ],
        )
    }
}

/// Start `program` and reap it on a thread of its own, so neither a slow
/// opener holds the caller nor its exit leaves a zombie.
fn spawn(program: &str, args: Vec<OsString>) -> Result<(), String> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn open(path: &Path) -> Result<(), String> {
    let (program, args) = open_invocation(path);
    spawn(program, args)
}

pub fn reveal(path: &Path) -> Result<(), String> {
    let (program, args) = reveal_invocation(path);
    if spawn(program, args).is_ok() {
        return Ok(());
    }
    // No D-Bus tool on this machine: the folder alone is still somewhere to
    // look.
    let folder = path.parent().unwrap_or(path);
    open(folder)
}

/// Put files on the clipboard as a file manager copies them. The backend
/// keeps serving them after this returns, as X11 requires of a clipboard's
/// owner.
pub fn copy(paths: &[PathBuf]) -> Result<(), String> {
    let content = clipboard_core::ClipboardContent::files(paths);
    let mut clipboard = clipboard_core::open().map_err(|e| e.to_string())?;
    clipboard
        .write(clipboard_core::Selection::Clipboard, &content)
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn file_uri(path: &Path) -> String {
    let content = clipboard_core::ClipboardContent::files(&[path]);
    let list = content
        .bytes(&clipboard_core::ClipboardFormat::UriList)
        .unwrap_or_default();
    String::from_utf8_lossy(list).trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_reaches_its_opener_as_one_argument_whatever_its_name() {
        let path = Path::new("/home/me/Pictures/Cursio/POCO/$(rm -rf ~) a;b.jpg");
        let (_, args) = open_invocation(path);
        assert_eq!(args, vec![path.as_os_str().to_owned()]);
    }

    #[cfg(not(windows))]
    #[test]
    fn the_file_manager_is_asked_to_select_the_file_by_its_uri() {
        let (program, args) = reveal_invocation(Path::new("/tmp/IMG 1.jpg"));
        assert_eq!(program, "dbus-send");
        assert!(args.contains(&OsString::from("array:string:file:///tmp/IMG%201.jpg")));
    }
}
