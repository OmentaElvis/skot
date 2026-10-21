//! `patch drop`: take a patch out of the series for good.
//!
//! A patch is two things: its line in `series` and its file in `patches/`. `drop` takes
//! both, and leaves every other patch exactly where it is: the lines below simply close
//! the gap, since order lives in the series and nowhere else.
//!
//! It refuses a patch that is applied rather than leaving the sandbox holding something
//! the series no longer lists: `skot patch pop` puts it down, `drop` takes it out, and
//! `skot patch apply` picks the set up again (interactively, it opens the picker). Beyond
//! that, deleting a file is the one thing `skot patch` cannot take back, so an
//! interactive session is asked first (No being the default) and everything else has to
//! say `--yes`: the refusal and the question together are the whole safety story, and
//! nothing here ever guesses. The file the guard will not delete is reported rather than
//! silently kept.

use anyhow::{Result, bail};
use log::{debug, info, warn};
use serde_json::json;

use crate::cli::DropArgs;
use crate::commands::authoring;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::prompt::Fallback;
use crate::series::FileGone;
use crate::session;
use crate::state::State;

/// What `drop` did, presented by [`run`].
#[derive(Debug)]
struct Removed {
    /// The patch's name as it was in the series.
    name: String,
    /// The 1-based position it occupied. `None` for a disabled patch.
    position: Option<usize>,
    /// What became of the patch's own file.
    file: FileGone,
    /// How many patches the series holds now.
    left: usize,
}

/// Take a patch out of the series: its line and its file.
///
/// Success is always [`Exit::Success`]: even with the file already gone, the line's
/// removal is the thing that was asked for. Every refusal (unknown patch, applied
/// patch, nobody to ask and no `--yes`) is an error rather than a negative verdict,
/// because the series is untouched
/// and the remedy is a different command, not a different exit code. Declining the
/// question is the one negative verdict: it is [`Exit::Failure`], the same as `move`'s
/// no-op, because the patch stays where it was.
pub fn run(ctx: &Ctx, args: &DropArgs) -> Result<Exit> {
    // Resolved before the question, the way `move` resolves before its prompts: the
    // question has to name the patch, and a doomed drop (unknown, or applied) should be
    // refused before anyone is asked to confirm it.
    let (_config, series) = ctx.layout.project()?;
    let names = series.names();
    let disabled = series.disabled();
    let mut universe: Vec<&str> = names.to_vec();
    universe.extend(disabled.iter().map(String::as_str));
    let name = authoring::resolve_patch(&universe, &names, &args.patch)?.name(&names);
    let state = State::load(&ctx.layout).unwrap_or_default();
    if state.applied(&ctx.layout).contains(&name) {
        return Err(refuse_applied(&name));
    }
    // An open work area is work the patch holds outside `patches/`, so it goes with the
    // patch rather than being left behind as a husk. Saying so before the question keeps
    // the two from being two decisions.
    let work_area = session::Manifest::load(&ctx.layout)
        .map(|m| m.get(&ctx.layout, &name).cloned())
        .unwrap_or(None);
    if !confirmed(ctx, &name, work_area.as_ref())? {
        info!(target: "drop", "kept {name}; nothing was removed");
        return Ok(Exit::Failure);
    }

    let removed = if disabled.contains(&name) {
        remove_disabled(&ctx.layout, &name)?
    } else {
        remove(&ctx.layout, &name)?
    };
    // The record must not outlive the worktree it names, so the work area is retired as
    // part of the same decision that took the patch out.
    let retired = if let Some(work_area) = work_area {
        let lock = ctx.layout.lock()?;
        session::end(&ctx.layout, &work_area)?;
        drop(lock);
        Some(work_area.patch)
    } else {
        None
    };

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "series_name": ctx.layout.series(),
            "name": removed.name,
            "position": removed.position,
            "file_removed": matches!(removed.file, FileGone::Deleted),
            "file_kept": match &removed.file {
                FileGone::Kept(path) => Some(path.display().to_string()),
                _ => None,
            },
            "work_area_retired": retired,
            "left": removed.left,
        }))?;
        return Ok(Exit::Success);
    }

    let holding = if removed.left == 0 {
        "the series is empty now".to_owned()
    } else {
        format!(
            "the series holds {}",
            super::push::count(removed.left, "patch", "patches")
        )
    };
    info!(target: "drop", "removed {}; {holding}", removed.name);
    if let Some(patch) = &retired {
        info!(target: "drop", "retired the work area on {patch}");
    }
    match &removed.file {
        FileGone::Kept(path) => warn!(
            target: "drop",
            "the file stays where it is: {} is outside the layout, which skot never deletes from",
            path.display()
        ),
        FileGone::Missing => debug!(target: "drop", "its file was already gone"),
        FileGone::Deleted => {}
    }
    debug!(target: "drop", "series now:");
    Hints::new("next")
        .entry(skot("check"), "verify the series applies")
        .entry(skot("list"), "the series as it stands")
        .print();
    Ok(Exit::Success)
}

