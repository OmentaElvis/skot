//! The series: the ordered list of patches, and the single source of truth for what
//! gets applied.
//!
//! The format is quilt's, deliberately: one patch filename per line, `#` starts a
//! comment, blank lines are ignored. Unrecognised lines are preserved byte-for-byte
//! so that saving a series with one new entry produces a one-line diff.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::layout::Layout;

/// One resolvable line of the series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The filename exactly as written in the series.
    pub name: String,
    /// Absolute path, resolved against the layout.
    pub path: PathBuf,
    /// 1-based line number in the series file, for error messages.
    pub line: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SeriesError {
    #[error("{}:{}: patch listed in the series does not exist: {}", .series.display(), .line, .name)]
    MissingPatch {
        series: PathBuf,
        line: usize,
        name: String,
    },
    #[error("{}:{}: `{name}` is already in the series", .series.display(), .line)]
    Duplicate {
        series: PathBuf,
        line: usize,
        name: String,
    },
    #[error("{}:{}: patch name contains a newline", .series.display(), .line)]
    InvalidName {
        series: PathBuf,
        line: usize,
        name: String,
    },
    #[error("{}: `{}` already exists and is not in the series.\nPick another name, or `skot clean` if you think it is stale", .path.display(), .name)]
    OccupiedTarget { path: PathBuf, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    /// The line exactly as it appeared, minus the line ending.
    raw: String,
    /// The patch name, if this line is a real entry.
    entry: Option<String>,
}

/// A parsed series file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    path: PathBuf,
    line_ending: String,
    lines: Vec<Line>,
    final_newline: bool,
}

