//! Every path skot touches, and the one function allowed to delete something.
//!
//! The invariant is structural rather than guarded at each call site: no module
//! reads a path directly, every path is derived from a single [`Layout`] built from
//! the project root, and every deletion goes through [`Layout::guarded_remove`],
//! which refuses anything outside the known layout.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Directory name for all generated state.
pub const STATE_DIR: &str = ".skot";
/// Config file name, looked up when walking up from the starting directory.
pub const CONFIG_FILE: &str = "skot.toml";
/// The ordered series file.
pub const SERIES_FILE: &str = "series";
/// Default patch directory, relative to the project root.
pub const PATCHES_DIR: &str = "patches";

/// All paths skot may read or write, derived from one project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// Build a layout for an explicit root. Does not check that the root exists.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Walk up from `start` looking for `skot.toml`.
    pub fn discover(start: &Path) -> Result<Self> {
        let start = absolute(start);
        for dir in start.ancestors() {
            if dir.join(CONFIG_FILE).is_file() {
                return Ok(Self::new(dir));
            }
        }
        bail!(
            "no {CONFIG_FILE} found in {} or any parent directory.\n\
             Run `skot init` inside the project you want to manage patches for.",
            start.display()
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    /// Load `skot.toml`.
    ///
    /// The config is a property of the layout, not of the command running, so this lives
    /// here: anything that works from a directory alone (a fetch, a test) can read it
    /// without having to fake a `Ctx`.
    pub fn config(&self) -> Result<crate::config::Config> {
        let path = self.config_file();
        if !path.is_file() {
            bail!(
                "no {} here.\n\
                 Run `skot init` to create one.",
                path.display()
            );
        }
        crate::config::Config::load(&path)
    }

    /// `skot.toml` and the series together, which is what most commands want.
    ///
    /// A series file that does not exist yet parses as empty, so commands can run before the
    /// first patch exists.
    pub fn project(&self) -> Result<(crate::config::Config, crate::series::Series)> {
        let config = self.config()?;
        let series_path = self.series_file();
        let series = if series_path.is_file() {
            crate::series::Series::load(&series_path)?
        } else {
            crate::series::Series::parse(&series_path, "")
        };
        Ok((config, series))
    }

    pub fn series_file(&self) -> PathBuf {
        self.root.join(SERIES_FILE)
    }

    pub fn patches_dir(&self) -> PathBuf {
        self.root.join(PATCHES_DIR)
    }

    /// Root of all generated state.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    pub fn mirror_dir(&self) -> PathBuf {
        self.state_dir().join("mirror.git")
    }

    pub fn sandbox_dir(&self) -> PathBuf {
        self.state_dir().join("sandbox")
    }

    /// Scratch that `clean build` does not remove.
    pub fn out_dir(&self) -> PathBuf {
        self.state_dir().join("out")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.state_dir().join("lock")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir().join("state.json")
    }

    /// Create the state directories. Safe to call repeatedly.
    pub fn ensure_state_dirs(&self) -> Result<()> {
        for dir in [self.state_dir(), self.out_dir()] {
            std::fs::create_dir_all(&dir).context(format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    /// The only way skot deletes anything.
    ///
    /// Refuses any path that is not inside the state directory, the patch directory,
    /// or the out directory. In particular it refuses the project root itself, so no
    /// combination of layout values can turn into a delete of the user's checkout.
    pub fn guarded_remove(&self, path: &Path) -> Result<bool> {
        let target = normalise(path);
        let allowed: Vec<PathBuf> = [self.state_dir(), self.patches_dir(), self.out_dir()]
            .iter()
            .map(|p| normalise(p))
            .collect();

        if !allowed.iter().any(|base| target.starts_with(base)) {
            bail!(
                "refusing to remove {}: not inside the layout.\n\
                 skot only deletes within {}, {} or {}.",
                target.display(),
                STATE_DIR,
                PATCHES_DIR,
                out_display(self),
            );
        }

        if !target.exists() {
            return Ok(false);
        }
        let meta = std::fs::symlink_metadata(&target)
            .context(format!("inspecting {}", target.display()))?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&target)
                .context(format!("removing directory {}", target.display()))?;
        } else {
            std::fs::remove_file(&target)
                .context(format!("removing file {}", target.display()))?;
        }
        Ok(true)
    }

    /// Take the advisory lock guarding the sandbox.
    ///
    /// This is an OS file lock (`flock` on Unix, `LockFileEx` on Windows) rather than a lock
    /// file whose existence *is* the lock, acquired through `fs4` because std's
    /// `File::try_lock` is not implemented on every target.
    ///
    /// The retry window exists so a concurrent run surfaces as a message naming the lock rather
    /// than as a hang or a spurious failure.
    pub fn lock(&self) -> Result<SandboxLock> {
        self.ensure_state_dirs()?;
        let resource = self.lock_file();
        // `truncate(false)` matters: truncating a file another process has locked would not
        // break its lock, but it would still destroy state a running skot owns.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&resource)
            .context(format!("opening the sandbox lock at {}", resource.display()))?;

        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            // UFCS so the trait method wins over the inherent `File::try_lock`.
            match fs4::FileExt::try_lock(&file) {
                Ok(()) => return Ok(SandboxLock { _file: file }),
                Err(fs4::TryLockError::WouldBlock) => {}
                // A real I/O failure, as opposed to contention. Surfaced immediately: retrying
                // a permission error for three quarters of a second would only delay it.
                Err(fs4::TryLockError::Error(e)) => {
                    return Err(e)
                        .context(format!("locking the sandbox at {}", resource.display()));
                }
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "could not take the sandbox lock at {} within {}ms.\n\
                     Another skot run is probably active. If you are sure it is not, \
                     wait for it to finish and try again.",
                    resource.display(),
                    LOCK_WAIT.as_millis()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Human-readable summary of what exists, for `status`.
    pub fn inventory(&self) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut push = |name: &str, path: PathBuf, _kind: &'static str| {
            let present = path.exists();
            out.push((format!("{name} ({})", path.display()), present));
        };
        push("config", self.config_file(), "");
        push("series", self.series_file(), "");
        push("patches", self.patches_dir(), "");
        push("mirror", self.mirror_dir(), "");
        push("sandbox", self.sandbox_dir(), "");
        push("out", self.out_dir(), "");
        out
    }
}

fn out_display(layout: &Layout) -> String {
    layout.out_dir().display().to_string()
}

/// How long [`Layout::lock`] waits for a concurrent run to finish before giving up.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(750);

/// Held for the duration of a command. Releases the lock on drop.
///
/// Not a manual `Drop`: the lock is released when the `File` closes, which dropping the guard
/// does. Holding the handle *is* the mechanism, so a destructor would have nothing to add — and
/// an early return or a panic releases it correctly without being written twice.
#[derive(Debug)]
pub struct SandboxLock {
    /// The locked handle. Never read; the lock is released when this is dropped.
    _file: std::fs::File,
}

/// Make `path` absolute without requiring it to exist.
///
/// `std::path::absolute` would do, but it resolves relative to the process cwd and
/// does not normalise `..` on all platforms; doing it ourselves keeps the guard in
/// `guarded_remove` predictable.
pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalise(path)
    } else {
        match std::env::current_dir() {
            Ok(cwd) => normalise(&cwd.join(path)),
            Err(_) => normalise(path),
        }
    }
}

