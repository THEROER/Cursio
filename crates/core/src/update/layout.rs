//! Where builds live on disk, and the two pointers that say which one runs.
//!
//! ```text
//! <root>/versions/0.1.0+c9798e7/{cursio, cursio-app, manifest.json}
//! <root>/versions/0.1.0+d41c2e0/...
//! <root>/current     "0.1.0+d41c2e0"
//! <root>/previous    "0.1.0+c9798e7"
//! <root>/cursio, <root>/cursio-app      stable copies of current's
//! ```
//!
//! **The root is per user and outside anything a package manager owns.**
//! Windows: `%LOCALAPPDATA%\Cursio` — per user, not roamed (a build for this
//! machine must not follow the user to another), writable without elevation.
//! Linux: `~/.local/lib/cursio` — executables are not data, so not
//! `~/.local/share`; `lib` is where per-user installs of whole programs go
//! (pipx, `cargo install --root`), and nothing else claims that name.
//! macOS: `~/Library/Application Support/Cursio`, the one per-user place an
//! unsigned build may write itself. `CURSIO_INSTALL_ROOT` overrides all three,
//! which is what the tests and a second node on one machine use.
//!
//! **The pointers are files holding a name, not symlinks.** A symlink needs a
//! privilege on Windows (or developer mode), and "current points at a missing
//! directory" is then a question the filesystem answers differently on each
//! platform. A file written to a temporary name and renamed over the old one is
//! atomic everywhere Cursio runs: a crash mid-update leaves either the old
//! pointer or the new one, never half of each.
//!
//! **The stable copies are the only paths anything outside registers.**
//! Autostart, shortcuts and the app point at `<root>/cursio`, which never
//! changes name; started from there, it hands over to the current version's
//! executable at once (see [`handover`]) and exits. A registry entry naming
//! `versions/<v>/cursio.exe` would outlive the version it names by one prune.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::manifest::{sha256_file, Delta, Manifest, MANIFEST_FILE};
use super::UpdateError;

pub const CURRENT: &str = "current";
pub const PREVIOUS: &str = "previous";
const VERSIONS: &str = "versions";

/// Set in the environment of a process started by [`handover`], so a stable
/// copy that somehow resolves to itself cannot loop.
const HANDED_OVER_ENV: &str = "CURSIO_HANDED_OVER";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallLayout {
    root: PathBuf,
}

impl InstallLayout {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// This user's layout: `CURSIO_INSTALL_ROOT`, else the platform default.
    pub fn for_user() -> Result<Self, UpdateError> {
        if let Some(root) = std::env::var_os("CURSIO_INSTALL_ROOT") {
            return Ok(Self::at(root));
        }
        platform_root().map(Self::at).ok_or(UpdateError::NoRoot)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.root.join(VERSIONS).join(version)
    }

    pub fn current(&self) -> Option<String> {
        self.read_pointer(CURRENT)
    }

    pub fn previous(&self) -> Option<String> {
        self.read_pointer(PREVIOUS)
    }

    fn read_pointer(&self, name: &str) -> Option<String> {
        let text = fs::read_to_string(self.root.join(name)).ok()?;
        let version = text.trim();
        (valid_version(version) && self.version_dir(version).is_dir()).then(|| version.to_owned())
    }

    fn write_pointer(&self, name: &str, version: &str) -> Result<(), UpdateError> {
        let path = self.root.join(name);
        let temp = self.root.join(format!(".{name}.tmp"));
        fs::write(&temp, version).map_err(|e| UpdateError::io(&temp, e))?;
        fs::rename(&temp, &path).map_err(|e| UpdateError::io(&path, e))
    }

