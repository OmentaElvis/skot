//! `patch move`: reorder the series, one patch at a time.
//!
//! A move changes the order the series file lists, and nothing else: the patch's line
//! goes to its new place, every patch keeps the name it was created with, and no file
//! under `patches/` is read or written. Only an unapplied patch can move: the applied
//! set is a tree the sandbox is holding, and reordering what it is made of would make
//! the next apply disagree with what is on disk, so `skot patch pop` puts it down first.
//!
//! The destination is a final position (`--to 3` means "become the third patch"), or an
//! existing patch name to land before. Left unnamed, the patch is picked from a list and
//! the position is typed: positions are what `patch list` shows and what the series
//! holds, and a script gets told to name both rather than a default it did not ask for.

use anyhow::{Context, Result, bail};
use log::info;
use serde_json::json;

use crate::cli::MoveArgs;
use crate::commands::authoring;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::prompt::{Asker, Fallback};
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
    /// The range the move touched, after the move.
    window: Vec<(usize, String)>,
}

/// Move a patch to a different position: its line, and nothing else.
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
    let asker = ctx.asker();

    let want = match &args.patch {
        Some(want) => want.clone(),
        None => pick_patch(&asker, &names, state.applied(&ctx.layout))?,
    };
    let disabled = series.disabled();
    let mut universe: Vec<&str> = names.iter().map(String::as_str).collect();
    universe.extend(disabled.iter().map(String::as_str));
    let from = resolve_position(&names, &universe, &want)?;
    let name = names[from].clone();

    let to = match &args.to {
        Some(at) => resolve_destination(&names, at, &name)?,
        None => ask_position(&asker, &names, &name)?,
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
            "series_name": ctx.layout.series(),
            "patch": moved.name,
            "from": moved.from,
            "to": moved.to,
        }))?;
        return Ok(Exit::Success);
    }

    info!(
        target: "move",
        "moved {} from position {} to position {}",
        moved.name, moved.from, moved.to
    );
    for line in window_lines(&moved.window) {
        report::item(&line);
    }
    Hints::new("next")
        .entry(skot("check"), "verify the series still applies")
        .entry(skot("list"), "the series in its new order")
        .print();
    Ok(Exit::Success)
}

/// The plain work behind [`run`]: lock, move the line, save. No prompts, no verdict.
///
/// The lock goes on before the series is read: two reorders racing over the same files
/// would otherwise both succeed, and the later save would put the earlier order back.
fn relocate(layout: &Layout, name: &str, to: usize) -> Result<Moved> {
    let lock = layout.lock()?;
    let (_config, mut series) = layout.project()?;
    let state = State::load(layout)?;

    if state.applied(layout).contains(&name.to_owned()) {
        bail!(
            "`{name}` is applied, so moving it would reorder what the sandbox is holding.\n\
             `{}` takes it off (and the patches above it); `{}` puts the set back afterwards.",
            skot_args("pop", name),
            skot("apply")
        );
    }

    let moved = series.reorder(name, to)?;
    series.save(layout)?;
    drop(lock);

    Ok(Moved {
        name: name.to_owned(),
        from: moved.from + 1,
        to: moved.to + 1,
        window: moved.window,
    })
}

/// Resolve a patch reference to a 0-based index among the enabled patches: the same
/// name, prefix or position reading every `[PATCH]` argument shares. A disabled patch
/// can still be named, and is refused with the command that puts it back.
fn resolve_position(names: &[String], all: &[&str], want: &str) -> Result<usize> {
    let enabled: Vec<&str> = names.iter().map(String::as_str).collect();
    let name = authoring::resolve_patch(all, &enabled, want)?.name(&enabled);
    names.iter().position(|n| *n == name).with_context(|| {
        format!(
            "`{name}` is disabled, so it has no position to move.\n`{}` puts it back in.",
            skot_args("enable", &name)
        )
    })
}

/// Resolve `--to`: a 1-based final position, or the name (or prefix) of an enabled
/// patch to land before.
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
    let enabled: Vec<&str> = names.iter().map(String::as_str).collect();
    let name = authoring::resolve_patch(&enabled, &enabled, at)?.name(&enabled);
    Ok(names
        .iter()
        .position(|n| *n == name)
        .expect("resolved name is enabled"))
}

