//! `build`: the whole run, end to end.
//!
//! Three phases in a fixed order: sync, apply, build. The first two are skot's own
//! libgit2 work; the third is entirely your hooks, in three slots: `pre-build` to
//! prepare (install dependencies, configure), `build` to produce the artifact, and
//! `post-build` to finish with it (copy it into place, smoke test it). skot knows
//! nothing about any of that; it is why a `skot.toml` for a Rust crate and one for a
//! Node monorepo differ only in the hook commands.
//!
//! The apply phase replays the applied set: what `push` and `pop` recorded, so a patch
//! popped out for testing stays out through the build. A sandbox that has never held the
//! series gets the whole series instead, so `import` followed by `build` applies
//! everything on a first run.
//!
//! Three decisions shape this file:
//!
//! - **The sandbox outlives a failure.** A dependency cache is the expensive thing to rebuild,
//!   and a run that fails in `build` should not throw it away. Cleanup happens on success
//!   (for `ephemeral`) or never (for `persistent`), so a CI policy still cannot leak a
//!   directory on the failure path.
//! - **State is checkpointed per phase**, so a run interrupted by Ctrl-C reports which phase
//!   was in flight rather than "something went wrong".
//! - **`--from` and `--only` select phases, not commands.** Starting at `build` does not re-sync,
//!   and that is what makes iterating on a build hook fast.

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use serde_json::json;

use crate::cli::BuildArgs;
use crate::commands::apply;
use crate::commands::authoring;
use crate::commands::{Ctx, Exit, emit_json};
use crate::config::{Config, SandboxPolicy};
use crate::git::sandbox;
use crate::hooks::{self, Env};
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::phase::{HookPoint, Phase, Selection};
use crate::report;
use crate::state::State;

/// Run the lifecycle.
pub fn run(ctx: &Ctx, args: &BuildArgs) -> Result<Exit> {
    let (config, series) = ctx.project()?;
    // Resolved up front so a missing patch file is reported before the network is touched,
    // rather than after a sync. `apply` re-reads them; that is cheap next to a fetch.
    series.require_resolvable(&ctx.layout)?;
    let ref_name = the_ref(&ctx.layout, &config, args.ref_name.as_deref())?;

    let selection = Selection {
        from: args.from,
        only: args.only,
    };
    let phases = selection.resolve();
    info!(target: "build", "building {ref_name}");
    debug!(
        target: "build",
        "phases: {}",
        phases.iter().map(|p| p.name()).collect::<Vec<_>>().join(" → ")
    );

    let lock = ctx.lock()?;
    debug!(target: "build", "lock acquired; held until the run ends");
    let mut state = State::load(&ctx.layout)?;
    state.begin(&ctx.layout, "build")?;
    debug!(target: "build", "state: run in flight (command `build`)");

    let ephemeral = config.sandbox.policy == SandboxPolicy::Ephemeral;
    let result = drive(&ctx.layout, &config, &ref_name, &phases, ctx.cli.json);

    // The result, not the exit path, decides cleanup. A failed run keeps its sandbox.
    if result.is_ok() && ephemeral {
        // Only on success: an ephemeral sandbox that survives a failed CI run is a leak on
        // the runner, and one that a developer is trying to debug is a feature. Success is the
        // case where neither argument applies.
        debug!(target: "build", "removing the ephemeral sandbox; the run succeeded");
        sandbox::remove(&ctx.layout)?;
    }
    // Reload rather than reusing the copy `begin` took: the phases wrote their own state
    // (`apply` records the ref and the applied stack), and finishing from a snapshot taken
    // before they ran would write those records back the way they were — empty.
    let mut state = State::load(&ctx.layout)?;
    state.finish(&ctx.layout)?;
    debug!(target: "build", "state: run finished; nothing in flight");
    drop(lock);

    // A failure still owes the caller a document. `check`, `apply` and `status` all report
    // their bad news in JSON when `--json` is set, and a caller that gets an empty stdout and
    // an exit code has to scrape stderr to find out what went wrong — which is exactly what
    // asking for `--json` is supposed to avoid.
    let ran = match result {
        Ok(ran) => ran,
        Err(e) if ctx.cli.json => {
            // `selected_phases`, not `phases`: the success document's `phases` lists what
            // actually ran, and on the failure path we cannot know that without threading the
            // partial progress out of `drive`. The error itself names the phase that stopped it.
            emit_json(&json!({
                "ok": false,
                "series_name": ctx.layout.series(),
                "ref": ref_name,
                "selected_phases": phases.iter().map(|p| p.name()).collect::<Vec<_>>(),
                "error": format!("{e:#}"),
            }))?;
            // The document is the report, so the verdict is an exit code rather than
            // another error: `main` would otherwise append a second document.
            debug!(target: "build", "failed: {e:#}");
            return Ok(Exit::Failure);
        }
        Err(e) => return Err(e),
    };

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "series_name": ctx.layout.series(),
            "ref": ref_name,
            "phases": ran,
            "sandbox": ctx.layout.sandbox_dir().display().to_string(),
        }))?;
    } else {
        info!(
            target: "build",
            "complete; the sandbox at {} is ready.",
            ctx.layout.sandbox_dir().display()
        );
        // The name and the ref together, since a build is the answer to "what did I
        // just build": the series, and the upstream it was built on.
        report::section("build");
        report::field("series", &format!("{} @ {}", ctx.layout.series(), ref_name));
    }
    Ok(Exit::Success)
}

