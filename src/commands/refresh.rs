//! `refresh`: turn what you changed in the sandbox into a patch file.
//!
//! This is the command that makes patch authoring a loop rather than a chore. You edit in the
//! sandbox, run `refresh`, and the patch on disk is the diff — no commit, no branch, no
//! rebase, nothing to remember to clean up.
//!
//! Three decisions are worth stating, because each is easy to get wrong, and each produced a
//! patch that looked entirely reasonable while being wrong:
//!
//! - **The base is the tree *below* the patch, not the tree the patch produced.** `apply`
//!   records the tree the whole stack produced, and that is the wrong thing to diff against:
//!   the sandbox is that tree plus your edits, so the diff comes out as *only your edits* and
//!   replaces the patch with a fragment of itself. The patch's own base is the series prefix
//!   beneath it, and the sandbox minus that is the patch's whole change plus your edits.
//! - **Only the patch on top of the applied stack can be refreshed.** One with another above it
//!   has its change interleaved with the ones above, and no single diff can separate them.
//!   `pop` the ones above it first; the error says so.
//! - **Author and date come from the patch being replaced.** Otherwise every refresh re-stamps
//!   the committer and a rebase turns into a rewrite of the whole series' history.

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::RefreshArgs;
use crate::commands::{Ctx, Exit, authoring, emit_json};
use crate::git::diff;
use crate::hints::Hints;
use crate::layout::Layout;
use crate::series::{self, Series};
use crate::state::State;

/// What `refresh` did, presented by [`run`].
#[derive(Debug)]
struct Refreshed {
    name: String,
    path: std::path::PathBuf,
    changed: bool,
    registered: bool,
    insert_at: Option<usize>,
    summary: String,
    bytes: usize,
}

pub fn run(ctx: &Ctx, args: &RefreshArgs) -> Result<Exit> {
    let done = refresh(
        &ctx.layout,
        args.name.as_deref(),
        args.insert_at,
        args.no_register,
    )?;

    if ctx.cli.json {
        if done.changed {
            emit_json(&json!({
                "patch": done.name,
                "path": done.path.display().to_string(),
                "registered": done.registered,
                "insert_at": done.insert_at,
                "summary": done.summary,
                "bytes": done.bytes,
            }))?;
        } else {
            emit_json(&json!({
                "patch": done.name,
                "path": done.path.display().to_string(),
                "changed": false,
                "summary": done.summary,
                "bytes": done.bytes,
            }))?;
        }
        return Ok(Exit::Success);
    }

    if !done.changed {
        info!(target: "refresh", "{} is already up to date", done.name);
        info!(
            target: "refresh",
            "The sandbox matches it. Edit {} to change anything.",
            ctx.layout.sandbox_dir().display()
        );
        return Ok(Exit::Success);
    }

    info!(target: "refresh", "refreshed {}", done.name);
    debug!(target: "refresh", "  {}", done.summary);
    debug!(target: "refresh", "  {}", done.path.display());
    if done.registered {
        debug!(target: "refresh", "registered in the series");
    } else {
        debug!(
            target: "refresh",
            "already in the series, so the series was left alone"
        );
    }
    Hints::new("next")
        .entry("skot check", "re-verify the whole series")
        .entry("skot push", "apply the next patch")
        .print();
    Ok(Exit::Success)
}

