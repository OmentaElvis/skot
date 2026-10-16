//! `patch push`: apply the next patch in the series, or a named one.
//!
//! The applied set is an ordered *subset* of the series (any patch can be on or off), and
//! `push` moves up it quilt-style: with a name it enables that patch and everything below
//! it (patch 3 cannot stand alone), without one it takes the single patch above the
//! current top, so a deliberate hole below is never silently filled. The set is replayed
//! against the sandbox's ref and checked out, so what the sandbox holds is always a tree
//! the series genuinely produces.
//!
//! [`pop`](super::pop) is the same operation pointed the other way, so the reasoning (which
//! patch the set moves to, and every case where the answer would be a guess) lives in
//! [`authoring::resolve_stack`] rather than here.
//!
//! With no sandbox yet and nothing recorded as applied, `push` starts the sandbox the way
//! `apply` does and lands the first patch: a directional apply can begin at the beginning.
//! A recorded set with no sandbox under it is drift, and still points at `apply`.

use anyhow::{Context, Result};
use log::{debug, info};
use serde_json::json;

use crate::cli::PatchArg;
use crate::commands::authoring::{self, Move};
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::{Sha, sandbox};
use crate::hints::Hints;
use crate::invocation::skot;
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
            "series_name": ctx.layout.series(),
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
        .entry(skot("edit-save"), "save what you change next")
        .entry(skot("pop"), "take a patch off")
        .print();
    Ok(verdict)
}

