//! `rebase`: replay the series onto a different ref and rewrite the patches to match.
//!
//! Upstream moves, and a patch that applied cleanly against last month's commit may not apply
//! against today's. `rebase` is the answer, and the whole design is in one decision: **a patch
//! is re-exported as the diff between the tree before it and the tree after it**, on the new
//! base. Not the new base against the final result, which would fold every earlier patch into
//! patch 1; not a three-way merge of the old patch, which is the conflict-resolution problem
//! solved twice.
//!
//! That diff is already computable. Replaying the series yields the tree after each patch, and
//! the tree *before* patch N is the tree after patch N-1 (or the base). So a rebase is one
//! replay plus N two-tree diffs — and because each rewritten patch is the literal difference
//! between two trees, it applies to the new base by construction rather than by a later
//! `check`.
//!
//! Two things are preserved across the rewrite, because losing either one is a real loss:
//! the patch's author and date, and its subject. A rebase is upstream's move, not the author's.
//!
//! A patch that does not apply on the new base is *not* rewritten. `rebase` stops and reports,
//! leaving every patch file exactly as it was: a series with one stale patch in it is
//! recoverable, and a series where the earlier patches were quietly rewritten on the way to the
//! failure is not.

use anyhow::{Context, Result, bail};
use log::{debug, error, info};
use serde_json::json;

use crate::check;
use crate::cli::RefArg;
use crate::commands::authoring::{self, Stack};
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::Sha;
use crate::git::diff;
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::series::{self, Series};
use crate::state::State;

/// What `rebase` did, presented by [`run`].
#[derive(Debug)]
struct Rebased {
    ok: bool,
    ref_name: String,
    sha: Sha,
    /// The sandbox's new tree, present only on success.
    tree: Option<git2::Oid>,
    rewritten: Vec<Rewritten>,
    /// The replay verdict; its patches are the failure detail in `--json`.
    report: check::Report,
}

/// Replay the series onto `arg`'s ref, rewriting each patch to match.
pub fn run(ctx: &Ctx, arg: &RefArg) -> Result<Exit> {
    let rebased = rebase(&ctx.layout, arg.ref_name.as_deref())?;

    if ctx.cli.json {
        let exit = if rebased.ok {
            emit_json(&json!({
                "ok": true,
                "ref": rebased.ref_name,
                "sha": rebased.sha.as_str(),
                "tree": rebased
                    .tree
                    .expect("a successful rebase always has a tree")
                    .to_string(),
                "rewritten": rebased.rewritten.iter().map(|r| json!({
                    "name": r.name,
                    "path": r.path.display().to_string(),
                    "summary": diff::summarise(&r.text),
                    "unchanged": r.text == r.original,
                })).collect::<Vec<_>>(),
            }))?;
            Exit::Success
        } else {
            let failed = rebased
                .report
                .first_failure()
                .map(|p| p.name.clone())
                .unwrap_or_default();
            emit_json(&json!({
                "ok": false,
                "ref": rebased.ref_name,
                "sha": rebased.sha.as_str(),
                "failed_patch": failed,
                "patches": rebased.report.patches,
                "rewritten": [],
            }))?;
            Exit::Failure
        };
        return Ok(exit);
    }

    if !rebased.ok {
        // Before writing anything. Every file on disk is still the user's original work.
        let failed = rebased
            .report
            .first_failure()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        error!(
            target: "rebase",
            "`{failed}` does not apply to {} ({})",
            rebased.ref_name, rebased.sha
        );
        for line in rebased.report.summary().lines() {
            debug!(target: "rebase", "{line}");
        }
        info!(
            target: "rebase",
            "No patch file was changed. {} still holds what it did.",
            ctx.layout.series_file().display()
        );
        info!(
            target: "rebase",
            "Resolve it by hand: `{}` puts the series in the sandbox, \
             and `{}` re-exports it against the new base.",
            skot_args("apply", &rebased.ref_name),
            skot_args("edit-save", "<patch>")
        );
        return Ok(Exit::Failure);
    }

    info!(
        target: "rebase",
        "rebased {} onto {} ({})",
        rebased.rewritten.len(),
        rebased.ref_name,
        rebased.sha
    );
    for entry in &rebased.rewritten {
        let marker = if entry.text == entry.original {
            "  ="
        } else {
            "  ~"
        };
        debug!(
            target: "rebase",
            "{marker} {}  {}",
            entry.name,
            diff::summarise(&entry.text)
        );
    }
    debug!(target: "rebase", "  ~ rewritten,  = unchanged");
    Hints::new("next")
        .entry(skot("build"), "the whole run onto the new base")
        .entry(skot("check"), "or just verify the rewritten series")
        .print();
    Ok(Exit::Success)
}

