//! The series: the ordered list of patches, and the single source of truth for what
//! gets applied.
//!
//! The format is one entry per line. A line that names a patch is that patch enabled,
//! a line starting `#!` is that patch disabled, `#` starts a comment, and a blank line
//! is ignored. Everything else is preserved byte-for-byte so that saving a series with
//! one new entry produces a one-line diff.
//!
//! A line says what it is: parsing looks at the line alone and never at the directory,
//! so what a line means cannot change underfoot. Order lives here and nowhere else, and
//! a name identifies a patch without saying anything about where it sits. A name is
//! chosen once, when the patch is created, and skot never rewrites one because
//! something moved. Reordering is therefore a change to this file alone, and the order
//! this file lists is the order `apply` follows.

use std::path::{Path, PathBuf};

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

    /// The disabled patches: `#!` lines, in the order they sit in the file.
    ///
    /// This reads the lines and nothing else. A patch whose file is missing is still
    /// listed here, because whether its line is disabled is a fact about the line; what
    /// is missing is caught when the series is resolved to paths.
    pub fn disabled(&self) -> Vec<String> {
        self.disabled_lines()
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    /// The disabled patches with their 1-based line numbers.
    pub fn disabled_lines(&self) -> Vec<(usize, String)> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                if l.entry.is_some() {
                    return None;
                }
                let rest = l.raw.trim_start().strip_prefix("#!")?.trim();
                (!rest.is_empty()).then(|| (i + 1, rest.to_owned()))
            })
            .collect()
    }

    /// Mark an entry disabled, in place. Returns false when the name is not an entry
    /// (already disabled, or never listed).
    pub fn disable(&mut self, name: &str) -> bool {
        let Some(line) = self
            .lines
            .iter_mut()
            .find(|l| l.entry.as_deref() == Some(name))
        else {
            return false;
        };
        line.entry = None;
        line.raw = format!("#!{name}");
        true
    }

    /// Mark a disabled patch enabled, in place. Returns false when the name is not a
    /// disabled line.
    pub fn enable(&mut self, name: &str) -> bool {
        let Some(line) = self.lines.iter_mut().find(|l| {
            l.entry.is_none() && l.raw.trim_start().strip_prefix("#!").map(str::trim) == Some(name)
        }) else {
            return false;
        };
        line.entry = Some(name.to_owned());
        line.raw = name.to_owned();
        true
    }

    /// Remove a disabled line entirely. Returns false when the name is not a disabled
    /// line.
    pub fn remove_disabled(&mut self, name: &str) -> bool {
        let Some(index) = self.lines.iter().position(|l| {
            l.entry.is_none() && l.raw.trim_start().strip_prefix("#!").map(str::trim) == Some(name)
        }) else {
            return false;
        };
        self.lines.remove(index);
        true
    }

    /// Remove a disabled patch: its `#!` line and its file. The caller saves.
    pub fn drop_disabled(&mut self, layout: &Layout, name: &str) -> Option<FileGone> {
        if !self.remove_disabled(name) {
            return None;
        }
        let path = resolve(layout, name);
        Some(match layout.guarded_remove(&path) {
            Ok(true) => FileGone::Deleted,
            Ok(false) => FileGone::Missing,
            Err(_) => FileGone::Kept(path),
        })
    }

    /// Remove the entry `name` and the file it names, leaving every other patch where it is.
    ///
    /// Returns the entry's 1-based position and what became of its file. The file goes
    /// first and the series line last, so an interrupted remove leaves a missing file that
    /// `status` reports and a rerun can finish, rather than a series line that names
    /// nothing. Nothing else in the series is read or written: the names around it are
    /// its own and stay that way.
    pub fn remove_entry(&mut self, layout: &Layout, name: &str) -> anyhow::Result<Removal> {
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

        let removed_path = resolve(layout, name);
        let file = match layout.guarded_remove(&removed_path) {
            Ok(true) => FileGone::Deleted,
            Ok(false) => FileGone::Missing,
            Err(_) => FileGone::Kept(removed_path),
        };

        let position = index + 1;
        self.remove(name).expect("the entry was resolved above");
        Ok(Removal { position, file })
    }

    /// Move `name` so that it becomes the `to`th entry, 0-based.
    ///
    /// The series is the order: this moves the patch's line and nothing else. The patch
    /// keeps the name it was created with, no file under `patches/` is read or written,
    /// and entries outside the two positions are not visited at all. Returns the 0-based
    /// positions and the range the move touches, each entry with the position it holds
    /// afterwards, for a caller that wants to show the result.
    pub fn reorder(&mut self, name: &str, to: usize) -> anyhow::Result<Moved> {
        let old: Vec<String> = self.names().into_iter().map(String::from).collect();
        let Some(from) = old.iter().position(|o| o == name) else {
            anyhow::bail!(
                "`{name}` is not in the series. Patches in it: {}",
                if old.is_empty() {
                    "(none)".to_owned()
                } else {
                    old.join(", ")
                }
            );
        };
        if to >= old.len() {
            anyhow::bail!(
                "the series holds {} patches, so position {} does not exist (1 to {})",
                old.len(),
                to + 1,
                old.len()
            );
        }
        let (lo, hi) = (from.min(to), from.max(to));
        if from == to {
            return Ok(Moved {
                from,
                to,
                window: Vec::new(),
            });
        }

        // The line goes, then comes back at its new place. Positions count entries and
        // not lines, so the line index is looked up again after the removal instead of
        // being carried across it.
        let from_line = self.entry_line(from);
        let line = self.lines.remove(from_line);
        let at = if to < from {
            self.entry_line(to)
        } else {
            self.entry_line(to - 1) + 1
        };
        self.lines.insert(at, line);

        let window = self
            .names()
            .iter()
            .enumerate()
            .skip(lo)
            .take(hi - lo + 1)
            .map(|(i, n)| (i + 1, (*n).to_owned()))
            .collect();
        Ok(Moved { from, to, window })
    }

    /// The line index of the entry at position `entry_index`.
    ///
    /// Comments and blank lines take lines of their own, so an entry's position in the
    /// series and its line in the file are two different numbers.
    fn entry_line(&self, entry_index: usize) -> usize {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.entry.is_some())
            .nth(entry_index)
            .map(|(i, _)| i)
            .expect("the position came from the entries themselves")
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

