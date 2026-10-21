//! `patch edit`: open an existing patch for editing, in a worktree of its own.
//!
//! This is the other half of `new`: `new` starts a patch and opens the first work area on
//! it, `edit` opens one on a patch that already exists. Both end the same way, with a
//! worktree under `.skot/work/` holding the patches below plus the patch's own content,
//! ready to change, and both refuse before anything is written when the series cannot
//! produce that tree.
//!
//! Two rules worth stating:
//!
//! - A work area that already exists is never reopened. Opening means *resetting* to the
//!   recorded base, which is exactly the thing that would throw away the edits the user
//!   came back for, so `edit` on a work area with unsaved changes reports that and stops.
//!   `--discard` is the informed override: throw the unsaved changes away and start over.
//! - A work area that matches its patch is not a complaint, it is just where the user was.
//!   `edit` returns to it without ceremony: being clean costs nothing, and it is what
//!   every freshly opened work area looks like before the first change.

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::EditArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::skot_args;
use crate::layout::Layout;
use crate::series::Series;
use crate::session::{self, Session};
use crate::state::State;

pub fn run(ctx: &Ctx, args: &EditArgs) -> Result<Exit> {
    let (session, opened, discarded) =
        open(&ctx.layout, &args.patch, args.base.as_deref(), args.discard)?;
    let dir = session.dir_path(&ctx.layout)?;

    if ctx.cli.json {
        emit_json(&json!({
            "series_name": ctx.layout.series(),
            "patch": session.patch,
            "worktree": dir.display().to_string(),
            "ref": session.ref_name,
            "base": session.base_sha,
            "opened": opened,
            "discarded": discarded,
        }))?;
        return Ok(if opened { Exit::Success } else { Exit::Failure });
    }

    if discarded {
        info!(
            target: "edit",
            "discarded the unsaved changes in {} and started over",
            session.patch
        );
    }
    if opened {
        info!(target: "edit", "editing {}", session.patch);
    } else {
        info!(target: "edit", "{} is already open", session.patch);
    }
    debug!(target: "edit", "{}", dir.display());
    Hints::new("next")
        .line(format!("edit in {}", dir.display()))
        .entry(
            skot_args("save", &session.patch),
            "save your changes to the patch",
        )
        .print();
    Ok(if opened { Exit::Success } else { Exit::Failure })
}

/// The plain work behind [`run`]: open a work area on `patch`, or report the one that is
/// already there. The bools say whether a work area was opened now, and whether unsaved
/// changes were discarded to make that possible.
///
/// Everything that can fail for a reason the series is responsible for is checked before
/// the first write, the same order `new` uses for the same reason: a ref that does not
/// resolve, or patches below that no longer apply, leave no half-opened worktree behind.
fn open(
    layout: &Layout,
    patch: &str,
    base: Option<&str>,
    discard: bool,
) -> Result<(Session, bool, bool)> {
    let _lock = layout.lock()?;
    let manifest = session::reconcile(layout)?;
    let mut discarded = false;
    if let Some(existing) = find(layout, &manifest, patch) {
        if !session::unsaved(layout, existing)? {
            // Matches the patch: not a complaint, just where the user was. Return to it.
            return Ok((existing.clone(), false, false));
        }
        if !discard {
            bail!(
                "{} has unsaved changes.\n\
                 `{}` saves them; `{}` discards them and starts over.",
                existing.patch,
                skot_args("save", &existing.patch),
                skot_args("edit", &format!("--discard {patch}"))
            );
        }
        // The informed override. `end` takes the worktree and the record; the patch file
        // is untouched either way, because unsaved changes live only in the worktree.
        session::end(layout, existing)?;
        discarded = true;
    }

    let (config, series) = layout.project()?;
    let entries = series.entries_lenient(layout);
    let Some(index) = position(&entries, patch) else {
        bail!("{}", not_in_series(&series, &entries, patch));
    };
    let entry = entries[index].clone();

    let base = match base {
        Some(base) => base.to_owned(),
        None => State::load(layout)?
            .ref_name(layout)
            .map(str::to_owned)
            .or_else(|| layout.base_ref())
            .or_else(|| config.upstream.default_ref.clone())
            .ok_or_else(|| {
                // `new`'s version of this error suggests `new --base`, which would start a
                // *different* patch; this one is about opening an existing one.
                anyhow::anyhow!(
                    "cannot tell which ref to open `{patch}` on.\n\
                     Pass one (`{}`) or set `upstream.default-ref` in {}.",
                    skot_args("edit", "--base main"),
                    layout.config_file().display()
                )
            })?,
    };

    let plan = session::plan(layout, &base, &entries[..index], Some(&entry))
        .context(format!("opening a work area for {patch}"))?;
    let session = session::materialise(layout, &entry.name, &plan)?;
    Ok((session, true, discarded))
}

