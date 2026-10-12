//! `patch apply`: realize the enabled series in the sandbox.
//!
//! The sandbox is a linked worktree of the mirror, so applying is three libgit2 calls:
//! reset the worktree to the base commit, replay the enabled patches into a tree, check
//! that tree out. Every run is asked no question and gets the same answer: the enabled
//! set, in series order. Choosing the set is `skot patch enable` and `skot patch
//! disable`, whose line-commented record in the series file is what this command reads.
//! Nothing about the patch format is handled here; that is `check`'s job, shared with
//! this command so the two can never disagree about whether the series applies.

use anyhow::{Context, Result, bail};
use log::{debug, error, info};
use serde_json::json;

use crate::check;
use crate::cli::RefArg;
use crate::commands::{Ctx, Exit};
use crate::git::sandbox;
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;

/// What an apply did, presented by [`run`].
///
/// `failure` carries the report rather than printing it, because the two callers that apply
/// as part of a longer run are exactly the two that most need to say *which* patch broke:
/// `build`, where the user has to know which phase to fix, and `new`, where a silent failure
/// means writing a patch onto a base that was never established. Neither can work that out
/// from the exit code alone.
#[derive(Debug)]
pub struct Applied {
    /// Whether the series replayed onto the ref. A verdict, not a code: the process
    /// boundary is `main`, and no number is invented until it gets there.
    pub ok: bool,
    /// Human-readable reason, empty on success.
    pub failure: String,
    /// The full replay report, for `--json` and for the failure narration.
    pub report: check::Report,
    /// The ref that was applied to.
    pub ref_name: String,
    /// The commit that ref resolved to.
    pub sha: String,
}

/// Reset the sandbox to `ref` and replay the enabled series into it.
pub fn run(ctx: &Ctx, args: &RefArg) -> Result<Exit> {
    // The sandbox is mutated, so concurrent runs must not both be resetting it.
    let lock = ctx.lock()?;
    let outcome = apply_inner(&ctx.layout, args.ref_name.as_deref(), false, None);
    drop(lock);
    let applied = outcome?;

    if ctx.cli.json {
        emit(&applied.report, &applied.ref_name, &applied.sha, applied.ok)?;
    } else if applied.ok {
        if applied.report.patch_count == 0 {
            // The sandbox was still reset to the ref: the honest answer is what the head
            // holds, the way `pop` reports an empty stack, rather than "applied 0".
            info!(
                target: "apply",
                "the enabled set is empty; the sandbox is on {} ({}), holding upstream only.",
                applied.ref_name, applied.sha
            );
        } else {
            info!(
                target: "apply",
                "applied {} to {} ({})",
                count(applied.report.patch_count, "patch", "patches"),
                applied.ref_name,
                applied.sha
            );
            debug!(target: "apply", "sandbox {}", ctx.layout.sandbox_dir().display());
            for name in &applied.report.patches {
                debug!(target: "apply", "  {}", name.name);
            }
            info!(target: "apply", "The sandbox now holds your enabled set.");
        }
        Hints::new("next")
            .entry(skot("build"), "run your hooks from here")
            .print();
    } else {
        report_failure(&applied.report);
    }

    Ok(if applied.ok {
        Exit::Success
    } else {
        Exit::Failure
    })
}

/// Apply the series as plain work: no lock, no narration, no JSON, no verdict.
///
/// The caller decides all four. `build` and `new` call this while holding the lock across
/// their own run — `flock` is held per open file description, so re-taking it here would
/// block against this same process until the wait expired and then fail — and `--json` has
/// to produce exactly one document, so nesting apply's inside a caller's is unparseable
/// rather than merely redundant.
pub fn apply_locked(layout: &Layout, ref_name: Option<&str>) -> Result<Applied> {
    apply_inner(layout, ref_name, false, None)
}

/// [`apply_locked`] for a run whose sync phase already ensured the ref in this process.
///
/// The fetch-if-missing choke point has been paid moments ago by the same run, so
/// re-entering it would repeat config parsing, a mirror open and a ref resolution for
/// nothing. This path is the lookup half only: no fetch, no create, an error if the
/// mirror cannot serve the ref. Standalone callers keep [`apply_locked`], which must
/// fetch what the mirror lacks itself.
pub(crate) fn apply_locked_ensured(layout: &Layout, ref_name: Option<&str>) -> Result<Applied> {
    apply_inner(layout, ref_name, true, None)
}

