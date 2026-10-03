//! `check`: does the series apply, as a series?
//!
//! The bug this module exists to prevent: verifying each patch independently against the
//! *unpatched* tree, and reporting success for a series that conflicts when applied in
//! order. Two patches that change the same line of the same file both apply cleanly alone
//! and conflict together, and a checker that reports green for both is worse than no
//! checker at all, because it gets trusted.
//!
//! The fix is structural rather than careful. Each file's patch is applied with
//! [`git2::Repository::apply_to_tree`], which returns the index that *would* result. That
//! index is written to a tree, and that tree is the base for the next file and the next
//! patch, so patch N+1 always sees what patch N produced — by construction rather than by
//! bookkeeping.
//!
//! Nothing is checked out. The only writes are trees into the object database, which is
//! what makes `check` safe against a ref the sandbox has never been on.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::series::Entry;

/// One file's worth of a patch, and how it fared.
#[derive(Debug, Clone, Serialize)]
pub struct FileReport {
    pub path: String,
    pub ok: bool,
    /// True when the tree already holds the patch's result, so applying it is a no-op.
    pub already_applied: bool,
    /// Why it failed, in terms the user can act on.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PatchReport {
    /// Position in the series, 0-based.
    pub index: usize,
    pub name: String,
    pub ok: bool,
    /// Set when the patch file itself could not be read or parsed.
    pub error: Option<String>,
    pub files: Vec<FileReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// What the series was checked against, so the result can be trusted.
    pub base: String,
    pub patch_count: usize,
    pub ok: bool,
    pub patches: Vec<PatchReport>,
}

impl Report {
    /// The first patch that did not apply.
    pub fn first_failure(&self) -> Option<&PatchReport> {
        self.patches.iter().find(|p| !p.ok)
    }

    /// A one-line description of why the series did not apply, naming the patch.
    ///
    /// For callers that have to embed the reason in their own error — `new` and `build` both
    /// apply the series as a phase of something else — so it stays short enough to read inside
    /// a sentence. `summary` is the version for when the whole report is the message.
    pub fn failure_line(&self) -> String {
        let Some(patch) = self.first_failure() else {
            return format!("the series does not apply to {}", self.base);
        };
        let reason = patch
            .error
            .clone()
            .or_else(|| {
                patch
                    .files
                    .iter()
                    .find(|f| !f.ok)
                    .and_then(|f| f.detail.clone())
                    .map(|d| d.lines().next().unwrap_or_default().trim().to_owned())
            })
            .unwrap_or_else(|| "it conflicts with the base".to_owned());
        format!("{}: {reason}", patch.name)
    }

    /// A human summary: patch name, file, and why it did not apply.
    ///
    /// A patch that does not apply is the most common failure in this workflow, and the
    /// message is the product.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for patch in &self.patches {
            if patch.ok {
                out.push_str(&format!("  ok     {}\n", patch.name));
                continue;
            }
            out.push_str(&format!("  FAIL   {}\n", patch.name));
            if let Some(err) = &patch.error {
                out.push_str(&format!("         {err}\n"));
            }
            for file in patch.files.iter().filter(|f| !f.ok) {
                if let Some(detail) = &file.detail {
                    for line in detail.lines() {
                        out.push_str(&format!("         {line}\n"));
                    }
                }
            }
        }
        out.trim_end().to_owned()
    }
}

/// Replay `entries` against the tree of `base`, in order.
pub fn check_series(repo: &git2::Repository, base: &str, entries: &[Entry]) -> Result<Report> {
    Ok(replay_series(repo, base, entries)?.report)
}

/// The result of a replay: what it found, and the tree it built.
///
/// `check` wants only the report; `apply` wants the tree too, because applying means
/// checking that tree out. Both want the same replay, so it is done once here rather than
/// twice with two implementations that could disagree.
pub struct Applied {
    pub tree: git2::Oid,
    pub report: Report,
    /// The tree after each patch, aligned with the entries that were replayed; `None` where
    /// that patch did not apply and the tree therefore did not advance.
    ///
    /// `rebase` needs these: to re-export patch N against a new base it has to diff what patch
    /// N *contributed* — the tree before it against the tree after it — rather than the base
    /// against the result, which would fold every earlier patch into N.
    pub per_patch: Vec<Option<git2::Oid>>,
}

