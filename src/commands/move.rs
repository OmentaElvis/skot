//! `patch move`: reorder the series, one patch at a time.
//!
//! Moving is a rotation: the patch steps through a temporary name, the entries it
//! passes fold one place at a time, and the numbers stay contiguous, so lexical order
//! remains application order. Only an unapplied patch can move — the applied set is a
//! tree the sandbox is holding, and reordering what it is made of would make the next
//! apply disagree with what is on disk, so `skot patch pop` puts it down first.
//!
//! The destination is a final position (`--to 3` means "become the third patch"), or an
//! existing patch name to land before. Left unnamed, the patch is picked from a list and
//! the position is typed: numbers are what `ls` shows and what they stay after the move,
//! while names renumber under the rotation. A script gets told to name both rather than
//! a default it did not ask for.

use std::io::IsTerminal as _;

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::MoveArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::report;
use crate::state::State;

/// What a move did, presented by [`run`].
#[derive(Debug)]
struct Moved {
    /// The patch's name as it was in the series.
    name: String,
    /// The 1-based position it occupied.
    from: usize,
    /// The 1-based position it occupies now.
    to: usize,
    /// The renames the rotation caused.
    renames: Vec<(String, String)>,
    /// The range the rotation touched, after the move.
    window: Vec<(usize, String)>,
}

/// Move a patch to a different position: its line, its file, and the numbers in between.
pub fn run(ctx: &Ctx, args: &MoveArgs) -> Result<Exit> {
    // The questions are asked before the lock, the way `apply` asks which patches: a
    // prompt sitting on the lock is one the user can walk away from mid-answer.
    let (_config, series) = ctx.layout.project()?;
    let names: Vec<String> = series.names().into_iter().map(String::from).collect();
    if names.is_empty() {
        bail!(
            "the series at {} holds no patches, so there is nothing to move.\n\
             `{}` starts a patch.",
            series.path().display(),
            skot_args("new", "<name>")
        );
    }
    let state = State::load(&ctx.layout).unwrap_or_default();
    let interactive = !ctx.cli.json && std::io::stdin().is_terminal();

    let want = match &args.patch {
        Some(want) => want.clone(),
        None if interactive => pick_patch(&names, &state)?,
        None => bail!(
            "cannot ask which patch to move here.\n\
             Name both ends: `{}`.",
            skot_args("move", "<patch> --to <patch|N>")
        ),
    };
    let from = resolve_position(&names, &want)?;
    let name = names[from].clone();

    let to = match &args.to {
        Some(at) => resolve_destination(&names, at, &name)?,
        None if interactive => ask_position(&names, &name)?,
        None => bail!(
            "cannot ask where the patch should go here.\n\
             Name the destination: `{}`.",
            skot_args("move", "<patch> --to <patch|N>")
        ),
    };

    if to == from {
        info!(
            target: "move",
            "{name} is already at position {}; nothing moved",
            from + 1
        );
        return Ok(Exit::Failure);
    }

    let moved = relocate(&ctx.layout, &name, to)?;

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "patch": moved.name,
            "from": moved.from,
            "to": moved.to,
            "renamed": moved
                .renames
                .iter()
                .map(|(from, to)| json!({ "from": from, "to": to }))
                .collect::<Vec<_>>(),
        }))?;
        return Ok(Exit::Success);
    }

    info!(
        target: "move",
        "moved {} from position {} to position {}",
        moved.name, moved.from, moved.to
    );
    for (from, to) in &moved.renames {
        debug!(target: "move", "  {from} -> {to}");
    }
    for line in window_lines(&moved.window) {
        report::item(&line);
    }
    Hints::new("next")
        .entry(skot("check"), "verify the series still applies")
        .entry(skot("list"), "the series in its new order")
        .print();
    Ok(Exit::Success)
}

