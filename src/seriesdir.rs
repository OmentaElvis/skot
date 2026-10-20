//! Building a series directory, and putting it where a series lives.
//!
//! A series is a directory of plain files: `series`, `base`, and the patches. Nothing
//! is registered anywhere else on its behalf, so the directory itself is the unit of
//! atomicity. Every operation that changes a series therefore builds a complete
//! replacement in `.skot/tmp/`, verifies it, and only then renames it into place. The
//! series is either the old one or the new one; there is no third thing it can be
//! part-way through becoming.
//!
//! The cost is one rename per operation, which on every filesystem skot supports is
//! atomic for a directory. The benefit is that a failure while writing files (a disk
//! that filled up, a permission that was revoked, a process that was killed) happens
//! in scratch, with the live series untouched. Nothing has to be recovered from,
//! because nothing was ever half-written where a series lives.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::layout::{self, Layout};
use crate::series::Series;

/// Where a built series waits for its rename.
fn new_dir(layout: &Layout, series: &str) -> PathBuf {
    layout.tmp_dir().join(format!("{series}.new"))
}

/// Where the live directory waits while a swap is in progress.
fn old_dir(layout: &Layout, series: &str) -> PathBuf {
    layout.tmp_dir().join(format!("{series}.old"))
}

/// Put back a series whose swap was interrupted, and clear scratch that is not waiting
/// on anything.
///
/// Called under the lock before any atomic operation. A swap renames the live directory
/// aside and the new one into its place, so a process that dies in between leaves the
/// series existing only as `.old`. Three things can be left behind, and each has
/// exactly one right answer:
///
/// * `.old` with no live series: the two renames were interrupted between them. The
///   series goes back.
/// * `.old` with a live series: the swap finished and only the hand-off of the original
///   to the undo slot was interrupted. The hand-off completes.
/// * `.new`: a build that was never renamed. It is scratch and nothing references it.
///
/// None of this asks the user anything. An interrupted swap is not a decision to make;
/// it is a state to leave.
pub fn recover(layout: &Layout) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(layout.tmp_dir()) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(series) = name.strip_suffix(".old") else {
            if name.ends_with(".new") {
                let _ = layout.guarded_remove(&entry.path());
            }
            continue;
        };
        // A series name cannot contain a path separator, so this is a plain name.
        if series.is_empty() || series.contains(['/', '\\']) {
            continue;
        }
        let live = layout.series_root().join(series);
        if live.exists() {
            keep_as_undo(layout, series, &entry.path())?;
        } else {
            std::fs::rename(entry.path(), &live).context(format!(
                "restoring {} from an interrupted swap",
                live.display()
            ))?;
        }
    }
    Ok(())
}

/// Move a swapped-out series into its undo slot, replacing whatever was there.
fn keep_as_undo(layout: &Layout, series: &str, from: &Path) -> Result<()> {
    let undo = layout.undo_dir().join(series);
    let _ = layout.guarded_remove(&undo);
    if let Some(parent) = undo.parent() {
        std::fs::create_dir_all(parent).context(format!("creating {}", parent.display()))?;
    }
    std::fs::rename(from, &undo)
        .context(format!("keeping the previous series at {}", undo.display()))
}

/// A replacement series, fully written and checked, waiting for a rename.
///
/// Dropping one without installing it leaves scratch behind, which the next atomic
/// operation clears. Nothing under `patches/` has been touched.
#[derive(Debug)]
pub struct Built {
    dir: PathBuf,
}

/// What a series directory holds, read into memory so it can be written out elsewhere.
#[derive(Debug)]
pub struct Source {
    pub series_text: String,
    pub base_text: Option<String>,
    /// Each patch by the name the series file lists it under, with its text.
    pub patches: Vec<(String, String)>,
}

impl Built {
    /// Write a complete series into scratch and check what landed.
    ///
    /// `patches` is every patch by the name the series file lists it under, paired with
    /// the text it should hold. The check afterwards is what makes the build
    /// trustworthy before a rename puts it where a series lives: each file is re-read
    /// and compared to what was meant to go in it, so a truncated or corrupt write dies
    /// here, with the real series untouched. Each patch is then read back as a patch,
    /// because a file that is not one would otherwise be installed and only found to be
    /// unreadable after the rename it should never have crossed.
    pub fn build(
        layout: &Layout,
        series: &str,
        series_text: &str,
        base_text: Option<&str>,
        patches: &[(String, String)],
    ) -> Result<Self> {
        let dir = new_dir(layout, series);
        // Anything a crashed run left behind is not part of this build.
        let _ = layout.guarded_remove(&dir);
        std::fs::create_dir_all(&dir).context(format!("creating {}", dir.display()))?;

        std::fs::write(dir.join(layout::SERIES_FILE), series_text).context(format!(
            "writing {}",
            dir.join(layout::SERIES_FILE).display()
        ))?;
        if let Some(base) = base_text {
            std::fs::write(dir.join(layout::BASE_FILE), base)
                .context(format!("writing {}", dir.join(layout::BASE_FILE).display()))?;
        }
        for (name, text) in patches {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .context(format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, text).context(format!("writing {}", path.display()))?;
        }

        verify(&dir, series_text, base_text, patches)?;
        Ok(Self { dir })
    }

