//! `patch save`: turn what you changed in a work area into a patch file.
//!
//! This is the command that makes patch authoring a loop rather than a chore. You edit in
//! the worktree under `.skot/work/`, run `save`, and the patch on disk is the diff:
//! no commit, no branch, no rebase, nothing to remember to clean up. After a save the work
//! area matches the patch, so it retires itself: saving is the end of the loop, and there
//! is no separate step for being done.
//!
//! Three decisions are worth stating, because each is easy to get wrong, and each produced a
//! patch that looked entirely reasonable while being wrong:
//!
//! - **The base is the work area's recorded base, not the patch's own output.** The worktree
//!   holds the tree the patches below produced, plus the patch's own content, plus your
//!   edits, so diffing against the recorded base yields the patch's whole change exactly
//!   once. Diffing against what the patch produced instead would export *only your edits*
//!   and replace the patch with a fragment of itself.
//! - **Each patch saves from its own work area, in any order.** A work area is one patch's
//!   worktree, so a lower patch can be saved while a higher one has unsaved changes: their
//!   trees never mix. The old rule, that only the patch on top of the applied set could be
//!   saved, existed because one shared sandbox could not tell the two apart.
//! - **Author and date come from the patch being replaced.** Otherwise every save re-stamps
//!   the committer and a rebase turns into a rewrite of the whole series' history.
//!
//! One guard sits in front of all that: the recorded base is a claim about the series as it
//! was when the work area opened. If the patches below no longer replay to that tree,
//! `save` refuses rather than exporting against a context the series cannot rebuild,
//! and `--force` is the way past it when the change below was deliberate.

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::SaveArgs;
use crate::commands::{Ctx, Exit, authoring, emit_json};
use crate::git::diff;
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::series::{self, Series};
use crate::session::{Manifest, Session, end, open as open_session, reconcile};

/// What `save` did, presented by [`run`].
#[derive(Debug)]
struct Saved {
    name: String,
    path: std::path::PathBuf,
    session: std::path::PathBuf,
    changed: bool,
    registered: bool,
    insert_at: Option<usize>,
    summary: String,
    bytes: usize,
}

pub fn run(ctx: &Ctx, args: &SaveArgs) -> Result<Exit> {
    let done = save(
        &ctx.layout,
        args.name.as_deref(),
        args.insert_at,
        args.no_register,
        args.force,
    )?;

    if ctx.cli.json {
        if done.changed {
            emit_json(&json!({
                "series_name": ctx.layout.series(),
                "patch": done.name,
                "path": done.path.display().to_string(),
                "worktree": done.session.display().to_string(),
                "registered": done.registered,
                "insert_at": done.insert_at,
                "summary": done.summary,
                "bytes": done.bytes,
            }))?;
        } else {
            emit_json(&json!({
                "series_name": ctx.layout.series(),
                "patch": done.name,
                "path": done.path.display().to_string(),
                "worktree": done.session.display().to_string(),
                "changed": false,
                "summary": done.summary,
                "bytes": done.bytes,
            }))?;
        }
        return Ok(Exit::Success);
    }

    if !done.changed {
        info!(target: "save", "{} was already saved", done.name);
        info!(
            target: "save",
            "The work area matches it. `{}` starts a new one when you want to change more.",
            skot_args("edit", &done.name)
        );
        return Ok(Exit::Success);
    }

    info!(target: "save", "saved {}", done.name);
    debug!(target: "save", "  {}", done.summary);
    debug!(target: "save", "  {}", done.path.display());
    if done.registered {
        debug!(target: "save", "registered in the series");
    } else {
        debug!(
            target: "save",
            "already in the series, so the series was left alone"
        );
    }
    Hints::new("next")
        .entry(skot("check"), "re-verify the whole series")
        .entry(skot("apply"), "stamp the saved patches into the sandbox")
        .print();
    Ok(Exit::Success)
}