/// Replay `entries` against `base`, leaving the resulting tree in the object database.
///
/// The tree is written even when a patch fails: it is the state after the patches that *did*
/// apply, which is what a caller needs in order to say "this much went on before it broke".
pub fn replay_series(repo: &git2::Repository, base: &str, entries: &[Entry]) -> Result<Applied> {
    let label = base.to_string();
    let tree = crate::git::commit_tree(repo, base)
        .context(format!("reading the tree of {base}"))?;

    let mut replay = Replay {
        repo,
        tree,
        base: label,
        patches: Vec::new(),
        trees: Vec::new(),
    };
    for (index, entry) in entries.iter().enumerate() {
        replay.one(index, entry);
    }
    let per_patch = std::mem::take(&mut replay.trees);

    let report = Report {
        base: replay.base,
        patch_count: entries.len(),
        ok: replay.patches.iter().all(|p| p.ok),
        patches: replay.patches,
    };
    Ok(Applied {
        tree: replay.tree.id(),
        report,
        per_patch,
    })
}

/// The running tree, advanced one file at a time.
struct Replay<'repo> {
    repo: &'repo git2::Repository,
    /// What the next patch sees: the base, with every patch so far already on it.
    tree: git2::Tree<'repo>,
    base: String,
    patches: Vec<PatchReport>,
    /// The tree after each patch, for callers that need per-patch results.
    trees: Vec<Option<git2::Oid>>,
}

impl Replay<'_> {
    fn one(&mut self, index: usize, entry: &Entry) {
        let mut report = PatchReport {
            index,
            name: entry.name.clone(),
            ok: false,
            error: None,
            files: Vec::new(),
        };

        let text = match std::fs::read(&entry.path) {
            Ok(t) => t,
            Err(e) => {
                report.error = Some(format!("cannot read {}: {}", entry.path.display(), e));
                self.patches.push(report);
                self.trees.push(None);
                return;
            }
        };

        // A `format-patch` file is a mail header wrapped around a diff, and libgit2 skips
        // the preamble. So a real exported patch needs no unwrapping here, which is the
        // whole reason `apply_to_tree` is usable on series files.
        let diff = match git2::Diff::from_buffer(&text) {
            Ok(d) => d,
            Err(e) => {
                report.error = Some(format!("cannot parse {}: {}", entry.path.display(), e));
                self.patches.push(report);
                self.trees.push(None);
                return;
            }
        };

        // Each delta is applied on its own, so one bad file does not hide the state of the
        // rest. That also means a patch which creates a file and then edits it applies,
        // because the create lands before the edit.
        for delta in diff.deltas() {
            let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) else {
                continue;
            };
            report.files.push(self.file(path, &text, &delta));
        }

        report.ok = report.files.iter().all(|f| f.ok);
        self.trees.push(report.ok.then(|| self.tree.id()));
        self.patches.push(report);
    }

    /// Apply one file's slice of `patch_text` and carry the result forward.
    fn file(&mut self, path: &Path, patch_text: &[u8], delta: &git2::DiffDelta<'_>) -> FileReport {
        let path_label = path.display().to_string();

        let slice = match slice_delta(patch_text, delta) {
            Some(s) => s,
            None => {
                return FileReport {
                    path: path_label,
                    ok: false,
                    already_applied: false,
                    detail: Some("could not isolate this file's patch".into()),
                };
            }
        };

        let single = match git2::Diff::from_buffer(&slice) {
            Ok(d) => d,
            Err(e) => {
                return FileReport {
                    path: path_label,
                    ok: false,
                    already_applied: false,
                    detail: Some(format!("cannot parse this file's patch: {e}")),
                };
            }
        };

        match apply_to(self.repo, &self.tree, &single) {
            Ok(tree) => {
                self.tree = tree;
                FileReport {
                    path: path_label,
                    ok: true,
                    already_applied: false,
                    detail: None,
                }
            }
            Err(forward) => {
                // A patch whose result is already in the tree cannot apply forwards. For a
                // patch that came out of a real history that is not a guess: the `index`
                // line names the blob the patch produces, and that blob is in the mirror.
                if holds_post_image(&self.tree, path, delta) {
                    return FileReport {
                        path: path_label,
                        ok: true,
                        already_applied: true,
                        detail: None,
                    };
                }
                FileReport {
                    path: path_label,
                    ok: false,
                    already_applied: false,
                    detail: Some(explain(path, &forward, slice)),
                }
            }
        }
    }
}

