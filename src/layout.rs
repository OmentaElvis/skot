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
/// The ordered series file, inside each series directory.
pub const SERIES_FILE: &str = "series";
/// The file recording the ref a series sits on, next to its `series` file.
pub const BASE_FILE: &str = "base";
/// Patch directory, relative to the project root: holds one subdirectory per series.
pub const PATCHES_DIR: &str = "patches";
/// The blob store, relative to the project root: binary file content, named by the
/// sha256 of the bytes, that patch headers reference. It sits beside `patches/` rather
/// than inside it (every subdirectory there means a series) and outside `.skot/` (the
/// content is the user's own commits and cannot be re-fetched, so it must not share the
/// fate of the state directory a checkout might ignore wholesale).
pub const BLOBS_DIR: &str = "blobs";
/// The series a project starts with, and the one used when no pointer is recorded.
pub const DEFAULT_SERIES: &str = "default";
/// The one slot a shared sandbox lives in, under `sandbox/`.
///
/// Starts with `@`, which a series name can never contain, so a shared sandbox and a
/// per-series one can never claim the same path.
pub const SHARED_SANDBOX: &str = "@shared";
/// Where a `--repo` URL's objects live: one tracked store per source, under `.skot/`.
pub const SOURCES_DIR: &str = "sources";

/// All paths skot may read or write, derived from one project root.
///
/// The active series is part of the layout rather than an argument every path function
/// takes: command code asks once (through [`crate::commands::Ctx`]) and never spells a
/// series name itself, so `patches/<series>/` cannot leak in from two directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
    /// The series this layout operates on. Validated by [`Layout::with_series`].
    series: String,
    /// Whether each series owns its sandbox worktree (`sandbox.per-series` in the config).
    per_series_sandbox: bool,
    /// Whether this run refreshes the fetch path even when the mirror has the ref
    /// (`--refresh`). On the layout, like the series, so every work function sees it
    /// without each command threading a flag through to the fetch.
    refresh: bool,
    /// Whether this run refuses the network (`--offline`). See [`Layout::offline`].
    offline: bool,
}