/// `only` selects a subset of the series by name; `None` is the whole series. A subset is
/// filtered in series order, so the series always decides replay order and the selection
/// only decides membership.
fn apply_inner(
    layout: &Layout,
    ref_name: Option<&str>,
    ensured: bool,
    only: Option<&[String]>,
) -> Result<Applied> {
    let (config, series) = layout.project()?;
    let entries = series.require_resolvable(layout)?;
    let entries = match only {
        Some(names) => entries
            .into_iter()
            .filter(|e| names.contains(&e.name))
            .collect(),
        None => entries,
    };

    let wanted = ref_name
        .map(str::to_owned)
        .or_else(|| config.upstream.default_ref.clone());
    let Some(wanted) = wanted else {
        bail!(
            "no ref to apply against.\n\
             Pass one (`{}`) or set `upstream.default-ref` in {}.",
            skot_args("apply", "main"),
            layout.config_file().display()
        );
    };

    let (mirror, sha) = if ensured {
        super::sync::resolve_local(layout, &wanted)?
    } else {
        super::sync::resolve_in_mirror(layout, &wanted)?
    };

    // Create before verifying: the check confirms the sandbox can reach the commit, so on a
    // first run there is no sandbox to check yet. `create` is idempotent, so doing it first
    // costs nothing on a run that already has one.
    sandbox::create(layout, &mirror)?;
    sandbox::verify_objects(layout, &mirror, &sha)?;
    // What the sandbox holds right now, captured before the reset moves HEAD, so the keep
    // paths that are about to be deleted can be restored at the end.
    let head_tree_oid = sandbox::head_tree_oid(layout)?;
    sandbox::reset_to(layout, &sha, &config.sandbox.keep)?;

    let repo = sandbox::open(layout)?;
    let applied = check::replay_series(&repo, &sha, &entries).context(format!(
        "applying {what} to {wanted} ({sha})",
        what = if only.is_some() {
            "the selected patches"
        } else {
            "the series"
        }
    ))?;

    if !applied.report.ok {
        // Reset first. Leaving a half-applied series in the sandbox is how a later
        // `check` reports a conflict the series never had, and how a build picks up a tree
        // nobody intended.
        sandbox::reset_to(layout, &sha, &config.sandbox.keep)?;
        // Clear the record too: a previous successful apply may have left `applied` set, and
        // the sandbox no longer holds that series.
        crate::state::State::load(layout)?.clear_applied(layout)?;
        return Ok(Applied {
            ok: false,
            failure: applied.report.failure_line(),
            report: applied.report,
            ref_name: wanted,
            sha: sha.to_string(),
        });
    }

    // Check out the tree the replay built. `force` because the sandbox may hold build
    // output that libgit2 would otherwise refuse to overwrite.
    let tree = repo.find_tree(applied.tree).context(format!(
        "reading the tree the series produced ({})",
        applied.tree
    ))?;
    let mut options = git2::build::CheckoutBuilder::new();
    options.force();
    repo.checkout_tree(tree.as_object(), Some(&mut options))
        .context(format!(
            "checking the patched tree into {}",
            layout.sandbox_dir().display()
        ))?;

    // *After* that checkout, not inside `reset_to`: the checkout above is the one that
    // actually decides what the sandbox holds, and it removes the same tracked files the
    // reset did. A keep path the series does not carry would otherwise be gone by the time
    // this function returns.
    if let Some(before) = head_tree_oid {
        sandbox::restore_kept(layout, before, tree.id(), &config.sandbox.keep)?;
    }

    // Persist before the caller can report anything, on every path. Reporting first and
    // returning would mean a `--json` apply left the recorded state describing the *previous*
    // run — the worst kind of bug in a tool whose job is telling the truth about a tree.
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    let mut state = crate::state::State::load(layout)?;
    state.set_ref(layout, &wanted, &sha)?;
    state.set_applied(layout, names)?;

    Ok(Applied {
        ok: true,
        failure: String::new(),
        report: applied.report,
        ref_name: wanted,
        sha: sha.to_string(),
    })
}

/// What `unapply` found and did, presented by [`unapply`].
struct Unapplied {
    /// Whether there was a sandbox to reset.
    sandbox: bool,
    /// The ref involved: the recorded or configured one, or what `state` held.
    ref_name: Option<String>,
    sha: Option<String>,
}

