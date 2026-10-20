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
//!
//! Nothing that reads a rebuilt patch runs after the rename. The landing, which replays the
//! applied set onto the new base, checks the sandbox out and records what it did, reads those
//! files out of scratch while they are still scratch, so the rename is the last thing the
//! command does. Every check runs on this side of it, and a failure anywhere reports a series
//! that is exactly where it was, which is the whole reason the replacement is built somewhere
//! else first.
//!
//! The rewrite is written to scratch and renamed into place, so the series on disk is either
//! the old one or the new one, never a half-written mix. `--into` puts the result under a new
//! name and leaves the original alone; without it the original becomes the undo entry that
//! `series undo` trades back.

use anyhow::{Context, Result};
use log::{debug, error, info};
use serde_json::json;

use crate::check;
use crate::cli::RebaseArgs;
use crate::commands::apply;
use crate::commands::authoring::{self, Stack};
use crate::commands::series::refuse_case_clash;
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::Sha;
use crate::git::diff;
use crate::git::sandbox;
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::series;
use crate::seriesdir;
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
    /// Where the series as it was before this rebase was kept, on success. A failed
    /// rebase writes nothing, so there is nothing to keep. `--into` leaves the
    /// original alone, so there is nothing to keep either.
    undo: Option<std::path::PathBuf>,
    /// The name `--into` asked for, when it did.
    into: Option<String>,
}

