//! `pop`: take the patch on top back off the sandbox.
//!
//! This is not `push` run backwards. Reverting one patch by hand is how a sandbox ends up
//! with files that are half-reverted, and the mistake is invisible until a build produces
//! something odd. The base is already known — the sandbox is still detached at it — so the
//! honest implementation of "undo the top patch" is "rebuild the tree the rest of the stack
//! produces", which libgit2 does in one call and cannot get wrong.
//!
//! That is also why `pop` takes a patch *name*. Popping from the middle of a stack is a real
//! thing to want ("0002 is wrong, drop it and keep 0003 on top of 0001"), and supporting it is
//! the same re-derivation, just aimed at a shorter prefix. See [`authoring::resolve_stack`].

use anyhow::Result;
use log::{debug, info};
use serde_json::json;

use crate::cli::PatchArg;
use crate::commands::authoring::{self, Move};
use crate::commands::push::count;
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::Sha;
use crate::layout::Layout;
use crate::state::State;

/// What `pop` did, presented by [`run`].
#[derive(Debug)]
pub(super) struct Popped {
    removed: String,
    ref_name: String,
    sha: Sha,
    names: Vec<String>,
    tree: git2::Oid,
}

/// Remove the last applied patch, or the named one.
pub fn run(ctx: &Ctx, arg: &PatchArg) -> Result<Exit> {
    let popped = pop(&ctx.layout, arg.patch.as_deref())?;

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "ref": popped.ref_name,
            "sha": popped.sha.as_str(),
            "removed": popped.removed,
            "applied": popped.names,
            "tree": popped.tree.to_string(),
        }))?;
        return Ok(Exit::Success);
    }

    info!("popped {}", popped.removed);
    let holding = if popped.names.is_empty() {
        "upstream only".to_owned()
    } else {
        count(popped.names.len(), "patch", "patches")
    };
    debug!("the sandbox now holds {holding}");
    for name in &popped.names {
        debug!("  {name}");
    }
    info!(
        "The sandbox is on {} ({}), holding {holding}.",
        popped.ref_name, popped.sha
    );
    Ok(Exit::Success)
}

/// Rebuild the sandbox as the prefix below the removed patch and check that out.
///
/// Everything below [`run`] is this: no clap, no stdout, no verdict — a `&Layout` and a
/// scalar in, a description of the outcome out.
pub(super) fn pop(layout: &Layout, want: Option<&str>) -> Result<Popped> {
    // The sandbox first, then the ref, then the mirror. A stack operation on a sandbox that is
    // not there has one clear remedy, and answering "which ref?" first would send the user
    // looking in the wrong place.
    let repo = authoring::require_sandbox(layout)?;
    drop(repo);

    let (config, series) = layout.project()?;
    let state = State::load(layout)?;
    let ref_name = authoring::stack_ref(layout, &state, &config)?;

    let removed = match want {
        Some(name) => name.to_owned(),
        None => state
            .applied
            .last()
            .cloned()
            .expect("resolve_stack refuses an empty stack"),
    };

    let lock = layout.lock()?;
    let (_mirror, sha) = super::sync::resolve_in_mirror(layout, &ref_name)?;
    let stack = authoring::resolve_stack(layout, &series, &state, want, Move::Shallower)?;
    let tree = authoring::land(layout, &ref_name, &sha, &stack)?;
    drop(lock);

    Ok(Popped {
        removed,
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
    use crate::layout::Layout;

    fn state_of(names: &[&str]) -> State {
        State {
            applied: names.iter().map(|n| (*n).to_owned()).collect(),
            ..State::default()
        }
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
            &[
                ("0001-a.patch", ""),
                ("0002-b.patch", ""),
                ("0003-c.patch", ""),
            ],
        );
        let series = layout.project().unwrap().1;

        let names = stack_names(
            &layout,
            &series,
            &state_of(&["0001-a.patch", "0002-b.patch"]),
            None,
        )
        .unwrap();
        assert_eq!(names, vec!["0001-a.patch"]);
    }

    #[test]
    fn popping_from_one_patch_leaves_nothing_applied() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = layout.project().unwrap().1;

        assert!(
            stack_names(&layout, &series, &state_of(&["0001-a.patch"]), None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn popping_from_an_empty_stack_says_so() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = stack_names(&layout, &series, &State::default(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing to pop"), "got {err}");
    }

    #[test]
    fn popping_the_bottom_patch_empties_the_stack() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", ""), ("0002-b.patch", "")]);
        let series = layout.project().unwrap().1;

        assert!(
            stack_names(
                &layout,
                &series,
                &state_of(&["0001-a.patch", "0002-b.patch"]),
                Some("0001-a.patch")
            )
            .unwrap()
            .is_empty(),
            "dropping the bottom patch leaves nothing under the ones above it"
        );
    }

    #[test]
    fn popping_something_not_applied_is_refused() {
        let (_tmp, layout) = testsupport::project();
        testsupport::write_series(&layout, &[("0001-a.patch", ""), ("0002-b.patch", "")]);
        let series = layout.project().unwrap().1;

        let err = stack_names(
            &layout,
            &series,
            &state_of(&["0001-a.patch"]),
            Some("0002-b.patch"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("0002-b.patch"), "got {err}");
    }

    #[test]
    fn pop_needs_a_sandbox() {
        // Checked before the mirror: a missing sandbox has one clear remedy, and answering
        // "no such ref" first would send the user looking in the wrong place.
        let (_tmp, layout) = testsupport::project();
        let err = pop(&layout, None).unwrap_err().to_string();
        assert!(err.contains("no sandbox"), "got {err}");
        assert!(
            err.contains("skot apply"),
            "the remedy should be offered: {err}"
        );
    }
}
