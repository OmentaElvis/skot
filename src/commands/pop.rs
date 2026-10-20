//! `patch pop`: take the patch on top back off the sandbox.
//!
//! This is not `push` run backwards. Reverting one patch by hand is how a sandbox ends up
//! with files that are half-reverted, and the mistake is invisible until a build produces
//! something odd. The base is already known — the sandbox is still detached at it — so the
//! honest implementation of "undo the top patch" is "rebuild the tree the rest of the set
//! produces", which libgit2 does in one call and cannot get wrong.
//!
//! That is also why `pop` takes a patch *name*, cut quilt-style: popping b takes b
//! and everything enabled after it, since those stand on it. Turning exactly one patch off
//! while keeping the set above it whole is a different question; it is a matter of the
//! enabled set, and [`super::disable`] answers it. See [`authoring::resolve_stack`].
//!
//! With no sandbox yet and nothing recorded as applied, `pop` reports the ordinary
//! negative answer: there is nothing to pop. A recorded set with no sandbox under it is
//! drift, and still points at `apply`.

use anyhow::{Context, Result};
use log::{debug, info};
use serde_json::json;

use crate::cli::PatchArg;
use crate::commands::authoring::{self, Move};
use crate::commands::push::count;
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::{Sha, sandbox};
use crate::layout::Layout;
use crate::state::State;

/// What `pop` did, presented by [`run`].
#[derive(Debug)]
pub(super) struct Popped {
    /// The patch taken off, or `None` for the empty answer.
    removed: Option<String>,
    ref_name: String,
    sha: Sha,
    names: Vec<String>,
    tree: git2::Oid,
}

/// Remove the last applied patch, or the named one.
///
/// With nothing applied the answer is negative rather than broken: the report is the
/// ordinary one — ref, sha, the applied list — with `removed` null and `ok` false, the
/// sandbox is left as it stands, and the exit code is `Exit::Failure` so a script can
/// stop there.
pub fn run(ctx: &Ctx, arg: &PatchArg) -> Result<Exit> {
    let popped = pop(&ctx.layout, arg.patch.as_deref())?;
    let verdict = if popped.removed.is_some() {
        Exit::Success
    } else {
        Exit::Failure
    };

    if ctx.cli.json {
        emit_json(&json!({
            "ok": popped.removed.is_some(),
            "series_name": ctx.layout.series(),
            "ref": popped.ref_name,
            "sha": popped.sha.as_str(),
            "removed": popped.removed,
            "applied": popped.names,
            "tree": popped.tree.to_string(),
        }))?;
        return Ok(verdict);
    }

    if let Some(removed) = &popped.removed {
        info!(target: "pop", "popped {removed}");
    }
    let holding = if popped.names.is_empty() {
        "upstream only".to_owned()
    } else {
        count(popped.names.len(), "patch", "patches")
    };
    debug!(target: "pop", "the sandbox now holds {holding}");
    for name in &popped.names {
        debug!(target: "pop", "  {name}");
    }
    info!(
        target: "pop",
        "The sandbox is on {} ({}), holding {holding}.",
        popped.ref_name, popped.sha
    );
    Ok(verdict)
}