/// The plain work behind [`run`]: lock, renumber, record. No prompts, no verdict.
///
/// The lock goes on before the series is read: two reorders racing over the same files
/// would otherwise both succeed, and the later save would put the earlier order back.
fn relocate(layout: &Layout, name: &str, to: usize) -> Result<Moved> {
    let lock = layout.lock()?;
    let (_config, mut series) = layout.project()?;
    let mut state = State::load(layout)?;

    if state.applied.contains(&name.to_owned()) {
        bail!(
            "`{name}` is applied, so moving it would reorder what the sandbox is holding.\n\
             `{}` takes it off (and the patches above it); `{}` puts the set back afterwards.",
            skot_args("pop", name),
            skot("apply")
        );
    }

    let moved = series.move_renumbering(layout, name, to)?;
    series.save(layout)?;
    state.follow_renames(layout, &moved.renames)?;
    drop(lock);

    Ok(Moved {
        name: name.to_owned(),
        from: moved.from + 1,
        to: moved.to + 1,
        renames: moved.renames,
        window: moved.window,
    })
}

/// Resolve a patch name or a 1-based position to a 0-based index.
fn resolve_position(names: &[String], want: &str) -> Result<usize> {
    if let Some(index) = names.iter().position(|n| n == want) {
        return Ok(index);
    }
    if let Ok(position) = want.parse::<usize>() {
        if position == 0 {
            bail!(
                "positions are 1-based: `{}` is the front of the series",
                skot_args("move", "1")
            );
        }
        if position > names.len() {
            bail!(
                "the series holds {}, so position {position} does not exist. Patches in it: {}",
                super::push::count(names.len(), "patch", "patches"),
                names.join(", ")
            );
        }
        return Ok(position - 1);
    }
    bail!(
        "`{want}` is not in the series. Patches in it: {}",
        names.join(", ")
    )
}

/// Resolve `--to`: a 1-based final position, or the name of a patch to land before.
fn resolve_destination(names: &[String], at: &str, moving: &str) -> Result<usize> {
    if let Ok(position) = at.parse::<usize>() {
        if position == 0 {
            bail!("positions are 1-based: `--to 1` is the front of the series");
        }
        if position > names.len() {
            bail!(
                "the series holds {} patches, so position {position} is past the end \
                 (the last position is {}).",
                names.len(),
                names.len()
            );
        }
        return Ok(position - 1);
    }
    if at == moving {
        bail!(
            "`{moving}` cannot move before itself.\n`{}` lists the series.",
            skot("list")
        );
    }
    if let Some(index) = names.iter().position(|n| n == at) {
        return Ok(index);
    }
    bail!(
        "`{at}` is not in the series. Patches in it: {}",
        names.join(", ")
    )
}

/// Ask an interactive session which patch to move. Only unapplied patches are offered:
/// the others cannot move, and offering them would only produce the refusal afterwards.
/// The labels carry the series position, the same numbers `ls` shows.
fn pick_patch(names: &[String], state: &State) -> Result<String> {
    let options: Vec<(String, String)> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| !state.applied.contains(*n))
        .map(|(i, n)| (format!("{:>3}  {n}", i + 1), n.clone()))
        .collect();
    if options.is_empty() {
        bail!(
            "every patch is applied, so there is nothing that can move.\n`{}` puts the \
             top one down.",
            skot("pop")
        );
    }
    let labels: Vec<String> = options.iter().map(|(label, _)| label.clone()).collect();
    let chosen = inquire::Select::new("Which patch to move", labels)
        .with_help_message("A patch that is applied cannot move; pop it first.")
        .prompt()
        .context("asking which patch to move")?;
    let (_, patch) = options
        .iter()
        .find(|(label, _)| *label == chosen)
        .expect("the label came from the list");
    Ok(patch.clone())
}

/// Ask an interactive session for the position the patch should end at. The answer is a
/// number, not a name: numbers are what `ls` shows and what they stay after the move,
/// while names renumber under the rotation, and a menu of positions is the whole series
/// long.
fn ask_position(names: &[String], moving: &str) -> Result<usize> {
    let count = names.len();
    if count == 1 {
        bail!("the series holds one patch, so there is nowhere to move it.");
    }
    let question = format!("New position for {moving} (1 to {count})");
    let answer = inquire::Text::new(&question)
        .with_help_message("The number the patch should have after the move. Esc cancels.")
        .with_validator(move |input: &str| {
            Ok(match parse_position(input, count) {
                Ok(_) => inquire::validator::Validation::Valid,
                Err(message) => inquire::validator::Validation::Invalid(message.into()),
            })
        })
        .prompt()
        .context("asking where the patch should go")?;
    Ok(parse_position(&answer, count).expect("the validator admitted this") - 1)
}