/// The ref to build: the argument, then the configured default, then the mirror's own
/// HEAD, with build's advice.
///
/// `layout` supplies the config file's path for the message and the mirror for that last
/// resort; the rest is scalars, so this is testable without a command line.
fn the_ref(layout: &Layout, config: &Config, ref_name: Option<&str>) -> Result<String> {
    if let Some(name) = ref_name {
        debug!(target: "build", "ref from the argument: {name}");
        return Ok(name.to_owned());
    }
    if let Some(base) = layout.base_ref() {
        debug!(target: "build", "ref from the series base: {base}");
        return Ok(base);
    }
    debug!(
        target: "build",
        "no ref argument and no base; consulting `upstream.default-ref`"
    );
    config
        .upstream
        .default_ref
        .clone()
        .or_else(|| super::mirror_head_ref(layout, "build"))
        .context(format!(
            "no ref to build.\n\
                 Pass one (`{}`), record a base for this series, or set `upstream.default-ref` in {}.",
            skot_args("build", "v1.2.0"),
            layout.config_file().display()
        ))
}

/// Run the selected phases in order, and return the names of the ones that ran.
///
/// No exit code and no output: a phase either runs or the whole run errors, and `run` states
/// the verdict and writes any document. `json` is a hook narration switch, not a command line.
fn drive(
    layout: &Layout,
    config: &Config,
    ref_name: &str,
    phases: &[Phase],
    json: bool,
) -> Result<Vec<&'static str>> {
    let mut ran: Vec<&'static str> = Vec::new();

    // Whether this run has already paid the fetch-if-missing choke point: it has when the
    // sync phase is in the selection, and apply can then resolve against the mirror
    // directly instead of ensuring the same ref all over again.
    let synced = phases.contains(&Phase::Sync);

    for phase in phases {
        let mut state = State::load(layout)?;
        state.checkpoint(layout, *phase)?;
        debug!(target: "build", "checkpoint: {phase} is the phase in flight");

        match phase {
            Phase::Sync => {
                sync_phase(layout, config, ref_name, json)?;
            }
            Phase::Apply => {
                apply_phase(layout, config, ref_name, json, synced)?;
            }
            Phase::Build => {
                hook_phase(layout, config, ref_name, *phase, json)?;
            }
        }
        ran.push(phase.name());
    }

    Ok(ran)
}