/// Export the sandbox's edits into the chosen patch file, registering it if needed.
///
/// Everything below [`run`] is this: no clap, no stdout, no verdict — a `&Layout` and
/// scalars in, a description of the outcome out.
fn refresh(
    layout: &Layout,
    name: Option<&str>,
    insert_at: Option<usize>,
    no_register: bool,
) -> Result<Refreshed> {
    // The sandbox first: everything below needs it, and "there is no sandbox" has one remedy.
    let repo = authoring::require_sandbox(layout)?;

    let (config, series) = layout.project()?;
    let state = State::load(layout)?;

    let lock = layout.lock()?;
    let (target, index) = choose(layout, &series, &state, name)?;
    let entries = series.entries(layout)?;
    let base = authoring::base_tree_for_patch(layout, &state, &config, &repo, &entries, index)?;

    let existing = std::fs::read_to_string(&target.path).unwrap_or_default();
    let header = authoring::header_for(
        Some(&existing),
        &authoring::subject_from_name(&target.name),
        &authoring::signature(&repo),
    );
    let patch = diff::tree_to_patch(&repo, &base, &header)
        .context(format!("exporting your edits as {}", target.name))?;

    // Refreshing a patch you have not touched is normal, not a mistake: it is what running
    // `refresh` twice looks like, and after `new` + `push` it is the only thing that happens.
    // Compare the *body*, because re-rendering the header normalises things the author never
    // wrote — hunk counts and offsets come from libgit2 now, not from whoever typed the patch
    // by hand — and rewriting those bytes is not a change to the patch.
    if change_of(&patch) == change_of(&existing) {
        drop(lock);
        return Ok(Refreshed {
            name: target.name,
            path: target.path,
            changed: false,
            registered: false,
            insert_at: None,
            summary: diff::summarise(&patch),
            bytes: patch.len(),
        });
    }

    // An export with no diff at all, when the existing patch is not empty, means the sandbox
    // no longer contains the change the patch describes — the working tree was reset, or the
    // patch was applied to something else. Writing it would delete the only record of that
    // change, so stop here where the reason can still be explained.
    if !patch.contains("diff --git ") {
        bail!(
            "there is nothing in the sandbox to export for {}, but that patch is not empty.\n\
             The change it describes is not in {} right now.\n\
             If you meant to undo it, `skot pop` and delete the patch; if the sandbox was \
             reset, `skot apply` puts the series back.",
            target.name,
            layout.sandbox_dir().display()
        );
    }

    // Prove the export applies before overwriting anything. A patch file that libgit2 cannot
    // apply back is worse than a stale one, because the series is now broken and the original
    // is gone.
    verify_round_trip(&repo, &base, &patch, &target.name)?;

    std::fs::write(&target.path, &patch).context(format!("writing {}", target.path.display()))?;

    let mut series = series;
    let registered = if no_register {
        false
    } else {
        match series.insert(&target.name, insert_at.unwrap_or(series.len())) {
            Ok(_) => {
                series.save(layout)?;
                true
            }
            // Already in the series is the normal case for a second refresh, not a failure.
            Err(series::SeriesError::Duplicate { .. }) => false,
            Err(e) => return Err(anyhow::Error::from(e)),
        }
    };

    drop(lock);
    Ok(Refreshed {
        name: target.name,
        path: target.path,
        changed: true,
        registered,
        insert_at,
        summary: diff::summarise(&patch),
        bytes: patch.len(),
    })
}

/// What a patch *does*, as opposed to how it is written.
///
/// Skips the parts that are re-derived every time: blob ids in `index`, and the line numbers in
/// hunk headers. Both come out of libgit2 now rather than off the author's keyboard, so they
/// change when a patch is refreshed even though the patch still makes exactly the same edit.
/// Comparing these is what makes `refresh` idempotent; comparing whole files does not.
fn change_of(patch: &str) -> Vec<&str> {
    let body = match patch.find("diff --git ") {
        Some(at) => &patch[at..],
        None => "",
    };
    body.lines()
        .filter(|line| {
            !line.starts_with("diff --git")
                && !line.starts_with("index ")
                && !line.starts_with("@@")
                && !line.starts_with("+++")
                && !line.starts_with("---")
        })
        .collect()
}

