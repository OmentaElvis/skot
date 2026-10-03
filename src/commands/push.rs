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

use anyhow::Result;
use log::{debug, info};
use serde_json::json;

use crate::cli::PatchArg;
use crate::commands::authoring::{self, Move};
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::Sha;
use crate::hints::Hints;
use crate::layout::Layout;
use crate::state::State;

/// What `push` did, presented by [`run`].
struct Pushed {
    ref_name: String,
    sha: Sha,
    names: Vec<String>,
    tree: git2::Oid,
}

/// Apply the next patch in the series, or the named one.
pub fn run(ctx: &Ctx, arg: &PatchArg) -> Result<Exit> {
    let pushed = push(&ctx.layout, arg.patch.as_deref())?;

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "ref": pushed.ref_name,
            "sha": pushed.sha.as_str(),
            "applied": pushed.names,
            "tree": pushed.tree.to_string(),
            "added": pushed.names.last(),
        }))?;
        return Ok(Exit::Success);
    }

    info!(
        "pushed {} — the sandbox now holds {}",
        pushed.names.last().map(String::as_str).unwrap_or("nothing"),
        count(pushed.names.len(), "patch", "patches")
    );
    debug!("sandbox {}", ctx.layout.sandbox_dir().display());
    for name in &pushed.names {
        debug!("  {name}");
    }
    debug!("  on {} ({})", pushed.ref_name, pushed.sha);
    Hints::new("next")
        .entry("skot refresh", "export what you change next")
        .entry("skot pop", "take a patch off")
        .print();
    Ok(Exit::Success)
}

/// Replay the prefix up to the next (or named) patch and check it out.
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
    let stack = authoring::resolve_stack(layout, &series, &state, want, Move::Deeper)?;
    let tree = authoring::land(layout, &ref_name, &sha, &stack)?;
    drop(lock);

    Ok(Pushed {
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
            authoring::resolve_stack(&layout, &series, &State::default(), None, Move::Deeper).unwrap();
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

        let err =
            authoring::resolve_stack(&layout, &series, &State::default(), Some("nope"), Move::Deeper)
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
}