/// `sync`, with the sync hooks around it.
fn sync_phase(layout: &Layout, config: &Config, ref_name: &str, json: bool) -> Result<()> {
    // Stated before the work: `ensure` can be a network fetch taking minutes, and this is
    // the line that says what the silence (or the spinner) is for.
    info!(target: "sync", "making sure {ref_name} is in the mirror");
    // `pre_sync` runs before there is a sandbox on a first run, so its working directory
    // is the project root (`HookPoint::runs_in_project_root`). Hooks that want the tree
    // wait for `post_apply`.
    let env = hook_env(layout, ref_name, "")?;
    hooks::run_point(HookPoint::PreSync, &env, config, json)?;

    // The work, not a sub-command: `build` owns this run's narration and its JSON, and
    // `ensure` is the piece that does both: the logs flow up, no document reaches stdout,
    // and an error propagates as an error rather than as an exit code nobody can produce.
    // Local-first: a mirror that already has the ref means no network call at all, so a
    // warm build runs even when the network would not.
    crate::commands::sync::ensure(layout, Some(ref_name))?;

    let env = hook_env(layout, ref_name, &current_sha(layout))?;
    hooks::run_point(HookPoint::PostSync, &env, config, json)?;
    Ok(())
}

/// `apply`, with the apply hooks around it.
///
/// What lands in the sandbox depends on where it already is: a fresh one (never landed)
/// gets the whole series, so `import` followed by `build` just works, and one holding a
/// pushed set gets that set back, so a patch popped out for testing stays out.
fn apply_phase(
    layout: &Layout,
    config: &Config,
    ref_name: &str,
    json: bool,
    synced: bool,
) -> Result<()> {
    let sha = current_sha(layout);
    // The sha comes from state, which a cold run has not filled in yet: on a first run
    // there is nothing to say but the ref itself.
    let against = if sha.is_empty() {
        ref_name.to_owned()
    } else {
        format!("{ref_name} ({sha})")
    };
    let (_, series) = layout.project()?;
    let total = series.entries_lenient(layout).len();
    let state = State::load(layout)?;
    let stack = if state.ref_name(layout).is_none() {
        None
    } else {
        Some(authoring::recorded_stack(layout, &series, &state)?)
    };
    let pushed = stack.as_ref().map_or(total, |s| s.entries.len());
    let enabled = series.names().len();
    // Two words, one meaning each: *enabled* is the series listing a patch (`enable`/
    // `disable`), *applied* is the sandbox holding it (`apply`/`pop`). The phase replays
    // the applied set, so a patch enabled but never applied is skipped, and the line
    // that says so names the command that stamps it in.
    if total == 0 {
        info!(
            target: "apply",
            "the series is empty; the sandbox holds {against} only"
        );
    } else if pushed == 0 {
        if enabled == 0 {
            info!(
                target: "apply",
                "no patches are enabled; the sandbox holds {against} alone"
            );
        } else {
            info!(
                target: "apply",
                "nothing applied; the sandbox holds {against} alone"
            );
        }
    } else if pushed == total {
        info!(
            target: "apply",
            "applying the whole series ({}) against {against}",
            apply::count(total, "patch", "patches")
        );
    } else {
        info!(
            target: "apply",
            "applying {pushed} of the {} against {against}",
            apply::count(total, "patch", "patches")
        );
    }
    if let Some(skipped) = enabled.checked_sub(pushed).filter(|n| *n > 0) {
        warn!(
            target: "apply",
            "{} enabled but not applied; run `{}` to stamp them into the sandbox",
            apply::count(skipped, "patch", "patches"),
            skot("apply")
        );
    }
    // `pre_apply` runs before `apply` has created the sandbox on a first run, so its
    // working directory is the project root (`HookPoint::runs_in_project_root`). A hook
    // that needs the patched tree belongs at `post_apply` or later.
    hooks::run_point(
        HookPoint::PreApply,
        &hook_env(layout, ref_name, &sha)?,
        config,
        json,
    )?;

    match stack {
        None => {
            // `apply_locked`, not `apply`: the run already holds the lock for the whole
            // build, and `apply` would try to take it again and block against this
            // process. When the sync phase ran in this run the ensured variant applies,
            // which is the same replay without a second pass through the choke point.
            let applied = if synced {
                apply::apply_locked_ensured(layout, Some(ref_name))?
            } else {
                apply::apply_locked(layout, Some(ref_name))?
            };
            if !applied.ok {
                // `applied.failure` rather than a generic "the series does not apply":
                // this is the point where the user learns which of their patches is the
                // problem, and it is the last place that information exists before the
                // run stops.
                bail!(
                    "the series does not apply to {ref_name} ({sha}); the run stopped.\n\
                     {failure}",
                    failure = applied.failure
                );
            }
        }
        Some(stack) => {
            // The same `land` push and pop use, so build cannot disagree with them about
            // what tree the recorded stack means.
            let (_mirror, base) = if synced {
                crate::commands::sync::resolve_local(layout, ref_name)?
            } else {
                crate::commands::sync::resolve_in_mirror(layout, ref_name)?
            };
            if let Err(e) = authoring::land(layout, ref_name, &base, &stack) {
                bail!(
                    "the recorded set does not apply to {ref_name} ({sha}); \
                     the run stopped.\n{e}"
                );
            }
        }
    }

    let env = hook_env(layout, ref_name, &sha)?;
    hooks::run_point(HookPoint::PostApply, &env, config, json)?;
    Ok(())
}