/// Replay the set up to the next (or named) patch and check it out.
///
/// With the whole series already applied and nothing named there is no next patch: the
/// ref, the sha and the tree the sandbox already holds are still resolved and reported,
/// but `added` comes back `None` and nothing is checked out.
///
/// Everything below [`run`] is this: no clap, no stdout, no verdict — a `&Layout` and a
/// scalar in, a description of the outcome out.
fn push(layout: &Layout, want: Option<&str>) -> Result<Pushed> {
    // The sandbox first, then the ref, then the mirror. But "first" has two readings:
    // a sandbox that should be there and is not (a recorded set with nothing under it)
    // is drift with one remedy, `apply`, while no sandbox and nothing recorded is just
    // a project that has not applied yet, and `push` is a directional apply, so it
    // starts the sandbox the same way `apply` does rather than sending the user around
    // the loop.
    let (config, series) = layout.project()?;
    let state = State::load(layout)?;
    authoring::require_holder(layout, &state)?;
    if !state.applied(layout).is_empty() {
        drop(authoring::require_sandbox(layout)?);
    }

    let ref_name = authoring::stack_ref(layout, &state, &config)?;

    let lock = layout.lock()?;
    let (mirror, sha) = super::sync::resolve_in_mirror(layout, &ref_name)?;
    if !sandbox::is_repository(layout) {
        sandbox::create(layout, &mirror)?;
    }

    // The whole series already applied, and no patch named: the answer is negative, not
    // broken. Anything short of that (including a set with holes) falls through to
    // `resolve_stack`, which validates membership and refuses a recorded name the series
    // no longer lists.
    let all = series.entries_lenient(layout);
    let whole_series = want.is_none()
        && !state.applied(layout).is_empty()
        && state.applied(layout).len() == all.len()
        && state
            .applied(layout)
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
            names: state.applied(layout).to_vec(),
            tree,
        });
    }

    let stack = authoring::resolve_stack(layout, &series, &state, want, Move::Deeper)?;
    let tree = authoring::land(layout, &ref_name, &sha, &stack)?;
    drop(lock);

    Ok(Pushed {
        // The named patch is the one pushed, even when healing holes below it leaves
        // other patches above in place; with no name the added patch is the new top.
        added: want
            .map(str::to_owned)
            .or_else(|| stack.names.last().cloned()),
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

    fn state_of(layout: &Layout, names: &[&str]) -> State {
        let mut state = State::load(layout).unwrap();
        state
            .set_applied(layout, names.iter().map(|n| (*n).to_owned()).collect())
            .unwrap();
        state
    }

    #[test]
    fn a_serial_prefix_pushes_that_patch_and_everything_below() {
        // The shared `[PATCH]` reading reaches `push` too: `0003` names the patch
        // whose file starts with it, and the set below it comes with it.
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
            Some("0003"),
            Move::Deeper,
        )
        .unwrap();
        assert_eq!(
            stack.names,
            vec!["0001-a.patch", "0002-b.patch", "0003-c.patch"]
        );
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
            &state_of(&layout, &["0001-a.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap();
        assert_eq!(stack.names, vec!["0001-a.patch", "0002-b.patch"]);
    }

    #[test]
    fn push_with_no_name_leaves_a_hole_below_it_alone() {
        // A hole below the top is a decision, not a fault: no-name push climbs one step
        // from the top and never heals what is underneath it.
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", ""),
                ("0002-b.patch", ""),
                ("0003-c.patch", ""),
                ("0004-d.patch", ""),
            ],
        );
        let series = layout.project().unwrap().1;

        let stack = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&layout, &["0003-c.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap();
        assert_eq!(stack.names, vec!["0003-c.patch", "0004-d.patch"]);
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
        // The reason `push` replays everything below it rather than one file: 0003 is
        // meaningless alone.
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
            &state_of(&layout, &["0001-a.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(&skot("new")), "got {err}");
    }

    #[test]
    fn pushing_something_already_applied_points_at_pop() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", ""), ("0002-b.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&layout, &["0001-a.patch"]),
            Some("0001-a.patch"),
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(&skot("pop")), "got {err}");
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
    fn push_named_fills_a_hole_below_without_silencing_one_above() {
        // A set with holes is legitimate, so the refusal tests are about names, not shape.
        // Pushing 0001 with 0003 on heals the hole under it (0003 already sits above
        // everything) and leaves the hole at 0002 exactly where it was.
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
            &state_of(&layout, &["0003-c.patch"]),
            Some("0001-a.patch"),
            Move::Deeper,
        )
        .unwrap();
        assert_eq!(stack.names, vec!["0001-a.patch", "0003-c.patch"]);
    }

    #[test]
    fn a_recorded_name_the_series_no_longer_lists_is_refused() {
        // Membership is the only shape rule: a name the series does not know means every
        // command would be guessing, so it is refused with the command that repairs it.
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", ""), ("0002-b.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&layout, &["0001-a.patch", "0009-gone.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("0009-gone.patch"),
            "the stale name should be named: {err}"
        );
        assert!(
            err.contains("does not list"),
            "the drift should be stated: {err}"
        );
        assert!(
            err.contains("unapply"),
            "the remedy should be offered: {err}"
        );
    }

    #[test]
    fn a_recorded_set_longer_than_the_series_is_refused() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = authoring::resolve_stack(
            &layout,
            &series,
            &state_of(&layout, &["0001-a.patch", "0002-gone.patch"]),
            None,
            Move::Deeper,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("0002-gone.patch"), "got {err}");
        assert!(err.contains("does not list"), "got {err}");
    }

    #[test]
    fn push_from_nothing_starts_the_sandbox_and_lands_the_first_patch() {
        // `push` is a directional apply, so a project that has not applied yet starts
        // here: the sandbox is created the way `apply` creates it, and the first patch
        // lands. The old behaviour was an error telling the user to run `apply`.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
        );
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        assert!(!sandbox::is_repository(&layout), "starts without a sandbox");

        let pushed = push(&layout, None).unwrap();

        assert_eq!(pushed.added, Some("0001-a.patch".to_owned()));
        assert_eq!(pushed.names, ["0001-a.patch"]);
        assert!(sandbox::is_repository(&layout));
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
        let cli = crate::cli::Cli::try_parse_from(["skot", "patch", "push"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(&ctx, &PatchArg { patch: None }).unwrap();

        assert_eq!(exit, Exit::Failure);
    }
}