    /// Installed versions, newest install first.
    pub fn installed(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.root.join(VERSIONS)) else {
            return Vec::new();
        };
        let mut found: Vec<(SystemTime, String)> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().join(MANIFEST_FILE).is_file())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                valid_version(&name).then_some(())?;
                let at = e.metadata().and_then(|m| m.modified()).ok()?;
                Some((at, name))
            })
            .collect();
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        found.into_iter().map(|(_, name)| name).collect()
    }

    pub fn manifest(&self, version: &str) -> Result<Manifest, UpdateError> {
        let dir = self.version_dir(version);
        if !dir.is_dir() {
            return Err(UpdateError::NotInstalled(version.to_owned()));
        }
        Manifest::read(&dir)
    }

    /// The installed executable of `component` in `version`.
    pub fn executable(&self, version: &str, component: &str) -> Result<PathBuf, UpdateError> {
        let manifest = self.manifest(version)?;
        let entry = manifest.components.get(component).ok_or_else(|| {
            UpdateError::Refused(format!("build {version} carries no {component}"))
        })?;
        Ok(self.version_dir(version).join(&entry.path))
    }

    /// Copy the build in `source` (with its manifest) into `versions/`.
    ///
    /// Verified before copying and again after, then renamed into place from a
    /// partial directory, so a half-copied build never looks installed.
    /// Installing a version that is already there and intact is a no-op.
    pub fn install(&self, source: &Path) -> Result<String, UpdateError> {
        let manifest = Manifest::read(source)?;
        check_version(&manifest.version)?;
        manifest.verify(source)?;
        let target = self.version_dir(&manifest.version);
        if target.is_dir() && manifest.verify(&target).is_ok() {
            return Ok(manifest.version);
        }
        let partial = self.partial_dir(&manifest.version)?;
        for component in manifest.components.values() {
            let from = source.join(&component.path);
            let to = partial.join(&component.path);
            fs::copy(&from, &to).map_err(|e| UpdateError::io(&to, e))?;
            make_executable(&to)?;
        }
        self.finish(&manifest, &partial)
    }

    /// Build `manifest.version` by patching the installed `from` version with
    /// the zstd deltas in `patches`: the launcher's delta mode, landing in a new
    /// version directory instead of over the running files.
    pub fn install_delta(
        &self,
        patches: &Path,
        manifest: &Manifest,
        from: &str,
    ) -> Result<String, UpdateError> {
        check_version(&manifest.version)?;
        let base = self.manifest(from)?;
        let group = manifest.deltas.get(from).ok_or_else(|| {
            UpdateError::Refused(format!("the build carries no deltas from {from}"))
        })?;
        let partial = self.partial_dir(&manifest.version)?;
        for (name, component) in &manifest.components {
            let delta = group
                .get(name)
                .ok_or_else(|| UpdateError::Refused(format!("no delta for {name} from {from}")))?;
            let installed = base
                .components
                .get(name)
                .ok_or_else(|| UpdateError::Refused(format!("{from} has no {name} to patch")))?;
            let out = partial.join(&component.path);
            apply_delta(
                &self.version_dir(from).join(&installed.path),
                patches,
                delta,
                &out,
            )
            .map_err(|e| match e {
                UpdateError::HashMismatch {
                    what,
                    expected,
                    actual,
                } => UpdateError::HashMismatch {
                    what: format!("{what} of {name}"),
                    expected,
                    actual,
                },
                other => other,
            })?;
            make_executable(&out)?;
        }
        self.finish(manifest, &partial)
    }

    fn partial_dir(&self, version: &str) -> Result<PathBuf, UpdateError> {
        let partial = self.root.join(VERSIONS).join(format!(".{version}.partial"));
        let _ = fs::remove_dir_all(&partial);
        fs::create_dir_all(&partial).map_err(|e| UpdateError::io(&partial, e))?;
        Ok(partial)
    }

    fn finish(&self, manifest: &Manifest, partial: &Path) -> Result<String, UpdateError> {
        manifest.verify(partial)?;
        let mut stripped = manifest.clone();
        stripped.deltas.clear();
        stripped.write(partial)?;
        let target = self.version_dir(&manifest.version);
        let _ = fs::remove_dir_all(&target);
        fs::rename(partial, &target).map_err(|e| UpdateError::io(&target, e))?;
        Ok(manifest.version.clone())
    }

    /// Make `version` current, remembering the one it replaces as `previous`.
    pub fn activate(&self, version: &str) -> Result<(), UpdateError> {
        if !self.version_dir(version).is_dir() {
            return Err(UpdateError::NotInstalled(version.to_owned()));
        }
        if let Some(current) = self.current() {
            if current != version {
                self.write_pointer(PREVIOUS, &current)?;
            }
        }
        self.write_pointer(CURRENT, version)
    }

    /// Point `current` back at `previous`, and `previous` at what was current.
    pub fn roll_back(&self) -> Result<String, UpdateError> {
        let previous = self.previous().ok_or_else(|| {
            UpdateError::Refused("there is no previous version to go back to".into())
        })?;
        self.activate(&previous)?;
        Ok(previous)
    }

    /// Copy current's executables to the root under their own names.
    ///
    /// Through a temporary name and a rename, so a process starting the stable
    /// copy at that moment gets the old file or the new one. A stable copy that
    /// is running cannot be replaced on Windows; that is reported, and the next
    /// update refreshes it — handover still reaches the current version.
    pub fn refresh_stable_copies(&self) -> Vec<(PathBuf, UpdateError)> {
        let Some(version) = self.current() else {
            return Vec::new();
        };
        let Ok(manifest) = self.manifest(&version) else {
            return Vec::new();
        };
        let mut failed = Vec::new();
        for component in manifest.components.values() {
            let from = self.version_dir(&version).join(&component.path);
            let to = self.root.join(&component.path);
            let temp = self.root.join(format!(".{}.tmp", component.path));
            let result = fs::copy(&from, &temp)
                .map_err(|e| UpdateError::io(&temp, e))
                .and_then(|_| make_executable(&temp))
                .and_then(|()| fs::rename(&temp, &to).map_err(|e| UpdateError::io(&to, e)));
            if let Err(e) = result {
                let _ = fs::remove_file(&temp);
                failed.push((to, e));
            }
        }
        failed
    }

    /// Delete all but the newest `keep` versions, never `current`, `previous`
    /// or anything in `protect`. Returns what was removed; a directory that
    /// cannot be removed (a running executable on Windows) is simply kept.
    pub fn prune(&self, keep: usize, protect: &[String]) -> Vec<String> {
        let spared: Vec<String> = [self.current(), self.previous()]
            .into_iter()
            .flatten()
            .chain(protect.iter().cloned())
            .collect();
        self.installed()
            .into_iter()
            .skip(keep)
            .filter(|v| !spared.contains(v))
            .filter(|v| fs::remove_dir_all(self.version_dir(v)).is_ok())
            .collect()
    }

    /// The version whose directory holds `exe`, if it is one of ours.
    pub fn version_of(&self, exe: &Path) -> Option<String> {
        let versions = self.root.join(VERSIONS);
        let rel = exe.strip_prefix(&versions).ok()?;
        let name = rel.components().next()?.as_os_str().to_str()?;
        valid_version(name).then(|| name.to_owned())
    }
}