    /// Where this build is waiting: the scratch directory a rename would move into place.
    ///
    /// The caller that has not renamed it yet has a series it can still read back, which
    /// is what makes proving the result before installing it possible at all.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read a series directory into memory, ready to be rebuilt elsewhere.
    ///
    /// The list, the base and every patch: a copy takes whatever the series holds, so
    /// the files are read rather than assumed.
    pub fn read(source: &Layout) -> Result<Source> {
        let series_text = std::fs::read_to_string(source.series_file()).unwrap_or_default();
        let base_text = std::fs::read_to_string(source.base_file()).ok();
        let mut patches = Vec::new();
        let entries = std::fs::read_dir(source.patches_dir())
            .context(format!("reading {}", source.patches_dir().display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|ext| ext != "patch") {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let text =
                std::fs::read_to_string(&path).context(format!("reading {}", path.display()))?;
            patches.push((name, text));
        }
        patches.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Source {
            series_text,
            base_text,
            patches,
        })
    }

    /// Put the built series where a series lives, leaving the old one as the undo entry.
    ///
    /// Two renames: the live directory aside, then the new one into its place. If the
    /// second fails the first is undone, so the series is left as it was. On success the
    /// directory that was moved aside becomes the series' undo entry, replacing any
    /// previous one.
    pub fn replace(self, layout: &Layout) -> Result<PathBuf> {
        let live = layout.patches_dir();
        let aside = old_dir(layout, layout.series());
        let _ = layout.guarded_remove(&aside);

        std::fs::rename(&live, &aside).context(format!("moving {} aside", live.display()))?;
        if let Err(err) = std::fs::rename(&self.dir, &live) {
            // The series must not be left missing because its replacement could not be
            // installed. Put the original back before reporting.
            let restored = std::fs::rename(&aside, &live);
            return match restored {
                Ok(()) => Err(err).context(format!(
                    "installing the rebased series at {}",
                    live.display()
                )),
                Err(restore) => Err(err).context(format!(
                    "installing the rebased series at {}, and putting the original back \
                     from {} also failed: {restore}",
                    live.display(),
                    aside.display(),
                )),
            };
        }

        keep_as_undo(layout, layout.series(), &aside)?;
        Ok(layout.undo_entry())
    }

    /// Put the built series at a new name, leaving every other series alone.
    ///
    /// One rename, and the destination does not exist, so nothing is overwritten and
    /// there is nothing to restore.
    pub fn install(self, target: &Layout) -> Result<()> {
        let live = target.patches_dir();
        if live.exists() {
            bail!("the series `{}` already exists.", target.series());
        }
        if let Some(parent) = live.parent() {
            std::fs::create_dir_all(parent).context(format!("creating {}", parent.display()))?;
        }
        std::fs::rename(&self.dir, &live).context(format!(
            "moving {} to {}",
            self.dir.display(),
            live.display()
        ))
    }
}

/// Read a built series back and check it against what was meant to go in.
///
/// Three checks, and the last two are the ones that catch a mistake rather than a fault:
/// a file that is not what was written is a disk problem; a series file that lists a
/// patch which is not beside it is a series that cannot be applied; and a listed patch
/// that is not a patch is a series that cannot be read. The third is what makes the
/// rename safe, because byte fidelity only proves the write landed: text libgit2
/// refuses compares equal to what was meant to go in it, and would otherwise be
/// installed and discovered to be unreadable after the rename it should never cross.
fn verify(
    dir: &Path,
    series_text: &str,
    base_text: Option<&str>,
    patches: &[(String, String)],
) -> Result<()> {
    let mut expected: Vec<(PathBuf, &str)> = vec![(dir.join(layout::SERIES_FILE), series_text)];
    if let Some(base) = base_text {
        expected.push((dir.join(layout::BASE_FILE), base));
    }
    for (name, text) in patches {
        expected.push((dir.join(name), text));
    }

    for (path, want) in expected {
        let got = std::fs::read_to_string(&path)
            .context(format!("reading back {} to check it", path.display()))?;
        if got != *want {
            bail!(
                "{} did not write as intended; refusing to install it.",
                path.display()
            );
        }
    }

    let listed = Series::parse(&dir.join(layout::SERIES_FILE), series_text);
    for name in listed.names() {
        let path = dir.join(name);
        if !path.is_file() {
            bail!(
                "{} lists `{}`, which is not beside it.",
                dir.join(layout::SERIES_FILE).display(),
                name
            );
        }
        let text = std::fs::read_to_string(&path)
            .context(format!("reading back {} to check it", path.display()))?;
        git2::Diff::from_buffer(text.as_bytes()).context(format!(
            "{} is not a patch skot can read; refusing to install it.",
            path.display()
        ))?;
    }
    Ok(())
}