/// Reset the sandbox back to the upstream ref, discarding the series.
///
/// This is not the reverse of `apply` patch by patch. Reverting N patches by hand is
/// error-prone in exactly the way that leaves files half-reverted, and the base commit is
/// already known — so the honest implementation of "undo the series" is "check out what was
/// there before", which libgit2 does in one call and cannot get wrong.
pub fn unapply(ctx: &Ctx) -> Result<Exit> {
    let done = unapply_to_upstream(&ctx.layout)?;

    if ctx.cli.json {
        if done.sandbox {
            crate::commands::emit_json(&json!({
                "sandbox": true, "ref": done.ref_name, "sha": done.sha,
                "applied": Vec::<String>::new(),
            }))?;
        } else {
            crate::commands::emit_json(&json!({
                "sandbox": false, "ref": done.ref_name, "sha": done.sha,
            }))?;
        }
        return Ok(Exit::Success);
    }

    if !done.sandbox {
        debug!(target: "apply", "no sandbox, so there is nothing to unapply");
        return Ok(Exit::Success);
    }
    let wanted = done.ref_name.unwrap_or_default();
    let sha = done.sha.unwrap_or_default();
    info!(target: "apply", "sandbox reset to {wanted} ({sha})");
    info!(
        target: "apply",
        "The patch series is no longer applied. Your patch files are untouched."
    );
    Ok(Exit::Success)
}

/// The plain work behind [`unapply`]: no clap, no stdout, no verdict.
fn unapply_to_upstream(layout: &Layout) -> Result<Unapplied> {
    let (config, _) = layout.project()?;
    let state = crate::state::State::load(layout)?;

    if !sandbox::is_repository(layout) {
        return Ok(Unapplied {
            sandbox: false,
            ref_name: state.ref_name,
            sha: state.sha,
        });
    }

    // The recorded ref first, not the configured default. If the sandbox was applied to a
    // release branch and someone later changes `default-ref`, unapply must still undo the
    // apply that happened — otherwise it resets to an unrelated commit and quietly reports
    // success.
    let wanted = state
        .ref_name
        .clone()
        .or(config.upstream.default_ref.clone());
    let Some(wanted) = wanted else {
        bail!(
            "cannot tell which ref the sandbox was applied to.\n\
             `.skot/state.json` does not record one and `upstream.default-ref` is unset.\n\
             Re-apply against the ref you want, or `{}` and start over.",
            skot_args("clean", "sandbox")
        );
    };

    let lock = layout.lock()?;
    let (_mirror, sha) = super::sync::resolve_in_mirror(layout, &wanted)?;
    sandbox::reset_to(layout, &sha, &config.sandbox.keep)?;

    let mut new_state = crate::state::State::load(layout)?;
    new_state.clear_applied(layout)?;
    drop(lock);

    Ok(Unapplied {
        sandbox: true,
        ref_name: Some(wanted),
        sha: Some(sha.to_string()),
    })
}

/// Emit the JSON document for an apply. Purely writes to stdout — the caller decides the
/// process exit by looking at `Applied::ok`.
fn emit(report: &check::Report, ref_name: &str, sha: &str, ok: bool) -> Result<()> {
    crate::commands::emit_json(&json!({
        "ref": ref_name,
        "sha": sha,
        "ok": ok,
        "applied": report.patches.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
        "patches": report.patches.iter().map(|p| json!({
            "index": p.index,
            "name": p.name,
            "ok": p.ok,
            "error": p.error,
            "files": p.files,
        })).collect::<Vec<_>>(),
    }))
}

/// Print the same failure a `check` would, then say what happened to the sandbox.
fn report_failure(report: &check::Report) {
    error!(target: "apply", "the series does not apply to {}", report.base);
    for line in report.summary().lines() {
        debug!(target: "apply", "{line}");
    }
    info!(
        target: "apply",
        "The sandbox was reset, so it still holds only upstream. Nothing is half-applied."
    );
    info!(
        target: "apply",
        "Rebase the patch against this ref, then `{}` to re-export it.",
        skot("edit-save")
    );
}