/// Which patch to move: only unapplied patches are offered, the others cannot move
/// and offering them would only produce the refusal afterwards. The labels carry the
/// series position, which is what the answer decides. A run that cannot ask is
/// refused with the flag that names both ends.
fn pick_patch(asker: &Asker, names: &[String], applied: &[String]) -> Result<String> {
    let options: Vec<(String, String)> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| !applied.contains(*n))
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
    let chosen = asker.select(
        "Which patch to move",
        Some("A patch that is applied cannot move; pop it first."),
        labels,
        Fallback::Flag(skot_args("move", "<patch> --to <patch|N>")),
        None,
        None,
    )?;
    let (_, patch) = options
        .iter()
        .find(|(label, _)| *label == chosen)
        .expect("the label came from the list");
    Ok(patch.clone())
}

/// The position the patch should end at, typed. The answer is a number, not a name:
/// a patch's name is its own and does not change, so the only thing to choose is
/// where its line goes, and a menu of positions is the whole series long. A run that
/// cannot ask is refused with the flag that names both ends.
fn ask_position(asker: &Asker, names: &[String], moving: &str) -> Result<usize> {
    let count = names.len();
    if count == 1 {
        bail!("the series holds one patch, so there is nowhere to move it.");
    }
    let question = format!("New position for {moving} (1 to {count})");
    let answer = asker.text_validated(
        &question,
        Some("The number the patch should have after the move. Esc cancels."),
        Fallback::Flag(skot_args("move", "<patch> --to <patch|N>")),
        move |input: &str| parse_position(input, count).map(|_| ()),
    )?;
    Ok(parse_position(&answer, count).expect("the validator admitted this") - 1)
}

/// A typed position: a number from 1 to `count`.
fn parse_position(input: &str, count: usize) -> std::result::Result<usize, String> {
    match input.trim().parse::<usize>() {
        Ok(position) if (1..=count).contains(&position) => Ok(position),
        Ok(_) | Err(_) => Err(format!("a number from 1 to {count}")),
    }
}

