//! `push`: apply the next patch in the series, or a named one.
//!
//! A stack is built one patch at a time. The rule that shapes the whole command is that a
//! stack is always a *prefix of the series*: patch 3 is only correct on top of 1 and 2, so
//! `push` cannot apply the one file it was given. It replays the prefix that ends at that
//! patch and checks that out, which is why pushing 0003 onto an empty sandbox works and lands
//! on a tree the series genuinely produces.
//!
//! [`pop`](super::pop) is the same operation pointed the other way, so the reasoning — which
//! patch the stack ends at, and every case where the answer would be a guess — lives in
//! [`authoring::resolve_stack`] rather than here.

use anyhow::{Context, Result};
use log::{debug, info};
use serde_json::json;

use crate::cli::PatchArg;
use crate::commands::authoring::{self, Move};
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::{Sha, sandbox};
use crate::hints::Hints;
use crate::layout::Layout;
use crate::state::State;

/// What `push` did, presented by [`run`].
struct Pushed {
    /// The patch applied, or `None` for the empty answer.
    added: Option<String>,
    ref_name: String,
    sha: Sha,
    names: Vec<String>,
    tree: git2::Oid,
}

/// Apply the next patch in the series, or the named one.
///
/// With the whole series already applied the answer is negative rather than broken:
/// the report is the ordinary one — ref, sha, the applied list — with `added` null and
/// `ok` false, the sandbox is left as it stands, and the exit code is `Exit::Failure`
/// so a script can stop there.
pub fn run(ctx: &Ctx, arg: &PatchArg) -> Result<Exit> {
    let pushed = push(&ctx.layout, arg.patch.as_deref())?;
    let verdict = if pushed.added.is_some() {
        Exit::Success
    } else {
        Exit::Failure
    };

    if ctx.cli.json {
        emit_json(&json!({
            "ok": pushed.added.is_some(),
            "ref": pushed.ref_name,
            "sha": pushed.sha.as_str(),
            "applied": pushed.names,
            "tree": pushed.tree.to_string(),
            "added": pushed.added,
        }))?;
        return Ok(verdict);
    }

    match &pushed.added {
        Some(added) => info!(
            target: "push",
            "pushed {added}; the sandbox now holds {}",
            count(pushed.names.len(), "patch", "patches")
        ),
        None => {
            // Nothing was added, so there is no push to announce. Report the head the
            // way `pop` does: the object a script reads is the exit code either way.
            let holding = if pushed.names.is_empty() {
                "upstream only".to_owned()
            } else {
                count(pushed.names.len(), "patch", "patches")
            };
            info!(
                target: "push",
                "The sandbox is on {} ({}), holding {holding}.",
                pushed.ref_name, pushed.sha
            );
        }
    }
    debug!(target: "push", "sandbox {}", ctx.layout.sandbox_dir().display());
    for name in &pushed.names {
        debug!(target: "push", "  {name}");
    }
    debug!(
        target: "push",
        "  on {} ({})",
        pushed.ref_name, pushed.sha
    );
    Hints::new("next")
        .entry("skot refresh", "export what you change next")
        .entry("skot pop", "take a patch off")
        .print();
    Ok(verdict)
}

/// Replay the prefix up to the next (or named) patch and check it out.
///
/// With the whole series already applied and nothing named there is no next patch: the
/// ref, the sha and the tree the sandbox already holds are still resolved and reported,
/// but `added` comes back `None` and nothing is checked out.
///
/// Everything below [`run`] is this: no clap, no stdout, no verdict — a `&Layout` and a
/// scalar in, a description of the outcome out.
fn push(layout: &Layout, want: Option<&str>) -> Result<Pushed> {
    // The sandbox first, then the ref, then the mirror. A stack operation on a sandbox that is
    // not there has one clear remedy, and answering "which ref?" first would send the user
    // looking in the wrong place.
    drop(authoring::require_sandbox(layout)?);

    let (config, series) = layout.project()?;
    let state = State::load(layout)?;
    let ref_name = authoring::stack_ref(layout, &state, &config)?;

    let lock = layout.lock()?;
    let (_mirror, sha) = super::sync::resolve_in_mirror(layout, &ref_name)?;

    // The whole series already applied, and no patch named: the answer is negative, not
    // broken. The recorded stack still has to be the series' own prefix — one that has
    // drifted out of step falls through to `resolve_stack`, which says so and refuses.
    let all = series.entries_lenient(layout);
    let whole_series = want.is_none()
        && state.applied.len() == all.len()
        && state
            .applied
            .iter()
            .zip(all.iter())
            .all(|(applied, entry)| applied == &entry.name);
    if whole_series {
        let tree =
            sandbox::head_tree_oid(layout)?.context("the sandbox holds no tree to report")?;
        drop(lock);
        return Ok(Pushed {
            added: None,
            ref_name,
            sha,
            names: state.applied,
            tree,
        });
    }

    let stack = authoring::resolve_stack(layout, &series, &state, want, Move::Deeper)?;
    let tree = authoring::land(layout, &ref_name, &sha, &stack)?;
    drop(lock);

    Ok(Pushed {
        added: stack.names.last().cloned(),
        ref_name,
        sha,
        names: stack.names,
        tree,
    })
}