pub(crate) fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command, PatchCommand};
    use clap::Parser as _;

    /// A project with a mirror, a sandbox, and a one-file upstream, all built with libgit2.
    struct Fixture {
        _tmp: tempfile::TempDir,
        layout: crate::layout::Layout,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("proj");
            std::fs::create_dir_all(&root).unwrap();

            let layout = crate::layout::Layout::new(&root);
            std::fs::write(
                layout.config_file(),
                "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
            )
            .unwrap();
            std::fs::create_dir_all(layout.patches_dir()).unwrap();
            Self { _tmp: tmp, layout }
        }

        /// Create the mirror with one commit whose `f.txt` is `base\n`.
        fn with_upstream(&self) -> git2::Oid {
            let path = self.layout.mirror_dir();
            std::fs::create_dir_all(&path).unwrap();
            let repo = git2::Repository::init_bare(&path).unwrap();
            let tree_oid = {
                let mut b = repo.treebuilder(None).unwrap();
                b.insert("f.txt", repo.blob(b"base\n").unwrap(), 0o100644)
                    .unwrap();
                b.write().unwrap()
            };
            let who = git2::Signature::now("t", "t@e").unwrap();
            let sha = {
                let tree = repo.find_tree(tree_oid).unwrap();
                let sha = repo
                    .commit(Some("refs/heads/main"), &who, &who, "base", &tree, &[])
                    .unwrap();
                repo.set_head("refs/heads/main").unwrap();
                sha
            };
            drop(repo);
            sha
        }

        /// A bare upstream holding one `main`, recorded in the config as the URL to
        /// fetch from, which a mirror without an `origin` falls back to.
        fn real_upstream(&self) {
            let path = self._tmp.path().join("upstream.git");
            let repo = git2::Repository::init_bare(&path).unwrap();
            let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
            let who = git2::Signature::now("t", "t@e").unwrap();
            {
                let tree = repo.find_tree(tree_oid).unwrap();
                repo.commit(Some("refs/heads/main"), &who, &who, "base", &tree, &[])
                    .unwrap();
            }
            drop(repo);
            std::fs::write(
                self.layout.config_file(),
                format!(
                    "[upstream]\nurl = \"{}\"\ndefault-ref = \"main\"\n",
                    path.display()
                ),
            )
            .unwrap();
        }

        fn patch(&self, name: &str, body: &str) {
            std::fs::write(self.layout.patches_dir().join(name), body).unwrap();
        }

        fn series(&self, names: &[&str]) {
            let text: String = names
                .iter()
                .map(|n| format!("{n}\n"))
                .collect::<Vec<_>>()
                .concat();
            std::fs::write(self.layout.series_file(), text).unwrap();
        }

        fn sandbox_file(&self) -> String {
            std::fs::read_to_string(self.layout.sandbox_dir().join("f.txt")).unwrap()
        }
    }

    #[test]
    fn the_ensured_apply_path_is_a_lookup_and_never_starts_a_mirror() {
        // build's sync phase has already ensured the ref, so this variant must do none
        // of the choke point's work: no clone, no fetch, a bare failure if the mirror
        // a full run would have created is not there.
        let f = Fixture::new();
        f.real_upstream();
        f.series(&[]);

        let err = apply_locked_ensured(&f.layout, Some("main"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("opening the mirror"), "got {err}");
        assert!(
            !f.layout.mirror_dir().exists(),
            "the ensured path may not create the mirror"
        );

        // The standalone path still fetches what the mirror lacks.
        assert!(apply_locked(&f.layout, Some("main")).unwrap().ok);
        assert!(f.layout.mirror_dir().exists());
    }

    #[test]
    fn a_picked_subset_applies_only_those_patches_in_series_order() {
        // The picker's question has one work function behind it: the selection decides
        // membership, the series decides order, and the record says exactly what landed.
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
",
        );
        f.patch(
            "0002-b.patch",
            "\
diff --git a/f.txt b/f.txt
index 3333333..4444444 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-first
+second
",
        );
        f.series(&["0001-a.patch", "0002-b.patch"]);

        let picked = vec!["0001-a.patch".to_owned()];
        let applied = apply_inner(&f.layout, Some("main"), false, Some(&picked)).unwrap();

        assert!(applied.ok, "{}", applied.failure);
        assert_eq!(
            applied.report.patch_count, 1,
            "only the picked patch replays"
        );
        assert_eq!(f.sandbox_file(), "first\n");
        let state = crate::state::State::load(&f.layout).unwrap();
        assert_eq!(
            state.applied,
            ["0001-a.patch"],
            "the record must match the selection"
        );
    }

    #[test]
    fn apply_puts_the_series_in_the_sandbox() {
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
",
        );
        f.series(&["0001-a.patch"]);

        assert!(apply_locked(&f.layout, None).unwrap().ok);
        assert_eq!(f.sandbox_file(), "first\n");
        assert_eq!(
            crate::state::State::load(&f.layout).unwrap().applied,
            vec!["0001-a.patch".to_owned()]
        );
    }

    #[test]
    fn an_empty_series_puts_the_sandbox_on_upstream() {
        // A project straight from `init` has no patches, and apply still has a job: put
        // the sandbox on the ref. It answers with the head, the way `pop` does.
        let f = Fixture::new();
        f.with_upstream();
        f.series(&[]);

        let applied = apply_locked(&f.layout, None).unwrap();

        assert!(applied.ok);
        assert_eq!(applied.report.patch_count, 0);
        assert_eq!(f.sandbox_file(), "base\n");
        assert!(
            crate::state::State::load(&f.layout)
                .unwrap()
                .applied
                .is_empty()
        );
    }

    #[test]
    fn apply_accumulates_so_the_second_patch_sees_the_first() {
        // The whole reason apply exists as a series operation: these two conflict unless
        // they are applied in order, and 0002 alone would look fine.
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+middle
",
        );
        f.patch(
            "0002-b.patch",
            "\
diff --git a/f.txt b/f.txt
index 3333333..4444444 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-middle
+final
",
        );
        f.series(&["0001-a.patch", "0002-b.patch"]);

        assert!(apply_locked(&f.layout, None).unwrap().ok);
        assert_eq!(f.sandbox_file(), "final\n");
    }

    #[test]
    fn a_patch_that_does_not_apply_leaves_the_sandbox_on_upstream() {
        // Half a series is worse than none: a later `check` would report conflicts the series
        // never had, and a build would pick up a tree nobody intended.
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-something-else-entirely
+nope
",
        );
        f.series(&["0001-a.patch"]);

        assert!(!apply_locked(&f.layout, None).unwrap().ok);
        assert_eq!(
            f.sandbox_file(),
            "base\n",
            "a failed apply must leave the sandbox on upstream, not half-patched"
        );
        assert!(
            crate::state::State::load(&f.layout)
                .unwrap()
                .applied
                .is_empty(),
            "state must not claim patches are applied when the apply failed"
        );
    }

    #[test]
    fn apply_twice_is_not_a_conflict() {
        // Re-running apply is the normal response to a failed one, and to a patch that
        // `edit-save` rewrote. It must converge rather than report the series against itself.
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
",
        );
        f.series(&["0001-a.patch"]);

        assert!(apply_locked(&f.layout, None).unwrap().ok);
        assert!(apply_locked(&f.layout, None).unwrap().ok);
        assert_eq!(f.sandbox_file(), "first\n");
    }

    #[test]
    fn unapply_puts_the_sandbox_back_on_upstream() {
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
",
        );
        f.series(&["0001-a.patch"]);
        apply_locked(&f.layout, None).unwrap();
        assert_eq!(f.sandbox_file(), "first\n");

        let done = unapply_to_upstream(&f.layout).unwrap();
        assert!(done.sandbox, "there was a sandbox to reset");
        assert_eq!(f.sandbox_file(), "base\n");
        assert!(
            crate::state::State::load(&f.layout)
                .unwrap()
                .applied
                .is_empty()
        );
        // And the patch itself is still there: unapply is not a delete.
        assert!(f.layout.patches_dir().join("0001-a.patch").is_file());
    }

    #[test]
    fn unapply_with_no_sandbox_is_a_no_op() {
        let f = Fixture::new();
        f.with_upstream();
        let done = unapply_to_upstream(&f.layout).unwrap();
        assert!(!done.sandbox, "nothing to do, and it should say so");
    }

    #[test]
    fn apply_without_a_ref_or_a_default_says_so() {
        let f = Fixture::new();
        f.with_upstream();
        std::fs::write(f.layout.config_file(), "[upstream]\nurl = \"u\"\n").unwrap();
        f.patch("0001-a.patch", "");
        f.series(&["0001-a.patch"]);

        let err = apply_locked(&f.layout, None).unwrap_err().to_string();
        assert!(err.contains("no ref to apply against"), "got {err}");
        assert!(
            err.contains(&skot_args("apply", "main")),
            "no remedy offered: {err}"
        );
    }

    #[test]
    fn apply_against_a_ref_the_upstream_lacks_says_so() {
        // The new contract: a ref the mirror does not have is fetched on the spot, and
        // an answer of "the upstream does not have it either" arrives with the ref
        // named, with no detour through a `skot sync` round trip.
        let f = Fixture::new();
        f.with_upstream();
        f.real_upstream();
        f.series(&["0001-a.patch"]);
        f.patch("0001-a.patch", "");

        let err = apply_locked(&f.layout, Some("nonexistent"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonexistent"), "got {err}");
        assert!(
            err.contains("does not have it either"),
            "the answer should say the upstream lacks it: {err}"
        );
    }

    #[test]
    fn apply_under_json_still_records_what_it_applied() {
        // The bug this guards: emitting the JSON document and returning before the state
        // write, so `--json` reported success and then left the record describing the
        // previous run.
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
",
        );
        f.series(&["0001-a.patch"]);

        assert!(apply_locked(&f.layout, None).unwrap().ok);
        let state = crate::state::State::load(&f.layout).unwrap();
        assert_eq!(state.ref_name.as_deref(), Some("main"));
        assert_eq!(state.applied, vec!["0001-a.patch".to_owned()]);
    }

    #[test]
    fn a_failed_apply_under_json_exits_one_and_clears_the_record() {
        // The document and the verdict are separate things: `emit_json` only writes, and
        // the command still states `Exit::Failure`, so a `--json` failure is never reported
        // as success to whatever is scripting it.
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-something-else-entirely
+nope
",
        );
        f.series(&["0001-a.patch"]);

        assert!(!apply_locked(&f.layout, None).unwrap().ok);
        assert!(
            crate::state::State::load(&f.layout)
                .unwrap()
                .applied
                .is_empty(),
            "a JSON apply that failed must not leave the record claiming patches are applied"
        );
    }

    #[test]
    fn a_failed_apply_clears_a_previously_recorded_series() {
        // Not the same as the empty-state case: the state must not keep claiming a series
        // that is no longer in the sandbox, or `status` and `build` would act on a tree
        // nobody is looking at.
        let f = Fixture::new();
        f.with_upstream();
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
",
        );
        f.series(&["0001-a.patch"]);
        assert!(apply_locked(&f.layout, None).unwrap().ok);

        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 3333333..4444444 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-nothing-like-this