impl Series {
    /// Read and parse a series file.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::parse(path, &text))
    }

    /// Parse series text. `path` is only used in error messages.
    pub fn parse(path: &Path, text: &str) -> Self {
        let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let final_newline = text.is_empty() || text.ends_with('\n');
        let lines: Vec<Line> = split_lines(text)
            .into_iter()
            .map(|raw| {
                let trimmed = raw.trim();
                let entry = if trimmed.is_empty() || trimmed.starts_with('#') {
                    None
                } else {
                    Some(trimmed.to_owned())
                };
                Line { raw, entry }
            })
            .collect();
        Self {
            path: path.to_path_buf(),
            line_ending: line_ending.to_owned(),
            lines,
            final_newline,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolve every entry to a path, erroring on the first missing patch.
    pub fn entries(&self, layout: &Layout) -> Result<Vec<Entry>, SeriesError> {
        let mut out = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            let Some(name) = &line.entry else { continue };
            out.push(Entry {
                path: resolve(layout, name),
                line: i + 1,
                name: name.clone(),
            });
        }
        Ok(out)
    }

    /// Resolve every entry, tolerating missing patches.
    ///
    /// For `status` and `list`, where reporting a missing patch is the point rather
    /// than a reason to refuse to run.
    pub fn entries_lenient(&self, layout: &Layout) -> Vec<Entry> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| {
                let name = line.entry.as_ref()?;
                Some(Entry {
                    path: resolve(layout, name),
                    line: i + 1,
                    name: name.clone(),
                })
            })
            .collect()
    }

    /// Error if a file listed in the series is gone.
    ///
    /// An empty series is a valid state, not an error: a project straight from `init` has
    /// one, and `build`, `check`, and `apply` all run with nothing to replay.
    pub fn require_resolvable(&self, layout: &Layout) -> Result<Vec<Entry>, SeriesError> {
        let entries = self.entries(layout)?;
        for entry in &entries {
            if !entry.path.is_file() {
                return Err(SeriesError::MissingPatch {
                    series: self.path.clone(),
                    line: entry.line,
                    name: entry.name.clone(),
                });
            }
        }
        Ok(entries)
    }

    pub fn names(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| l.entry.as_deref())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.lines.iter().filter(|l| l.entry.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert `name` at `index`, 0-based, and renumber everything after it so the numbers
    /// stay contiguous: lexical order *is* application order, and an insertion renames the
    /// tail rather than leaving a gap where the numbers from `index` onwards used to sit
    /// (`0001 0002 0004` becoming `0001 0003-x 0004`).
    ///
    /// Returns the new entry's 1-based line and the tail renames, already applied to the
    /// series and to the files on disk (highest position first, so a chain like
    /// `0001-a`/`0002-a` never clobbers a file that is about to move). The caller writes
    /// `name`'s own file afterwards; it does not exist yet. Refuses any target name an
    /// out-of-series file occupies, and a name already in the series.
    pub fn insert_renumbering(
        &mut self,
        layout: &Layout,
        name: &str,
        index: usize,
    ) -> anyhow::Result<(usize, Vec<(String, String)>)> {
        let old: Vec<String> = self.names().into_iter().map(String::from).collect();
        let at = index.min(old.len());

        // Refusals that `insert` would also make, moved ahead of the renames: renaming the
        // tail and *then* refusing the new name would leave the files out of step with the
        // series. (A duplicate would be a tail file about to be renamed, which is exactly
        // the case the move protects.)
        if name.contains(['\n', '\r']) {
            return Err(SeriesError::InvalidName {
                series: self.path.clone(),
                line: self.lines.len() + 1,
                name: name.to_owned(),
            }
            .into());
        }
        if let Some(existing) = old.iter().position(|o| o == name) {
            return Err(SeriesError::Duplicate {
                series: self.path.clone(),
                line: existing + 1,
                name: name.to_owned(),
            }
            .into());
        }

        let mut renames = Vec::new();
        for (offset, old_name) in old[at..].iter().enumerate() {
            let new_name = renumbered(old_name, at + offset + 2);
            if &new_name != old_name {
                renames.push((old_name.clone(), new_name));
            }
        }

        // Everything else is checked before anything moves: a *rename* target an
        // out-of-series file occupies is refused rather than overwritten, and one that
        // belongs to a tail entry about to be renamed is fine because those are freed
        // first. The new name's own file is the caller's to write (or, for `refresh`, already
        // written), so guarding it lives with the caller.
        let source_names: Vec<&str> = old[at..].iter().map(String::as_str).collect();
        for (_, new_name) in &renames {
            let path = resolve(layout, new_name);
            if path.exists() && !source_names.contains(&new_name.as_str()) {
                return Err(SeriesError::OccupiedTarget {
                    path,
                    name: new_name.clone(),
                }
                .into());
            }
        }

        // Descending: the highest tail entry takes its new number first, freeing its old
        // name for the entry below it.
        for (old_name, new_name) in renames.iter().rev() {
            let from = resolve(layout, old_name);
            let to = resolve(layout, new_name);
            std::fs::rename(&from, &to).context(format!(
                "renaming {} to {}",
                from.display(),
                to.display()
            ))?;
        }

        let line = self.insert(name, at)?;
        // By position, not by name: a sequential name lookup would find the entry the
        // previous rename just produced instead of the one this rename means. The tail is
        // walked the same way the rename plan was, so positions line up even where a
        // rename was skipped as a no-op.
        let entry_lines: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.entry.is_some())
            .map(|(i, _)| i)
            .collect();
        for (offset, old_name) in old[at..].iter().enumerate() {
            let new_name = renumbered(old_name, at + offset + 2);
            if &new_name != old_name {
                let l = &mut self.lines[entry_lines[at + 1 + offset]];
                l.entry = Some(new_name.clone());
                l.raw = new_name.clone();
            }
        }
        Ok((line, renames))
    }

    /// Remove the entry `name`, renumbering the tail down so the numbers stay contiguous,
    /// and move the tail's files with their new names (lowest first: the removed patch's
    /// number is free the moment its file is gone, and each later rename frees its old
    /// name for the next).
    ///
    /// Returns the entry's 1-based position, the renames (already applied to the series
    /// and to the files on disk), and what became of the removed patch's own file. The
    /// file goes first and the series line last, so an interrupted remove leaves a
    /// missing file that `status` reports and a rerun can finish, rather than a series
    /// line that names nothing. Refuses any rename target an out-of-series file occupies
    /// before anything is renamed, and never overwrites the file the layout guard spared.
    pub fn remove_renumbering(&mut self, layout: &Layout, name: &str) -> anyhow::Result<Removal> {
        let old: Vec<String> = self.names().into_iter().map(String::from).collect();
        let Some(index) = old.iter().position(|o| o == name) else {
            anyhow::bail!(
                "`{name}` is not in the series. Patches in it: {}",
                if old.is_empty() {
                    "(none)".to_owned()
                } else {
                    old.join(", ")
                }
            );
        };

        // The tail fills the hole, numbered past the highest number below the removal
        // point. Taking the start from the prefix rather than from the position means a
        // series that already has a gap under where the removal lands still cannot have
        // the tail's new numbers collide with a prefix entry.
        let start = old[..index]
            .iter()
            .filter_map(|n| leading_number(n))
            .max()
            .map_or(1, |highest| highest + 1);
        let mut renames = Vec::new();
        for (offset, old_name) in old[index + 1..].iter().enumerate() {
            let new_name = renumbered(old_name, start + offset);
            if &new_name != old_name {
                renames.push((old_name.clone(), new_name));
            }
        }

        // Everything is checked before anything moves. The removed patch's own path is
        // excluded: its slot is the one the tail is moving into, and the file is about to
        // go (or already has). A prefix file, a stray, or any other out-of-series
        // occupant of a target is refused instead of overwritten.
        let removed_path = resolve(layout, name);
        let source_names: Vec<&str> = old[index + 1..].iter().map(String::as_str).collect();
        for (_, new_name) in &renames {
            let path = resolve(layout, new_name);
            if path.exists() && path != removed_path && !source_names.contains(&new_name.as_str()) {
                return Err(SeriesError::OccupiedTarget {
                    path,
                    name: new_name.clone(),
                }
                .into());
            }
        }

        // The file first: a rename may be aiming at its number, and a file the guard
        // spares (outside the layout) must not be overwritten by that rename later.
        let file = match layout.guarded_remove(&removed_path) {
            Ok(true) => FileGone::Deleted,
            Ok(false) => FileGone::Missing,
            Err(_) => FileGone::Kept(removed_path.clone()),
        };
        let spared_taken = match &file {
            FileGone::Kept(_) => renames
                .iter()
                .find(|(_, new_name)| resolve(layout, new_name) == removed_path),
            _ => None,
        };
        if let Some((_, taken)) = spared_taken {
            return Err(SeriesError::OccupiedTarget {
                path: removed_path,
                name: taken.clone(),
            }
            .into());
        }

        // Ascending: the lowest tail entry takes the slot the removed file just freed,
        // and every later rename lands in the slot the previous one freed.
        for (old_name, new_name) in &renames {
            let from = resolve(layout, old_name);
            let to = resolve(layout, new_name);
            std::fs::rename(&from, &to).context(format!(
                "renaming {} to {}",
                from.display(),
                to.display()
            ))?;
        }

        let position = index + 1;
        self.remove(name).expect("the entry was resolved above");
        // By position, not by name, and counted after the removal shifted the tail down:
        // a sequential name lookup would find the entry a rename just produced, and the
        // rename plan skips no-ops, so the tail is walked the same way it was planned.
        let entry_lines: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.entry.is_some())
            .map(|(i, _)| i)
            .collect();
        for (offset, old_name) in old[index + 1..].iter().enumerate() {
            let new_name = renumbered(old_name, start + offset);
            if &new_name != old_name {
                let l = &mut self.lines[entry_lines[index + offset]];
                l.entry = Some(new_name.clone());
                l.raw = new_name.clone();
            }
        }
        Ok(Removal {
            position,
            renames,
            file,
        })
    }

    /// Insert `name` so that it becomes the `index`th entry, 0-based.
    ///
    /// Appends when `index` is past the end. Refuses a duplicate rather than
    /// creating a series that applies the same patch twice.
    pub fn insert(&mut self, name: &str, index: usize) -> Result<usize, SeriesError> {
        if name.contains(['\n', '\r']) {
            return Err(SeriesError::InvalidName {
                series: self.path.clone(),
                line: self.lines.len() + 1,
                name: name.to_owned(),
            });
        }
        if let Some(existing) = self
            .lines
            .iter()
            .position(|l| l.entry.as_deref() == Some(name))
        {
            return Err(SeriesError::Duplicate {
                series: self.path.clone(),
                line: existing + 1,
                name: name.to_owned(),
            });
        }

        // Count only real entries: `index` is a position in the series, not a line.
        let mut seen = 0usize;
        let mut at = self.lines.len();
        for (i, line) in self.lines.iter().enumerate() {
            if line.entry.is_some() {
                if seen == index {
                    at = i;
                    break;
                }
                seen += 1;
            }
        }
        self.lines.insert(
            at,
            Line {
                raw: name.to_owned(),
                entry: Some(name.to_owned()),
            },
        );
        Ok(at + 1)
    }

    /// Remove the entry named `name`. Returns its former 0-based position.
    pub fn remove(&mut self, name: &str) -> Option<usize> {
        let pos = self
            .lines
            .iter()
            .position(|l| l.entry.as_deref() == Some(name))?;
        self.lines.remove(pos);
        Some(pos)
    }

    /// Serialise back to text, preserving line endings and any final newline.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push_str(&self.line_ending);
            }
            out.push_str(&line.raw);
        }
        if self.final_newline && !self.lines.is_empty() {
            out.push_str(&self.line_ending);
        }
        out
    }

    /// Write the series back, creating the file if needed. Refuses to clobber
    /// anything that is not a series.
    pub fn save(&self, layout: &Layout) -> anyhow::Result<()> {
        if self.path != layout.series_file() {
            anyhow::bail!("refusing to write a series to {}", self.path.display());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, self.render())?;
        Ok(())
    }
}