/// Rebuild the sandbox as the set that stays and check that out.
///
/// With nothing applied and nothing named there is no stack to rebuild: the ref, the
/// sha and the tree the sandbox already holds are still resolved and reported, but
/// `removed` comes back `None` and nothing is checked out.
///
/// Everything below [`run`] is this: no clap, no stdout, no verdict — a `&Layout` and a
/// scalar in, a description of the outcome out.
pub(super) fn pop(layout: &Layout, want: Option<&str>) -> Result<Popped> {
    // Same as `push`: a recorded set with no sandbox under it is drift with one remedy,
    // `apply`; no sandbox and nothing recorded is a project that has not applied yet,
    // and with nothing applied `pop` owes the ordinary negative answer, not an error.
    let (config, series) = layout.project()?;
    let state = State::load(layout)?;
    authoring::require_holder(layout, &state)?;
    if !state.applied(layout).is_empty() {
        drop(authoring::require_sandbox(layout)?);
    }

    // Which patch comes off: the named one, or the top of the stack — `None` when there
    // is no top to name.
    let removed = match want {
        Some(name) => Some(name.to_owned()),
        None => state.applied(layout).last().cloned(),
    };

    let ref_name = authoring::stack_ref(layout, &state, &config)?;

    let lock = layout.lock()?;
    let (mirror, sha) = super::sync::resolve_in_mirror(layout, &ref_name)?;

    let Some(removed) = removed else {
        // No set to rebuild, so nothing is checked out: rebuilding an empty set
        // would reset the sandbox to the ref and take any local work with it. The
        // report is still the ordinary one, `removed` null among the rest. Without a
        // sandbox to read a tree from, the base tree in the mirror is what an absent
        // sandbox would be holding anyway.
        let tree = if sandbox::is_repository(layout) {
            sandbox::head_tree_oid(layout)?.context("the sandbox holds no tree to report")?
        } else {
            let oid = git2::Oid::from_str(sha.as_str())?;
            mirror.repo().find_commit(oid)?.tree()?.id()
        };
        drop(lock);
        return Ok(Popped {
            removed: None,
            ref_name,
            sha,
            names: state.applied(layout).to_vec(),
            tree,
        });
    };

    let stack = authoring::resolve_stack(layout, &series, &state, want, Move::Shallower)?;
    let tree = authoring::land(layout, &ref_name, &sha, &stack)?;
    drop(lock);

    Ok(Popped {
        removed: Some(removed),
        ref_name,
        sha,
        names: stack.names,
        tree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;
    use crate::invocation::skot;
    use crate::layout::Layout;
    use clap::Parser as _;

    fn state_of(layout: &Layout, names: &[&str]) -> State {
        let mut state = State::load(layout).unwrap();
        state
            .set_applied(layout, names.iter().map(|n| (*n).to_owned()).collect())
            .unwrap();
        state
    }

    /// A sandbox on `main` with nothing applied and a series to be on.
    fn sandbox_on_main() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::project_with_sandbox(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        (tmp, layout)
    }

    fn stack_names(
        layout: &Layout,
        series: &crate::series::Series,
        s: &State,
        want: Option<&str>,
    ) -> Result<Vec<String>> {
        Ok(authoring::resolve_stack(layout, series, s, want, Move::Shallower)?.names)
    }

    #[test]
    fn pop_defaults_to_the_patch_on_top() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[("a.patch", ""), ("b.patch", ""), ("c.patch", "")],
        );
        let series = layout.project().unwrap().1;

        let names = stack_names(
            &layout,
            &series,
            &state_of(&layout, &["a.patch", "b.patch"]),
            None,
        )
        .unwrap();
        assert_eq!(names, vec!["a.patch"]);
    }

    #[test]
    fn pop_with_no_name_takes_the_topmost_and_keeps_the_hole() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[("a.patch", ""), ("b.patch", ""), ("c.patch", "")],
        );
        let series = layout.project().unwrap().1;

        let names = stack_names(
            &layout,
            &series,
            &state_of(&layout, &["a.patch", "c.patch"]),
            None,
        )
        .unwrap();
        assert_eq!(names, vec!["a.patch"]);
    }

    #[test]
    fn popping_a_named_patch_also_takes_what_was_enabled_after_it() {
        // Quilt direction: b and c were enabled together after a, so turning b off takes c
        // with it rather than leaving it standing on a patch that
        // is no longer there.
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[("a.patch", ""), ("b.patch", ""), ("c.patch", "")],
        );
        let series = layout.project().unwrap().1;

        let names = stack_names(
            &layout,
            &series,
            &state_of(&layout, &["a.patch", "b.patch", "c.patch"]),
            Some("b.patch"),
        )
        .unwrap();
        assert_eq!(names, vec!["a.patch"]);
    }

    #[test]
    fn popping_from_one_patch_leaves_nothing_applied() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("a.patch", "")]);
        let series = layout.project().unwrap().1;

        assert!(
            stack_names(&layout, &series, &state_of(&layout, &["a.patch"]), None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn popping_from_an_empty_stack_says_so() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("a.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = stack_names(&layout, &series, &State::default(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing to pop"), "got {err}");
    }

    #[test]
    fn popping_the_bottom_patch_empties_the_stack() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("a.patch", ""), ("b.patch", "")]);
        let series = layout.project().unwrap().1;

        assert!(
            stack_names(
                &layout,
                &series,
                &state_of(&layout, &["a.patch", "b.patch"]),
                Some("a.patch")
            )
            .unwrap()
            .is_empty(),
            "dropping the bottom patch leaves nothing under the ones above it"
        );
    }

    #[test]
    fn popping_something_not_applied_is_refused() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("a.patch", ""), ("b.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = stack_names(
            &layout,
            &series,
            &state_of(&layout, &["a.patch"]),
            Some("b.patch"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("b.patch"), "got {err}");
    }

    #[test]
    fn pop_needs_a_sandbox_when_a_set_is_recorded() {
        // A recorded set with no sandbox under it is drift, not a fresh project, and the
        // remedy is still `apply`. Checked before the mirror: answering "no such ref"
        // first would send the user looking in the wrong place.
        let (_tmp, layout) = testsupport::project();
        let mut state = State::default();
        state.sandboxes.insert(
            layout.series().to_owned(),
            crate::state::SandboxState {
                ref_name: Some("main".to_owned()),
                applied: vec!["a.patch".to_owned()],
                ..Default::default()
            },
        );
        state.save(&layout).unwrap();
        let err = pop(&layout, None).unwrap_err().to_string();
        assert!(err.contains("no sandbox"), "got {err}");
        assert!(
            err.contains(&skot("apply")),
            "the remedy should be offered: {err}"
        );
    }

    #[test]
    fn pop_with_no_sandbox_and_nothing_applied_is_the_ordinary_negative() {
        // Nothing recorded and no sandbox is a project that has not applied yet, so
        // `pop` owes the same answer it gives an empty stack: a report, no error.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = testsupport::mirror_with_one_commit(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();

        let popped = pop(&layout, None).unwrap();

        assert_eq!(popped.removed, None);
        assert!(popped.names.is_empty());
        assert_eq!(popped.ref_name, "main");
        assert_eq!(popped.sha.as_str(), sha);
    }

    #[test]
    fn pop_with_nothing_applied_reports_the_usual_status_with_nothing_removed() {
        // The top-of-stack name used to be read before anything checked there was one.
        // The empty answer is still a report — same object, `removed` null — and the
        // sandbox itself is left alone.
        let (_tmp, layout) = sandbox_on_main();

        let popped = pop(&layout, None).unwrap();

        assert_eq!(popped.removed, None);
        assert!(popped.names.is_empty());
        assert_eq!(popped.ref_name, "main");
    }

    #[test]
    fn pop_with_nothing_applied_exits_with_a_negative_verdict() {
        // The verdict a script reads is the exit code: `Exit::Failure` with the ordinary
        // report — no `Error:`, no panic.
        let (_tmp, layout) = sandbox_on_main();
        let cli = crate::cli::Cli::try_parse_from(["skot", "patch", "pop"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(&ctx, &PatchArg { patch: None }).unwrap();

        assert_eq!(exit, Exit::Failure);
    }
}