/// Which patch to refresh — the named one, or the last one applied — and where it sits in the
/// series.
fn choose(
    layout: &Layout,
    series: &Series,
    state: &State,
    name: Option<&str>,
) -> Result<(series::Entry, usize)> {
    let entries = series.entries_lenient(layout);

    if let Some(name) = name {
        let Some(found) = entries
            .iter()
            .find(|e| e.name == name || e.path.file_name().is_some_and(|f| f == name))
        else {
            let known: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            anyhow::bail!(
                "`{name}` is not in the series. Patches in it: {}",
                if known.is_empty() {
                    "(none)".to_owned()
                } else {
                    known.join(", ")
                }
            );
        };

        // Only the patch on top of the applied stack can be exported from the working tree, and
        // this is the one case where being permissive would be actively harmful.
        //
        // The sandbox holds *base plus the whole applied series* plus your edits. Any patch with
        // another on top of it therefore has its own contribution interleaved with the ones
        // above it, and no single diff can separate them. Writing that into the lower patch
        // produces something that passes the round-trip check against the wrong base and leaves
        // the series unable to apply. So the mismatch is refused here, where it can still be
        // explained, rather than discovered as a broken series later.
        match state.applied.last() {
            Some(top) if &found.name == top => {}
            Some(top) => anyhow::bail!(
                "`{}` has {} on top of it in the sandbox, so its changes cannot be told apart \
                 from the ones above it.\n\
                 `skot pop` takes the patches above it off, then `skot refresh {}` exports \
                 just this one.",
                found.name,
                top,
                found.name
            ),
            None => anyhow::bail!(
                "nothing is applied to the sandbox, so there is nothing to refresh.\n\
                 Run `skot apply` first.",
            ),
        }
        let index = entries
            .iter()
            .position(|e| e.name == found.name)
            .expect("found came from entries");
        return Ok((found.clone(), index));
    }

    // The last applied patch is the one being authored. If the sandbox is ahead of the series
    // — the user ran `new` but never refreshed — say so, because exporting into the wrong
    // patch is silent and confusing.
    let Some(last_applied) = state.applied.last() else {
        if entries.is_empty() {
            bail!(
                "the series at {} is empty, so there is nothing to refresh.\n\
                 `skot new <name>` starts a patch.",
                series.path().display()
            );
        }
        bail!(
            "nothing is applied to the sandbox, so there is no patch to refresh.\n\
             Run `skot apply` first, or name the patch: `skot refresh {}`.",
            entries.last().map(|e| e.name.as_str()).unwrap_or("")
        );
    };
    let index = entries
        .iter()
        .position(|e| &e.name == last_applied)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the last applied patch is `{last_applied}`, which the series does not list.\n\
                 Name the one to refresh: `skot refresh <patch>`."
            )
        })?;
    Ok((entries[index].clone(), index))
}