/// A phase whose entire work is running hooks.
fn hook_phase(
    layout: &Layout,
    config: &Config,
    ref_name: &str,
    phase: Phase,
    json: bool,
) -> Result<()> {
    let sha = current_sha(layout);
    let env = hook_env(layout, ref_name, &sha)?;
    hooks::ensure_sandbox(&env.sandbox)?;

    // Stated with the exact points about to run, so a failure below names its stage. A
    // phase with nothing configured logs nothing: nothing is running and nothing went
    // wrong, and `run_point` already says so per point at debug.
    let points = unique_points(phase);
    let configured: Vec<String> = points
        .iter()
        .filter(|point| config.hook(**point).is_some())
        .map(|point| point.to_string())
        .collect();
    if !configured.is_empty() {
        info!(target: "build", "running {}", configured.join(" → "));
    }

    for point in points {
        hooks::run_point(point, &env, config, json)?;
    }
    Ok(())
}

/// A phase's hook points, each once, in order.
///
/// `hooks()` has no duplicates, so this is a defensive pass: the one place that runs
/// hooks for a phase must never run a configured command twice, whatever shape a
/// future phase's point list takes.
pub(crate) fn unique_points(phase: Phase) -> Vec<HookPoint> {
    let mut out: Vec<HookPoint> = Vec::with_capacity(3);
    for &point in phase.hooks() {
        if !out.contains(&point) {
            out.push(point);
        }
    }
    out
}

/// The commit the last successful sync or apply recorded, for the hook environment.
///
/// Read back from state rather than resolved again, so every hook in a run is told the same
/// commit. Resolving per phase would let a hook see a different tree from the one the run
/// actually applied.
fn current_sha(layout: &Layout) -> String {
    State::load(layout)
        .ok()
        .and_then(|s| s.sha(layout).map(str::to_owned))
        .unwrap_or_default()
}