/// Save the chosen work area's edits into the chosen patch file, registering it if needed.
///
/// Everything below [`run`] is this: no clap, no stdout, no verdict, a `&Layout` and
/// scalars in, a description of the outcome out.
fn save(
    layout: &Layout,
    name: Option<&str>,
    insert_at: Option<usize>,
    no_register: bool,
    force: bool,
) -> Result<Saved> {
    let lock = layout.lock()?;
    let (_, series) = layout.project()?;
    // Reconciled, not merely loaded: a manifest that still records a directory someone
    // deleted is a work area that cannot be saved from, and the edit remedies below are
    // the way back from that.
    let manifest = reconcile(layout)?;
    let (session, entry, index) = choose(layout, &series, &manifest, name)?;
    let dir = layout.work_patch_dir(session.dir_name())?;
    let entries = series.entries(layout)?;
    let repo = open_session(layout, &session)?;

    // The recorded base is a claim: when this work area opened, the patches below it
    // produced exactly that tree. Replaying them now must reach the same place. If it does
    // not, the series below has moved under the work area, and the worktree's context no
    // longer names a tree the series produces, so saving would write a patch against
    // something nobody can rebuild. `--force` is the way past it when the change below was
    // deliberate; the save then uses the base as recorded, and `skot check` reports the
    // inconsistency below.
    let recorded = git2::Oid::from_str(&session.base_tree)
        .context("reading the work area's recorded base tree")?;
    if !force {
        let applied = crate::check::replay_series(&repo, &session.base_sha, &entries[..index])
            .context(format!(
                "replaying the patches below {} on its recorded base",
                session.patch
            ))?;
        if applied.tree != recorded {
            bail!(
                "the patches below `{}` changed after its work area opened, so the recorded \
                 base no longer matches what the series produces.\n\
                 `{}` saves against the recorded base anyway, which is only right if the \
                 change below was deliberate.",
                session.patch,
                skot_args("save", "--force")
            );
        }
    }
    let base = repo.find_tree(recorded).context(format!(
        "reading the tree {} the work area opened on",
        session.base_tree
    ))?;

    let existing = std::fs::read_to_string(&entry.path).unwrap_or_default();
    let header = authoring::header_for(
        Some(&existing),
        &authoring::subject_from_name(&entry.name),
        &authoring::signature(&repo),
    );
    let patch = diff::tree_to_patch(&repo, &base, &header)
        .context(format!("exporting your edits as {}", entry.name))?;

    // Saving a patch you have not touched is normal, not a mistake: it is what running
    // `save` twice looks like. Compare the *body*, because re-rendering the header
    // normalises things the author never wrote, hunk counts and offsets among them, and
    // rewriting those bytes is not a change to the patch.
    if diff::change_of(&patch) == diff::change_of(&existing) {
        // Matches, so there is nothing unsaved here: the work area retires rather than
        // sitting around looking like work in progress.
        end(layout, &session)?;
        drop(lock);
        return Ok(Saved {
            name: entry.name,
            path: entry.path,
            session: dir,
            changed: false,
            registered: false,
            insert_at: None,
            summary: diff::summarise(&patch),
            bytes: patch.len(),
        });
    }

    // A save with no diff at all, when the existing patch is not empty, means the worktree
    // no longer contains the change the patch describes: it was reset, or the work area was
    // opened on something else. Writing it would delete the only record of that change, so
    // stop here where the reason can still be explained.
    if !patch.contains("diff --git ") {
        bail!(
            "there is nothing in {} to save for {}, but that patch is not empty.\n\
             The change it describes is not in the worktree right now.\n\
             If you meant to undo it, delete {} and run `{}`; if the worktree was reset \
             behind your back, look at it before saving anything.",
            dir.display(),
            entry.name,
            entry.path.display(),
            skot("check"),
        );
    }

    // Prove the export applies before overwriting anything. A patch file that libgit2 cannot
    // apply back is worse than a stale one, because the series is now broken and the original
    // is gone.
    verify_round_trip(&repo, &base, &patch, &entry.name)?;

    std::fs::write(&entry.path, &patch).context(format!("writing {}", entry.path.display()))?;

    let mut series = series;
    let registered = if no_register {
        false
    } else if series.names().contains(&entry.name.as_str()) {
        // Already in the series is the normal case for a second save, not a failure.
        false
    } else {
        let index = insert_at.unwrap_or(series.len());
        series.insert(&entry.name, index)?;
        series.save(layout)?;
        true
    };

    // The patch on disk now says what the worktree says, so there is nothing unsaved in
    // it and it stops being a work area: saving is the end of the loop, and there is no
    // separate close for the user to remember.
    end(layout, &session)?;
    drop(lock);
    Ok(Saved {
        name: entry.name,
        path: entry.path,
        session: dir,
        changed: true,
        registered,
        insert_at,
        summary: diff::summarise(&patch),
        bytes: patch.len(),
    })
}

