//! `patch drop`: take a patch out of the series for good.
//!
//! A patch is two things: its line in `series` and its file in `patches/`. `drop` takes
//! both, and renumbers the patches after it down so the numbers stay contiguous, the same
//! promise `new --at` and `import --at` keep in the other direction. Lexical order stays
//! application order either way.
//!
//! It refuses a patch that is applied rather than leaving the sandbox holding something
//! the series no longer lists: `skot patch pop` puts it down, `drop` takes it out, and
//! `skot patch apply` picks the set up again (interactively, it opens the picker). The refusal
//! is the whole safety story; nothing here ever guesses, and the file the guard will not
//! delete is reported rather than silently kept.

use anyhow::{Result, bail};
use log::{debug, info, warn};
use serde_json::json;

use crate::cli::DropArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::series::FileGone;
use crate::state::State;

/// What `drop` did, presented by [`run`].
#[derive(Debug)]
struct Removed {
    /// The patch's name as it was in the series.
    name: String,
    /// The 1-based position it occupied.
    position: usize,
    /// The renames the tail's renumbering caused.
    renames: Vec<(String, String)>,
    /// What became of the patch's own file.
    file: FileGone,
    /// How many patches the series holds now.
    left: usize,
}

/// Take a patch out of the series: its line and its file.
///
/// Success is always [`Exit::Success`]: even with the file already gone, the line's
/// removal is the thing that was asked for. Every refusal (unknown patch, applied
/// patch, an out-of-series file squatting on a tail rename) is an error rather than a
/// negative verdict, because the series is untouched and the remedy is a different
/// command, not a different exit code.
pub fn run(ctx: &Ctx, args: &DropArgs) -> Result<Exit> {
    let removed = remove(&ctx.layout, &args.patch)?;

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "name": removed.name,
            "position": removed.position,
            "renamed": removed
                .renames
                .iter()
                .map(|(from, to)| json!({ "from": from, "to": to }))
                .collect::<Vec<_>>(),
            "file_removed": matches!(removed.file, FileGone::Deleted),
            "file_kept": match &removed.file {
                FileGone::Kept(path) => Some(path.display().to_string()),
                _ => None,
            },
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
    for (from, to) in &removed.renames {
        debug!(target: "drop", "  {from} -> {to}");
    }
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
    let mut state = State::load(layout)?;

    let names = series.names();
    let index = if let Some(i) = names.iter().position(|n| *n == want) {
        // An exact name always wins: a patch may be called `3`.
        i
    } else if let Ok(position) = want.parse::<usize>() {
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
                listed(&names)
            );
        }
        position - 1
    } else {
        bail!(
            "`{want}` is not in the series. Patches in it: {}",
            listed(&names)
        );
    };
    let name = names[index].to_owned();

    if state.applied.contains(&name) {
        bail!(
            "`{name}` is applied, so dropping it would leave the sandbox holding a patch the \
             series no longer lists.\n\
             `{}` takes it off (and the patches above it); `{}` picks the \
             set up again afterwards.",
            skot_args("pop", &name),
            skot("apply")
        );
    }

    let removal = series.remove_renumbering(layout, &name)?;
    series.save(layout)?;
    state.follow_renames(layout, &removal.renames)?;
    drop(lock);

    Ok(Removed {
        name,
        position: removal.position,
        renames: removal.renames,
        file: removal.file,
        left: series.len(),
    })
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
            &[
                ("0001-a.patch", ""),
                ("0002-b.patch", ""),
                ("0003-c.patch", ""),
            ],
        );
        (tmp, layout)
    }

    #[test]
    fn removing_a_patch_takes_the_line_the_file_and_folds_the_numbers_down() {
        let (_tmp, layout) = three_patches();

        let removed = remove(&layout, "0002-b.patch").unwrap();

        assert_eq!(removed.name, "0002-b.patch");
        assert_eq!(removed.position, 2);
        assert_eq!(
            removed.renames,
            vec![("0003-c.patch".to_owned(), "0002-c.patch".to_owned())]
        );
        assert_eq!(removed.file, FileGone::Deleted);
        assert_eq!(removed.left, 2);
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-a.patch\n0002-c.patch\n",
            "the tail renumbered down: no gap, and lexical order still is application order"
        );
        assert!(layout.patches_dir().join("0002-c.patch").is_file());
        assert!(!layout.patches_dir().join("0002-b.patch").exists());
        assert!(!layout.patches_dir().join("0003-c.patch").exists());
    }

    #[test]
    fn removing_by_position_does_the_same() {
        let (_tmp, layout) = three_patches();

        let removed = remove(&layout, "2").unwrap();

        assert_eq!(removed.name, "0002-b.patch");
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-a.patch\n0002-c.patch\n"
        );
    }

    #[test]
    fn removing_the_head_renumbers_everything_behind_it() {
        let (_tmp, layout) = three_patches();

        remove(&layout, "0001-a.patch").unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-b.patch\n0002-c.patch\n",
            "the tail folds down into the freed numbers, in order"
        );
        assert!(layout.patches_dir().join("0001-b.patch").is_file());
        assert!(layout.patches_dir().join("0002-c.patch").is_file());
    }

    #[test]
    fn an_applied_patch_is_refused_with_a_way_to_put_it_down() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
            &["0001-a.patch", "0002-b.patch"],
        )
        .0;

        let err = remove(&layout, "0001-a.patch").unwrap_err().to_string();

        assert!(
            err.contains("is applied"),
            "the refusal states the reason: {err}"
        );
        assert!(err.contains(&skot("pop")), "it names the remedy: {err}");
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-a.patch\n0002-b.patch\n",
            "a refused remove changes nothing"
        );
        assert!(layout.patches_dir().join("0001-a.patch").is_file());
    }

    #[test]
    fn an_unapplied_patch_below_the_applied_set_can_go_and_the_set_follows_the_renames() {
        // A hole below an applied patch is a legal set (the picker can build one), so the
        // tail's renumbering has to carry `applied` along with it. The applied patch is
        // the orthogonal one, so it lands without the patch below it.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_adds_g()),
            ],
            &["0002-b.patch"],
        )
        .0;

        remove(&layout, "0001-a.patch").unwrap();

        let state = State::load(&layout).unwrap();
        assert_eq!(
            state.applied,
            ["0001-b.patch"],
            "the applied patch followed its rename"
        );
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-b.patch\n"
        );
    }

    #[test]
    fn a_stray_file_squatting_on_a_tail_rename_is_refused_before_anything_moves() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", ""), ("0002-b.patch", "")]);
        // Removing 0001 frees its number for 0002-b, which would become 0001-b: a file
        // the series does not list already sits there.
        std::fs::write(layout.patches_dir().join("0001-b.patch"), b"not mine").unwrap();

        let err = remove(&layout, "0001-a.patch").unwrap_err().to_string();

        assert!(err.contains("0001-b.patch"), "{err}");
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-a.patch\n0002-b.patch\n",
            "the refusal came before the file went or the tail moved"
        );
        assert!(layout.patches_dir().join("0001-a.patch").is_file());
        assert!(layout.patches_dir().join("0002-b.patch").is_file());
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0001-b.patch")).unwrap(),
            "not mine",
            "a stranger's file is never overwritten"
        );
    }

    #[test]
    fn an_unknown_patch_names_what_the_series_holds() {
        let (_tmp, layout) = three_patches();

        let err = remove(&layout, "nope.patch").unwrap_err().to_string();

        assert!(err.contains("`nope.patch` is not in the series"), "{err}");
        assert!(
            err.contains("0001-a.patch, 0002-b.patch, 0003-c.patch"),
            "{err}"
        );
        assert_eq!(layout.patches_dir().read_dir().unwrap().count(), 3);
    }

    #[test]
    fn positions_are_one_based_and_cannot_run_past_the_end() {
        let (_tmp, layout) = three_patches();

        let zero = remove(&layout, "0").unwrap_err().to_string();
        assert!(zero.contains("1-based"), "{zero}");

        let past = remove(&layout, "9").unwrap_err().to_string();
        assert!(past.contains("holds 3 patches"), "{past}");
        assert!(past.contains("0001-a.patch"), "{past}");

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
        std::fs::remove_file(layout.patches_dir().join("0001-a.patch")).unwrap();

        let removed = remove(&layout, "0001-a.patch").unwrap();

        assert_eq!(removed.file, FileGone::Missing);
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-b.patch\n0002-c.patch\n"
        );
    }

    #[test]
    fn the_last_patch_empties_the_series() {
        let (tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);

        let removed = remove(&layout, "0001-a.patch").unwrap();

        assert_eq!(removed.left, 0);
        assert_eq!(std::fs::read_to_string(layout.series_file()).unwrap(), "");
        assert_eq!(layout.patches_dir().read_dir().unwrap().count(), 0);
        drop(tmp);
    }

    #[test]
    fn run_reports_the_removal_as_a_success() {
        let (tmp, layout) = three_patches();
        let cli = Cli::try_parse_from(["skot", "patch", "drop", "0003-c.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DropArgs {
                patch: "0003-c.patch".to_owned(),
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        drop(tmp);
    }
}