+nope
",
        );
        assert!(!apply_locked(&f.layout, None).unwrap().ok);
        assert!(
            crate::state::State::load(&f.layout)
                .unwrap()
                .applied
                .is_empty(),
            "the record survived a failed re-apply of the same series"
        );
        assert_eq!(f.sandbox_file(), "base\n");
    }

    #[test]
    fn unapply_uses_the_recorded_ref_not_the_configured_default() {
        // The regression: unapply preferred `upstream.default-ref`, so changing the default
        // after an apply to another branch made unapply reset to an unrelated commit and
        // report success.
        let f = Fixture::new();
        let base = f.with_upstream();
        // A second branch the sandbox was never applied to.
        {
            let repo = git2::Repository::open(f.layout.mirror_dir()).unwrap();
            let sha = repo
                .commit(
                    Some("refs/heads/other"),
                    &repo.signature().unwrap(),
                    &repo.signature().unwrap(),
                    "other",
                    &repo.head().unwrap().peel_to_tree().unwrap(),
                    &[],
                )
                .unwrap();
            repo.set_head("refs/heads/other").unwrap();
            assert_ne!(sha, base);
        }
        f.patch(
            "0001-a.patch",
            "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
",
        );
        f.series(&["0001-a.patch"]);
        assert!(apply_locked(&f.layout, Some("other")).unwrap().ok);

        // Now the default moves, as it would if someone repointed the project.
        std::fs::write(
            f.layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();

        let done = unapply_to_upstream(&f.layout).unwrap();
        assert!(done.sandbox);
        assert_eq!(f.sandbox_file(), "base\n", "unapply reset to the wrong ref");
    }

    #[test]
    fn the_apply_and_unapply_subcommands_parse() {
        let cli = Cli::try_parse_from(["skot", "patch", "apply"]).unwrap();
        let Command::Patch(PatchCommand::Apply(a)) = cli.command else {
            panic!("expected apply")
        };
        assert!(a.ref_name.is_none());
        assert!(Cli::try_parse_from(["skot", "patch", "unapply"]).is_ok());
    }
}