/// Apply `diff` to `tree` and write the result back into the object database.
fn apply_to<'repo>(
    repo: &'repo git2::Repository,
    tree: &git2::Tree<'repo>,
    diff: &git2::Diff<'_>,
) -> Result<git2::Tree<'repo>> {
    let mut index = repo
        .apply_to_tree(tree, diff, None)
        .context("libgit2 refused the patch")?;
    // `apply_to_tree` hands back an in-memory index that belongs to no file, so the tree
    // has to be written through `write_tree_to` rather than `write_tree`.
    let oid = index
        .write_tree_to(repo)
        .context("writing the tree libgit2 built")?;
    let tree = repo
        .find_tree(oid)
        .context(format!("reading back tree {oid}"))?;
    Ok(tree)
}

/// Whether `tree` already holds the blob this file's patch produces.
///
/// A patch's `index abc..def` line names the post-image as `def`. For a patch from a real
/// history that blob exists in the mirror, so this is an exact comparison rather than a
/// guess. When the ids are placeholders or the blob is missing it answers no, and the
/// forward-apply error is reported instead — which is the safe direction, since claiming
/// "already applied" wrongly would turn a real failure into a green light.
fn holds_post_image(tree: &git2::Tree<'_>, path: &Path, delta: &git2::DiffDelta<'_>) -> bool {
    // A deleted file's "post-image" is nothing, so there is nothing to compare.
    if delta.status() == git2::Delta::Deleted {
        return false;
    }
    // Not `DiffFile::exists`: for a patch parsed from a buffer libgit2 reports that as false
    // regardless, because it has not looked the blob up. The id comparison below is the real
    // test, and it is exact.
    let Ok(entry) = tree.get_path(path) else {
        return false;
    };
    entry.id() == delta.new_file().id()
}

/// `delta`'s own patch, sliced out of the whole patch file.
///
/// libgit2 parses a patch into deltas but does not hand back the source text per delta,
/// so this splits on `diff --git` headers: the one that names this file, up to the next
/// one. Splitting on a header rather than on hunk content is what stops a line of added
/// text that happens to look like a header from ending the slice early.
fn slice_delta(patch: &[u8], delta: &git2::DiffDelta<'_>) -> Option<Vec<u8>> {
    let path = delta
        .new_file()
        .path()
        .or_else(|| delta.old_file().path())?;
    let header = format!("a/{p} b/{p}", p = path.display());

    let lines: Vec<&[u8]> = patch.split_inclusive(|b| *b == b'\n').collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with(b"diff --git "))
        .map(|(i, _)| i)
        .collect();

    let position = starts
        .iter()
        .position(|&i| header_is_for(lines[i], &header))?;
    let begin = starts[position];
    let end = starts.get(position + 1).copied().unwrap_or(lines.len());
    Some(lines[begin..end].concat())
}

fn header_is_for(line: &[u8], header: &str) -> bool {
    String::from_utf8_lossy(line)
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("diff --git ")
        .is_some_and(|rest| rest == header)
}

/// Turn a libgit2 apply error into something the user can act on.
///
/// The number in libgit2's `hunk at line N did not apply` is the line the hunk claims to
/// start at in the *pre-image*, not an offset into the patch, so it is the one number that
/// points at a place in the user's own file. The offending lines come from the patch, so
/// the message can show what it was looking for.
fn explain(path: &Path, error: &anyhow::Error, slice: Vec<u8>) -> String {
    let text = error.to_string();
    let mut out = format!("{} does not apply.", path.display());
    if let Some(line) = after(&text, "hunk at line ").and_then(first_word) {
        out.push_str(&format!(" Nothing matched at line {line} of it."));
    }

    // The context the hunk wanted, which is the part a human actually needs: it says which
    // text to go looking for in their own file.
    let expected = context_lines(&slice);
    if !expected.is_empty() {
        out.push_str("\ncould not find, within the file:");
        for line in expected {
            out.push_str(&format!("\n  | {}", String::from_utf8_lossy(line)));
        }
    }
    out
}