/// Replay the series onto `arg`'s ref, rewriting each patch to match.
pub fn run(ctx: &Ctx, arg: &RebaseArgs) -> Result<Exit> {
    let rebased = rebase(&ctx.layout, arg.ref_name.as_deref(), arg.into.as_deref())?;

    if ctx.cli.json {
        let exit = if rebased.ok {
            emit_json(&json!({
                "ok": true,
                "series_name": ctx.layout.series(),
                "into": rebased.into,
                "ref": rebased.ref_name,
                "sha": rebased.sha.as_str(),
                "tree": rebased
                    .tree
                    .map(|tree| tree.to_string()),
                "undo": rebased
                    .undo
                    .as_ref()
                    .map(|path| path.display().to_string()),
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
                "series_name": ctx.layout.series(),
                "into": rebased.into,
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
            skot_args("save", "<patch>")
        );
        return Ok(Exit::Failure);
    }

    if let Some(into) = &rebased.into {
        // The original was never touched; the rewritten series stands under a new name.
        info!(
            target: "rebase",
            "rebased {} onto {} ({}) as `{into}`; `{}` still holds what it did",
            rebased.rewritten.len(),
            rebased.ref_name,
            rebased.sha,
            ctx.layout.series()
        );
        Hints::new("next")
            .entry(
                skot_args("series", &format!("use {into}")),
                "switch to the rebased copy",
            )
            .entry(
                skot_args("series", &format!("drop {into}")),
                "or throw it away",
            )
            .print();
        return Ok(Exit::Success);
    }

    if rebased.rewritten.is_empty() {
        // An empty series: the base moved and the sandbox followed, and there is no
        // per-patch story to tell.
        info!(
            target: "rebase",
            "nothing to rebase; the base is now {} ({})",
            rebased.ref_name, rebased.sha
        );
        Hints::new("next")
            .entry(skot_args("new", "<name>"), "start a patch on the new base")
            .print();
        return Ok(Exit::Success);
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
    info!(
        target: "rebase",
        "the previous series is kept; `{}` puts it back",
        skot_args("series", "undo")
    );
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
fn rebase(layout: &Layout, ref_name: Option<&str>, into: Option<&str>) -> Result<Rebased> {
    let (config, series) = layout.project()?;
    let new_base = authoring::wanted_ref(layout, ref_name, &config, &skot("rebase"))?;

    // An empty series has nothing to replay, so the rebase is the base move alone.
    if series.is_empty() {
        return rebase_empty(layout, &new_base, into);
    }
    let entries = series.require_resolvable(layout)?;
    // The destination, settled before the sandbox moves or a single file is written.
    let target = destination(layout, into)?;

    let lock = layout.lock()?;
    seriesdir::recover(layout)?;
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
            undo: None,
            into: into.map(str::to_owned),
        });
    }

    let rewritten = rewrite(&repo, &entries, &result)?;

    // The series file the rewrite produces: the rewritten names, in order. Comments
    // from the old file are not carried across, which is what rebase has always done.
    let mut series_text = String::new();
    for entry in rewritten.iter() {
        series_text.push_str(&entry.name);
        series_text.push('\n');
    }
    let base_text = Layout::base_text(&new_base, Some(sha.as_str()));
    let patches: Vec<(String, String)> = rewritten
        .iter()
        .map(|entry| (entry.name.clone(), entry.text.clone()))
        .collect();

    let built = seriesdir::Built::build(
        &target,
        target.series(),
        &series_text,
        Some(&base_text),
        &patches,
    )?;

    // The landing is the proof of the rebuilt series, and it runs against the files in
    // scratch, before the rename that makes them the series. Nothing that reads a patch
    // may happen after that rename: the replay, the checkout and the record are the proof,
    // and they belong on this side of the line. Once the rename has happened there is
    // nothing left to read and nothing left to check, so the rename is the last thing the
    // command does and a failure anywhere above it reports a series where it was.
    let (undo, tree) = match into {
        Some(_) => {
            built.install(&target)?;
            (None, None)
        }
        None => {
            let state = State::load(layout)?;
            let applied = state.applied(layout);
            // The landing reads the patches themselves, so it reads them from scratch:
            // the files about to be renamed in, not the ones being renamed out.
            let enabled: Vec<series::Entry> = entries
                .iter()
                .filter(|entry| applied.contains(&entry.name))
                .map(|entry| series::Entry {
                    name: entry.name.clone(),
                    path: built.dir().join(&entry.name),
                    line: entry.line,
                })
                .collect();
            let stack = Stack {
                names: enabled.iter().map(|entry| entry.name.clone()).collect(),
                entries: enabled,
            };
            let tree = authoring::land(layout, &new_base, &sha, &stack)?;
            (Some(built.replace(layout)?), Some(tree))
        }
    };
    drop(lock);

    Ok(Rebased {
        ok: true,
        ref_name: new_base,
        sha,
        tree,
        rewritten,
        report: result.report,
        undo,
        into: into.map(str::to_owned),
    })
}

/// Move the base of an empty series: the same swap, with no patches in it.
fn rebase_empty(layout: &Layout, new_base: &str, into: Option<&str>) -> Result<Rebased> {
    // The destination, settled before the sandbox moves or a single file is written.
    let target = destination(layout, into)?;

    let lock = layout.lock()?;
    seriesdir::recover(layout)?;

    // The report the run hands back: the apply that moved the sandbox, or an empty
    // success when `--into` took the sandbox nowhere.
    let mut report = None;
    let (ref_name, sha) = match into {
        Some(_) => {
            // `--into` on an empty series still resolves the ref, but takes the sandbox
            // nowhere: the new series gets the base file and the original is left as is.
            let (_mirror, sha) = super::sync::resolve_in_mirror(layout, new_base)?;
            (new_base.to_owned(), sha)
        }
        None => {
            // The sandbox follows the base onto the new ref, and the series records it
            // in the same swap that keeps everything else consistent.
            let applied = apply::apply_locked(layout, Some(new_base))?;
            if !applied.ok {
                drop(lock);
                return Ok(Rebased {
                    ok: false,
                    ref_name: applied.ref_name,
                    sha: applied.sha,
                    tree: None,
                    rewritten: Vec::new(),
                    report: applied.report,
                    undo: None,
                    into: None,
                });
            }
            report = Some(applied.report);
            (applied.ref_name, applied.sha)
        }
    };

    // The series file as it stands: an empty series still has a file, possibly with
    // comments, and those comments are not rebase's to drop.
    let series_text = std::fs::read_to_string(layout.series_file()).unwrap_or_default();
    let base_text = Layout::base_text(&ref_name, Some(sha.as_str()));
    let built = seriesdir::Built::build(
        &target,
        target.series(),
        &series_text,
        Some(&base_text),
        &[],
    )?;

    let undo = match into {
        Some(_) => {
            built.install(&target)?;
            None
        }
        None => Some(built.replace(layout)?),
    };
    let tree = if undo.is_some() {
        sandbox::head_tree_oid(layout).ok().flatten()
    } else {
        None
    };
    drop(lock);

    let report = report.unwrap_or_else(|| check::Report {
        base: ref_name.clone(),
        patch_count: 0,
        ok: true,
        patches: Vec::new(),
    });

    Ok(Rebased {
        ok: true,
        ref_name,
        sha,
        tree,
        rewritten: Vec::new(),
        report,
        undo,
        into: into.map(str::to_owned),
    })
}

/// Where the rebased series lands: under `--into`'s name, otherwise the active series.
///
/// The name is checked here rather than at the rename, so a refusal costs no work and
/// an invalid name never reaches `.skot/tmp`.
fn destination(layout: &Layout, into: Option<&str>) -> Result<Layout> {
    let Some(name) = into else {
        return Ok(layout.clone());
    };
    refuse_case_clash(layout, name)?;
    let target = layout.with_series(name)?;
    if target.patches_dir().exists() {
        anyhow::bail!(
            "the series `{name}` already exists.\n`{}` switches to it.",
            skot_args("series", &format!("use {name}"))
        );
    }
    Ok(target)
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
    fn an_empty_patch_stays_empty_through_a_rebase() {
        // `patch new` writes a 0-byte patch: it applies as a no-op, so check, push and pop all
        // take it. The rewrite used to render a header over that empty diff, producing a file
        // libgit2 reads back as "no patch found": the rebase then corrupted the very series
        // it was rewriting. An empty patch must round-trip as empty.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::landed_set(
            tmp.path(),
            &[("a.patch", ""), ("b.patch", testsupport::patch_one())],
            &["a.patch", "b.patch"],
        );

        let rebased = rebase(&layout, None, None).unwrap();
        assert!(rebased.ok, "{}", rebased.report.summary());

        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            "",
            "an empty patch must stay empty, not become an unparseable header"
        );

        // The whole series still reads back: nothing in it is a file skot cannot parse.
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let result = check::replay_series(&repo, &sha_of(&layout), &entries).unwrap();
        assert!(result.report.ok, "{}", result.report.summary());
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
                ("a.patch", testsupport::patch_one()),
                ("b.patch", testsupport::patch_adds_g()),
                ("c.patch", testsupport::patch_three()),
            ],
            &["a.patch", "c.patch"],
        );

        rebase(&layout, None, None).unwrap();

        let state = crate::state::State::load(&layout).unwrap();
        assert_eq!(
            state.applied(&layout).to_vec(),
            ["a.patch", "c.patch"],
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

        std::fs::write(layout.patches_dir().join("a.patch"), body("base", "first")).unwrap();
        std::fs::write(
            layout.patches_dir().join("b.patch"),
            body("first", "second"),
        )
        .unwrap();
        let entries = vec![entry(&layout, "a.patch"), entry(&layout, "b.patch")];

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
        std::fs::write(layout.patches_dir().join("a.patch"), body("base", "first")).unwrap();
        let entries = vec![entry(&layout, "a.patch")];

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
        std::fs::write(layout.patches_dir().join("a.patch"), &original).unwrap();
        let entries = vec![entry(&layout, "a.patch")];

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
        std::fs::write(layout.patches_dir().join("a.patch"), &original).unwrap();
        let entries = vec![entry(&layout, "a.patch")];

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
        std::fs::write(layout.patches_dir().join("a.patch"), body("base", "first")).unwrap();
        std::fs::write(
            layout.patches_dir().join("b.patch"),
            body("nothing", "second"),
        )
        .unwrap();
        let entries = vec![entry(&layout, "a.patch"), entry(&layout, "b.patch")];

        let result = check::replay_series(&repo, &sha, &entries).unwrap();
        assert!(!result.report.ok);
        // `rewrite` refuses rather than producing a partial series.
        assert!(rewrite(&repo, &entries, &result).is_err());
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            body("base", "first"),
            "the first patch must not be touched when the second fails"
        );
    }

    #[test]
    fn a_landing_that_cannot_be_recorded_leaves_the_series_where_it_was() {
        // The landing is where the rebuilt series is proved: replay it onto the new base,
        // check it out, record it. It used to run after the rename that installed the
        // rebuilt series, so it could report a failure with the rewrite already on disk,
        // which is the one thing the rename exists to prevent. Blocking the write that
        // records the landing must leave the series exactly as it was.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::landed_set(
            tmp.path(),
            &[
                ("a.patch", testsupport::patch_one()),
                ("b.patch", testsupport::patch_two()),
            ],
            &["a.patch", "b.patch"],
        );
        // The landing's record is written here, and nothing else in a rebase writes here.
        std::fs::create_dir(layout.state_file().with_extension("json.tmp")).unwrap();

        let err = rebase(&layout, None, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("state.json.tmp"),
            "the failure must be the one that was blocked, got {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            testsupport::patch_one(),
            "the series must be untouched by a rebase that failed"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap(),
            testsupport::patch_two(),
            "every patch, not only the first"
        );
        assert!(
            !layout.undo_entry().exists(),
            "nothing was replaced, so nothing may be waiting in the undo slot"
        );
    }

    #[test]
    fn per_patch_trees_track_where_each_patch_left_the_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        std::fs::write(layout.patches_dir().join("a.patch"), body("base", "first")).unwrap();
        std::fs::write(
            layout.patches_dir().join("b.patch"),
            body("first", "second"),
        )
        .unwrap();
        let entries = vec![entry(&layout, "a.patch"), entry(&layout, "b.patch")];

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
        std::fs::write(layout.patches_dir().join("a.patch"), body("nope", "x")).unwrap();
        let entries = vec![entry(&layout, "a.patch")];

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
        testsupport::write_series(&layout, &[("a.patch", "")]);

        let err = rebase(&layout, None, None).unwrap_err().to_string();
        assert!(err.contains("no ref"), "got {err}");
    }

    #[test]
    fn a_rebase_keeps_the_series_it_was_before() {
        // The rewrite replaces every patch file; the undo entry is what makes that
        // recoverable without the user having remembered to copy anything first.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::landed_set(
            tmp.path(),
            &[("a.patch", testsupport::patch_one())],
            &["a.patch"],
        );

        let rebased = rebase(&layout, None, None).unwrap();
        assert!(rebased.ok);

        let undo = rebased.undo.expect("a rebase keeps the old series");
        assert_eq!(undo, layout.undo_entry());
        assert_eq!(
            std::fs::read_to_string(undo.join("a.patch")).unwrap(),
            testsupport::patch_one(),
            "the undo entry holds the patches as they were"
        );
        assert_eq!(
            std::fs::read_to_string(undo.join("series")).unwrap(),
            "a.patch\n"
        );
        // Nothing was written to scratch and left there.
        assert!(
            !layout
                .tmp_dir()
                .join(format!("{}.new", layout.series()))
                .exists(),
            "the built directory must be renamed away, not left in tmp"
        );
    }

    #[test]
    fn a_failed_rebase_keeps_nothing_because_it_changed_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::landed_set(
            tmp.path(),
            &[
                ("a.patch", testsupport::patch_one()),
                ("b.patch", "not a patch\n"),
            ],
            &["a.patch"],
        );

        let rebased = rebase(&layout, None, None).unwrap();
        assert!(!rebased.ok);
        assert!(rebased.undo.is_none());
        // The failure happens in planning, before a build exists; nothing waits in tmp
        // and nothing was handed to the undo slot.
        let left_in_tmp = std::fs::read_dir(layout.tmp_dir())
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        assert_eq!(left_in_tmp, 0, "scratch must be empty after a failure");
        assert!(
            !layout.undo_entry().exists(),
            "a failure keeps no undo entry"
        );
    }

    #[test]
    fn the_undo_entry_holds_one_version_and_the_next_rebase_replaces_it() {
        // One deep by construction: the swap hands the live directory over, so the
        // buffer is always exactly "what this rebase replaced".
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::landed_set(
            tmp.path(),
            &[("a.patch", testsupport::patch_one())],
            &["a.patch"],
        );

        let first = rebase(&layout, None, None).unwrap();
        let first_undo = first.undo.unwrap();
        let first_series = std::fs::read_to_string(first_undo.join("series")).unwrap();

        let second = rebase(&layout, None, None).unwrap();
        let second_undo = second.undo.unwrap();

        assert_eq!(
            second_undo, first_undo,
            "the undo slot is one path, not a growing list"
        );
        assert_eq!(
            std::fs::read_to_string(second_undo.join("series")).unwrap(),
            first_series,
            "one successful rebase replaces the held version with its own predecessor"
        );
    }

    #[test]
    fn rebase_into_builds_a_new_series_and_leaves_the_original_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::landed_set(
            tmp.path(),
            &[("a.patch", testsupport::patch_one())],
            &["a.patch"],
        );
        let before = std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap();

        let rebased = rebase(&layout, None, Some("experiment")).unwrap();
        assert!(rebased.ok);
        assert!(rebased.undo.is_none(), "--into keeps no undo entry");

        // The original is byte-for-byte what it was, and holds its own base.
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            before,
            "the series --into was asked not to touch was touched"
        );
        assert!(
            !layout.undo_entry().exists(),
            "--into has nothing to undo: the original still stands"
        );

        // The new series holds the rebased patches and the new base, and is not active.
        let target = layout.with_series("experiment").unwrap();
        assert!(target.patches_dir().join("a.patch").is_file());
        assert_eq!(
            std::fs::read_to_string(target.base_file())
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap(),
            "main"
        );
        assert_eq!(layout.series(), "default", "--into does not switch");
    }

    #[test]
    fn rebase_into_an_existing_series_is_refused_before_anything_is_built() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::landed_set(
            tmp.path(),
            &[("a.patch", testsupport::patch_one())],
            &["a.patch"],
        );
        let taken = layout.with_series("taken").unwrap();
        std::fs::create_dir_all(taken.patches_dir()).unwrap();
        testsupport::write_series(&taken, &[("a.patch", "x\n")]);

        let err = rebase(&layout, None, Some("taken")).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("already exists"), "got {text}");
        assert_eq!(
            std::fs::read_to_string(
                layout
                    .with_series("taken")
                    .unwrap()
                    .patches_dir()
                    .join("a.patch")
            )
            .unwrap(),
            "x\n",
            "the existing series is untouched"
        );
        // The refusal came before the build, so scratch never held anything.
        assert!(
            !layout
                .tmp_dir()
                .join(format!("{}.new", layout.series()))
                .exists(),
            "nothing should be built for a refused name"
        );
    }

    #[test]
    fn an_empty_series_moves_the_base_and_takes_the_sandbox_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::project_with_sandbox(tmp.path());

        let rebased = rebase(&layout, Some("main"), None).unwrap();

        assert!(rebased.ok);
        assert!(rebased.rewritten.is_empty());
        assert!(
            rebased.undo.is_some(),
            "even a base move keeps the series as it was"
        );
        assert_eq!(rebased.ref_name, "main");
        let base = std::fs::read_to_string(layout.base_file()).unwrap();
        assert!(
            base.starts_with("main "),
            "the base file records the ref and the commit it moved to, got {base:?}"
        );
        assert_eq!(
            rebased.sha.as_str(),
            base.split_whitespace().nth(1).unwrap(),
            "the recorded sha is the one the rebase landed on"
        );
        let state = State::load(&layout).unwrap();
        assert_eq!(state.ref_name(&layout), Some("main"));
        assert!(
            state.applied(&layout).is_empty(),
            "an empty series applies nothing"
        );
    }
}