/// Trade the active series with its undo entry.
///
/// The two directories exchange places, so a second undo puts back what the first one
/// took away. Nothing is copied and nothing is deleted: the undo buffer is one entry
/// deep by construction, and this is what keeps it that way.
pub fn undo_swap(layout: &Layout) -> Result<()> {
    let live = layout.patches_dir();
    let undo = layout.undo_entry();
    if !undo.is_dir() {
        bail!(
            "there is nothing to undo for `{}`.\n\
             Undo is the version the last successful rebase replaced, and this series \
             has none.",
            layout.series()
        );
    }

    let aside = old_dir(layout, layout.series());
    // The scratch directory exists whenever a build created it, and `undo` never
    // builds: the rename's destination has to be made first.
    std::fs::create_dir_all(layout.tmp_dir())
        .context(format!("creating {}", layout.tmp_dir().display()))?;
    let _ = layout.guarded_remove(&aside);

    std::fs::rename(&live, &aside).context(format!("moving {} aside", live.display()))?;
    if let Err(err) = std::fs::rename(&undo, &live) {
        let restored = std::fs::rename(&aside, &live);
        return match restored {
            Ok(()) => Err(err).context(format!(
                "restoring the previous series at {}",
                live.display()
            )),
            Err(restore) => Err(err).context(format!(
                "restoring the previous series at {}, and putting the current one back \
                 from {} also failed: {restore}",
                live.display(),
                aside.display(),
            )),
        };
    }

    // The one that was live becomes the new undo entry, so the trade is reversible.
    keep_as_undo(layout, layout.series(), &aside)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        std::fs::create_dir_all(layout.patches_dir()).unwrap();
        std::fs::create_dir_all(layout.series_root().join("default")).unwrap();
        (tmp, layout)
    }

    /// Write a series directory by hand, standing in for whatever produced it.
    fn series_dir(path: &Path, patch: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(layout::SERIES_FILE), "a.patch\n").unwrap();
        std::fs::write(path.join("a.patch"), patch).unwrap();
    }

    /// Text a build can install: every patch a build holds has to read back as one.
    fn rebuilt() -> String {
        "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
"
        .to_owned()
    }

    #[test]
    fn a_swap_interrupted_before_the_second_rename_is_put_back() {
        // The crash window: the live directory was moved aside and the replacement
        // never took its place. The series exists only as `.old`, and recovery has
        // to notice before anything else runs.
        let (_tmp, l) = layout();
        let live = l.patches_dir();
        series_dir(&live, "original\n");
        let aside = l.tmp_dir().join("default.old");
        std::fs::create_dir_all(l.tmp_dir()).unwrap();
        std::fs::rename(&live, &aside).unwrap();
        // The replacement sits in scratch, half-delivered.
        series_dir(&l.tmp_dir().join("default.new"), "rebuilt\n");

        recover(&l).unwrap();

        assert_eq!(
            std::fs::read_to_string(live.join("a.patch")).unwrap(),
            "original\n",
            "the interrupted swap must restore the series, not prefer the scratch build"
        );
        assert!(
            !l.tmp_dir().join("default.new").exists(),
            "the never-installed build is scratch and goes"
        );
        assert!(!aside.exists(), "the aside copy was renamed back");
    }

    #[test]
    fn a_swap_interrupted_before_the_undo_handoff_completes_it() {
        // The other window: both renames landed and only the hand-off of the original
        // to the undo slot was lost. The series is correct; the buffer is not, and
        // recovery finishes the job rather than restoring over the new content.
        let (_tmp, l) = layout();
        let live = l.patches_dir();
        series_dir(&live, "rebuilt\n");
        let aside = l.tmp_dir().join("default.old");
        std::fs::create_dir_all(l.tmp_dir()).unwrap();
        series_dir(&aside, "original\n");

        recover(&l).unwrap();

        assert_eq!(
            std::fs::read_to_string(live.join("a.patch")).unwrap(),
            "rebuilt\n",
            "the live series is the completed swap and must not be rolled back"
        );
        assert_eq!(
            std::fs::read_to_string(l.undo_entry().join("a.patch")).unwrap(),
            "original\n",
            "the original belongs in the undo slot, where the interrupted hand-off left it"
        );
        assert!(!aside.exists(), "the aside copy was handed off");
    }

    #[test]
    fn stray_scratch_without_a_swap_is_removed_on_sight() {
        // A build that was never renamed: nothing references it, and leaving it would
        // make the next build clean up a stranger's work.
        let (_tmp, l) = layout();
        std::fs::create_dir_all(l.patches_dir()).unwrap();
        series_dir(&l.patches_dir(), "live\n");
        series_dir(&l.tmp_dir().join("default.new"), "abandoned\n");

        recover(&l).unwrap();

        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("a.patch")).unwrap(),
            "live\n"
        );
        assert!(
            !l.tmp_dir().join("default.new").exists(),
            "a build nothing renamed is cleared"
        );
    }

    #[test]
    fn a_failed_replacement_leaves_the_series_exactly_as_it_was() {
        // The second rename fails (scratch vanished under us, a filesystem refusing
        // the operation): the original goes back rather than being parked in tmp with
        // the series nowhere.
        let (_tmp, l) = layout();
        series_dir(&l.patches_dir(), "original\n");
        let built = Built::build(
            &l,
            "default",
            "a.patch\n",
            None,
            &[("a.patch".to_owned(), rebuilt())],
        )
        .unwrap();
        // Kill the replacement so only the rename into place can fail, after the
        // original has already been moved aside.
        std::fs::remove_dir_all(l.tmp_dir().join("default.new")).unwrap();

        let err = built.replace(&l).unwrap_err();
        assert!(format!("{err:#}").contains("installing"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("a.patch")).unwrap(),
            "original\n",
            "the series must be back where it was, not left as scratch"
        );
        assert!(
            !l.tmp_dir().join("default.old").exists(),
            "a failed install must not leave the original parked in scratch"
        );
        assert!(
            !l.undo_entry().exists(),
            "a failed swap keeps nothing: it wrote nothing"
        );
    }

    #[test]
    fn build_refuses_a_series_file_that_lists_a_patch_it_does_not_hold() {
        // The verification is against what the directory actually contains, not what
        // the caller meant to write: a list without its patch is a series that cannot
        // be applied, and it must die in scratch.
        let (_tmp, l) = layout();
        let err = Built::build(&l, "default", "a.patch\n", None, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("a.patch"), "{err:#}");
        assert!(
            !l.patches_dir().exists() || !l.patches_dir().join("series").exists(),
            "the refused build never reaches the live path"
        );
    }

    #[test]
    fn a_build_holding_a_patch_skot_cannot_read_is_refused_in_scratch() {
        // What "verifies it" has to mean. Byte fidelity only proves the write landed:
        // a file libgit2 refuses to read still matches the text it was meant to be given.
        // The header rendered over an empty diff is exactly that shape, and it must die
        // here rather than be renamed over the live series and discovered by whatever
        // reads the series next.
        let (_tmp, l) = layout();
        series_dir(&l.patches_dir(), "original\n");

        let header_only = "From: A U Thor <author@example.com>\n\
                           Date: Thu, 1 Jan 2026 00:00:00 +0000\n\
                           Subject: [PATCH] subject\n\n---\n\n";
        let err = Built::build(
            &l,
            "default",
            "a.patch\n",
            None,
            &[("a.patch".to_owned(), header_only.to_owned())],
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("a.patch"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("a.patch")).unwrap(),
            "original\n",
            "the live series must be untouched by a build that refused to install"
        );
    }

    #[test]
    fn an_empty_patch_is_still_a_patch_a_series_can_hold() {
        // `patch new` writes a 0-byte file, and libgit2 reads it back as zero deltas: an
        // empty patch applies as a no-op, so the check that refuses a header-only file
        // must not refuse this one.
        let (_tmp, l) = layout();
        Built::build(
            &l,
            "default",
            "a.patch\n",
            None,
            &[("a.patch".to_owned(), String::new())],
        )
        .unwrap();
    }

    #[test]
    fn undo_without_an_entry_is_a_message_not_a_swap() {
        let (_tmp, l) = layout();
        series_dir(&l.patches_dir(), "live\n");

        let err = undo_swap(&l).unwrap_err().to_string();
        assert!(err.contains("nothing to undo"), "got {err}");
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("a.patch")).unwrap(),
            "live\n"
        );
    }

    #[test]
    fn installing_over_an_existing_series_is_refused() {
        let (_tmp, l) = layout();
        series_dir(&l.patches_dir(), "live\n");
        let built = Built::build(
            &l,
            "default",
            "a.patch\n",
            None,
            &[("a.patch".to_owned(), rebuilt())],
        )
        .unwrap();

        let err = built.install(&l).unwrap_err().to_string();
        assert!(err.contains("already exists"), "got {err}");
        assert_eq!(
            std::fs::read_to_string(l.patches_dir().join("a.patch")).unwrap(),
            "live\n"
        );
    }
}