/// The path to register for `component` (autostart, a shortcut): the stable
/// copy at the root, when there is an install with one.
pub fn stable_executable(component: &str) -> Option<PathBuf> {
    let layout = InstallLayout::for_user().ok()?;
    let manifest = layout.manifest(&layout.current()?).ok()?;
    let path = layout
        .root()
        .join(&manifest.components.get(component)?.path);
    path.is_file().then_some(path)
}

/// Started as a stable copy, the path to hand over to: current's `component`.
///
/// `None` means "run as you are" — not started from the root, no current
/// version, the component missing, or already handed over once. Development
/// builds in `target/` are never under the root, so they always run as they are.
pub fn handover(component: &str) -> Option<PathBuf> {
    if std::env::var_os(HANDED_OVER_ENV).is_some() {
        return None;
    }
    let layout = InstallLayout::for_user().ok()?;
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let same_dir = match (dir.canonicalize(), layout.root().canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same_dir {
        return None;
    }
    let target = layout.executable(&layout.current()?, component).ok()?;
    target.is_file().then_some(target)
}

/// What an installed node restarting itself should run: current's `component`.
///
/// A restart is how a node picks up a version made current while it ran —
/// the app says so when the two differ. Re-running its own executable instead
/// started the very version it was leaving, from under `versions/`, however
/// many times the user pressed restart. `None` for a node not running from
/// the install (a development build restarts as itself) or when current has
/// no such component.
pub fn restart_target(component: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    InstallLayout::for_user()
        .ok()?
        .restart_target(&exe, component)
}

impl InstallLayout {
    /// [`restart_target`] for a process running `exe`.
    fn restart_target(&self, exe: &Path, component: &str) -> Option<PathBuf> {
        self.version_of(exe)?;
        let target = self.executable(&self.current()?, component).ok()?;
        target.is_file().then_some(target)
    }
}

/// Run `target` with this process's arguments in this process's place.
///
/// Unix replaces the process image, so the PID a supervisor started is the one
/// that keeps running. Starting a child and exiting instead looks, to systemd,
/// like the service ending: it stops the unit and kills the child it just
/// gained, so a node started through the stable copy never came up. Windows
/// has no exec; there a detached child is started and the caller exits.
pub fn hand_over(target: &Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new(target);
    command
        .args(std::env::args_os().skip(1))
        .env(HANDED_OVER_ENV, "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
    }
    #[cfg(not(unix))]
    {
        super::apply::detach(&mut command);
        command.spawn().map(drop)
    }
}

fn platform_root() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Cursio"))
    }
    #[cfg(target_os = "macos")]
    {
        directories::BaseDirs::new()
            .map(|d| d.home_dir().join("Library/Application Support/Cursio"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        directories::BaseDirs::new().map(|d| d.home_dir().join(".local/lib/cursio"))
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        None
    }
}

/// A version is a directory name, so it must not be able to name anything
/// else: no separators, no `..`, nothing a shell or Windows would reinterpret.
pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 96
        && !v.starts_with('.')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

fn check_version(v: &str) -> Result<(), UpdateError> {
    match valid_version(v) {
        true => Ok(()),
        false => Err(UpdateError::BadVersion(v.to_owned())),
    }
}

/// Apply a `zstd --patch-from` delta: decode the patch with `base` as the
/// dictionary, then check the patch before and the result after.
fn apply_delta(base: &Path, patches: &Path, delta: &Delta, out: &Path) -> Result<(), UpdateError> {
    let patch = patches.join(&delta.patch_path);
    let actual = sha256_file(&patch)?;
    if !actual.eq_ignore_ascii_case(delta.patch_sha256.trim()) {
        return Err(UpdateError::HashMismatch {
            what: "the patch".into(),
            expected: delta.patch_sha256.trim().to_owned(),
            actual,
        });
    }
    let base_bytes = fs::read(base).map_err(|e| UpdateError::io(base, e))?;
    let patch_file = fs::File::open(&patch).map_err(|e| UpdateError::io(&patch, e))?;
    let io = |e| UpdateError::io(&patch, e);
    let mut decoder =
        zstd::stream::Decoder::with_dictionary(std::io::BufReader::new(patch_file), &base_bytes)
            .map_err(io)?;
    // The reference file may span the whole window; allow the maximum.
    decoder.window_log_max(31).map_err(io)?;
    let mut out_file = fs::File::create(out).map_err(|e| UpdateError::io(out, e))?;
    std::io::copy(&mut decoder, &mut out_file).map_err(|e| UpdateError::io(out, e))?;
    let result = sha256_file(out)?;
    if !result.eq_ignore_ascii_case(delta.result_sha256.trim()) {
        return Err(UpdateError::HashMismatch {
            what: "the patched result".into(),
            expected: delta.result_sha256.trim().to_owned(),
            actual: result,
        });
    }
    Ok(())
}

fn make_executable(path: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|e| UpdateError::io(path, e))?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cursio-layout-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A staged build of `version` whose node holds `body`.
    fn stage(root: &Path, version: &str, body: &str) -> PathBuf {
        let dir = root.join(format!("stage-{version}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cursio"), body).unwrap();
        fs::write(dir.join("cursio-app"), format!("app {body}")).unwrap();
        Manifest::describe(&dir, version, &["cursio", "cursio-app"])
            .unwrap()
            .write(&dir)
            .unwrap();
        dir
    }

    #[test]
    fn an_installed_build_lands_in_its_version_directory_intact() {
        let base = scratch("install");
        let layout = InstallLayout::at(base.join("root"));
        let version = layout.install(&stage(&base, "1.0+a", "one")).unwrap();
        assert_eq!(version, "1.0+a");
        let exe = layout.executable("1.0+a", "cursio").unwrap();
        assert_eq!(fs::read_to_string(exe).unwrap(), "one");
        assert_eq!(layout.installed(), vec!["1.0+a".to_owned()]);
        assert!(!layout.root().join("versions/.1.0+a.partial").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_tampered_staged_build_is_refused_before_anything_is_copied() {
        let base = scratch("tamper");
        let layout = InstallLayout::at(base.join("root"));
        let dir = stage(&base, "1.0+a", "one");
        fs::write(dir.join("cursio"), "evil").unwrap();
        assert!(matches!(
            layout.install(&dir),
            Err(UpdateError::HashMismatch { .. })
        ));
        assert!(layout.installed().is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn activating_remembers_the_replaced_version_and_rollback_swaps_them() {
        let base = scratch("activate");
        let layout = InstallLayout::at(base.join("root"));
        layout.install(&stage(&base, "1", "one")).unwrap();
        layout.install(&stage(&base, "2", "two")).unwrap();
        layout.activate("1").unwrap();
        assert_eq!(layout.previous(), None);
        layout.activate("2").unwrap();
        assert_eq!(layout.current().as_deref(), Some("2"));
        assert_eq!(layout.previous().as_deref(), Some("1"));
        assert_eq!(layout.roll_back().unwrap(), "1");
        assert_eq!(layout.current().as_deref(), Some("1"));
        assert_eq!(layout.previous().as_deref(), Some("2"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_pointer_naming_a_missing_version_reads_as_none() {
        let base = scratch("dangling");
        let layout = InstallLayout::at(&base);
        fs::write(base.join(CURRENT), "gone").unwrap();
        assert_eq!(layout.current(), None);
        fs::write(base.join(CURRENT), "../etc").unwrap();
        assert_eq!(layout.current(), None);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn version_names_that_could_escape_the_versions_directory_are_refused() {
        for bad in ["", "..", "../x", "a/b", "a\\b", ".hidden", "a b", "c:"] {
            assert!(!valid_version(bad), "{bad:?}");
        }
        for good in [
            "0.1.0",
            "0.1.0+c9798e7",
            "0.1.0+c9798e7.dirty-01234567",
            "dev_1",
        ] {
            assert!(valid_version(good), "{good:?}");
        }
    }

    #[test]
    fn prune_keeps_the_newest_and_never_current_previous_or_protected() {
        let base = scratch("prune");
        let layout = InstallLayout::at(base.join("root"));
        for v in ["1", "2", "3", "4", "5"] {
            layout.install(&stage(&base, v, v)).unwrap();
            // Distinct install times, so "newest" is not a coin toss.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        layout.activate("1").unwrap();
        layout.activate("2").unwrap();
        let removed = layout.prune(1, &["3".to_owned()]);
        assert_eq!(removed, vec!["4".to_owned()]);
        let mut left = layout.installed();
        left.sort();
        assert_eq!(left, ["1", "2", "3", "5"]);
        fs::remove_dir_all(&base).unwrap();
    }

    /// The app tells the user to restart when current moved under a running
    /// node; restarting has to land on current, not on the version it left.
    #[test]
    fn an_installed_node_restarts_into_current_not_itself() {
        let base = scratch("restart");
        let layout = InstallLayout::at(base.join("root"));
        layout.install(&stage(&base, "1", "one")).unwrap();
        layout.install(&stage(&base, "2", "two")).unwrap();
        layout.activate("1").unwrap();
        let running = layout.executable("1", "cursio").unwrap();
        layout.activate("2").unwrap();
        let target = layout.restart_target(&running, "cursio").unwrap();
        assert_eq!(fs::read_to_string(target).unwrap(), "two");
        // A build outside the install restarts as itself.
        assert!(layout
            .restart_target(&base.join("target/release/cursio"), "cursio")
            .is_none());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn stable_copies_follow_current() {
        let base = scratch("stable");
        let layout = InstallLayout::at(base.join("root"));
        layout.install(&stage(&base, "1", "one")).unwrap();
        layout.install(&stage(&base, "2", "two")).unwrap();
        layout.activate("1").unwrap();
        assert!(layout.refresh_stable_copies().is_empty());
        assert_eq!(
            fs::read_to_string(layout.root().join("cursio")).unwrap(),
            "one"
        );
        layout.activate("2").unwrap();
        assert!(layout.refresh_stable_copies().is_empty());
        assert_eq!(
            fs::read_to_string(layout.root().join("cursio")).unwrap(),
            "two"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_delta_rebuilds_the_new_version_from_the_installed_one() {
        let base = scratch("delta");
        let layout = InstallLayout::at(base.join("root"));
        layout
            .install(&stage(&base, "1", "hello world, version one"))
            .unwrap();

        let new_dir = stage(&base, "2", "hello world, version two");
        let mut manifest = Manifest::read(&new_dir).unwrap();
        let patches = base.join("patches");
        fs::create_dir_all(&patches).unwrap();
        let mut group = std::collections::BTreeMap::new();
        for (name, component) in &manifest.components {
            let old = fs::read(layout.executable("1", name).unwrap()).unwrap();
            let new = fs::read(new_dir.join(&component.path)).unwrap();
            let mut encoder = zstd::stream::Encoder::with_dictionary(Vec::new(), 3, &old).unwrap();
            std::io::Write::write_all(&mut encoder, &new).unwrap();
            let patch_path = format!("{name}.zst");
            fs::write(patches.join(&patch_path), encoder.finish().unwrap()).unwrap();
            group.insert(
                name.clone(),
                Delta {
                    patch_sha256: sha256_file(&patches.join(&patch_path)).unwrap(),
                    patch_path,
                    result_sha256: component.sha256.clone(),
                },
            );
        }
        manifest.deltas.insert("1".into(), group);

        assert_eq!(layout.install_delta(&patches, &manifest, "1").unwrap(), "2");
        let exe = layout.executable("2", "cursio").unwrap();
        assert_eq!(fs::read_to_string(exe).unwrap(), "hello world, version two");
        assert!(Manifest::read(&layout.version_dir("2"))
            .unwrap()
            .deltas
            .is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_path_inside_a_version_directory_names_that_version() {
        let layout = InstallLayout::at("/r");
        assert_eq!(
            layout
                .version_of(Path::new("/r/versions/1.0+a/cursio"))
                .as_deref(),
            Some("1.0+a")
        );
        assert_eq!(layout.version_of(Path::new("/r/cursio")), None);
        assert_eq!(layout.version_of(Path::new("/elsewhere/cursio")), None);
    }
}