pub(crate) fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;
    use clap::Parser as _;

    fn state_of(names: &[&str]) -> State {
        State {
            applied: names.iter().map(|n| (*n).to_owned()).collect(),
            ..State::default()
        }
    }

    #[test]
    fn push_defaults_to_the_next_patch() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", ""),
                ("0002-b.patch", ""),
                ("0003-c.patch", ""),
            ],
        );
        let series = layout.project().unwrap().1;

        let stack = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&["0001-a.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap();
        assert_eq!(stack.names, vec!["0001-a.patch", "0002-b.patch"]);
    }

    #[test]
    fn push_from_empty_applies_only_the_one_named() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = layout.project().unwrap().1;

        let stack =
            authoring::resolve_stack(&layout, &series, &State::default(), None, Move::Deeper)
                .unwrap();
        assert_eq!(stack.names, vec!["0001-a.patch"]);
    }

    #[test]
    fn push_of_a_named_patch_replays_everything_below_it() {
        // The reason `push` replays a prefix rather than one file: 0003 is meaningless alone.
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", ""),
                ("0002-b.patch", ""),
                ("0003-c.patch", ""),
            ],
        );
        let series = layout.project().unwrap().1;

        let stack = authoring::resolve_stack(
            &layout,
            &series,
            &State::default(),
            Some("0003-c.patch"),
            Move::Deeper,
        )
        .unwrap();
        assert_eq!(
            stack.names,
            vec!["0001-a.patch", "0002-b.patch", "0003-c.patch"]
        );
    }

    #[test]
    fn push_past_the_end_says_to_start_a_new_patch() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&["0001-a.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("skot new"), "got {err}");
    }

    #[test]
    fn pushing_something_already_applied_points_at_pop() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", ""), ("0002-b.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&["0001-a.patch"]),
            Some("0001-a.patch"),
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("skot pop"), "got {err}");
    }

    #[test]
    fn pushing_a_patch_that_is_not_in_the_series_lists_the_ones_that_are() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = authoring::resolve_stack(
            &layout,
            &series,
            &State::default(),
            Some("nope"),
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("0001-a.patch"), "got {err}");
    }

    #[test]
    fn pushing_from_an_empty_series_is_refused_rather_than_succeeding_vacuously() {
        let (_tmp, layout) = testsupport::project();
        let series = layout.project().unwrap().1;
        let err = authoring::resolve_stack(&layout, &series, &State::default(), None, Move::Deeper)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no next patch"), "got {err}");
    }

    #[test]
    fn a_stack_that_is_not_a_series_prefix_is_refused() {
        // Any prefix we picked from a stack that does not match the series is a guess, and a
        // guessed stack passes `check` while building the wrong tree.
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", ""), ("0002-b.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&["0002-b.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("0002-b.patch"),
            "the mismatch should name it: {err}"
        );
        assert!(
            err.contains("unapply"),
            "the remedy should be offered: {err}"
        );
    }

    #[test]
    fn a_longer_recorded_stack_than_the_series_is_refused() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&["0001-a.patch", "0002-gone.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("lists only 1"), "got {err}");
    }

    #[test]
    fn push_with_the_whole_series_applied_reports_the_usual_status_with_nothing_added() {
        // Nothing is wrong, so nothing is refused: the report is the ordinary one —
        // same object, `added` null — and the sandbox itself is left alone.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());

        let pushed = push(&layout, None).unwrap();

        assert_eq!(pushed.added, None);
        assert_eq!(pushed.names, ["0001-a.patch", "0002-b.patch"]);
    }

    #[test]
    fn push_with_nothing_left_exits_with_a_negative_verdict() {
        // The verdict a script reads is the exit code: `Exit::Failure` with the ordinary
        // report — no `Error:`, no panic.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::applied_two_patch_stack(tmp.path());
        let cli = crate::cli::Cli::try_parse_from(["skot", "push"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(&ctx, &PatchArg { patch: None }).unwrap();

        assert_eq!(exit, Exit::Failure);
    }
}