/// A typed position: a number from 1 to `count`.
fn parse_position(input: &str, count: usize) -> std::result::Result<usize, String> {
    match input.trim().parse::<usize>() {
        Ok(position) if (1..=count).contains(&position) => Ok(position),
        Ok(_) | Err(_) => Err(format!("a number from 1 to {count}")),
    }
}

/// The range a rotation touched as `ls`-style lines, shortened to its ends when the
/// range is wide: the middle of a long rotation is the least informative part of it,
/// and fifty lines of renumbered names helps nobody read the result.
fn window_lines(window: &[(usize, String)]) -> Vec<String> {
    const ENDS: usize = 3;
    let render = |(pos, name): &(usize, String)| format!("{pos:>3}  {name}");
    if window.len() <= ENDS * 2 + 1 {
        return window.iter().map(render).collect();
    }
    let mut lines: Vec<String> = window[..ENDS].iter().map(render).collect();
    lines.push("...".to_owned());
    lines.extend(window[window.len() - ENDS..].iter().map(render));
    lines
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
                ("0001-a.patch", "a\n"),
                ("0002-b.patch", "b\n"),
                ("0003-c.patch", "c\n"),
            ],
        );
        (tmp, layout)
    }

    fn applied(names: &[&str], layout: &Layout) -> State {
        let mut state = State::load(layout).unwrap();
        state
            .set_applied(layout, names.iter().map(|n| n.to_string()).collect())
            .unwrap();
        state
    }

    #[test]
    fn moving_forward_rotates_the_patches_and_the_numbers_follow() {
        let (_tmp, layout) = three_patches();

        let moved = relocate(&layout, "0001-a.patch", 2).unwrap();

        assert_eq!(moved.from, 1);
        assert_eq!(moved.to, 3);
        assert_eq!(
            moved.window,
            vec![
                (1, "0001-b.patch".to_owned()),
                (2, "0002-c.patch".to_owned()),
                (3, "0003-a.patch".to_owned()),
            ],
            "the touched range, ready to show"
        );
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-b.patch\n0002-c.patch\n0003-a.patch\n"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0003-a.patch")).unwrap(),
            "a\n",
            "the patch's own file moved with it, contents intact"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0001-b.patch")).unwrap(),
            "b\n"
        );
        assert!(!layout.patches_dir().join("0001-a.patch").exists());
    }

    #[test]
    fn moving_backward_is_the_same_rotation_the_other_way() {
        let (_tmp, layout) = three_patches();

        relocate(&layout, "0003-c.patch", 0).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-c.patch\n0002-a.patch\n0003-b.patch\n"
        );
    }

    #[test]
    fn moving_through_the_middle_only_touches_the_range_it_passes() {
        let (tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", "a\n"),
                ("0002-b.patch", "b\n"),
                ("0003-c.patch", "c\n"),
                ("0004-d.patch", "d\n"),
            ],
        );

        relocate(&layout, "0001-a.patch", 2).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-b.patch\n0002-c.patch\n0003-a.patch\n0004-d.patch\n",
            "the last patch keeps its name: the rotation stops where the move does"
        );
        drop(tmp);
    }

    #[test]
    fn the_applied_set_follows_the_renames() {
        let (_tmp, layout) = three_patches();
        applied(&["0002-b.patch", "0003-c.patch"], &layout);

        relocate(&layout, "0001-a.patch", 2).unwrap();

        let state = State::load(&layout).unwrap();
        assert_eq!(
            state.applied,
            vec!["0001-b.patch".to_owned(), "0002-c.patch".to_owned()],
            "the same patches, under their new names, still in series order"
        );
    }

    #[test]
    fn moving_to_its_own_position_is_a_no_op() {
        let (_tmp, layout) = three_patches();

        let moved = relocate(&layout, "0002-b.patch", 1).unwrap();

        assert_eq!(moved.renames, Vec::new());
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-a.patch\n0002-b.patch\n0003-c.patch\n"
        );
    }

    #[test]
    fn an_unknown_patch_names_what_the_series_holds() {
        let (_tmp, layout) = three_patches();

        let err = relocate(&layout, "nope.patch", 0).unwrap_err().to_string();

        assert!(err.contains("not in the series"), "{err}");
        assert!(
            err.contains("0001-a.patch, 0002-b.patch, 0003-c.patch"),
            "{err}"
        );
    }

    #[test]
    fn a_position_past_the_end_is_refused() {
        let (_tmp, layout) = three_patches();

        let err = relocate(&layout, "0001-a.patch", 3)
            .unwrap_err()
            .to_string();

        assert!(err.contains("holds 3 patches"), "{err}");
    }

    #[test]
    fn a_stray_file_squatting_on_a_rename_target_is_refused_before_anything_moves() {
        let (tmp, layout) = testsupport::project();
        // A gap at 2, so moving 0003-c.patch to the front wants 0002-a.patch, a name
        // no series member holds.
        testsupport::write_series(&layout, &[("0001-a.patch", "a\n"), ("0003-c.patch", "c\n")]);
        std::fs::write(layout.patches_dir().join("0002-a.patch"), "stray\n").unwrap();

        let err = relocate(&layout, "0003-c.patch", 0)
            .unwrap_err()
            .to_string();

        assert!(err.contains("0002-a.patch"), "{err}");
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-a.patch\n0003-c.patch\n",
            "the series is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0003-c.patch")).unwrap(),
            "c\n",
            "so are the files"
        );
        drop(tmp);
    }

    #[test]
    fn a_leftover_temporary_from_an_interrupted_move_is_refused() {
        let (_tmp, layout) = three_patches();
        std::fs::write(
            layout.patches_dir().join(".skot-move-0001-a.patch"),
            "half-moved\n",
        )
        .unwrap();

        let err = relocate(&layout, "0001-a.patch", 2)
            .unwrap_err()
            .to_string();

        assert!(err.contains("interrupted"), "{err}");
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0001-a.patch")).unwrap(),
            "a\n",
            "nothing moved"
        );
    }

    #[test]
    fn run_reports_the_move_as_a_success() {
        let (tmp, layout) = three_patches();
        let cli =
            Cli::try_parse_from(["skot", "patch", "move", "0001-a.patch", "--to", "3"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &MoveArgs {
                patch: Some("0001-a.patch".to_owned()),
                to: Some("3".to_owned()),
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        drop(tmp);
    }

    #[test]
    fn a_script_that_names_neither_end_is_told_how() {
        let (tmp, layout) = three_patches();
        let cli = Cli::try_parse_from(["skot", "patch", "move"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        // No terminal under the test harness, which is exactly the case the message
        // exists for.
        let err = run(
            &ctx,
            &MoveArgs {
                patch: None,
                to: None,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("cannot ask"), "{err}");
        assert!(
            err.contains(&skot_args("move", "<patch> --to <patch|N>")),
            "{err}"
        );
        drop(tmp);
    }

    #[test]
    fn an_applied_patch_is_refused_with_a_way_to_put_it_down() {
        let (tmp, layout) = three_patches();
        applied(&["0002-b.patch"], &layout);

        let err = relocate(&layout, "0002-b.patch", 0)
            .unwrap_err()
            .to_string();

        assert!(err.contains("is applied"), "{err}");
        assert!(err.contains(&skot_args("pop", "0002-b.patch")), "{err}");
        drop(tmp);
    }

    #[test]
    fn a_short_window_is_shown_line_for_line() {
        let window: Vec<(usize, String)> =
            (1..=3).map(|i| (i, format!("000{i}-p.patch"))).collect();

        assert_eq!(
            window_lines(&window),
            vec![
                "  1  0001-p.patch".to_owned(),
                "  2  0002-p.patch".to_owned(),
                "  3  0003-p.patch".to_owned(),
            ]
        );
    }

    #[test]
    fn a_wide_window_is_shortened_to_its_ends() {
        let window: Vec<(usize, String)> =
            (1..=10).map(|i| (i, format!("00{i:02}-p.patch"))).collect();

        let lines = window_lines(&window);

        assert_eq!(lines.len(), 7, "three, an ellipsis, three");
        assert_eq!(lines[0], "  1  0001-p.patch");
        assert_eq!(lines[3], "...");
        assert_eq!(lines[6], " 10  0010-p.patch");
    }

    #[test]
    fn a_position_outside_the_series_does_not_parse() {
        assert!(parse_position("0", 5).is_err());
        assert!(parse_position("6", 5).is_err());
        assert!(parse_position("two", 5).is_err());
        assert_eq!(parse_position(" 3 ", 5), Ok(3));
    }
}