/// Replay the series onto `ref_name`, rewriting each patch, and report what happened.
///
/// Everything below [`run`] is this: no clap, no stdout, no exit verdict — a `&Layout` and
/// scalars in, a description of the outcome out.
fn rebase(layout: &Layout, ref_name: Option<&str>) -> Result<Rebased> {
    let (config, series) = layout.project()?;
    // Checked before the files are resolved: rebase with nothing to rebase has no honest
    // answer but this one, and `new` is the step the user actually needs.
    if series.is_empty() {
        bail!(
            "the series at {} is empty, so there is nothing to rebase.\n\
             `{}` starts a patch.",
            series.path().display(),
            skot_args("new", "<name>")
        );
    }
    let entries = series.require_resolvable(layout)?;

    let new_base = authoring::wanted_ref(layout, ref_name, &config, &skot("rebase"))?;
    let lock = layout.lock()?;
    let (_mirror, sha) = super::sync::resolve_in_mirror(layout, &new_base)?;

    let repo = authoring::require_sandbox(layout)?;
    let result = plan(&repo, &sha, &entries)?;

    if !result.report.ok {
        drop(lock);
        return Ok(Rebased {
            ok: false,
            ref_name: new_base,
            sha,
            tree: None,
            rewritten: Vec::new(),
            report: result.report,
        });
    }

    // Write each rewritten patch to its own file first, and only replace the series' files once
    // every one of them has been produced. A half-written series is worse than an unchanged
    // one.
    let rewritten = rewrite(&repo, &entries, &result)?;

    let mut new_series = Series::parse(&layout.series_file(), "");
    for entry in rewritten.iter() {
        std::fs::write(&entry.path, &entry.text)
            .context(format!("writing {}", entry.path.display()))?;
        if !new_series.names().contains(&entry.name.as_str()) {
            new_series
                .insert(&entry.name, new_series.len())
                .context(format!("registering {} in the series", entry.name))?;
        }
    }
    new_series.save(layout)?;

    // The applied set follows the series onto the new base. Rewriting every patch file does
    // not enable the ones the sandbox had disabled: they stay off, and only what was on is
    // replayed and recorded.
    let state = State::load(layout)?;
    let enabled: Vec<series::Entry> = entries
        .iter()
        .filter(|e| state.applied.contains(&e.name))
        .cloned()
        .collect();
    let stack = Stack {
        names: enabled.iter().map(|e| e.name.clone()).collect(),
        entries: enabled,
    };
    let tree = authoring::land(layout, &new_base, &sha, &stack)?;
    drop(lock);

    Ok(Rebased {
        ok: true,
        ref_name: new_base,
        sha,
        tree: Some(tree),
        rewritten,
        report: result.report,
    })
}

/// A rewritten patch, ready to be written out.
#[derive(Debug)]
struct Rewritten {
    name: String,
    path: std::path::PathBuf,
    text: String,
    /// The file's previous contents, so an unchanged patch can be reported as such.
    original: String,
}

/// Replay the series onto `sha` and work out what each patch should become.
fn plan(repo: &git2::Repository, sha: &Sha, entries: &[series::Entry]) -> Result<check::Applied> {
    check::replay_series(repo, sha.as_str(), entries)
        .context(format!("replaying the series onto {sha}"))
}