/// The file a new patch is written to: its slug, as it stands.
///
/// Nothing is added to make it unique. Two patches that want the same name are two
/// patches the filesystem will not hold, and the caller reports that as it comes.
pub fn default_patch_name(slug: &str) -> String {
    format!("{slug}.patch")
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

/// What [`Series::remove_entry`] did, in one value.
#[derive(Debug)]
pub struct Removal {
    /// The entry's 1-based position as it was in the series.
    pub position: usize,
    /// What became of the removed patch's own file.
    pub file: FileGone,
}

/// What [`Series::reorder`] did, in one value.
#[derive(Debug)]
pub struct Moved {
    /// The entry's 0-based position before the move.
    pub from: usize,
    /// The entry's 0-based position after the move.
    pub to: usize,
    /// The range the move touched, each entry's name paired with the 1-based position it
    /// holds afterwards: what a person needs in order to see the result without reading
    /// the whole series.
    pub window: Vec<(usize, String)>,
}

/// A `--at` value, a 1-based position (`3`) or an existing patch name (`b.patch`),
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
pub fn resolve(layout: &Layout, name: &str) -> PathBuf {
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
        let series = s("# a comment\n\na.patch\n   \nb.patch  # trailing\n");
        // Quilt's rule, which skot follows: only a line whose first non-blank
        // character is `#` is a comment. Everything else is the filename, verbatim.
        // A trailing `#` is therefore part of the name, because a patch may legitimately
        // be called `fix-#123.patch`, and guessing otherwise would silently apply the
        // wrong file.
        assert_eq!(series.names(), vec!["a.patch", "b.patch  # trailing"]);
        assert_eq!(series.len(), 2);
        // Rendered byte for byte, comments and blank lines and all: a `series` file is
        // something a person edits by hand and may well carry commentary.
        assert_eq!(
            series.render(),
            "# a comment\n\na.patch\n   \nb.patch  # trailing\n"
        );
    }

    #[test]
    fn a_move_leaves_commentary_where_it_is() {
        let (tmp, layout) = layout();
        std::fs::write(layout.patches_dir().join("a.patch"), "a\n").unwrap();
        std::fs::write(layout.patches_dir().join("b.patch"), "b\n").unwrap();
        let mut series = Series::parse(
            &layout.series_file(),
            "# lead\na.patch\n# between\nb.patch\n",
        );

        series.reorder("b.patch", 0).unwrap();

        assert_eq!(
            series.render(),
            "# lead\nb.patch\na.patch\n# between\n",
            "only entry lines take part in the order"
        );
        drop(tmp);
    }

    #[test]
    fn a_move_changes_the_order_and_no_name() {
        let (tmp, layout) = layout();
        for name in ["a.patch", "b.patch", "f.patch"] {
            std::fs::write(layout.patches_dir().join(name), "x\n").unwrap();
        }
        let mut series = Series::parse(&layout.series_file(), "a.patch\nb.patch\nf.patch\n");

        series.reorder("b.patch", 0).unwrap();

        assert_eq!(series.render(), "b.patch\na.patch\nf.patch\n");
        for name in ["a.patch", "b.patch", "f.patch"] {
            assert_eq!(
                std::fs::read_to_string(layout.patches_dir().join(name)).unwrap(),
                "x\n",
                "{name} is still the file it was"
            );
        }
        drop(tmp);
    }

    #[test]
    fn disabling_marks_the_line_and_enabling_puts_it_back() {
        let (tmp, layout) = layout();
        let mut series = Series::parse(&layout.series_file(), "a.patch\nb.patch\nc.patch\n");

        assert!(series.disable("b.patch"));
        assert_eq!(series.render(), "a.patch\n#!b.patch\nc.patch\n");
        assert_eq!(series.names(), vec!["a.patch", "c.patch"]);
        assert_eq!(series.disabled(), ["b.patch"]);

        assert!(series.enable("b.patch"));
        assert_eq!(
            series.render(),
            "a.patch\nb.patch\nc.patch\n",
            "the line comes back exactly as it was"
        );
        assert!(series.disabled().is_empty());
        drop(tmp);
    }

    #[test]
    fn a_comment_never_becomes_an_entry() {
        let (tmp, layout) = layout();
        let mut series = Series::parse(
            &layout.series_file(),
            "# someday, fix b.patch properly\na.patch\n#!b.patch\n",
        );

        assert_eq!(
            series.disabled(),
            ["b.patch"],
            "only the #! line is a patch"
        );
        assert!(series.disable("a.patch"));
        assert_eq!(
            series.render(),
            "# someday, fix b.patch properly\n#!a.patch\n#!b.patch\n"
        );
        drop(tmp);
    }

    #[test]
    fn a_disabled_line_is_read_without_looking_at_the_directory() {
        let (tmp, layout) = layout();
        let mut series = Series::parse(&layout.series_file(), "#!ghost.patch\n");

        assert_eq!(series.disabled(), ["ghost.patch"]);
        assert!(series.enable("ghost.patch"));
        assert_eq!(series.render(), "ghost.patch\n");
        drop(tmp);
    }

    #[test]
    fn removing_a_disabled_line_takes_the_line_and_nothing_else() {
        let (tmp, layout) = layout();
        let mut series = Series::parse(
            &layout.series_file(),
            "# a note kept verbatim\na.patch\n#!b.patch\n",
        );

        assert!(series.remove_disabled("b.patch"));
        assert_eq!(series.render(), "# a note kept verbatim\na.patch\n");
        assert!(!series.remove_disabled("nope.patch"));
        drop(tmp);
    }

    #[test]
    fn render_round_trips_byte_for_byte() {
        for text in [
            "a.patch\nb.patch\n",
            "a.patch\nb.patch", // no final newline
            "# header\n\na.patch\n",
            "",
            "\n",
            "# only a comment\n",
        ] {
            assert_eq!(s(text).render(), text, "round trip failed for {text:?}");
        }
    }

    #[test]
    fn crlf_round_trips() {
        let text = "a.patch\r\nb.patch\r\n";
        assert_eq!(s(text).render(), text);
        assert_eq!(s(text).names(), vec!["a.patch", "b.patch"]);
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
        let series = s("a.patch\n");
        assert_eq!(
            series.entries(&l).unwrap()[0].path,
            l.patches_dir().join("a.patch")
        );
    }

    #[test]
    fn insert_positions_by_entry_not_by_line() {
        let mut series = s("# header\na.patch\nb.patch\n");
        let line = series.insert("c.patch", 1).unwrap();
        assert_eq!(series.names(), vec!["a.patch", "c.patch", "b.patch"]);
        // Inserted at physical line 3, after the header.
        assert_eq!(line, 3);
        assert_eq!(series.render(), "# header\na.patch\nc.patch\nb.patch\n");
    }

    #[test]
    fn insert_appends_when_past_the_end() {
        let mut series = s("a.patch\n");
        series.insert("b.patch", 99).unwrap();
        assert_eq!(series.render(), "a.patch\nb.patch\n");
    }

    #[test]
    fn insert_refuses_a_duplicate() {
        let mut series = s("a.patch\n");
        let err = series.insert("a.patch", 0).unwrap_err();
        assert!(matches!(err, SeriesError::Duplicate { .. }), "got {err:?}");
    }

    #[test]
    fn insert_refuses_a_name_with_a_newline() {
        let mut series = s("a.patch\n");
        let err = series.insert("evil\nrm -rf /", 0).unwrap_err();
        assert!(
            matches!(err, SeriesError::InvalidName { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn remove_reports_the_position_and_round_trips() {
        let mut series = s("a.patch\nb.patch\nc.patch\n");
        assert_eq!(series.remove("b.patch"), Some(1));
        assert_eq!(series.render(), "a.patch\nc.patch\n");
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
        std::fs::write(l.patches_dir().join("a.patch"), "").unwrap();
        let series = s("a.patch\ngone.patch\n");

        let err = series.require_resolvable(&l).unwrap_err().to_string();
        assert!(err.contains("gone.patch"), "got {err}");
        assert!(err.contains(":2:"), "should name the line: {err}");
    }

    #[test]
    fn lenient_entries_report_missing_patches_instead_of_failing() {
        let (_tmp, l) = layout();
        let series = s("a.patch\ngone.patch\n");
        let entries = series.entries_lenient(&l);
        assert_eq!(entries.len(), 2);
        assert!(!entries[1].path.is_file());
    }

    #[test]
    fn a_default_name_is_the_patch_name_it_was_given() {
        assert_eq!(default_patch_name("trim-widgets"), "trim-widgets.patch");
        assert_eq!(default_patch_name("add_greeting"), "add_greeting.patch");
    }

    #[test]
    fn inserting_a_line_leaves_every_other_name_alone() {
        let (_tmp, l) = layout();
        for name in ["a.patch", "b.patch", "c.patch"] {
            std::fs::write(l.patches_dir().join(name), name).unwrap();
        }
        let mut series = s("a.patch\nb.patch\nc.patch\n");

        let line = series.insert("new.patch", 1).unwrap();

        assert_eq!(line, 2);
        assert_eq!(
            series.names(),
            vec!["a.patch", "new.patch", "b.patch", "c.patch"]
        );
        for name in ["a.patch", "b.patch", "c.patch"] {
            assert_eq!(
                std::fs::read_to_string(l.patches_dir().join(name)).unwrap(),
                name,
                "{name} is the file it always was"
            );
        }
    }

    #[test]
    fn position_at_accepts_a_number_a_name_or_nothing() {
        let series = s("a.patch\nb.patch\nc.patch\n");
        assert_eq!(position_at(&series, None).unwrap(), 3, "absent appends");
        assert_eq!(position_at(&series, Some("2")).unwrap(), 1);
        assert_eq!(position_at(&series, Some("b.patch")).unwrap(), 1);
        assert_eq!(
            position_at(&series, Some("99")).unwrap(),
            3,
            "past the end appends"
        );
        let err = position_at(&series, Some("0")).unwrap_err().to_string();
        assert!(err.contains("1-based"), "got {err}");
        let err = position_at(&series, Some("gone.patch"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("gone.patch"), "got {err}");
        assert!(err.contains("a.patch"), "it should list the series: {err}");
    }

    #[test]
    fn save_refuses_a_path_outside_the_project() {
        let (_tmp, l) = layout();
        let rogue = Series::parse(Path::new("/etc/skot-series"), "a.patch\n");
        assert!(rogue.save(&l).is_err());
    }

    #[test]
    fn save_and_reload_round_trips_through_the_filesystem() {
        let (_tmp, l) = layout();
        let mut series = Series::parse(&l.series_file(), "# header\na.patch\n");
        series.insert("b.patch", 1).unwrap();
        series.save(&l).unwrap();

        let reloaded = Series::load(&l.series_file()).unwrap();
        assert_eq!(reloaded.render(), series.render());
    }
}