impl Layout {
    /// Build a layout for an explicit root, operating on the default series with a
    /// shared sandbox. Does not check that the root exists.
    ///
    /// Commands get their real series from [`crate::commands::Ctx::discover`], which
    /// resolves `--series`, the recorded pointer, and the configured sandbox scope;
    /// everything else (tests, fetches, anything working from a directory alone) means
    /// the default series in shared-sandbox mode, which is what this builds.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            series: DEFAULT_SERIES.to_owned(),
            per_series_sandbox: false,
            refresh: false,
            offline: false,
        }
    }

    /// The platform cache directory skot keeps standalone state in.
    ///
    /// `dirs` resolves `$XDG_CACHE_HOME` on Linux and the BSDs, `~/Library/Caches` on
    /// macOS, and `%LOCALAPPDATA%` on Windows. A platform that cannot name one falls back
    /// to the temporary directory, which is at least writable and disposable.
    pub fn cache_root() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("skot")
    }

    /// A layout for a standalone import, outside any project.
    ///
    /// The mirror and the `--repo` URL stores live under the cache directory rather than
    /// a project's `.skot/`, so sourcing a patch does not need a workspace or leave one
    /// behind in the directory the command was run from.
    pub fn standalone() -> Self {
        Self::new(Self::cache_root().join("standalone"))
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
             Run `{}` inside the project you want to manage patches for.",
            start.display(),
            crate::invocation::skot("init")
        )
    }

    /// The same layout, operating on `series`.
    ///
    /// Validates the name: it becomes a directory under `patches/` and part of worktree
    /// registration names in the mirror, so a name that is not a single path component,
    /// or that git would refuse as a ref, is rejected here rather than at the first
    /// filesystem call that happens to notice.
    pub fn with_series(&self, series: &str) -> Result<Self> {
        validate_series_name(series)?;
        Ok(Self {
            series: series.to_owned(),
            ..self.clone()
        })
    }

    /// The same layout, with the sandbox scope from the config.
    pub fn with_per_series_sandbox(&self, per_series: bool) -> Self {
        Self {
            per_series_sandbox: per_series,
            ..self.clone()
        }
    }

    /// The same layout, asking this run to refresh the fetch path (`--refresh`).
    pub fn with_refresh(&self, refresh: bool) -> Self {
        Self {
            refresh,
            ..self.clone()
        }
    }

    /// The same layout, refusing this run the network (`--offline`).
    ///
    /// The mirror already on disk keeps answering, and a fetch from a local directory is
    /// still allowed; anything a remote transport would carry is refused at the one place
    /// skot touches the network, before anything connects or is written.
    pub fn with_offline(&self, offline: bool) -> Self {
        Self {
            offline,
            ..self.clone()
        }
    }

    /// Whether the fetch path refreshes even when the mirror already has the ref.
    pub fn refresh(&self) -> bool {
        self.refresh
    }

    /// Whether this run refuses the network. See [`Layout::with_offline`].
    pub fn offline(&self) -> bool {
        self.offline
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The active series this layout operates on.
    pub fn series(&self) -> &str {
        &self.series
    }

    /// Whether each series has its own sandbox worktree rather than sharing one.
    pub fn per_series_sandbox(&self) -> bool {
        self.per_series_sandbox
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
                 Run `{}` to create one.",
                path.display(),
                crate::invocation::skot("init")
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

    /// The active series' directory: its `series` file, its `base`, and its patches.
    ///
    /// Kept under its old name because this is where every patch file read or written
    /// goes; the series framing is [`Layout::series_root`] and [`Layout::series_file`].
    pub fn patches_dir(&self) -> PathBuf {
        self.series_root().join(&self.series)
    }

    /// The directory holding one subdirectory per series.
    pub fn series_root(&self) -> PathBuf {
        self.root.join(PATCHES_DIR)
    }

    /// Where stored binary content lives: one file per distinct content, named by hash.
    ///
    /// Shared by every series, so a blob two patches reference is written and kept once.
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join(BLOBS_DIR)
    }

    /// The series that exist on disk, sorted.
    ///
    /// Every directory under the series root is one: nothing skot writes there is not
    /// a series, so a name that looks unusual is a series that looks unusual.
    pub fn series_names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.series_root()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The active series' ordered list of patch filenames.
    pub fn series_file(&self) -> PathBuf {
        self.patches_dir().join(SERIES_FILE)
    }

    /// The active series' base: the ref it sits on, and (since rebase recorded it) the
    /// commit that ref pointed at, on the same line.
    ///
    /// Absent until something records one (`series create`, `series copy`, `rebase`), which
    /// is why every ref chain treats it as a fallback rather than a requirement.
    pub fn base_file(&self) -> PathBuf {
        self.patches_dir().join(BASE_FILE)
    }

    /// The ref recorded in [`Layout::base_file`], if there is one.
    ///
    /// A missing file and an empty one both mean "nothing recorded": the file appears
    /// when something establishes a base, and the chain above it (the configured default)
    /// is what a series that has never been based anywhere falls back to.
    pub fn base_ref(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.base_file()).ok()?;
        // The first word: `rebase` records the ref and the commit it moved from, and
        // only the ref answers "what is this series based on".
        text.split_whitespace().next().map(str::to_owned)
    }

    /// Record what this series sits on: the ref, and the commit it pointed at when it
    /// was recorded (when known).
    ///
    /// Both halves fit on the one line the file has always held, so a base written
    /// before the sha was recorded still reads the same way everywhere.
    pub fn set_base(&self, ref_name: &str, sha: Option<&str>) -> Result<()> {
        std::fs::write(self.base_file(), Self::base_text(ref_name, sha))
            .context(format!("writing {}", self.base_file().display()))
    }

    /// The text [`Layout::set_base`] writes, as a value rather than a write.
    ///
    /// A series directory is assembled in full before it is renamed into place, so the
    /// base has to be computable without touching the live file.
    pub fn base_text(ref_name: &str, sha: Option<&str>) -> String {
        match sha {
            Some(sha) => format!("{ref_name} {sha}\n"),
            None => format!("{ref_name}\n"),
        }
    }

    /// Root of all generated state.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    /// Where a series directory is assembled before a rename puts it under `patches/`.
    ///
    /// Scratch, and the only place a half-built series can exist. Cleared by the next
    /// atomic operation and by nothing else, because a leftover here is either a build
    /// that never finished or a swap that was interrupted, and
    /// [`crate::seriesdir::recover`] puts the second kind back before anything else runs.
    pub fn tmp_dir(&self) -> PathBuf {
        self.state_dir().join("tmp")
    }

    /// The version of a series that the last successful rebase replaced.
    ///
    /// One entry per series, overwritten rather than accumulated: this is a recovery
    /// buffer for an accidental rebase, not a history. Nothing lists it and nothing
    /// except [`Self::undo_entry`] reads it, so it costs one small directory and
    /// disappears with `.skot`.
    pub fn undo_dir(&self) -> PathBuf {
        self.state_dir().join("undo")
    }

    /// This series' undo entry, if one is being held.
    pub fn undo_entry(&self) -> PathBuf {
        self.undo_dir().join(&self.series)
    }

    pub fn mirror_dir(&self) -> PathBuf {
        self.state_dir().join("mirror.git")
    }

    /// The tracked stores skot keeps for `--repo` sources that are URLs, one bare
    /// repository each, keyed by where they came from. The mirror's own directory is
    /// never used for these: a source the user named is its own universe, with its own
    /// history, and keeps it out of what `sync` and `check` read.
    pub fn sources_dir(&self) -> PathBuf {
        self.state_dir().join(SOURCES_DIR)
    }

    /// One source's store, `key` being a stable digest of the location as given
    /// (see `source_key` in `commands::import`).
    pub fn source_store(&self, key: &str) -> PathBuf {
        self.sources_dir().join(key)
    }

    /// The active series' sandbox working tree.
    ///
    /// One worktree per series under `.skot/sandbox/<series>`, whatever the mode; the
    /// difference is only how many are ever *registered at once*. `sandbox.per-series =
    /// false` keeps a single slot for the whole project, at `.skot/sandbox/@shared`, and
    /// a switch moves the one sandbox rather than opening another. The slot's name starts
    /// with `@`, which a series name can never contain, so the two can never collide.
    pub fn sandbox_dir(&self) -> PathBuf {
        if self.per_series_sandbox {
            self.sandbox_root().join(&self.series)
        } else {
            self.sandbox_root().join(SHARED_SANDBOX)
        }
    }

    /// The root every sandbox lives in: one directory per series, plus the shared slot
    /// when `sandbox.per-series` is off. `clean sandbox` frees this.
    pub fn sandbox_root(&self) -> PathBuf {
        self.state_dir().join("sandbox")
    }

    /// Scratch that `clean sandbox` does not remove.
    pub fn out_dir(&self) -> PathBuf {
        self.state_dir().join("out")
    }

    /// The authoring area: one worktree per patch that has a work area open, plus the
    /// manifest.
    ///
    /// Derived state like the sandbox, but never touched by a default clean: these
    /// directories hold edits that exist nowhere else until `save` writes them into a
    /// patch file, so removing them is a decision the user makes explicitly
    /// (`clean --sessions`) rather than a side effect of tidying.
    pub fn work_dir(&self) -> PathBuf {
        self.state_dir().join("work")
    }

    /// The worktree holding one patch's in-progress edits, in the active series.
    ///
    /// The argument is a directory name, not a patch name: an open session's is
    /// [`Session::dir_name`](crate::session::Session::dir_name), the patch's own name
    /// without the `.patch` its series entry carries (`sub/fix.patch` keeps its
    /// subdirectory), so what `list` shows and what a file manager opens are the same
    /// path. A name that would leave the work directory is refused rather than rewritten:
    /// a session found somewhere other than its own name would be invisible, and a
    /// manifest pointing outside `work/` would be a delete waiting to happen.
    pub fn work_patch_dir(&self, patch: &str) -> Result<PathBuf> {
        self.work_patch_dir_for(&self.series, patch)
    }

    /// [`Layout::work_patch_dir`] for a session in `series`, which is not necessarily the
    /// active one: `clean` and `reconcile` walk the manifest, and the manifest can hold
    /// open work areas in every series at once.
    pub fn work_patch_dir_for(&self, series: &str, patch: &str) -> Result<PathBuf> {
        let work = normalise(&self.work_dir());
        let series_dir = normalise(&work.join(series));
        let path = normalise(&series_dir.join(patch));
        // Both halves are checked: the series comes from a manifest that any corruption
        // can rewrite, and a patch name of `../x` escapes its series directory even when
        // it stays inside `work/`.
        if !series_dir.starts_with(&work) || !path.starts_with(&series_dir) {
            bail!(
                "`{series}/{patch}` cannot be a session directory: it points outside {}.",
                self.work_dir().display()
            );
        }
        Ok(path)
    }

    /// The manifest recording which patches have a session open, and on what base.
    pub fn sessions_file(&self) -> PathBuf {
        self.work_dir().join("sessions.json")
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
        let allowed: Vec<PathBuf> = [self.state_dir(), self.series_root(), self.out_dir()]
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
            std::fs::remove_file(&target).context(format!("removing file {}", target.display()))?;
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
            .context(format!(
                "opening the sandbox lock at {}",
                resource.display()
            ))?;

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
                     Another skot process is probably active. If you are sure it is not, \
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
        push("work", self.work_dir(), "");
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

/// Check a series name before it becomes a directory and a worktree registration.
///
/// Series names are user-chosen labels, so the rules are the ones any file name has to
/// obey on the platforms skot supports, plus the two git refuses in a ref name: a `..`
/// anywhere and a `.lock` ending. The latter matters because a sandbox is registered in
/// the mirror under a name derived from the series, and that registration is a branch
/// while it is being made. Two names commands or skot itself own are refused outright,
/// for the same reason: the same word would mean two things.
pub fn validate_series_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("a series name cannot be empty.");
    }
    if name == "." || name == ".." {
        bail!("`{name}` cannot be a series name: it names a directory position, not a series.");
    }
    if name.trim() != name {
        bail!("`{name}` cannot be a series name: it starts or ends with whitespace.");
    }
    if let Some(c) = name.chars().find(|c| c.is_control()) {
        bail!("`{name}` cannot be a series name: `{c:?}` is not a printable character.");
    }
    if let Some(c) = name.chars().find(|c| "<>:\"/\\|?*".contains(*c)) {
        bail!("`{name}` cannot be a series name: `{c}` is not allowed in a file name.");
    }
    if name.contains("..") {
        bail!("`{name}` cannot be a series name: it contains `..`.");
    }
    if name.ends_with(".lock") {
        bail!("`{name}` cannot be a series name: git reserves names ending in `.lock`.");
    }
    if name.eq_ignore_ascii_case(SHARED_SANDBOX) {
        bail!(
            "`{name}` cannot be a series name: `{SHARED_SANDBOX}` is the slot the shared \
             sandbox lives in, and a series of that name would claim the same path."
        );
    }
    if name.eq_ignore_ascii_case("all") || name.eq_ignore_ascii_case("current") {
        bail!(
            "`{name}` cannot be a series name: `all` and `current` are words commands use \
             for every series and the active one."
        );
    }
    Ok(())
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
            l.sessions_file(),
            l.work_patch_dir("a.patch").unwrap(),
            l.work_patch_dir("sub/b.patch").unwrap(),
        ] {
            assert!(p.starts_with(l.root()), "{} escaped the root", p.display());
        }
    }

    #[test]
    fn a_session_name_that_escapes_the_work_directory_is_refused() {
        // The manifest is read back and acted on, including by deletion: a name that
        // climbs out of `work/` has to be refused at the only place that forms the path.
        let (_tmp, l) = layout();
        for bad in ["..", "../x", "/etc/passwd", "a/../../x"] {
            assert!(
                l.work_patch_dir(bad).is_err(),
                "{bad:?} was allowed as a session directory"
            );
        }
        // A subdirectory inside `work/` is a normal patch name, not an escape.
        assert!(
            l.work_patch_dir("sub/b.patch")
                .unwrap()
                .starts_with(l.work_dir())
        );
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
        assert!(err.contains(&crate::invocation::skot("init")));
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

    #[test]
    fn a_series_layout_puts_every_series_path_under_its_directory() {
        let (_tmp, l) = layout();
        let foo = l.with_series("foo").unwrap();

        assert_eq!(foo.series(), "foo");
        assert_eq!(foo.series_root(), l.root().join("patches"));
        assert_eq!(
            foo.patches_dir(),
            l.root().join("patches").join("foo"),
            "patches_dir is the series directory"
        );
        assert_eq!(foo.series_file(), l.root().join("patches/foo/series"));
        assert_eq!(foo.base_file(), l.root().join("patches/foo/base"));
        // The default series is the layout every caller gets without asking.
        assert_eq!(l.series(), DEFAULT_SERIES);
        assert_eq!(l.patches_dir(), l.root().join("patches/default"));
    }

    #[test]
    fn series_names_that_are_not_single_safe_components_are_refused() {
        let (_tmp, l) = layout();
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a..b",
            "x.lock",
            " leading",
            "trailing ",
            "co:lon",
            "star*",
            "question?",
            "@shared",
            "@Shared",
            "all",
            "Current",
        ] {
            let err = l.with_series(bad).unwrap_err().to_string();
            assert!(!err.is_empty(), "{bad:?} was accepted as a series name");
        }
        // Names that are unusual but safe go through. `@` is a character like any
        // other: only the one name that is already the shared sandbox slot is taken.
        for good in [
            "default",
            "experiment-1",
            "v2.0",
            "plus+one",
            "long_descriptive",
            "exp@v1.4",
            "a@b",
        ] {
            l.with_series(good).unwrap();
        }
    }

    #[test]
    fn every_directory_under_the_series_root_is_a_series() {
        // Nothing skot writes lives beside a series any more, so a directory there is
        // one, however it is named.
        let (_tmp, l) = layout();
        std::fs::create_dir_all(l.series_root().join("alpha")).unwrap();
        std::fs::create_dir_all(l.series_root().join("exp")).unwrap();
        std::fs::create_dir_all(l.series_root().join("exp@v1.4")).unwrap();

        assert_eq!(
            l.series_names(),
            vec!["alpha".to_owned(), "exp".to_owned(), "exp@v1.4".to_owned()]
        );
    }

    #[test]
    fn the_sandbox_dir_follows_the_configured_scope() {
        let (_tmp, l) = layout();
        // Shared mode parks one slot beside the per-series trees, never on top of them.
        assert_eq!(
            l.sandbox_dir(),
            l.state_dir().join("sandbox").join(SHARED_SANDBOX)
        );
        assert_eq!(l.sandbox_root(), l.state_dir().join("sandbox"));

        let per = l.with_per_series_sandbox(true);
        assert_eq!(
            per.sandbox_dir(),
            per.state_dir().join("sandbox").join(DEFAULT_SERIES)
        );
        // The root does not move: `clean sandbox` frees every sandbox either way, and
        // the two scopes are siblings under it rather than nested.
        assert_eq!(per.sandbox_root(), l.sandbox_root());
        assert_ne!(per.sandbox_dir(), l.sandbox_dir());
    }

    #[test]
    fn a_session_in_another_series_is_found_under_its_own_directory() {
        // `clean` and `reconcile` walk sessions from every series while the layout
        // still names the active one, so the path has to come from the session.
        let (_tmp, l) = layout();
        let path = l.work_patch_dir_for("other", "sub/a.patch").unwrap();
        assert!(path.starts_with(l.work_dir()));
        assert!(path.ends_with("work/other/sub/a.patch"));

        // Containment is checked against the work directory for every series.
        assert!(l.work_patch_dir_for("..", "x").is_err());
        assert!(l.work_patch_dir_for("ok", "../../x").is_err());
    }
}
