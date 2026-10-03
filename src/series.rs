//! The series: the ordered list of patches, and the single source of truth for what
//! gets applied.
//!
//! The format is quilt's, deliberately: one patch filename per line, `#` starts a
//! comment, blank lines are ignored. Unrecognised lines are preserved byte-for-byte
//! so that saving a series with one new entry produces a one-line diff.

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
    #[error("the series is empty: there is nothing to apply")]
    Empty,
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

    /// Error if the series has no patches or references a file that is gone.
    pub fn require_resolvable(&self, layout: &Layout) -> Result<Vec<Entry>, SeriesError> {
        let entries = self.entries(layout)?;
        if entries.is_empty() {
            return Err(SeriesError::Empty);
        }
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
    fn require_resolvable_rejects_an_empty_series() {
        let (_tmp, l) = layout();
        let err = s("# nothing yet\n").require_resolvable(&l).unwrap_err();
        assert!(matches!(err, SeriesError::Empty));
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