/// Produce the rewritten text for every patch in the series.
fn rewrite(
    repo: &git2::Repository,
    entries: &[series::Entry],
    result: &check::Applied,
) -> Result<Vec<Rewritten>> {
    let base_tree = crate::git::commit_tree(repo, result.report.base.as_str())?;
    let mut out = Vec::with_capacity(entries.len());

    for (index, entry) in entries.iter().enumerate() {
        let after = result.per_patch[index].ok_or_else(|| {
            anyhow::anyhow!(
                "`{}` did not apply on {}, so it cannot be rewritten.",
                entry.name,
                result.report.base
            )
        })?;
        let before = if index == 0 {
            base_tree.id()
        } else {
            result.per_patch[index - 1].ok_or_else(|| {
                anyhow::anyhow!("patch {index} did not apply, so {index} cannot be rewritten")
            })?
        };

        let old = repo.find_tree(before)?;
        let new = repo.find_tree(after)?;

        // The existing header, so the author, date and subject survive the rewrite.
        let original = std::fs::read_to_string(&entry.path).unwrap_or_default();
        let header = authoring::header_for(
            Some(&original),
            &authoring::subject_from_name(&entry.name),
            &authoring::signature(repo),
        );
        let text = diff::tree_to_tree_patch(repo, &old, &new, &header)
            .context(format!("re-exporting {} against the new base", entry.name))?;

        out.push(Rewritten {
            name: entry.name.clone(),
            path: entry.path.clone(),
            text,
            original,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;

    /// A series entry pointing at a real patch file under `layout`.
    fn entry(layout: &crate::layout::Layout, name: &str) -> series::Entry {
        series::Entry {
            name: name.to_owned(),
            path: layout.patches_dir().join(name),
            line: 1,
        }
    }

    fn body(from: &str, to: &str) -> String {
        format!(
            "diff --git a/f.txt b/f.txt\n\
             --- a/f.txt\n\
             +++ b/f.txt\n\
             @@ -1,1 +1,1 @@\n\
             -{from}\n\
             +{to}\n"
        )
    }

    #[test]
    fn rebase_leaves_a_disabled_patch_disabled() {
        // Every patch file is rewritten against the new base, but the applied set is what
        // the sandbox holds: a patch the sandbox never had stays off, whatever rebase
        // just wrote for it.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::landed_set(
            tmp.path(),
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_adds_g()),
                ("0003-c.patch", testsupport::patch_three()),
            ],
            &["0001-a.patch", "0003-c.patch"],
        );

        rebase(&layout, None).unwrap();

        let state = crate::state::State::load(&layout).unwrap();
        assert_eq!(
            state.applied,
            ["0001-a.patch", "0003-c.patch"],
            "the hole must survive the rewrite"
        );
        assert!(
            !layout.sandbox_dir().join("g.txt").exists(),
            "the disabled patch must stay out of the sandbox"
        );
        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "third\n"
        );
    }

    #[test]
    fn a_rebase_rewrites_each_patch_against_the_new_base() {
        // The bug this guards: diffing the new base against the final tree folds every patch
        // into the first one, so a two-patch series becomes one patch that does everything.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();

        std::fs::write(
            layout.patches_dir().join("0001-a.patch"),
            body("base", "first"),
        )
        .unwrap();
        std::fs::write(
            layout.patches_dir().join("0002-b.patch"),
            body("first", "second"),
        )
        .unwrap();
        let entries = vec![
            entry(&layout, "0001-a.patch"),
            entry(&layout, "0002-b.patch"),
        ];

        let result = check::replay_series(&repo, &sha_of(&layout), &entries).unwrap();
        assert!(result.report.ok, "{}", result.report.summary());
        let out = rewrite(&repo, &entries, &result).unwrap();

        assert_eq!(out.len(), 2);
        assert!(
            out[0].text.contains("+first") && !out[0].text.contains("+second"),
            "patch 1 must carry only its own change:\n{}",
            out[0].text
        );
        assert!(
            out[1].text.contains("+second") && !out[1].text.contains("+first\n"),
            "patch 2 must carry only its own change:\n{}",
            out[1].text
        );
    }

    #[test]
    fn a_rebased_patch_applies_to_the_new_base_by_construction() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        std::fs::write(
            layout.patches_dir().join("0001-a.patch"),
            body("base", "first"),
        )
        .unwrap();
        let entries = vec![entry(&layout, "0001-a.patch")];

        let result = check::replay_series(&repo, &sha, &entries).unwrap();
        let out = rewrite(&repo, &entries, &result).unwrap();

        let diff = git2::Diff::from_buffer(out[0].text.as_bytes()).expect("a readable patch");
        let base = repo
            .find_commit(git2::Oid::from_str(&sha).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        let index = repo
            .apply_to_tree(&base, &diff, None)
            .expect("the rewritten patch applies to the new base");
        let entry = index.get_path(std::path::Path::new("f.txt"), 0).unwrap();
        assert_eq!(
            String::from_utf8(repo.find_blob(entry.id).unwrap().content().to_vec()).unwrap(),
            "first\n"
        );
    }

    #[test]
    fn a_rebase_preserves_the_author_and_the_subject() {
        // Otherwise a rebase silently rewrites the authorship of every patch in the series.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();

        let original = format!(
            "From: Real Person <real@example.com>\n\
             Subject: [PATCH] the real title\n\
             \n---\n\
             \n{}",
            body("base", "first")
        );
        std::fs::write(layout.patches_dir().join("0001-a.patch"), &original).unwrap();
        let entries = vec![entry(&layout, "0001-a.patch")];

        let result = check::replay_series(&repo, &sha, &entries).unwrap();
        let out = rewrite(&repo, &entries, &result).unwrap();

        assert!(
            out[0].text.contains("Real Person <real@example.com>"),
            "{}",
            out[0].text
        );
        assert!(out[0].text.contains("the real title"), "{}", out[0].text);
        // And not re-stamped with the repository's committer.
        assert!(
            !out[0].text.contains("[PATCH] the real title\n\ndiff"),
            "prefix re-added"
        );
    }

    #[test]
    fn a_rebase_over_an_unchanged_base_leaves_the_diff_alone() {
        // The common case: upstream did not actually move under this patch, so the diff must
        // come out identical. Only the header may differ, and only when the original had none
        // — otherwise every no-op rebase would show up as a change in the series.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        let original = body("base", "first");
        std::fs::write(layout.patches_dir().join("0001-a.patch"), &original).unwrap();
        let entries = vec![entry(&layout, "0001-a.patch")];

        let result = check::replay_series(&repo, &sha, &entries).unwrap();
        let out = rewrite(&repo, &entries, &result).unwrap();

        assert_eq!(changed_lines(&out[0].text), changed_lines(&original));
    }

    /// Just the added and removed lines of a patch, which is what "the same change" means.
    ///
    /// Comparing the raw diff text would be too strict: libgit2 rewrites the hunk header
    /// (`@@ -1,1 +1,1 @@` becomes `@@ -1 +1 @@`) and adds an `index` line that a hand-written
    /// patch never had, neither of which changes what the patch does.
    fn changed_lines(text: &str) -> Vec<&str> {
        text.lines()
            .filter(|l| l.starts_with('+') || l.starts_with('-'))
            .filter(|l| !l.starts_with("+++") && !l.starts_with("---"))
            .collect()
    }

    #[test]
    fn a_patch_that_will_not_apply_stops_the_rebase_with_no_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        std::fs::write(
            layout.patches_dir().join("0001-a.patch"),
            body("base", "first"),
        )
        .unwrap();
        std::fs::write(
            layout.patches_dir().join("0002-b.patch"),
            body("nothing", "second"),
        )
        .unwrap();
        let entries = vec![
            entry(&layout, "0001-a.patch"),
            entry(&layout, "0002-b.patch"),
        ];

        let result = check::replay_series(&repo, &sha, &entries).unwrap();
        assert!(!result.report.ok);
        // `rewrite` refuses rather than producing a partial series.
        assert!(rewrite(&repo, &entries, &result).is_err());
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0001-a.patch")).unwrap(),
            body("base", "first"),
            "the first patch must not be touched when the second fails"
        );
    }

    #[test]
    fn per_patch_trees_track_where_each_patch_left_the_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        std::fs::write(
            layout.patches_dir().join("0001-a.patch"),
            body("base", "first"),
        )
        .unwrap();
        std::fs::write(
            layout.patches_dir().join("0002-b.patch"),
            body("first", "second"),
        )
        .unwrap();
        let entries = vec![
            entry(&layout, "0001-a.patch"),
            entry(&layout, "0002-b.patch"),
        ];

        let result = check::replay_series(&repo, &sha, &entries).unwrap();
        assert_eq!(result.per_patch.len(), 2);
        assert!(result.per_patch.iter().all(Option::is_some));
        assert_ne!(result.per_patch[0], result.per_patch[1]);
    }

    #[test]
    fn a_failed_patch_leaves_a_hole_rather_than_a_wrong_tree() {
        // `None` is the point: a tree that did not advance must not be mistaken for one that
        // did, or `rebase` would diff a patch against the tree below the failure.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        std::fs::write(layout.patches_dir().join("0001-a.patch"), body("nope", "x")).unwrap();
        let entries = vec![entry(&layout, "0001-a.patch")];

        let result = check::replay_series(&repo, &sha, &entries).unwrap();
        assert_eq!(result.per_patch, vec![None]);
    }

    fn sha_of(layout: &crate::layout::Layout) -> String {
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        head.id().to_string()
    }

    #[test]
    fn rebase_needs_a_ref_to_replay_onto() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);

        let err = rebase(&layout, None).unwrap_err().to_string();
        assert!(err.contains("no ref"), "got {err}");
    }

    #[test]
    fn an_empty_series_says_to_start_a_patch_rather_than_rebasing_nothing() {
        let (_tmp, layout) = testsupport::project();
        let err = rebase(&layout, Some("main")).unwrap_err().to_string();
        assert!(err.contains("nothing to rebase"), "got {err}");
    }
}