/// The hook environment for a lifecycle hook: the project, the ref, the commit.
///
/// Which directory a relative hook `cwd` resolves against is not this function's
/// business; that rule lives on the point itself, in `HookPoint::runs_in_project_root`.
fn hook_env(layout: &Layout, ref_name: &str, sha: &str) -> Result<Env> {
    Ok(Env {
        project_root: layout.root().to_path_buf(),
        sandbox: layout.sandbox_dir(),
        ref_name: ref_name.to_owned(),
        sha: sha.to_owned(),
        version: Env::version_from_ref(ref_name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;
    use clap::Parser as _;

    #[test]
    fn a_build_needs_a_ref() {
        let (_tmp, layout) = testsupport::project();
        let (config, _) = layout.project().unwrap();

        let err = the_ref(&layout, &config, None).unwrap_err().to_string();
        assert!(err.contains("no ref to build"), "got {err}");
        assert!(
            err.contains(&skot_args("build", "v1.2.0")),
            "no example offered: {err}"
        );
    }

    #[test]
    fn a_build_says_where_to_put_the_ref_when_the_default_is_unset() {
        let (_tmp, layout) = testsupport::project();
        let (config, _) = layout.project().unwrap();
        let err = the_ref(&layout, &config, None).unwrap_err().to_string();
        assert!(err.contains("upstream.default-ref"), "got {err}");
    }

    #[test]
    fn the_ref_falls_back_to_the_mirrors_own_head() {
        // No argument, no series base, no `upstream.default-ref`: the branch the mirror
        // itself points at is the base, the same one `import` takes for a range that
        // left it to be worked out.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        let (config, _) = layout.project().unwrap();

        assert_eq!(the_ref(&layout, &config, None).unwrap(), "main");
    }

    #[test]
    fn version_parsing_comes_from_the_ref_the_user_passed() {
        // A hook reads SKOT_VERSION; it must be derived from the ref, never guessed.
        assert_eq!(Env::version_from_ref("v1.2.0"), "1.2.0");
        assert_eq!(Env::version_from_ref("1.2.0"), "1.2.0");
        assert_eq!(Env::version_from_ref("main"), "");
        assert_eq!(Env::version_from_ref("release-1.2"), "");
    }

    #[test]
    fn a_missing_sandbox_stops_a_hook_phase_with_a_message_that_says_why() {
        let (_tmp, layout) = testsupport::project();
        let config = Config::default();
        let err = hook_phase(&layout, &config, "main", Phase::Build, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no sandbox"), "got {err}");
    }

    #[test]
    fn a_cold_sync_runs_its_hooks_in_the_project_root() {
        // On a first run the sandbox does not exist yet when `pre_sync` runs, so its
        // working directory has to be the project root: spawning `sh` in the missing
        // sandbox is what a cold build used to die on.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n\n\
             [hooks]\npre-sync = \"pwd > where.txt\"\n",
        )
        .unwrap();
        let (config, _) = layout.project().unwrap();
        assert!(!layout.sandbox_dir().exists());

        let ran = drive(&layout, &config, "main", &[Phase::Sync], false).unwrap();
        assert_eq!(ran, vec!["sync"]);
        assert_eq!(
            std::fs::read_to_string(layout.root().join("where.txt"))
                .unwrap()
                .trim(),
            layout.root().display().to_string()
        );
        assert!(
            !layout.sandbox_dir().exists(),
            "sync does not build a sandbox"
        );
    }

    #[test]
    fn an_empty_series_still_builds() {
        // A fresh project, empty series and all: build runs the whole lifecycle with
        // nothing to replay instead of refusing at the door.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        testsupport::write_series(&layout, &[]);
        let (config, _) = layout.project().unwrap();

        let ran = drive(&layout, &config, "main", &Phase::ORDER, false).unwrap();

        assert_eq!(ran, vec!["sync", "apply", "build"]);
        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "base\n"
        );
    }

    #[test]
    fn the_build_phase_runs_its_three_points_in_order() {
        assert_eq!(
            Phase::Build.hooks(),
            &[HookPoint::PreBuild, HookPoint::Build, HookPoint::PostBuild]
        );
        assert_eq!(
            unique_points(Phase::Build),
            vec![HookPoint::PreBuild, HookPoint::Build, HookPoint::PostBuild]
        );
        // The other phases are pairs around work skot does itself.
        assert_eq!(
            unique_points(Phase::Sync),
            vec![HookPoint::PreSync, HookPoint::PostSync]
        );
        assert_eq!(
            unique_points(Phase::Apply),
            vec![HookPoint::PreApply, HookPoint::PostApply]
        );
    }

    #[test]
    fn every_hook_point_is_run_exactly_once_by_some_phase() {
        // If deduping ever dropped a point rather than repeating one, a configured hook would
        // silently never run. This is the check that catches that.
        let mut all = std::collections::BTreeSet::new();
        for phase in Phase::ORDER {
            for point in unique_points(phase) {
                all.insert(point);
            }
        }
        let expected: std::collections::BTreeSet<_> = HookPoint::ALL.into_iter().collect();
        assert_eq!(all, expected);
    }

    /// A project with a mirror, a sandbox, and a two-patch series, but no hooks.
    fn buildable(root: &std::path::Path) -> crate::layout::Layout {
        let (layout, _sha) = crate::commands::testsupport::project_with_sandbox(root);
        crate::commands::testsupport::write_series(
            &layout,
            &[("a.patch", crate::commands::testsupport::patch_one())],
        );
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        layout
    }

    #[test]
    fn build_applies_the_series() {
        // `build` holds the lock for the whole run. If it handed `apply` the same lock again,
        // `flock` would block against this very process until the wait expired, and the run
        // would fail with "the sandbox is locked" — by its own hand.
        let tmp = tempfile::tempdir().unwrap();
        let layout = buildable(tmp.path());
        let (config, _) = layout.project().unwrap();

        let ran = drive(&layout, &config, "main", &[Phase::Apply], false).unwrap();
        assert_eq!(ran, vec!["apply"]);
        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "first\n"
        );
    }

    /// A project with a mirror, a two-patch series, a sandbox, and a recorded stack.
    fn stacked(root: &std::path::Path, pushed: &[&str]) -> crate::layout::Layout {
        let (layout, sha) = crate::commands::testsupport::project_with_sandbox(root);
        crate::commands::testsupport::write_series(
            &layout,
            &[
                ("a.patch", crate::commands::testsupport::patch_one()),
                ("b.patch", crate::commands::testsupport::patch_two()),
            ],
        );
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        let mut state = State::load(&layout).unwrap();
        state.set_ref(&layout, "main", &sha).unwrap();
        state
            .set_applied(&layout, pushed.iter().map(|n| (*n).to_owned()).collect())
            .unwrap();
        layout
    }

    #[test]
    fn build_respects_a_popped_stack() {
        // push/pop and build must agree: a patch popped out for testing must stay out
        // through a build, or the stack is useless for building without one.
        let tmp = tempfile::tempdir().unwrap();
        let layout = stacked(tmp.path(), &["a.patch"]);
        let (config, _) = layout.project().unwrap();

        let ran = drive(&layout, &config, "main", &[Phase::Apply], false).unwrap();

        assert_eq!(ran, vec!["apply"]);
        let state = State::load(&layout).unwrap();
        assert_eq!(
            state.applied(&layout).to_vec(),
            ["a.patch"],
            "the popped patch must stay popped"
        );
        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "first\n",
            "only the pushed patch's tree, not the popped one's"
        );
    }

    #[test]
    fn build_with_nothing_pushed_builds_the_bare_ref() {
        // Popped all the way out: build must build upstream alone, not re-apply the series.
        let tmp = tempfile::tempdir().unwrap();
        let layout = stacked(tmp.path(), &[]);
        let (config, _) = layout.project().unwrap();

        drive(&layout, &config, "main", &[Phase::Apply], false).unwrap();

        let state = State::load(&layout).unwrap();
        assert!(
            state.applied(&layout).is_empty(),
            "nothing pushed, nothing applied: {:?}",
            state.applied(&layout)
        );
        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "base\n",
            "the bare upstream tree"
        );
    }

    #[test]
    fn build_lands_a_recorded_set_that_has_a_hole_in_it() {
        // push/pop can leave a hole, and build must land exactly that set: the disabled
        // patch's work stays out, the ones around it stay on.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = crate::commands::testsupport::project_with_sandbox(tmp.path());
        crate::commands::testsupport::write_series(
            &layout,
            &[
                ("a.patch", crate::commands::testsupport::patch_one()),
                ("b.patch", crate::commands::testsupport::patch_two()),
                ("c.patch", crate::commands::testsupport::patch_three()),
            ],
        );
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        let mut state = State::load(&layout).unwrap();
        state.set_ref(&layout, "main", &sha).unwrap();
        state
            .set_applied(&layout, ["a.patch", "c.patch"].map(String::from).to_vec())
            .unwrap();
        let (config, _) = layout.project().unwrap();

        drive(&layout, &config, "main", &[Phase::Apply], false).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "third\n",
            "the tree must be the recorded set's, not the series' prefix"
        );
        let state = State::load(&layout).unwrap();
        assert_eq!(
            state.applied(&layout).to_vec(),
            ["a.patch", "c.patch"],
            "the hole must survive the build"
        );
    }

    #[test]
    fn the_sync_phase_never_touches_a_url_it_does_not_need() {
        // `url = "unused"` is not a URL: any attempt to contact it fails loudly. The
        // sync phase's job on a warm mirror is exactly not to try: `main` is in the
        // mirror, so the phase has nothing to fetch and no network to fetch it from.
        let tmp = tempfile::tempdir().unwrap();
        let layout = buildable(tmp.path());
        let (config, _) = layout.project().unwrap();

        let ran = drive(&layout, &config, "main", &[Phase::Sync], false).unwrap();

        assert_eq!(ran, vec!["sync"]);
    }

    #[test]
    fn build_is_usable_more_than_once() {
        // A lock leaked by the first run would make the second one wait and fail.
        let tmp = tempfile::tempdir().unwrap();
        let layout = buildable(tmp.path());
        let (config, _) = layout.project().unwrap();
        drive(&layout, &config, "main", &[Phase::Apply], false).unwrap();
        drive(&layout, &config, "main", &[Phase::Apply], false).unwrap();
    }

    #[test]
    fn a_finished_run_keeps_what_the_phases_recorded() {
        // `run` marks the run in flight, runs the phases, and then marks it finished. If
        // it finishes from the snapshot taken before the phases, `apply`'s records are
        // written back as they were — empty — and the sandbox holds the series while the
        // state says nothing is applied, which every later command believes.
        let tmp = tempfile::tempdir().unwrap();
        let layout = buildable(tmp.path());
        let cli = crate::cli::Cli::try_parse_from(["skot", "build", "--only", "apply"]).unwrap();
        let crate::cli::Command::Build(args) = cli.command.clone() else {
            panic!("expected build")
        };
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        run(&ctx, &args).unwrap();

        let state = State::load(&ctx.layout).unwrap();
        assert_eq!(state.ref_name(&ctx.layout), Some("main"));
        assert_eq!(state.applied(&ctx.layout), ["a.patch"]);
        assert!(state.sha(&ctx.layout).is_some_and(|s| s.len() == 40));
        assert!(
            state.in_flight.is_none() && state.in_flight_command.is_none(),
            "the run finished, so nothing is in flight"
        );
    }

    #[test]
    fn the_build_command_parses_its_phase_selection() {
        let cli = crate::cli::Cli::try_parse_from(["skot", "build", "v1.2.0"]).unwrap();
        let crate::cli::Command::Build(a) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(a.ref_name.as_deref(), Some("v1.2.0"));
        assert!(a.from.is_none() && a.only.is_none());

        let cli = crate::cli::Cli::try_parse_from(["skot", "build", "--from", "build"]).unwrap();
        let crate::cli::Command::Build(a) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(a.from, Some(Phase::Build));

        // --from and --only are the same axis, so asking for both is a contradiction rather
        // than a silent precedence rule.
        assert!(
            crate::cli::Cli::try_parse_from(["skot", "build", "--from", "sync", "--only", "apply"])
                .is_err()
        );
    }
}