/// The plain work behind [`run`]: no clap, no stdout, no verdict.
///
/// The lock goes on before the series is read: two removals racing over the same file
/// would otherwise both succeed, and the later save would put the earlier removal's
/// line back.
fn remove(layout: &Layout, want: &str) -> Result<Removed> {
    let lock = layout.lock()?;
    let (_config, mut series) = layout.project()?;
    let state = State::load(layout)?;

    let names = series.names();
    let index = position_of(&names, want)?;
    let name = names[index].to_owned();

    if state.applied(layout).contains(&name) {
        return Err(refuse_applied(&name));
    }

    let removal = series.remove_entry(layout, &name)?;
    series.save(layout)?;
    drop(lock);

    Ok(Removed {
        name,
        position: Some(removal.position),
        file: removal.file,
        left: series.len(),
    })
}

/// Remove a disabled patch: its `#!` line and its file.
fn remove_disabled(layout: &Layout, name: &str) -> Result<Removed> {
    let lock = layout.lock()?;
    let (_config, mut series) = layout.project()?;
    let Some(file) = series.drop_disabled(layout, name) else {
        bail!("`{name}` is no longer a disabled line in the series.");
    };
    series.save(layout)?;
    let left = series.len();
    drop(lock);

    Ok(Removed {
        name: name.to_owned(),
        position: None,
        file,
        left,
    })
}

/// The question, before anything is deleted: a `--yes` caller has already answered,
/// and a run that cannot ask is refused with the flag that says yes for it.
///
/// An open work area changes the stakes from "the file goes" to "the file and the edits
/// go", and it changes the wording for the same reason: someone who only meant to tidy the
/// series should not lose a worktree full of unsaved work by answering a question that
/// never mentioned it.
fn confirmed(ctx: &Ctx, name: &str, work_area: Option<&session::Session>) -> Result<bool> {
    let (question, help) = match work_area {
        Some(session) => (
            format!(
                "Drop {name}, delete its file, and retire its work area at {}?",
                ctx.layout
                    .work_patch_dir(session.dir_name())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| session.patch.clone())
            ),
            "The worktree goes with it, so anything unsaved is lost. Esc keeps both.",
        ),
        None => (
            format!("Drop {name} and delete its file?"),
            "Nothing else moves; only this patch and its file go. Esc keeps it.",
        ),
    };
    Ok(ctx.asker().confirm(
        &question,
        Some(help),
        Fallback::Flag(skot_args("drop", "<patch> --yes")),
    )?)
}

/// Resolve a patch name or a 1-based position to a 0-based index. An exact name always
/// wins: a patch may be called `3`.
fn position_of(names: &[&str], want: &str) -> Result<usize> {
    if let Some(i) = names.iter().position(|n| *n == want) {
        return Ok(i);
    }
    if let Ok(position) = want.parse::<usize>() {
        if position == 0 {
            bail!(
                "positions are 1-based: `{}` is the front of the series",
                skot_args("drop", "1")
            );
        }
        if position > names.len() {
            bail!(
                "the series holds {} patches, so position {position} does not exist. \
                 Patches in it: {}",
                names.len(),
                listed(names)
            );
        }
        return Ok(position - 1);
    }
    bail!(
        "`{want}` is not in the series. Patches in it: {}",
        listed(names)
    )
}

/// The refusal shared by the prompt path and the work itself: a patch the sandbox is
/// holding never goes, whatever asked.
fn refuse_applied(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`{name}` is applied, so dropping it would leave the sandbox holding a patch the \
         series no longer lists.\n\
         `{}` takes it off (and the patches above it); `{}` picks the \
         set up again afterwards.",
        skot_args("pop", name),
        skot("apply")
    )
}