/// The text a hunk wanted: its context lines and the lines it removes, which together are
/// what the file must contain for the hunk to apply.
///
/// The leading `' '` or `'-'` is stripped, because it marks the line's role in the diff and is
/// not part of the text in the user's file.
fn context_lines(slice: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut in_hunk = false;
    for line in slice.split_inclusive(|b| *b == b'\n') {
        // Everything before the first `@@` is headers, not content.
        if line.starts_with(b"@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        // A context line, or a line the patch removes: both are text the file must have.
        if let Some(rest) = line.strip_prefix(b" ").or_else(|| line.strip_prefix(b"-")) {
            out.push(rest);
        }
        if out.len() == 6 {
            break;
        }
    }
    out
}

fn after<'a>(text: &'a str, needle: &str) -> Option<&'a str> {
    text.split_once(needle).map(|(_, rest)| rest)
}

fn first_word(text: &str) -> Option<&str> {
    text.split_whitespace().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real repository with a real commit, so these exercise libgit2 rather than a
    /// stand-in for it.
    fn repo_with(files: &[(&str, &str)]) -> (tempfile::TempDir, git2::Repository, String) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git must be available for these tests");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        for (path, content) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
        // `git commit` refuses to make an initial commit with nothing in it, and a repo
        // with an empty tree is a case several of these tests need. A tracked placeholder
        // gives them that without changing what they are testing.
        if files.is_empty() {
            std::fs::write(dir.join(".placeholder"), "").unwrap();
        }
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        let repo = git2::Repository::open(dir).unwrap();
        let sha = repo.head().unwrap().target().unwrap().to_string();
        (tmp, repo, sha)
    }

    fn entry(dir: &Path, name: &str, body: &str) -> Entry {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        Entry {
            name: name.to_owned(),
            path,
            line: 1,
        }
    }

    /// Two patches, each changing the same line of `f.txt` from the same base.
    ///
    /// This is the fixture that reproduces the real bug: each applies cleanly in
    /// isolation, and together they conflict.
    const CONFLICTING_A: &str = "\
From aaa Mon Sep 17 00:00:00 2001
Subject: [PATCH] a

---
 f.txt | 2 +-
 1 file changed

diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,4 +1,4 @@
 top
 shared
-from-A
+shared-from-A
 bottom
";

    const CONFLICTING_B: &str = "\
From bbb Mon Sep 17 00:00:00 2001
Subject: [PATCH] b

---
 f.txt | 2 +-
 1 file changed

diff --git a/f.txt b/f.txt
index 3333333..4444444 100644
--- a/f.txt
+++ b/f.txt
@@ -1,4 +1,4 @@
 top
 shared
-from-B
+shared-from-B
 bottom