/// The range a move touched as `patch list`-style lines, shortened to its ends when the
/// range is wide: the middle of a long run is the least informative part of it, and
/// fifty lines help nobody read the result.
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
            &[("a.patch", "a\n"), ("b.patch", "b\n"), ("c.patch", "c\n")],
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
    fn moving_a_patch_forward_reorders_the_series() {
        let (_tmp, layout) = three_patches();

        let moved = relocate(&layout, "a.patch", 2).unwrap();

        assert_eq!(moved.from, 1);
        assert_eq!(moved.to, 3);
        assert_eq!(
            moved.window,
            vec![
                (1, "b.patch".to_owned()),
                (2, "c.patch".to_owned()),
                (3, "a.patch".to_owned()),
            ],
            "the touched range, ready to show"
        );
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "b.patch\nc.patch\na.patch\n"
        );
        for (name, body) in [("a.patch", "a\n"), ("b.patch", "b\n"), ("c.patch", "c\n")] {
            assert_eq!(
                std::fs::read_to_string(layout.patches_dir().join(name)).unwrap(),
                body,
                "{name} is the file it always was"
            );
        }
    }

    #[test]
    fn moving_a_patch_backward_reorders_the_series() {
        let (_tmp, layout) = three_patches();

        relocate(&layout, "c.patch", 0).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "c.patch\na.patch\nb.patch\n"
        );
    }

    #[test]
    fn a_move_changes_only_the_order_of_the_series() {
        let (_tmp, layout) = three_patches();
        // A disabled patch has a line of its own in the file, between the two patches that
        // trade places here: a reorder has to leave it exactly where it is.
        let mut series = crate::series::Series::load(&layout.series_file()).unwrap();
        assert!(series.disable("b.patch"));
        series.save(&layout).unwrap();

        relocate(&layout, "c.patch", 0).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "c.patch\na.patch\n#!b.patch\n",
            "the order is the series file's; only its lines moved"
        );
        let mut on_disk: Vec<String> = std::fs::read_dir(layout.patches_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".patch"))
            .collect();
        on_disk.sort();
        assert_eq!(
            on_disk,
            ["a.patch", "b.patch", "c.patch"],
            "a name belongs to the patch for good, so every patch answers to its own"
        );
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("c.patch")).unwrap(),
            "c\n",
            "and the patch itself is the same file, holding what it held"
        );
    }

    #[test]
    fn moving_through_the_middle_leaves_the_rest_in_order() {
        let (tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[
                ("a.patch", "a\n"),
                ("b.patch", "b\n"),
                ("c.patch", "c\n"),
                ("d.patch", "d\n"),
            ],
        );

        relocate(&layout, "a.patch", 2).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "b.patch\nc.patch\na.patch\nd.patch\n",
            "the move passes over two patches; the one past them keeps its place"
        );
        drop(tmp);
    }

    #[test]
    fn a_move_leaves_the_applied_set_alone() {
        let (_tmp, layout) = three_patches();
        applied(&["b.patch", "c.patch"], &layout);

        relocate(&layout, "a.patch", 2).unwrap();

        let state = State::load(&layout).unwrap();
        assert_eq!(
            state.applied(&layout).to_vec(),
            vec!["b.patch".to_owned(), "c.patch".to_owned()],
            "a move is a fact about the series file, and the applied set is not that file"
        );
    }

    #[test]
    fn moving_to_its_own_position_is_a_no_op() {
        let (_tmp, layout) = three_patches();

        relocate(&layout, "b.patch", 1).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n"
        );
    }

    #[test]
    fn an_unknown_patch_names_what_the_series_holds() {
        let (_tmp, layout) = three_patches();

        let err = relocate(&layout, "nope.patch", 0).unwrap_err().to_string();

        assert!(err.contains("not in the series"), "{err}");
        assert!(err.contains("a.patch, b.patch, c.patch"), "{err}");
    }

    #[test]
    fn a_position_past_the_end_is_refused() {
        let (_tmp, layout) = three_patches();

        let err = relocate(&layout, "a.patch", 3).unwrap_err().to_string();

        assert!(err.contains("holds 3 patches"), "{err}");
    }

    #[test]
    fn run_reports_the_move_as_a_success() {
        let (tmp, layout) = three_patches();
        let cli = Cli::try_parse_from(["skot", "patch", "move", "a.patch", "--to", "3"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &MoveArgs {
                patch: Some("a.patch".to_owned()),
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
        applied(&["b.patch"], &layout);

        let err = relocate(&layout, "b.patch", 0).unwrap_err().to_string();

        assert!(err.contains("is applied"), "{err}");
        assert!(err.contains(&skot_args("pop", "b.patch")), "{err}");
        drop(tmp);
    }

    #[test]
    fn a_short_window_is_shown_line_for_line() {
        let window: Vec<(usize, String)> = ["alpha.patch", "beta.patch", "gamma.patch"]
            .iter()
            .enumerate()
            .map(|(i, n)| (i + 1, (*n).to_owned()))
            .collect();

        assert_eq!(
            window_lines(&window),
            vec![
                "  1  alpha.patch".to_owned(),
                "  2  beta.patch".to_owned(),
                "  3  gamma.patch".to_owned(),
            ]
        );
    }

    #[test]
    fn a_wide_window_is_shortened_to_its_ends() {
        let names = [
            "aa.patch", "bb.patch", "cc.patch", "dd.patch", "ee.patch", "ff.patch", "gg.patch",
            "hh.patch", "ii.patch", "jj.patch",
        ];
        let window: Vec<(usize, String)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (i + 1, (*n).to_owned()))
            .collect();

        let lines = window_lines(&window);

        assert_eq!(lines.len(), 7, "three, an ellipsis, three");
        assert_eq!(lines[0], "  1  aa.patch");
        assert_eq!(lines[3], "...");
        assert_eq!(lines[6], " 10  jj.patch");
    }

    #[test]
    fn a_position_outside_the_series_does_not_parse() {
        assert!(parse_position("0", 5).is_err());
        assert!(parse_position("6", 5).is_err());
        assert!(parse_position("two", 5).is_err());
        assert_eq!(parse_position(" 3 ", 5), Ok(3));
    }
}