/// Every patch in the series, as one line of an error message.
fn listed(names: &[&str]) -> String {
    if names.is_empty() {
        "(none)".to_owned()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::commands::testsupport;
    use clap::Parser as _;

    fn three_patches() -> (tempfile::TempDir, Layout) {
        let (tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[("a.patch", ""), ("b.patch", ""), ("c.patch", "")],
        );
        (tmp, layout)
    }

    #[test]
    fn removing_a_patch_takes_its_line_and_its_file() {
        let (_tmp, layout) = three_patches();

        let removed = remove(&layout, "b.patch").unwrap();

        assert_eq!(removed.name, "b.patch");
        assert_eq!(removed.position, Some(2));
        assert_eq!(removed.file, FileGone::Deleted);
        assert_eq!(removed.left, 2);
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "a.patch\nc.patch\n",
            "the hole closes and the patch below keeps the name it was created with"
        );
        assert!(layout.patches_dir().join("c.patch").is_file());
        assert!(!layout.patches_dir().join("b.patch").exists());
    }

    #[test]
    fn removing_by_position_does_the_same() {
        let (_tmp, layout) = three_patches();

        let removed = remove(&layout, "2").unwrap();

        assert_eq!(removed.name, "b.patch");
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "a.patch\nc.patch\n"
        );
    }

    #[test]
    fn removing_the_head_leaves_the_rest_where_they_are() {
        let (_tmp, layout) = three_patches();

        remove(&layout, "a.patch").unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "b.patch\nc.patch\n",
            "the lines below close the gap, in the order they were in"
        );
        assert!(layout.patches_dir().join("b.patch").is_file());
        assert!(layout.patches_dir().join("c.patch").is_file());
    }

    #[test]
    fn an_applied_patch_is_refused_with_a_way_to_put_it_down() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("a.patch", testsupport::patch_one()),
                ("b.patch", testsupport::patch_two()),
            ],
            &["a.patch", "b.patch"],
        )
        .0;

        let err = remove(&layout, "a.patch").unwrap_err().to_string();

        assert!(
            err.contains("is applied"),
            "the refusal states the reason: {err}"
        );
        assert!(err.contains(&skot("pop")), "it names the remedy: {err}");
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "a.patch\nb.patch\n",
            "a refused remove changes nothing"
        );
        assert!(layout.patches_dir().join("a.patch").is_file());
    }

    #[test]
    fn removing_below_the_applied_set_leaves_the_set_as_it_is() {
        // A hole below an applied patch is a legal set (the picker can build one). The
        // applied patch is the orthogonal one: the line above it goes, and it goes on
        // answering to the same name.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("a.patch", testsupport::patch_one()),
                ("b.patch", testsupport::patch_adds_g()),
            ],
            &["b.patch"],
        )
        .0;

        remove(&layout, "a.patch").unwrap();

        let state = State::load(&layout).unwrap();
        assert_eq!(
            state.applied(&layout).to_vec(),
            ["b.patch"],
            "the applied patch still answers to its own name"
        );
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "b.patch\n"
        );
    }

    #[test]
    fn an_unknown_patch_names_what_the_series_holds() {
        let (_tmp, layout) = three_patches();

        let err = remove(&layout, "nope.patch").unwrap_err().to_string();

        assert!(err.contains("`nope.patch` is not in the series"), "{err}");
        assert!(err.contains("a.patch, b.patch, c.patch"), "{err}");
        assert_eq!(
            layout
                .patches_dir()
                .read_dir()
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "patch"))
                .count(),
            3
        );
    }

    #[test]
    fn positions_are_one_based_and_cannot_run_past_the_end() {
        let (_tmp, layout) = three_patches();

        let zero = remove(&layout, "0").unwrap_err().to_string();
        assert!(zero.contains("1-based"), "{zero}");

        let past = remove(&layout, "9").unwrap_err().to_string();
        assert!(past.contains("holds 3 patches"), "{past}");
        assert!(past.contains("a.patch"), "{past}");

        assert_eq!(
            std::fs::read_to_string(layout.series_file())
                .unwrap()
                .lines()
                .count(),
            3
        );
    }

    #[test]
    fn a_missing_file_still_loses_its_line() {
        let (_tmp, layout) = three_patches();
        std::fs::remove_file(layout.patches_dir().join("a.patch")).unwrap();

        let removed = remove(&layout, "a.patch").unwrap();

        assert_eq!(removed.file, FileGone::Missing);
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "b.patch\nc.patch\n"
        );
    }

    #[test]
    fn the_last_patch_empties_the_series() {
        let (tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("a.patch", "")]);

        let removed = remove(&layout, "a.patch").unwrap();

        assert_eq!(removed.left, 0);
        assert_eq!(std::fs::read_to_string(layout.series_file()).unwrap(), "");
        assert_eq!(
            layout
                .patches_dir()
                .read_dir()
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "patch"))
                .count(),
            0
        );
        drop(tmp);
    }

    #[test]
    fn run_reports_the_removal_as_a_success() {
        let (tmp, layout) = three_patches();
        let cli = Cli::try_parse_from(["skot", "patch", "drop", "c.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: true,
        };

        let exit = run(
            &ctx,
            &DropArgs {
                patch: "c.patch".to_owned(),
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        drop(tmp);
    }

    #[test]
    fn a_script_that_does_not_say_yes_is_told_how() {
        let (tmp, layout) = three_patches();
        let cli = Cli::try_parse_from(["skot", "patch", "drop", "c.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        // No terminal under the test harness, which is the case the message exists for.
        let err = run(
            &ctx,
            &DropArgs {
                patch: "c.patch".to_owned(),
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("cannot ask"), "{err}");
        assert!(err.contains("--yes"), "{err}");
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n",
            "nothing was removed"
        );
        assert!(ctx.layout.patches_dir().join("c.patch").is_file());
        drop(tmp);
    }

    #[test]
    fn json_does_not_delete_anything_without_yes_either() {
        let (tmp, layout) = three_patches();
        let cli = Cli::try_parse_from(["skot", "patch", "drop", "c.patch", "--json"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &DropArgs {
                patch: "c.patch".to_owned(),
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("cannot ask"), "{err}");
        assert!(ctx.layout.patches_dir().join("c.patch").is_file());
        drop(tmp);
    }

    #[test]
    fn a_name_prefix_drops_that_patch() {
        // The shared `[PATCH]` reading: `b` names the patch whose file starts with
        // it, without the full name typed out.
        let (_tmp, layout) = three_patches();
        let cli = Cli::try_parse_from(["skot", "patch", "drop", "b", "--yes"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: true,
        };

        let exit = run(
            &ctx,
            &DropArgs {
                patch: "b".to_owned(),
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nc.patch\n",
            "b.patch is gone; c.patch keeps the name it was created with"
        );
        assert!(!ctx.layout.patches_dir().join("b.patch").exists());
        assert!(ctx.layout.patches_dir().join("c.patch").is_file());
    }

    /// A two-patch series on a mirror that serves `main`, with no sessions open: the state
    /// a drop runs in when authoring is in play.
    fn series_on_a_mirror(root: &std::path::Path) -> Layout {
        let (layout, _sha) = testsupport::mirror_with_one_commit(root);
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        testsupport::write_series(
            &layout,
            &[("a.patch", testsupport::patch_one()), ("b.patch", "")],
        );
        layout
    }

    #[test]
    fn dropping_a_patch_retires_its_work_area_too() {
        // A work area is work the patch holds outside `patches/`. Leaving it behind would
        // strand a worktree whose patch no longer exists, reachable only by knowing to go
        // looking for it.
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_on_a_mirror(tmp.path());
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let plan = crate::session::plan(&layout, "main", &entries[..1], Some(&entries[1])).unwrap();
        crate::session::materialise(&layout, "b.patch", &plan).unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "drop", "b.patch", "--yes"]).unwrap();
        let ctx = Ctx {
            layout: layout.clone(),
            cli,
            yes: true,
        };

        let exit = run(
            &ctx,
            &DropArgs {
                patch: "b.patch".to_owned(),
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert!(
            !layout.work_patch_dir("b.patch").unwrap().exists(),
            "the work area went with the patch"
        );
        assert!(
            crate::session::Manifest::load(&layout)
                .unwrap()
                .get(&layout, "b.patch")
                .is_none(),
            "the manifest forgets it"
        );
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "a.patch\n"
        );
        drop(tmp);
    }

    #[test]
    fn a_work_area_on_a_different_patch_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = series_on_a_mirror(tmp.path());
        let entries = layout.project().unwrap().1.entries(&layout).unwrap();
        let plan = crate::session::plan(&layout, "main", &[], Some(&entries[0])).unwrap();
        crate::session::materialise(&layout, "a.patch", &plan).unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "drop", "b.patch", "--yes"]).unwrap();
        let ctx = Ctx {
            layout: layout.clone(),
            cli,
            yes: true,
        };

        let exit = run(
            &ctx,
            &DropArgs {
                patch: "b.patch".to_owned(),
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert!(
            layout.work_patch_dir("a.patch").unwrap().exists(),
            "the unrelated work area must survive"
        );
        assert!(
            crate::session::Manifest::load(&layout)
                .unwrap()
                .get(&layout, "a.patch")
                .is_some(),
            "and stay recorded"
        );
        drop(tmp);
    }
}