/// Check that the exported patch applies back onto the base, before it replaces anything.
fn verify_round_trip(
    repo: &git2::Repository,
    base: &git2::Tree<'_>,
    patch: &str,
    name: &str,
) -> Result<()> {
    let diff = git2::Diff::from_buffer(patch.as_bytes()).context(format!(
        "the exported {name} is not a patch skot can read back"
    ))?;
    repo.apply_to_tree(base, &diff, None)
        .map(|_| ())
        .context(format!(
            "the exported {name} does not apply back onto the sandbox base.\n\
         Nothing was written, so your previous patch is intact."
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;

    #[test]
    fn refresh_needs_a_sandbox() {
        let (_tmp, layout) = testsupport::project();
        let err = refresh(&layout, None, None, false).unwrap_err().to_string();
        assert!(err.contains("skot apply"), "got {err}");
    }

    #[test]
    fn an_empty_series_says_to_start_a_patch() {
        let (_tmp, layout) = testsupport::project();
        let series = Series::parse(&layout.series_file(), "");

        let err = choose(&layout, &series, &State::default(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("skot new"), "got {err}");
    }

    #[test]
    fn naming_a_patch_that_is_not_in_the_series_lists_the_ones_that_are() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = Series::load(&layout.series_file()).unwrap();

        let err = choose(&layout, &series, &State::default(), Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("0001-a.patch"),
            "the message should list it: {err}"
        );
    }

    #[test]
    fn nothing_applied_says_to_apply_first() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = Series::load(&layout.series_file()).unwrap();

        let err = choose(&layout, &series, &State::default(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("skot apply"), "got {err}");
        assert!(
            err.contains("0001-a.patch"),
            "the message should name it: {err}"
        );
    }

    #[test]
    fn the_last_applied_patch_is_the_default_target() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", ""), ("0002-b.patch", "")]);
        let series = Series::load(&layout.series_file()).unwrap();
        let state = State {
            applied: vec!["0001-a.patch".into(), "0002-b.patch".into()],
            ..State::default()
        };

        let (chosen, index) = choose(&layout, &series, &state, None).unwrap();
        assert_eq!(chosen.name, "0002-b.patch");
        assert_eq!(index, 1, "the top patch, and its position in the series");
    }

    #[test]
    fn a_patch_that_cannot_be_reapplied_is_never_written() {
        // The point of the round trip: a patch file that libgit2 cannot read back breaks the
        // series, and by then the original would already be gone.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        let tree = repo
            .find_commit(git2::Oid::from_str(&sha).unwrap())
            .unwrap()
            .tree()
            .unwrap();

        let broken = "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +1,1 @@\n-nothing matches this\n+oops\n";
        let err = verify_round_trip(&repo, &tree, broken, "0001-a.patch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("0001-a.patch"), "got {err}");
        assert!(
            err.contains("intact"),
            "should say nothing was written: {err}"
        );
    }

    #[test]
    fn a_good_patch_survives_the_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
        let tree = repo
            .find_commit(git2::Oid::from_str(&sha).unwrap())
            .unwrap()
            .tree()
            .unwrap();

        let good = "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +1,1 @@\n-base\n+changed\n";
        assert!(verify_round_trip(&repo, &tree, good, "0001-a.patch").is_ok());
    }

    #[test]
    fn refreshing_the_top_patch_captures_the_edits() {
        // The whole authoring loop, end to end: apply a series, edit, refresh, and the patch
        // on disk is the edit. Unit tests on the diff cannot catch a wrong base tree here —
        // that mistake produces a patch that looks fine and drops an earlier patch.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());

        std::fs::write(layout.sandbox_dir().join("f.txt"), "edited\n").unwrap();

        refresh(&layout, Some("0002-b.patch"), None, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("0002-b.patch")).unwrap();
        // 0002's base is the tree 0001 leaves behind, which holds `first` — not `second`, which
        // is what 0002 itself put there. Diffing against the wrong tree here yields a patch that
        // cannot apply, and one that still looks perfectly reasonable.
        assert!(
            patch.contains("-first"),
            "the base should be removed:\n{patch}"
        );
        assert!(
            patch.contains("+edited"),
            "the new contents should be added:\n{patch}"
        );
        assert!(
            !patch.contains("-second"),
            "the base must be 0001's output, not 0002's own:\n{patch}"
        );
        assert_eq!(
            patch.matches("diff --git").count(),
            1,
            "one file, one diff section:\n{patch}"
        );

        // And the series it produces actually applies, which is the property that matters.
        let repo = git2::Repository::open(layout.sandbox_dir()).unwrap();
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let report = crate::check::check_series(&repo, "main", &entries).unwrap();
        assert!(report.ok, "{}", report.summary());
    }

    #[test]
    fn refreshing_a_patch_that_is_not_on_top_is_refused() {
        // The dangerous case. The sandbox holds base + the whole stack, so a workdir diff
        // cannot tell patch 1's edits from patch 2's. Writing that into patch 1 produces a
        // patch that looks fine and leaves the series unable to apply.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());
        let before = std::fs::read_to_string(layout.patches_dir().join("0001-a.patch")).unwrap();

        std::fs::write(layout.sandbox_dir().join("f.txt"), "from the bottom\n").unwrap();
        let err = refresh(&layout, Some("0001-a.patch"), None, false)
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("0002-b.patch"),
            "it should say what is on top: {err}"
        );
        assert!(
            err.contains("skot pop"),
            "it should offer the way out: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0001-a.patch")).unwrap(),
            before,
            "the patch must be untouched"
        );
    }

    #[test]
    fn popping_the_patches_above_makes_a_lower_patch_refreshable_again() {
        // The workflow the refusal above points at, so the advice is not a dead end.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());
        crate::commands::pop::pop(&layout, None).unwrap();
        // Now 0001 is the top, and the sandbox holds base + 0001 + the edit.
        std::fs::write(layout.sandbox_dir().join("f.txt"), "from the bottom\n").unwrap();
        refresh(&layout, Some("0001-a.patch"), None, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("0001-a.patch")).unwrap();
        // 0001 is the first patch, so its base is the series ref itself, which holds `base`.
        // Its own original change ("first") is exactly what must *not* be in here: that was the
        // bug where the patch was diffed against itself and kept only the edit.
        assert!(
            patch.contains("-base"),
            "the ref's contents should be removed:\n{patch}"
        );
        assert!(
            patch.contains("+from the bottom"),
            "should add the edit:\n{patch}"
        );
        assert!(
            !patch.contains("-first"),
            "0001's own old change leaked back in:\n{patch}"
        );
        assert!(
            !patch.contains("second"),
            "0002's change leaked into 0001:\n{patch}"
        );

        // 0001 now applies to the base it was rewritten against. 0002 does not, because it was
        // written on top of a `first` that no longer exists — and skot says so instead of
        // leaving a broken series unmarked. `skot rebase` is the way out.
        let repo = git2::Repository::open(layout.sandbox_dir()).unwrap();
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let report = crate::check::check_series(&repo, "main", &entries).unwrap();
        assert!(
            !report.ok,
            "0002's base moved, so the series is now inconsistent"
        );
        assert_eq!(
            report.first_failure().map(|p| p.name.as_str()),
            Some("0002-b.patch"),
            "0002 is the patch whose base moved, not the one that was edited"
        );
    }

    #[test]
    fn refreshing_a_new_file_adds_it_as_the_patch_content() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());
        std::fs::write(layout.sandbox_dir().join("extra.txt"), "a new file\n").unwrap();

        refresh(&layout, Some("0002-b.patch"), None, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("0002-b.patch")).unwrap();
        assert!(
            patch.contains("extra.txt"),
            "the new file is missing:\n{patch}"
        );
        assert!(
            patch.contains("+a new file"),
            "its contents are missing:\n{patch}"
        );

        let repo = git2::Repository::open(layout.sandbox_dir()).unwrap();
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        assert!(
            crate::check::check_series(&repo, "main", &entries)
                .unwrap()
                .ok
        );
    }

    #[test]
    fn refreshing_a_patch_that_is_already_registered_leaves_the_series_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());
        let before = std::fs::read_to_string(layout.series_file()).unwrap();

        std::fs::write(layout.sandbox_dir().join("f.txt"), "edited\n").unwrap();
        refresh(&layout, Some("0002-b.patch"), None, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            before,
            "a second refresh must not append the patch again"
        );
    }

    #[test]
    fn refreshing_records_the_new_tree_so_a_second_refresh_is_not_a_no_op() {
        // The refresh has to leave the sandbox describing the patch it just wrote. If it
        // left the pre-edit tree in place, the second refresh would re-export the first
        // refresh's contents and fold them in twice.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());
        let sandbox = layout.sandbox_dir();

        std::fs::write(sandbox.join("f.txt"), "one\n").unwrap();
        refresh(&layout, Some("0002-b.patch"), None, false).unwrap();

        std::fs::write(sandbox.join("f.txt"), "two\n").unwrap();
        refresh(&layout, Some("0002-b.patch"), None, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("0002-b.patch")).unwrap();
        assert!(
            patch.contains("+two"),
            "the second edit is missing:\n{patch}"
        );
        assert!(
            !patch.contains("+one"),
            "the first edit leaked in:\n{patch}"
        );
    }

    #[test]
    fn refreshing_an_untouched_patch_is_a_no_op_not_an_error() {
        // Refreshing twice is a normal thing to do, and after `new` + `push` it is the only
        // thing that happens. Rewriting the identical bytes is harmless, but reporting success
        // as if work had been done is not, and erroring teaches people to avoid their tools.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());
        let before = std::fs::read_to_string(layout.patches_dir().join("0002-b.patch")).unwrap();

        refresh(&layout, Some("0002-b.patch"), None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0002-b.patch")).unwrap(),
            before,
            "nothing should change on disk"
        );
    }

    #[test]
    fn a_patch_whose_change_is_gone_from_the_sandbox_is_never_erased() {
        // The destructive case. Put the file back the way the ref has it while 0001 is on top, so
        // the export would be empty while the patch is not — writing it would delete the only
        // record of a change nobody has committed anywhere else.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());

        crate::commands::pop::pop(&layout, None).unwrap();
        std::fs::write(layout.sandbox_dir().join("f.txt"), "base\n").unwrap();
        let before = std::fs::read_to_string(layout.patches_dir().join("0001-a.patch")).unwrap();

        let err = refresh(&layout, Some("0001-a.patch"), None, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not in"),
            "it should say the change is gone: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0001-a.patch")).unwrap(),
            before,
            "the patch must survive"
        );
    }

    #[test]
    fn refresh_keeps_the_author_of_the_patch_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());

        let headered = format!(
            "From: Real Person <real@example.com>\nSubject: [PATCH] the real title\n\n---\n\n{}",
            testsupport::patch_two()
        );
        std::fs::write(layout.patches_dir().join("0002-b.patch"), &headered).unwrap();
        // Re-apply so the recorded base tree matches the headered patch's result.
        crate::commands::apply::apply_locked(&layout, Some("main")).unwrap();

        std::fs::write(layout.sandbox_dir().join("f.txt"), "edited\n").unwrap();
        refresh(&layout, Some("0002-b.patch"), None, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("0002-b.patch")).unwrap();
        assert!(
            patch.contains("Real Person <real@example.com>"),
            "author lost:\n{patch}"
        );
        assert!(patch.contains("the real title"), "subject lost:\n{patch}");
    }
}