/// A stable default name for a new patch: zero-padded so that lexical order matches
/// application order.
pub fn default_patch_name(prefix: &str, slug: &str) -> String {
    format!("{prefix:0>4}-{slug}.patch")
}

/// What became of a removed patch's own file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileGone {
    /// Skot deleted it, from inside the layout.
    Deleted,
    /// There was nothing there to delete.
    Missing,
    /// It sits outside the layout, which [`Layout::guarded_remove`] refuses to delete;
    /// the path stays as it is.
    Kept(PathBuf),
}

/// What [`Series::remove_renumbering`] did, in one value.
#[derive(Debug)]
pub struct Removal {
    /// The entry's 1-based position as it was in the series.
    pub position: usize,
    /// The tail renames, applied to the series and to the files on disk.
    pub renames: Vec<(String, String)>,
    /// What became of the removed patch's own file.
    pub file: FileGone,
}

/// The same patch file under a new 1-based series number.
///
/// A leading run of digits before the first `-` is swapped for the new number; a name
/// without one gains it; a directory above the file is kept.
fn renumbered(name: &str, number: usize) -> String {
    let number = format!("{number:0>4}");
    match name.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/{}", renumber_file(file, &number)),
        None => renumber_file(name, &number),
    }
}

fn renumber_file(file: &str, number: &str) -> String {
    let numbered = file
        .split_once('-')
        .filter(|(digits, _)| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()));
    if let Some((_, rest)) = numbered {
        return format!("{number}-{rest}");
    }
    match file.strip_suffix(".patch") {
        Some(stem) => format!("{number}-{stem}.patch"),
        None => format!("{number}-{file}"),
    }
}