/// Find a work area by its patch's name, or by the file name that name ends in.
///
/// Only the layout's series: two series can hold a patch of the same name, and `edit`
/// opens work areas in the series it is running against.
fn find<'s>(
    layout: &crate::layout::Layout,
    manifest: &'s session::Manifest,
    patch: &str,
) -> Option<&'s Session> {
    manifest.sessions.iter().find(|s| {
        s.series_name() == layout.series()
            && (s.patch == patch
                || std::path::Path::new(&s.patch)
                    .file_name()
                    .is_some_and(|f| f == patch))
    })
}

/// Where `patch` sits in the enabled series, if it is there at all.
fn position(entries: &[crate::series::Entry], patch: &str) -> Option<usize> {
    entries
        .iter()
        .position(|e| e.name == patch || e.path.file_name().is_some_and(|f| f == patch))
}

fn not_in_series(series: &Series, entries: &[crate::series::Entry], patch: &str) -> String {
    let known: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    format!(
        "`{patch}` is not in the series at {}. Patches in it: {}",
        series.path().display(),
        if known.is_empty() {
            "(none)".to_owned()
        } else {
            known.join(", ")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command, PatchCommand};
    use crate::commands::testsupport;
    use clap::Parser as _;

    /// A project with a two-patch series and no work areas: what is left after everything
    /// has been saved, or before the first `edit`.
    fn series_only(root: &std::path::Path) -> Layout {
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
        layout
    }

    #[test]
    fn edit_opens_a_work_area_holding_the_patch_below_and_its_own_content() {
        // The point of `edit`: the worktree is not blank and not just the series below, it
        // is the patch as it stands, so the edits land on top of the change being edited.
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_only(tmp.path());

        let (session, opened, discarded) = open(&layout, "b.patch", None, false).unwrap();

        assert!(opened);
        assert!(!discarded);
        assert_eq!(session.patch, "b.patch");
        assert_eq!(session.ref_name, "main");
        let dir = layout.work_patch_dir("b.patch").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "second\n",
            "the patch's own content, on top of what is below it"
        );
        assert!(dir.join(".git").is_file(), "a linked worktree, not a clone");
        assert!(
            !layout.state_file().exists(),
            "work areas never write the applied set"
        );
        assert!(!layout.sandbox_dir().exists(), "work areas never touch it");
    }

    #[test]
    fn edit_reports_unsaved_changes_and_never_resets_them() {
        // The rule that protects the work: opening means resetting to the recorded base,
        // and resetting would throw away exactly what the user came back for.
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_only(tmp.path());
        let (session, _, _) = open(&layout, "a.patch", None, false).unwrap();
        let dir = layout.work_patch_dir("a.patch").unwrap();
        std::fs::write(dir.join("f.txt"), "work in progress\n").unwrap();

        let err = open(&layout, "a.patch", None, false).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("unsaved changes"),
            "it should name what is at stake: {text}"
        );
        assert!(
            text.contains(&skot_args("save", "a.patch")),
            "it should say how to save them: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "work in progress\n",
            "the edits must survive the refusal"
        );
        assert_eq!(session.patch, "a.patch");
    }

    #[test]
    fn edit_yes_discards_unsaved_changes_and_starts_over() {
        // The informed override: the user said --yes, so the work area is taken away and a
        // fresh one opens from the patch. The patch file is untouched either way.
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_only(tmp.path());
        open(&layout, "a.patch", None, false).unwrap();
        let dir = layout.work_patch_dir("a.patch").unwrap();
        std::fs::write(dir.join("f.txt"), "work in progress\n").unwrap();

        let (session, opened, discarded) = open(&layout, "a.patch", None, true).unwrap();

        assert!(opened);
        assert!(discarded);
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "first\n",
            "the work area starts over from the patch's own content"
        );
        assert_eq!(session.patch, "a.patch");
    }

    #[test]
    fn edit_returns_to_a_clean_work_area_without_complaint() {
        // Being clean is not a state to manage: a freshly opened work area matches its
        // patch, and coming back to it is the normal resume path.
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_only(tmp.path());
        let (session, _, _) = open(&layout, "a.patch", None, false).unwrap();

        let (again, opened, discarded) = open(&layout, "a.patch", None, false).unwrap();

        assert!(!opened, "an existing work area is not reopened");
        assert!(!discarded);
        assert_eq!(again, session);
    }

    #[test]
    fn edit_honours_an_explicit_base_and_writes_nothing_when_it_cannot_resolve() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_only(tmp.path());
        // A real upstream, so the fetch that checks for the ref succeeds offline and the
        // error can be about the ref rather than about the URL.
        let up = tmp.path().join("upstream.git");
        let repo = git2::Repository::init_bare(&up).unwrap();
        let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let who = git2::Signature::now("t", "t@e").unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            repo.commit(Some("refs/heads/main"), &who, &who, "base", &tree, &[])
                .unwrap();
        }
        drop(repo);
        std::fs::write(
            layout.config_file(),
            format!(
                "[upstream]\nurl = \"{}\"\ndefault-ref = \"main\"\n",
                up.display()
            ),
        )
        .unwrap();

        let err = open(&layout, "a.patch", Some("nope"), false).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("nope"),
            "the error should name the ref: {text}"
        );
        assert!(
            !layout.work_dir().exists(),
            "no work area may exist for a base that never resolved"
        );
    }

    #[test]
    fn edit_without_a_base_says_how_to_choose_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        testsupport::write_series(&layout, &[("a.patch", "")]);

        let err = open(&layout, "a.patch", None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--base"), "should point at the flag: {err}");
        assert!(
            err.contains("default-ref"),
            "should point at the config: {err}"
        );
    }

    #[test]
    fn editing_an_empty_patch_still_opens() {
        // `new` writes an empty patch; `edit` must be able to open one too, which comes
        // down to an empty patch replaying as no change at all.
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_only(tmp.path());
        std::fs::write(layout.patches_dir().join("a.patch"), "").unwrap();

        let (session, opened, _) = open(&layout, "a.patch", None, false).unwrap();

        assert!(opened);
        assert_eq!(session.patch, "a.patch");
        let dir = layout.work_patch_dir("a.patch").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "base\n",
            "nothing of its own to add, so the worktree is the base"
        );
    }

    #[test]
    fn naming_a_patch_the_series_does_not_have_lists_the_ones_it_does() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_only(tmp.path());

        let err = open(&layout, "nope", None, false).unwrap_err().to_string();
        assert!(err.contains("a.patch"), "the message should list it: {err}");
        assert!(err.contains("b.patch"), "the message should list it: {err}");
    }

    #[test]
    fn the_edit_command_parses_with_a_patch_and_an_optional_base() {
        let cli = Cli::try_parse_from(["skot", "patch", "edit", "a.patch"]).unwrap();
        let Command::Patch(PatchCommand::Edit(args)) = cli.command else {
            panic!("expected edit")
        };
        assert_eq!(args.patch, "a.patch");
        assert!(args.base.is_none());
        assert!(!args.discard);
        assert!(Cli::try_parse_from(["skot", "patch", "edit", "x", "--base", "v2"]).is_ok());
        assert!(Cli::try_parse_from(["skot", "patch", "edit", "x", "--discard"]).is_ok());
        let cli = Cli::try_parse_from(["skot", "patch", "edit", "x", "--yes"]).unwrap();
        let Command::Patch(PatchCommand::Edit(args)) = cli.command else {
            panic!("expected edit")
        };
        assert!(
            !args.discard,
            "the global --yes skips questions; it no longer doubles as a discard"
        );
    }
}