/// The work area to save from: the one named, the only one unsaved, or an error that
/// lists them.
///
/// A patch saves from its own work area and never from "whatever is on top of the applied
/// set". That is what let a lower patch's save pick up the content of the patch above it:
/// one worktree per patch means the two trees never mix, and any patch with unsaved
/// changes can be saved in any order.
fn choose(
    layout: &Layout,
    series: &Series,
    manifest: &Manifest,
    name: Option<&str>,
) -> Result<(Session, series::Entry, usize)> {
    let entries = series.entries_lenient(layout);

    let session = if let Some(name) = name {
        let found = manifest.sessions.iter().find(|s| {
            s.series_name() == layout.series()
                && (s.patch == name
                    || std::path::Path::new(&s.patch)
                        .file_name()
                        .is_some_and(|f| f == name))
        });
        match found {
            Some(session) => session.clone(),
            None => {
                let unsaved = manifest
                    .sessions
                    .iter()
                    .filter(|s| s.series_name() == layout.series())
                    .filter(|s| crate::session::unsaved(layout, s).unwrap_or(true))
                    .map(|s| s.patch.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "nothing to save for `{name}`: no work area holds unsaved changes for it.\n\
                     Patches with unsaved changes: {}.\n\
                     `{}` opens a work area for it.",
                    if unsaved.is_empty() {
                        "(none)".to_owned()
                    } else {
                        unsaved
                    },
                    skot_args("edit", name)
                );
            }
        }
    } else {
        // Only unsaved changes are targets. A work area that matches its patch has nothing
        // to save; it is kept because the user may not be finished, not because it wants
        // saving. A comparison that fails counts as unsaved: the safe direction is to
        // surface it to `save` rather than skip it here.
        let unsaved: Vec<Session> = manifest
            .sessions
            .iter()
            .filter(|s| crate::session::unsaved(layout, s).unwrap_or(true))
            .cloned()
            .collect();
        match unsaved.as_slice() {
            [] => bail!(
                "nothing to save: no patch has unsaved changes.\n\
                 `{}` opens a work area for an existing patch; `{}` starts a new one.",
                skot_args("edit", "<patch>"),
                skot("new")
            ),
            [only] => only.clone(),
            rest => bail!(
                "{} patches have unsaved changes: {}.\n\
                 Name the one to save: `{}`.",
                rest.len(),
                unsaved
                    .iter()
                    .map(|s| s.patch.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                skot_args("save", "<patch>")
            ),
        }
    };

    // The session's patch must still be in the series: its position decides what sits below
    // it, and a patch the series dropped cannot be exported into.
    let Some(index) = entries.iter().position(|e| e.name == session.patch) else {
        let known: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        bail!(
            "`{}` is not in the series. Patches in it: {}",
            session.patch,
            if known.is_empty() {
                "(none)".to_owned()
            } else {
                known.join(", ")
            }
        );
    };
    Ok((session, entries[index].clone(), index))
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
            "the exported {name} does not apply back onto its recorded base.\n\
         Nothing was written, so your previous patch is intact."
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;

    /// A project with a two-patch series and a session open on `b.patch`, its worktree
    /// holding a.patch's output plus b.patch's own content: what the authoring loop leaves
    /// behind.
    fn session_on_b(root: &std::path::Path) -> Layout {
        let (layout, _sha) = testsupport::mirror_with_one_commit(root);
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        testsupport::write_series(
            &layout,
            &[
                ("a.patch", testsupport::patch_one()),
                ("b.patch", testsupport::patch_two()),
            ],
        );
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let plan = crate::session::plan(&layout, "main", &entries[..1], Some(&entries[1])).unwrap();
        crate::session::materialise(&layout, "b.patch", &plan).unwrap();
        layout
    }

    /// The same, with a session open on each patch.
    fn session_pair(root: &std::path::Path) -> Layout {
        let layout = session_on_b(root);
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let plan = crate::session::plan(&layout, "main", &[], Some(&entries[0])).unwrap();
        crate::session::materialise(&layout, "a.patch", &plan).unwrap();
        layout
    }

    /// What the sessions in `layout` can reach: open the mirror, which is where check
    /// replays from anyway.
    fn mirror_repo(layout: &Layout) -> git2::Repository {
        git2::Repository::open(layout.mirror_dir()).unwrap()
    }

    fn record(patch: &str) -> Session {
        Session {
            series: crate::layout::DEFAULT_SERIES.to_owned(),
            patch: patch.to_owned(),
            dir: patch.to_owned(),
            ref_name: "main".into(),
            base_sha: "abc".into(),
            base_tree: "def".into(),
        }
    }

    #[test]
    fn save_without_unsaved_changes_says_so() {
        let (_tmp, layout) = testsupport::project();
        let err = save(&layout, None, None, false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&skot("new")), "got {err}");
    }

    #[test]
    fn an_empty_series_says_to_start_a_patch() {
        let (_tmp, layout) = testsupport::project();
        let series = Series::parse(&layout.series_file(), "");

        let err = choose(&layout, &series, &Manifest::default(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&skot("new")), "got {err}");
        assert!(
            err.contains(&skot_args("edit", "<patch>")),
            "it should also say how to open an existing one: {err}"
        );
    }

    #[test]
    fn naming_a_patch_with_no_session_says_so() {
        // The patch exists; the worktree does not. Saying "not in the series" here would
        // be a lie that sends the user looking at the wrong file.
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("a.patch", "")]);
        let series = Series::load(&layout.series_file()).unwrap();

        let err = choose(&layout, &series, &Manifest::default(), Some("a.patch"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("a.patch"), "it should name it: {err}");
        assert!(err.contains("nothing to save"), "got {err}");
        assert!(
            err.contains(&skot_args("edit", "a.patch")),
            "it should say how to open one: {err}"
        );
    }

    #[test]
    fn several_open_sessions_must_be_named() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("a.patch", ""), ("b.patch", "")]);
        let series = Series::load(&layout.series_file()).unwrap();
        let manifest = Manifest {
            sessions: vec![record("a.patch"), record("b.patch")],
        };

        let err = choose(&layout, &series, &manifest, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("a.patch"), "it should list them: {err}");
        assert!(err.contains("b.patch"), "it should list them: {err}");
        assert!(err.contains(&skot("save")), "and say how to pick: {err}");
    }

    #[test]
    fn the_only_open_session_is_the_default_target() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("a.patch", ""), ("b.patch", "")]);
        let series = Series::load(&layout.series_file()).unwrap();
        let manifest = Manifest {
            sessions: vec![record("b.patch")],
        };

        let (session, entry, index) = choose(&layout, &series, &manifest, None).unwrap();
        assert_eq!(session.patch, "b.patch");
        assert_eq!(entry.name, "b.patch");
        assert_eq!(index, 1, "the patch, and its position in the series");
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
        let err = verify_round_trip(&repo, &tree, broken, "a.patch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("a.patch"), "got {err}");
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
        assert!(verify_round_trip(&repo, &tree, good, "a.patch").is_ok());
    }

    #[test]
    fn saving_a_work_area_captures_the_edits() {
        // The whole authoring loop, end to end: a work area on the patch, edit, save, and
        // the patch on disk is the edit. Unit tests on the diff cannot catch a wrong base
        // tree here, because that mistake produces a patch that looks fine and drops an
        // earlier patch.
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_on_b(tmp.path());

        let dir = layout.work_patch_dir("b.patch").unwrap();
        std::fs::write(dir.join("f.txt"), "edited\n").unwrap();

        save(&layout, Some("b.patch"), None, false, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap();
        // b.patch's recorded base is the tree a.patch leaves behind, which holds `first`, not
        // `second`, which is what b.patch itself put in the worktree. Diffing against the wrong
        // tree here yields a patch that cannot apply, and one that still looks reasonable.
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
            "the base must be a.patch's output, not b.patch's own:\n{patch}"
        );
        assert_eq!(
            patch.matches("diff --git").count(),
            1,
            "one file, one diff section:\n{patch}"
        );

        // And the series it produces actually applies, which is the property that matters.
        let repo = mirror_repo(&layout);
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let report = crate::check::check_series(&repo, "main", &entries).unwrap();
        assert!(report.ok, "{}", report.summary());
    }

    #[test]
    fn a_lower_patch_saves_while_another_work_area_is_open() {
        // The case the shared sandbox had to refuse: with one worktree per patch, patch 1's
        // edits are in patch 1's worktree and patch 2's are somewhere else entirely, so
        // there is nothing to tell apart and no reason to send the user away to pop first.
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_pair(tmp.path());
        let before = std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap();

        let one = layout.work_patch_dir("a.patch").unwrap();
        std::fs::write(one.join("f.txt"), "from the bottom\n").unwrap();
        save(&layout, Some("a.patch"), None, false, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap();
        // a.patch's base is the series ref itself, which holds `base`. Its own original change
        // ("first") must not be in here, and neither must b.patch's ("second").
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
            "a.patch's own old change leaked back in:\n{patch}"
        );
        assert!(
            !patch.contains("second"),
            "b.patch's change leaked into a.patch:\n{patch}"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap(),
            before,
            "b.patch must be untouched by a save of a.patch"
        );

        // a.patch now applies to the base it was rewritten against. b.patch does not, because it
        // was written on top of a `first` that no longer exists, and skot says so instead of
        // leaving a broken series unmarked.
        let repo = mirror_repo(&layout);
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let report = crate::check::check_series(&repo, "main", &entries).unwrap();
        assert!(
            !report.ok,
            "b.patch's base moved, so the series is now inconsistent"
        );
        assert_eq!(
            report.first_failure().map(|p| p.name.as_str()),
            Some("b.patch"),
            "b.patch is the patch whose base moved, not the one that was edited"
        );
    }

    #[test]
    fn saving_a_new_file_adds_it_as_the_patch_content() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_on_b(tmp.path());
        let dir = layout.work_patch_dir("b.patch").unwrap();
        std::fs::write(dir.join("extra.txt"), "a new file\n").unwrap();

        save(&layout, Some("b.patch"), None, false, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap();
        assert!(
            patch.contains("extra.txt"),
            "the new file is missing:\n{patch}"
        );
        assert!(
            patch.contains("+a new file"),
            "its contents are missing:\n{patch}"
        );

        let repo = mirror_repo(&layout);
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        assert!(
            crate::check::check_series(&repo, "main", &entries)
                .unwrap()
                .ok
        );
    }

    #[test]
    fn saving_a_patch_that_is_already_registered_leaves_the_series_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_on_b(tmp.path());
        let before = std::fs::read_to_string(layout.series_file()).unwrap();

        let dir = layout.work_patch_dir("b.patch").unwrap();
        std::fs::write(dir.join("f.txt"), "edited\n").unwrap();
        save(&layout, Some("b.patch"), None, false, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            before,
            "a second save must not append the patch again"
        );
    }

    #[test]
    fn saving_twice_keeps_both_edits_out_of_each_other() {
        // Every export is measured against the recorded base, never against what the last
        // save wrote, so the second edit replaces the first instead of stacking on it. The
        // work area retires after a save, so the second round opens a fresh one, which is
        // what the loop looks like in practice.
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_on_b(tmp.path());
        let dir = layout.work_patch_dir("b.patch").unwrap();

        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        save(&layout, Some("b.patch"), None, false, false).unwrap();

        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let plan = crate::session::plan(&layout, "main", &entries[..1], Some(&entries[1])).unwrap();
        crate::session::materialise(&layout, "b.patch", &plan).unwrap();
        std::fs::write(dir.join("f.txt"), "two\n").unwrap();
        save(&layout, Some("b.patch"), None, false, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap();
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
    fn saving_an_untouched_patch_is_a_no_op_not_an_error() {
        // Saving twice is a normal thing to do. Rewriting identical bytes is harmless,
        // but reporting success as if work had been done is not, and erroring teaches people
        // to avoid their tools. The work area retires either way: it matches the patch, so
        // there is nothing unsaved in it.
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_on_b(tmp.path());
        let before = std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap();

        save(&layout, Some("b.patch"), None, false, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap(),
            before,
            "nothing should change on disk"
        );
        assert!(
            crate::session::Manifest::load(&layout)
                .unwrap()
                .sessions
                .is_empty(),
            "a work area that matches its patch is no longer a work area"
        );
    }

    #[test]
    fn saving_above_a_disabled_patch_diffs_against_only_what_is_below_it() {
        // b is disabled, so c's base is a's output alone. A base computed by
        // replaying the numbered lines instead would claim the worktree sits on `second`,
        // and the save would rewrite a patch that was never wrong.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(layout.patches_dir()).unwrap();
        std::fs::write(
            layout.patches_dir().join("a.patch"),
            testsupport::patch_one(),
        )
        .unwrap();
        std::fs::write(
            layout.patches_dir().join("b.patch"),
            testsupport::patch_two(),
        )
        .unwrap();
        std::fs::write(
            layout.patches_dir().join("c.patch"),
            testsupport::patch_three(),
        )
        .unwrap();
        std::fs::write(layout.series_file(), "a.patch\n#!b.patch\nc.patch\n").unwrap();
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["a.patch", "c.patch"],
            "disabled patches are not positions in the series"
        );
        let plan = crate::session::plan(&layout, "main", &entries[..1], Some(&entries[1])).unwrap();
        crate::session::materialise(&layout, "c.patch", &plan).unwrap();
        let before = std::fs::read_to_string(layout.patches_dir().join("c.patch")).unwrap();

        save(&layout, Some("c.patch"), None, false, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("c.patch")).unwrap(),
            before,
            "the disabled patch below must not leak into this one's base"
        );
    }

    #[test]
    fn a_patch_whose_change_is_gone_from_the_worktree_is_never_erased() {
        // The destructive case. Put the file back the way the ref has it, so the export
        // would be empty while the patch is not, and writing it would delete the only
        // record of a change nobody has committed anywhere else.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        testsupport::write_series(&layout, &[("a.patch", testsupport::patch_one())]);
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let plan = crate::session::plan(&layout, "main", &[], Some(&entries[0])).unwrap();
        crate::session::materialise(&layout, "a.patch", &plan).unwrap();

        let dir = layout.work_patch_dir("a.patch").unwrap();
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        let before = std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap();

        let err = save(&layout, Some("a.patch"), None, false, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not in"),
            "it should say the change is gone: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            before,
            "the patch must survive"
        );
    }

    #[test]
    fn edit_save_keeps_the_author_of_the_patch_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        testsupport::write_series(&layout, &[("a.patch", testsupport::patch_one())]);
        std::fs::create_dir_all(layout.patches_dir()).unwrap();
        let headered = format!(
            "From: Real Person <real@example.com>\nSubject: [PATCH] the real title\n\n---\n\n{}",
            testsupport::patch_two()
        );
        std::fs::write(layout.patches_dir().join("b.patch"), &headered).unwrap();
        std::fs::write(layout.series_file(), "a.patch\nb.patch\n").unwrap();

        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let plan = crate::session::plan(&layout, "main", &entries[..1], Some(&entries[1])).unwrap();
        crate::session::materialise(&layout, "b.patch", &plan).unwrap();

        let dir = layout.work_patch_dir("b.patch").unwrap();
        std::fs::write(dir.join("f.txt"), "edited\n").unwrap();
        save(&layout, Some("b.patch"), None, false, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap();
        assert!(
            patch.contains("Real Person <real@example.com>"),
            "author lost:\n{patch}"
        );
        assert!(patch.contains("the real title"), "subject lost:\n{patch}");
    }

    #[test]
    fn the_patches_below_moving_under_an_open_session_is_refused() {
        // The drift guard. The session's recorded base names a tree the series produced when
        // it opened; if a.patch has been rewritten since, that tree is not one the series
        // produces any more, and exporting against it would write a patch nobody can rebuild.
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_on_b(tmp.path());
        let before = std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap();
        std::fs::write(
            layout.patches_dir().join("a.patch"),
            "\
diff --git a/f.txt b/f.txt
index 1111111..9999999 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+uno
",
        )
        .unwrap();

        let err = save(&layout, Some("b.patch"), None, false, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("changed after its work area opened"),
            "got {err}"
        );
        assert!(err.contains("--force"), "the way out: {err}");
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap(),
            before,
            "nothing may be written when the base no longer matches"
        );
    }

    #[test]
    fn force_exports_against_the_recorded_base_when_the_below_changed() {
        // The informed override: save what the worktree holds against the base the work area
        // recorded. `check` reports the inconsistency below; `save` does not pretend it
        // away and does not refuse to let the user finish.
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_on_b(tmp.path());
        std::fs::write(
            layout.patches_dir().join("a.patch"),
            "\
diff --git a/f.txt b/f.txt
index 1111111..9999999 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+uno
",
        )
        .unwrap();

        save(&layout, Some("b.patch"), None, false, true).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap();
        assert!(
            patch.contains("+second"),
            "the worktree's own content, measured against the recorded base:\n{patch}"
        );
        assert!(
            !patch.contains("uno"),
            "the changed patch below must not be folded in:\n{patch}"
        );
    }

    /// A session on a tree that has files in subdirectories, with no patches below it: what
    /// `patch new` leaves behind on a real project.
    fn session_with_directories(root: &std::path::Path) -> Layout {
        let (layout, _sha) = testsupport::mirror_with_directories(root);
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        testsupport::write_series(&layout, &[("p1.patch", "")]);
        let plan = crate::session::plan(&layout, "main", &[], None).unwrap();
        crate::session::materialise(&layout, "p1.patch", &plan).unwrap();
        layout
    }

    #[test]
    fn a_materialised_worktree_is_clean() {
        // The state every save is computed from. A worktree that starts with staged changes
        // is a diff waiting to invent deletions the author never made, and no assertion on
        // the exported patch can see where the extra sections came from.
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_with_directories(tmp.path());
        let dir = layout.work_patch_dir("p1.patch").unwrap();

        let repo = git2::Repository::open(&dir).unwrap();
        let statuses = repo
            .statuses(Some(
                git2::StatusOptions::new()
                    .include_untracked(true)
                    .recurse_untracked_dirs(true),
            ))
            .unwrap();
        let listed: Vec<String> = statuses
            .iter()
            .map(|s| format!("{} {:?}", s.path().unwrap_or("<none>"), s.status()))
            .collect();
        assert!(
            listed.is_empty(),
            "a fresh worktree must hold no changes at all, got: {listed:?}"
        );
    }

    #[test]
    fn saving_an_edit_touches_only_the_edited_file() {
        // The whole point of the export: one file edited, one file in the patch. A tree with
        // subdirectories is what makes this able to fail; against a root-only tree the count
        // is one no matter what the diff does.
        let tmp = tempfile::tempdir().unwrap();
        let layout = session_with_directories(tmp.path());

        let dir = layout.work_patch_dir("p1.patch").unwrap();
        let f = dir.join("f.txt");
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str("// edit1\n");
        std::fs::write(&f, text).unwrap();

        save(&layout, Some("p1.patch"), None, false, false).unwrap();

        let patch = std::fs::read_to_string(layout.patches_dir().join("p1.patch")).unwrap();
        assert_eq!(
            patch.matches("diff --git").count(),
            1,
            "one file was edited, so one file may change:\n{patch}"
        );
        assert!(
            patch.contains("f.txt"),
            "the edited file is missing:\n{patch}"
        );
        assert!(
            !patch.contains("src/a.c"),
            "an untouched file leaked into the patch:\n{patch}"
        );
        assert!(
            !patch.contains("docs/readme.md"),
            "an untouched file leaked into the patch:\n{patch}"
        );

        // And the series it produces applies, which is the property that matters.
        let repo = mirror_repo(&layout);
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let report = crate::check::check_series(&repo, "main", &entries).unwrap();
        assert!(report.ok, "{}", report.summary());
    }
}