/// The 1-based number a name already carries, if any: the digits before the first `-`
/// in the file part of `name`, the same run [`renumbered`] would swap. A name without
/// one (or with digits too large to be a series number) counts as unnumbered.
fn leading_number(name: &str) -> Option<usize> {
    let file = name.rsplit_once('/').map(|(_, file)| file).unwrap_or(name);
    let (digits, _) = file.split_once('-')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A `--at` value, a 1-based position (`3`) or an existing patch name (`0002-b.patch`),
/// as the 0-based index a new patch should occupy. `None` appends; a position past the
/// end appends too, the way [`Series::insert`] already behaves.
pub fn position_at(series: &Series, at: Option<&str>) -> anyhow::Result<usize> {
    let Some(at) = at else {
        return Ok(series.len());
    };
    if let Ok(number) = at.parse::<usize>() {
        if number == 0 {
            anyhow::bail!("positions are 1-based: `--at 1` is the front of the series");
        }
        return Ok((number - 1).min(series.len()));
    }
    if let Some(index) = series.names().iter().position(|name| *name == at) {
        return Ok(index);
    }
    Err(anyhow::anyhow!(
        "`{at}` is not in the series. Patches in it: {}",
        if series.is_empty() {
            "(none)".to_owned()
        } else {
            series.names().join(", ")
        }
    ))
}

/// Resolve a series entry to a path.
///
/// - absolute: used as written
/// - containing a `/`: relative to the project root
/// - otherwise: relative to the patch directory
///
/// This is deliberately three simple rules rather than something cleverer, because a
/// surprising resolution is the kind of thing that costs an afternoon.
fn resolve(layout: &Layout, name: &str) -> PathBuf {
    let p = Path::new(name);
    if p.is_absolute() {
        p.to_path_buf()
    } else if name.contains('/') {
        layout.root().join(p)
    } else {
        layout.patches_dir().join(p)
    }
}

/// Split on newlines without losing the final empty segment's meaning.
fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    // The `\r` of a CRLF file is stripped here and re-added by `render` from the
    // recorded `line_ending`. Keeping it in `raw` as well would double it on the way
    // out, turning `\r\n` into `\r\r\n`.
    body.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let l = Layout::new(tmp.path());
        std::fs::create_dir_all(l.patches_dir()).unwrap();
        (tmp, l)
    }

    fn s(text: &str) -> Series {
        Series::parse(Path::new("series"), text)
    }

    #[test]
    fn comments_and_blanks_are_ignored_but_kept_verbatim() {
        let series = s("# a comment\n\n0001-a.patch\n   \n0002-b.patch  # trailing\n");
        // Quilt's rule, which skot follows: only a line whose first non-blank
        // character is `#` is a comment. Everything else is the filename, verbatim.
        // A trailing `#` is therefore part of the name, because a patch may legitimately
        // be called `fix-#123.patch`, and guessing otherwise would silently apply the
        // wrong file.
        assert_eq!(
            series.names(),
            vec!["0001-a.patch", "0002-b.patch  # trailing"]
        );
        assert_eq!(series.len(), 2);
        // Rendered byte for byte, comments and blank lines and all: a `series` file is
        // something a person edits by hand and may well carry commentary.
        assert_eq!(
            series.render(),
            "# a comment\n\n0001-a.patch\n   \n0002-b.patch  # trailing\n"
        );
    }

    #[test]
    fn render_round_trips_byte_for_byte() {
        for text in [
            "0001-a.patch\n0002-b.patch\n",
            "0001-a.patch\n0002-b.patch", // no final newline
            "# header\n\n0001-a.patch\n",
            "",
            "\n",
            "# only a comment\n",
        ] {
            assert_eq!(s(text).render(), text, "round trip failed for {text:?}");
        }
    }

    #[test]
    fn crlf_round_trips() {
        let text = "0001-a.patch\r\n0002-b.patch\r\n";
        assert_eq!(s(text).render(), text);
        assert_eq!(s(text).names(), vec!["0001-a.patch", "0002-b.patch"]);
    }

    #[test]
    fn entries_resolve_by_the_three_documented_rules() {
        let (_tmp, l) = layout();
        std::fs::create_dir_all(l.root().join("vendor")).unwrap();
        let series = s("bare.patch\nsub/dir.patch\n/vendor/abs.patch\n");
        let entries = series.entries(&l).unwrap();
        assert_eq!(entries[0].path, l.patches_dir().join("bare.patch"));
        assert_eq!(entries[1].path, l.root().join("sub/dir.patch"));
        assert_eq!(entries[2].path, PathBuf::from("/vendor/abs.patch"));
    }

    #[test]
    fn bare_names_land_in_the_patch_directory() {
        let (_tmp, l) = layout();
        let series = s("0001-a.patch\n");
        assert_eq!(
            series.entries(&l).unwrap()[0].path,
            l.patches_dir().join("0001-a.patch")
        );
    }

    #[test]
    fn insert_positions_by_entry_not_by_line() {
        let mut series = s("# header\n0001-a.patch\n0002-b.patch\n");
        let line = series.insert("0003-c.patch", 1).unwrap();
        assert_eq!(
            series.names(),
            vec!["0001-a.patch", "0003-c.patch", "0002-b.patch"]
        );
        // Inserted at physical line 3, after the header.
        assert_eq!(line, 3);
        assert_eq!(
            series.render(),
            "# header\n0001-a.patch\n0003-c.patch\n0002-b.patch\n"
        );
    }

    #[test]
    fn insert_appends_when_past_the_end() {
        let mut series = s("0001-a.patch\n");
        series.insert("0002-b.patch", 99).unwrap();
        assert_eq!(series.render(), "0001-a.patch\n0002-b.patch\n");
    }

    #[test]
    fn insert_refuses_a_duplicate() {
        let mut series = s("0001-a.patch\n");
        let err = series.insert("0001-a.patch", 0).unwrap_err();
        assert!(matches!(err, SeriesError::Duplicate { .. }), "got {err:?}");
    }

    #[test]
    fn insert_refuses_a_name_with_a_newline() {
        let mut series = s("0001-a.patch\n");
        let err = series.insert("evil\nrm -rf /", 0).unwrap_err();
        assert!(
            matches!(err, SeriesError::InvalidName { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn remove_reports_the_position_and_round_trips() {
        let mut series = s("0001-a.patch\n0002-b.patch\n0003-c.patch\n");
        assert_eq!(series.remove("0002-b.patch"), Some(1));
        assert_eq!(series.render(), "0001-a.patch\n0003-c.patch\n");
        assert_eq!(series.remove("absent.patch"), None);
    }

    #[test]
    fn require_resolvable_accepts_an_empty_series() {
        // An empty series is a fresh project, not a broken one: `init` writes one, and
        // build, check, and apply all run with nothing to replay.
        let (_tmp, l) = layout();
        let entries = s("# nothing yet\n").require_resolvable(&l).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn require_resolvable_names_the_line_and_the_missing_file() {
        let (_tmp, l) = layout();
        std::fs::write(l.patches_dir().join("0001-a.patch"), "").unwrap();
        let series = s("0001-a.patch\n0002-gone.patch\n");

        let err = series.require_resolvable(&l).unwrap_err().to_string();
        assert!(err.contains("0002-gone.patch"), "got {err}");
        assert!(err.contains(":2:"), "should name the line: {err}");
    }

    #[test]
    fn lenient_entries_report_missing_patches_instead_of_failing() {
        let (_tmp, l) = layout();
        let series = s("0001-a.patch\n0002-gone.patch\n");
        let entries = series.entries_lenient(&l);
        assert_eq!(entries.len(), 2);
        assert!(!entries[1].path.is_file());
    }

    #[test]
    fn default_names_sort_lexically_in_application_order() {
        let names: Vec<_> = (1..12)
            .map(|n| default_patch_name(&n.to_string(), "thing"))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn a_renumbered_name_swaps_its_number_and_keeps_everything_else() {
        assert_eq!(renumbered("0001-a.patch", 7), "0007-a.patch");
        assert_eq!(renumbered("add-feature.patch", 3), "0003-add-feature.patch");
        assert_eq!(renumbered("sub/0005-b.patch", 2), "sub/0002-b.patch");
        assert_eq!(renumbered("fix-#123.patch", 4), "0004-fix-#123.patch");
    }

    #[test]
    fn inserting_renumbers_the_tail_and_moves_the_files() {
        let (_tmp, l) = layout();
        for name in ["0001-a.patch", "0002-b.patch", "0003-c.patch"] {
            std::fs::write(l.patches_dir().join(name), name).unwrap();
        }
        let mut series = s("0001-a.patch\n0002-b.patch\n0003-c.patch\n");

        let (line, renames) = series.insert_renumbering(&l, "0002-new.patch", 1).unwrap();

        assert_eq!(line, 2);
        assert_eq!(
            series.names(),
            vec![
                "0001-a.patch",
                "0002-new.patch",
                "0003-b.patch",
                "0004-c.patch"
            ],
            "the tail stays contiguous, so lexical order is application order"
        );
        assert_eq!(
            renames,
            [
                ("0002-b.patch".to_owned(), "0003-b.patch".to_owned()),
                ("0003-c.patch".to_owned(), "0004-c.patch".to_owned())
            ]
        );
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("0003-b.patch")).unwrap(),
            "0002-b.patch",
            "the file moves with its name"
        );
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("0004-c.patch")).unwrap(),
            "0003-c.patch"
        );
        assert!(!l.patches_dir().join("0002-b.patch").exists());
    }

    #[test]
    fn tail_files_rename_highest_first_so_a_chain_never_clobbers() {
        // Three files with the same slug: an ascending pass would rename 0001-a onto the
        // still-present 0002-a and lose it.
        let (_tmp, l) = layout();
        for name in ["0001-a.patch", "0002-a.patch", "0003-a.patch"] {
            std::fs::write(l.patches_dir().join(name), name).unwrap();
        }
        let mut series = s("0001-a.patch\n0002-a.patch\n0003-a.patch\n");

        let (_line, renames) = series.insert_renumbering(&l, "0001-new.patch", 0).unwrap();

        assert_eq!(
            series.names(),
            vec![
                "0001-new.patch",
                "0002-a.patch",
                "0003-a.patch",
                "0004-a.patch"
            ]
        );
        assert_eq!(renames.len(), 3);
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("0002-a.patch")).unwrap(),
            "0001-a.patch",
            "0002-a is the old 0001-a, not the file that used to be called 0002-a"
        );
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("0003-a.patch")).unwrap(),
            "0002-a.patch"
        );
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("0004-a.patch")).unwrap(),
            "0003-a.patch"
        );
        assert!(!l.patches_dir().join("0001-a.patch").exists());
    }

    #[test]
    fn inserting_refuses_a_rename_target_a_stray_file_occupies() {
        let (_tmp, l) = layout();
        std::fs::write(l.patches_dir().join("0001-a.patch"), "one").unwrap();
        std::fs::write(l.patches_dir().join("0003-c.patch"), "three").unwrap();
        // Not in the series, but in the way: inserting at the front would want to rename
        // 0001-a to 0002-a, which this occupies.
        std::fs::write(l.patches_dir().join("0002-a.patch"), "stray").unwrap();
        let mut series = s("0001-a.patch\n0003-c.patch\n");

        let err = series
            .insert_renumbering(&l, "0004-new.patch", 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0002-a.patch"), "got {err}");
        assert!(err.contains("not in the series"), "got {err}");
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("0002-a.patch")).unwrap(),
            "stray",
            "the stray file must be untouched"
        );
        assert!(l.patches_dir().join("0001-a.patch").exists());
    }

    #[test]
    fn inserting_refuses_a_duplicate_before_moving_anything() {
        let (_tmp, l) = layout();
        std::fs::write(l.patches_dir().join("0001-a.patch"), "one").unwrap();
        std::fs::write(l.patches_dir().join("0002-b.patch"), "two").unwrap();
        let mut series = s("0001-a.patch\n0002-b.patch\n");

        let err = series
            .insert_renumbering(&l, "0002-b.patch", 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already in the series"), "got {err}");
        assert!(
            l.patches_dir().join("0002-b.patch").exists(),
            "no tail file may have been renamed on the way to the refusal"
        );
        assert_eq!(
            series.names(),
            vec!["0001-a.patch", "0002-b.patch"],
            "the series must be unchanged"
        );
    }

    #[test]
    fn position_at_accepts_a_number_a_name_or_nothing() {
        let series = s("0001-a.patch\n0002-b.patch\n0003-c.patch\n");
        assert_eq!(position_at(&series, None).unwrap(), 3, "absent appends");
        assert_eq!(position_at(&series, Some("2")).unwrap(), 1);
        assert_eq!(position_at(&series, Some("0002-b.patch")).unwrap(), 1);
        assert_eq!(
            position_at(&series, Some("99")).unwrap(),
            3,
            "past the end appends"
        );
        let err = position_at(&series, Some("0")).unwrap_err().to_string();
        assert!(err.contains("1-based"), "got {err}");
        let err = position_at(&series, Some("0009-gone.patch"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("0009-gone.patch"), "got {err}");
        assert!(
            err.contains("0001-a.patch"),
            "it should list the series: {err}"
        );
    }

    #[test]
    fn save_refuses_a_path_outside_the_project() {
        let (_tmp, l) = layout();
        let rogue = Series::parse(Path::new("/etc/skot-series"), "0001-a.patch\n");
        assert!(rogue.save(&l).is_err());
    }

    #[test]
    fn save_and_reload_round_trips_through_the_filesystem() {
        let (_tmp, l) = layout();
        let mut series = Series::parse(&l.series_file(), "# header\n0001-a.patch\n");
        series.insert("0002-b.patch", 1).unwrap();
        series.save(&l).unwrap();

        let reloaded = Series::load(&l.series_file()).unwrap();
        assert_eq!(reloaded.render(), series.render());
    }
}