/// Lexically remove `.` and `..` components. Does not touch the filesystem, so it
/// works for paths that do not exist.
pub fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Only pop a real name segment. A leading `..` on a relative path,
                // or a `..` at the root of an absolute path, has to be kept.
                match out.components().next_back() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    _ => out.push(c),
                }
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(Component::CurDir);
    }
    out
}

/// Recursive size in bytes, for `status`. Returns 0 for a missing path.
pub fn dir_size(path: &Path) -> u64 {
    fn walk(dir: &Path, acc: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), acc);
            } else {
                *acc += meta.len();
            }
        }
    }
    let mut acc = 0;
    if path.is_dir() {
        walk(path, &mut acc);
    }
    acc
}

/// Format a byte count the way `status` and `clean` report it.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// Convert an `OsString` path argument into a `PathBuf`, for CLI plumbing.
pub fn path_from_os(s: OsString) -> PathBuf {
    PathBuf::from(s)
}

/// Print the layout inventory as a report. Used by `status`.
pub fn print_inventory(layout: &Layout) {
    crate::report::section("layout");
    for (name, present) in layout.inventory() {
        if present {
            crate::report::field(&name, &crate::report::ok("present"));
        } else {
            crate::report::field(&name, &crate::report::muted("absent"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        (tmp, layout)
    }

    #[test]
    fn every_path_is_under_the_root() {
        let (_tmp, l) = layout();
        for p in [
            l.config_file(),
            l.series_file(),
            l.patches_dir(),
            l.state_dir(),
            l.mirror_dir(),
            l.sandbox_dir(),
            l.out_dir(),
            l.lock_file(),
            l.state_file(),
        ] {
            assert!(p.starts_with(l.root()), "{} escaped the root", p.display());
        }
    }

    #[test]
    fn guarded_remove_deletes_inside_the_state_dir() {
        let (tmp, l) = layout();
        let victim = l.state_dir().join("sandbox");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("f"), b"x").unwrap();

        assert!(l.guarded_remove(&victim).unwrap());
        assert!(!victim.exists());
        drop(tmp);
    }

    #[test]
    fn guarded_remove_is_idempotent_on_a_missing_path() {
        let (_tmp, l) = layout();
        assert!(!l.guarded_remove(&l.state_dir().join("nope")).unwrap());
    }

    #[test]
    fn guarded_remove_refuses_the_project_root() {
        let (tmp, l) = layout();
        std::fs::write(tmp.path().join("important"), b"do not delete").unwrap();

        let err = l.guarded_remove(l.root()).unwrap_err().to_string();
        assert!(err.contains("refusing"), "unexpected error: {err}");
        assert!(
            tmp.path().join("important").exists(),
            "the root must survive"
        );
    }

    #[test]
    fn guarded_remove_refuses_paths_outside_the_root() {
        let (_tmp, l) = layout();
        for outside in [
            Path::new("/"),
            Path::new("/tmp"),
            Path::new("../escape"),
            Path::new(".."),
        ] {
            let err = l.guarded_remove(outside).unwrap_err().to_string();
            assert!(
                err.contains("refusing"),
                "{outside:?} was not refused: {err}"
            );
        }
    }

    #[test]
    fn guarded_remove_refuses_a_sibling_with_a_matching_prefix() {
        // `.skot-backup` starts with the same characters as `.skot` but is a
        // different directory. A string prefix check would delete the wrong thing.
        let (_tmp, l) = layout();
        let decoy = l.root().join(format!("{STATE_DIR}-backup"));
        std::fs::create_dir_all(&decoy).unwrap();

        assert!(l.guarded_remove(&decoy).is_err());
        assert!(decoy.exists(), "a prefix-matching sibling must survive");
    }

    #[test]
    fn normalise_removes_dot_and_parent_segments() {
        assert_eq!(normalise(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalise(Path::new("/a/b/../..")), PathBuf::from("/"));
        // A relative path may legitimately start with `..`.
        assert_eq!(normalise(Path::new("../x")), PathBuf::from("../x"));
    }

    #[test]
    fn discover_walks_up_to_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(CONFIG_FILE), "").unwrap();
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let found = Layout::discover(&nested).unwrap();
        assert_eq!(normalise(found.root()), normalise(root));
    }

    #[test]
    fn discover_explains_itself_when_there_is_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Layout::discover(tmp.path()).unwrap_err().to_string();
        assert!(err.contains(CONFIG_FILE));
        assert!(err.contains("skot init"));
    }

    #[test]
    fn the_lock_is_released_on_drop() {
        let (_tmp, l) = layout();
        {
            let _guard = l.lock().unwrap();
            // A second attempt while held must fail.
            assert!(l.lock().is_err());
        }
        // Once dropped, it is available again.
        assert!(l.lock().is_ok());
    }

    #[test]
    fn human_bytes_is_readable() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1536), "1.5K");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0M");
    }
}