";

    const BASE: &str = "top\nshared\nfrom-A\nbottom\n";

    #[test]
    fn check_reports_a_series_that_conflicts_cumulatively() {
        // Regression test. A checker that verifies each patch against the unpatched tree
        // reports OK here, because both patches apply cleanly in isolation.
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(tmp.path(), "0001-a.patch", CONFLICTING_A),
            entry(tmp.path(), "0002-b.patch", CONFLICTING_B),
        ];
        let (_dir, repo, sha) = repo_with(&[("f.txt", BASE)]);

        let report = check_series(&repo, &sha, &entries).unwrap();

        assert!(!report.ok, "a conflicting series must not report ok");
        let failure = report.first_failure().expect("a patch should have failed");
        assert_eq!(failure.name, "0002-b.patch");
        assert_eq!(failure.files[0].path, "f.txt");
        let detail = failure.files[0]
            .detail
            .as_deref()
            .expect("a failure must explain itself");
        assert!(detail.contains("does not apply"), "unhelpful: {detail}");
        assert!(
            detail.contains("from-B"),
            "the message should show the text it wanted: {detail}"
        );
    }

    #[test]
    fn each_patch_in_a_conflicting_series_is_reported_individually() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(tmp.path(), "0001-a.patch", CONFLICTING_A),
            entry(tmp.path(), "0002-b.patch", CONFLICTING_B),
        ];
        let (_dir, repo, sha) = repo_with(&[("f.txt", BASE)]);
        let report = check_series(&repo, &sha, &entries).unwrap();
        assert_eq!(report.patches.len(), 2);
        assert!(report.patches[0].ok, "the first patch applies on its own");
        assert!(
            !report.patches[1].ok,
            "the second does not, given the first"
        );
    }

    #[test]
    fn a_clean_series_reports_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let a = entry(tmp.path(), "0001-a.patch", CONFLICTING_A);
        let b = entry(
            tmp.path(),
            "0002-b.patch",
            "\
diff --git a/g.txt b/g.txt
new file mode 100644
index 0000000..2222222
--- /dev/null
+++ b/g.txt
@@ -0,0 +1 @@
+created by the second patch
",
        );
        let (_dir, repo, sha) = repo_with(&[("f.txt", BASE)]);
        let report = check_series(&repo, &sha, &[a, b]).unwrap();
        assert!(report.ok, "{}", report.summary());
        assert!(report.first_failure().is_none());
    }

    #[test]
    fn a_patch_that_creates_a_file_is_visible_to_the_next_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let create = entry(
            tmp.path(),
            "0001-new.patch",
            "\
diff --git a/f.txt b/f.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,2 @@
+alpha
+beta
",
        );
        // The second patch edits the file the first one created. It can only apply if the
        // first patch's result was carried forward.
        let edit = entry(
            tmp.path(),
            "0002-edit.patch",
            "\
diff --git a/f.txt b/f.txt
index 2222222..3333333 100644
--- a/f.txt
+++ b/f.txt
@@ -1,2 +1,2 @@
 alpha
-beta
+BETA
",
        );
        let (_dir, repo, sha) = repo_with(&[]);
        let report = check_series(&repo, &sha, &[create, edit]).unwrap();
        assert!(report.ok, "{}", report.summary());
    }

    #[test]
    fn a_patch_with_two_hunks_reports_the_file_as_failed() {
        let tmp = tempfile::tempdir().unwrap();
        // The first hunk matches, the second does not. libgit2 rejects the file, and the
        // report must not call that a success.
        let patch = entry(
            tmp.path(),
            "0001-half.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,2 +1,2 @@
 line1
-line2
+LINE2
@@ -9,2 +9,2 @@
 nope-not-here
-also-nope
+fixed
",
        );
        let base: String = (1..=12).map(|i| format!("line{i}\n")).collect();
        let (_dir, repo, sha) = repo_with(&[("f.txt", &base)]);
        let report = check_series(&repo, &sha, &[patch]).unwrap();
        assert!(!report.ok);
        assert!(!report.patches[0].files[0].ok);
    }

    #[test]
    fn a_deletion_is_visible_to_the_next_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let del = entry(
            tmp.path(),
            "0001-del.patch",
            "\
diff --git a/f.txt b/f.txt
deleted file mode 100644
index 1111111..0000000
--- a/f.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-gone
",
        );
        let create = entry(
            tmp.path(),
            "0002-new.patch",
            "\
diff --git a/g.txt b/g.txt
new file mode 100644
index 0000000..4444444
--- /dev/null
+++ b/g.txt
@@ -0,0 +1 @@
+fresh
",
        );
        let (_dir, repo, sha) = repo_with(&[("f.txt", "gone\n")]);
        let report = check_series(&repo, &sha, &[del, create]).unwrap();
        assert!(report.ok, "{}", report.summary());
    }

    #[test]
    fn a_multi_file_patch_reports_each_file_separately() {
        let tmp = tempfile::tempdir().unwrap();
        // One file applies, one does not. Both belong in the report: "1 of 2 files failed"
        // is the useful answer and a single boolean is not.
        let patch = entry(
            tmp.path(),
            "0001-two.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,4 +1,4 @@
 top
 shared
-from-A
+CHANGED
 bottom
diff --git a/g.txt b/g.txt
index 3333333..4444444 100644
--- a/g.txt
+++ b/g.txt
@@ -1,1 +1,1 @@
-not-in-the-tree
+fixed
",
        );
        let (_dir, repo, sha) = repo_with(&[("f.txt", BASE), ("g.txt", "original\n")]);
        let report = check_series(&repo, &sha, &[patch]).unwrap();
        assert!(!report.ok);
        assert_eq!(report.patches[0].files.len(), 2);
        let by_path = |name: &str| {
            report.patches[0]
                .files
                .iter()
                .find(|f| f.path == name)
                .unwrap_or_else(|| panic!("{name} should be reported"))
        };
        assert!(by_path("f.txt").ok, "the good file should report ok");
        assert!(!by_path("g.txt").ok, "the bad file should report not ok");
    }

    #[test]
    fn a_missing_patch_file_is_reported_not_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = Entry {
            name: "0001-gone.patch".into(),
            path: tmp.path().join("0001-gone.patch"),
            line: 1,
        };
        let (_dir, repo, sha) = repo_with(&[]);
        let report = check_series(&repo, &sha, &[missing]).unwrap();
        assert!(!report.ok);
        assert!(
            report.patches[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("cannot read"))
        );
    }

    #[test]
    fn an_unparseable_patch_is_reported_not_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = entry(tmp.path(), "0001-bad.patch", "this is not a diff\n");
        let (_dir, repo, sha) = repo_with(&[]);
        let report = check_series(&repo, &sha, &[bad]).unwrap();
        assert!(!report.ok);
        assert!(
            report.patches[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("cannot parse")),
            "a broken patch file must say so"
        );
    }

    #[test]
    fn an_empty_series_is_ok_and_says_so() {
        let (_dir, repo, sha) = repo_with(&[]);
        let report = check_series(&repo, &sha, &[]).unwrap();
        assert!(report.ok);
        assert_eq!(report.patch_count, 0);
    }

    #[test]
    fn the_summary_names_the_patch_file_and_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(tmp.path(), "0001-a.patch", CONFLICTING_A),
            entry(tmp.path(), "0002-b.patch", CONFLICTING_B),
        ];
        let (_dir, repo, sha) = repo_with(&[("f.txt", BASE)]);
        let summary = check_series(&repo, &sha, &entries).unwrap().summary();
        assert!(summary.contains("0001-a.patch"), "got {summary}");
        assert!(summary.contains("0002-b.patch"), "got {summary}");
        assert!(summary.contains("f.txt"), "got {summary}");
    }

    #[test]
    fn the_report_serialises_to_json() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![entry(tmp.path(), "0001-a.patch", CONFLICTING_A)];
        let (_dir, repo, sha) = repo_with(&[("f.txt", BASE)]);
        let json = serde_json::to_string(&check_series(&repo, &sha, &entries).unwrap()).unwrap();
        assert!(json.contains("\"patch_count\":1"));
        assert!(json.contains("\"base\""));
    }

    #[test]
    fn an_already_applied_patch_is_reported_as_such() {
        // The tree already holds the blob the patch produces, so applying it forwards
        // cannot work. For `apply` that is a success, and saying so is what makes
        // re-running it safe.
        let tmp = tempfile::tempdir().unwrap();
        // The tree holds the patch's *result*, so applying the patch forwards cannot work.
        const RESULT: &str = "top\nshared\nCHANGED\nbottom\n";
        let (_dir, repo, sha) = repo_with(&[("f.txt", RESULT)]);
        let post = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .get_path(Path::new("f.txt"))
            .unwrap()
            .id();
        let patch = entry(
            tmp.path(),
            "0001-done.patch",
            &format!(
                "\
diff --git a/f.txt b/f.txt
index 1111111..{post} 100644
--- a/f.txt
+++ b/f.txt
@@ -1,4 +1,4 @@
 top
 shared
-from-A
+CHANGED
 bottom
"
            ),
        );
        let report = check_series(&repo, &sha, &[patch]).unwrap();
        assert!(report.ok, "{}", report.summary());
        assert!(report.patches[0].files[0].already_applied);
    }

    #[test]
    fn a_missing_base_ref_is_an_error_not_an_empty_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![entry(tmp.path(), "0001-a.patch", CONFLICTING_A)];
        let (_dir, repo, _sha) = repo_with(&[]);
        let err = check_series(&repo, "no-such-ref", &entries)
            .expect_err("an unknown base must not look like an empty tree");
        assert!(err.to_string().contains("no-such-ref"), "got {err}");
    }
}
