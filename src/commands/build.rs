//! `build`: the whole run, end to end.
//!
//! Five phases in a fixed order — sync, apply, deps, build, verify — each with its own hooks.
//! skot does the first two with libgit2 and knows nothing about the last three; your hooks
//! are the build system. That is the whole contract, and it is why a `skot.toml` for a Rust
//! crate and one for a Node monorepo differ only in the hook commands.
//!
//! Three decisions shape this file:
//!
//! - **The sandbox outlives a failure.** A dependency cache is the expensive thing to rebuild,
//!   and a run that fails at `verify` should not throw it away. Cleanup happens on success
//!   (for `ephemeral`) or never (for `persistent`), so a CI policy still cannot leak a
//!   directory on the failure path.
//! - **State is checkpointed per phase**, so a run interrupted by Ctrl-C reports which phase
//!   was in flight rather than "something went wrong".
//! - **`--from` and `--only` select phases, not commands.** Starting at `build` does not re-sync,
//!   and that is what makes iterating on a build hook fast.

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::BuildArgs;
use crate::commands::apply;
use crate::commands::{Ctx, Exit, emit_json};
use crate::config::{Config, SandboxPolicy};
use crate::git::sandbox;
use crate::hooks::{self, Env, Outcome};
use crate::layout::Layout;
use crate::phase::{HookPoint, Phase, Selection};
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

    let lock = ctx.lock()?;
    let mut state = State::load(&ctx.layout)?;
    state.begin(&ctx.layout, "build")?;

    let ephemeral = config.sandbox.policy == SandboxPolicy::Ephemeral;
    let result = drive(
        &ctx.layout,
        &config,
        &ref_name,
        &phases,
        ctx.cli.json,
        ctx.cli.verbose,
    );

    // The result, not the exit path, decides cleanup. A failed run keeps its sandbox.
    if result.is_ok() && ephemeral {
        // Only on success: an ephemeral sandbox that survives a failed CI run is a leak on
        // the runner, and one that a developer is trying to debug is a feature. Success is the
        // case where neither argument applies.
        sandbox::remove(&ctx.layout)?;
    }
    // Reload rather than reusing the copy `begin` took: the phases wrote their own state
    // (`apply` records the ref and the applied stack), and finishing from a snapshot taken
    // before they ran would write those records back the way they were — empty.
    let mut state = State::load(&ctx.layout)?;
    state.finish(&ctx.layout)?;
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
                "ref": ref_name,
                "selected_phases": phases.iter().map(|p| p.name()).collect::<Vec<_>>(),
                "error": format!("{e:#}"),
            }))?;
            return Err(e);
        }
        Err(e) => return Err(e),
    };

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "ref": ref_name,
            "phases": ran,
            "sandbox": ctx.layout.sandbox_dir().display().to_string(),
        }))?;
    } else {
        info!(
            "{} — the sandbox at {} is ready.",
            ran.join(" → "),
            ctx.layout.sandbox_dir().display()
        );
    }
    Ok(Exit::Success)
}

/// The ref to build: the argument, then the configured default, with build's advice.
///
/// `layout` only supplies the config file's path for the message; the parameters are all
/// scalars, so this is testable without a command line.
fn the_ref(layout: &Layout, config: &Config, ref_name: Option<&str>) -> Result<String> {
    ref_name
        .map(str::to_owned)
        .or_else(|| config.upstream.default_ref.clone())
        .context(format!(
            "no ref to build.\n\
             Pass one (`skot build v1.2.0`) or set `upstream.default-ref` in {}.",
            layout.config_file().display()
        ))
}

/// Run the selected phases in order, and return the names of the ones that ran.
///
/// No exit code and no output: a phase either runs or the whole run errors, and `run` states
/// the verdict and writes any document. `json` and `verbose` are hook narration switches, not
/// a command line.
fn drive(
    layout: &Layout,
    config: &Config,
    ref_name: &str,
    phases: &[Phase],
    json: bool,
    verbose: u8,
) -> Result<Vec<&'static str>> {
    let mut ran: Vec<&'static str> = Vec::new();

    for phase in phases {
        let mut state = State::load(layout)?;
        state.checkpoint(layout, *phase)?;

        match phase {
            Phase::Sync => {
                sync_phase(layout, config, ref_name, json)?;
            }
            Phase::Apply => {
                apply_phase(layout, config, ref_name, json)?;
            }
            Phase::Deps | Phase::Build | Phase::Verify => {
                hook_phase(layout, config, ref_name, *phase, json, verbose)?;
            }
        }
        ran.push(phase.name());
    }

    Ok(ran)
}

/// `sync`, with the sync hooks around it.
fn sync_phase(layout: &Layout, config: &Config, ref_name: &str, json: bool) -> Result<()> {
    // `pre_sync` runs before there is a sandbox on a first run, so it runs in the project
    // root. Hooks that want the tree wait for `deps`.
    let env = root_env(layout, ref_name, "")?;
    hooks::run_point(HookPoint::PreSync, &env, config, json)?;

    // The work, not a sub-command: `build` owns this run's narration and its JSON, and
    // `fetch` is the piece that does both sync and any caller share — the logs flow up,
    // no document reaches stdout, and an error propagates as an error rather than as an
    // exit code nobody can produce.
    crate::commands::sync::fetch(layout, Some(ref_name))?;

    let env = root_env(layout, ref_name, &current_sha(layout))?;
    hooks::run_point(HookPoint::PostSync, &env, config, json)?;
    Ok(())
}

/// `apply`, with the apply hooks around it.
fn apply_phase(layout: &Layout, config: &Config, ref_name: &str, json: bool) -> Result<()> {
    let sha = current_sha(layout);
    // `pre_apply` runs before `apply` has created the sandbox on a first run, so it runs in
    // the project root. A hook that needs the patched tree belongs in `deps`.
    hooks::run_point(
        HookPoint::PreApply,
        &root_env(layout, ref_name, &sha)?,
        config,
        json,
    )?;

    // `apply_locked`, not `apply`: the run already holds the lock for the whole build, and
    // `apply` would try to take it again and block against this process.
    let applied = apply::apply_locked(layout, Some(ref_name))?;
    if !applied.ok {
        // `applied.failure` rather than a generic "the series does not apply": this is the
        // point where the user learns which of their patches is the problem, and it is the
        // last place that information exists before the run stops.
        bail!(
            "the series does not apply to {ref_name} ({sha}); the run stopped.\n\
             {failure}",
            failure = applied.failure
        );
    }

    let env = sandbox_env(layout, ref_name, &sha)?;
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
    verbose: u8,
) -> Result<()> {
    let sha = current_sha(layout);
    let env = sandbox_env(layout, ref_name, &sha)?;
    hooks::ensure_sandbox(&env.sandbox)?;

    // `Phase::Deps` names the same hook point twice, so that its `hooks()` array has the same
    // shape as every other phase's. Deduplicating is what stops that shape from doubling every
    // dependency install; doing it generically means a future phase with the same quirk is
    // covered without touching this.
    for point in unique_points(phase) {
        match hooks::run_point(point, &env, config, json)? {
            Outcome::Ran | Outcome::Failed => {}
            Outcome::Skipped => {
                if verbose > 0 {
                    debug!("  no hook at {point}");
                }
            }
        }
    }
    Ok(())
}

/// A phase's hook points, each once, in order.
///
/// `Phase::Deps` has a single point but reports it twice, so that `hooks()` has the same
/// `[pre, post]` shape as every other phase and `--from`/`--only` reasoning never has to
/// special-case it. Taken literally that would run a dependency install twice per build, so
/// the duplicates are dropped here — the single place that runs hooks for a phase.
pub(crate) fn unique_points(phase: Phase) -> Vec<HookPoint> {
    let mut out: Vec<HookPoint> = Vec::with_capacity(2);
    for point in phase.hooks() {
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
        .and_then(|s| s.sha)
        .unwrap_or_default()
}

/// The hook environment for a hook that may run before the sandbox exists.
fn root_env(layout: &Layout, ref_name: &str, sha: &str) -> Result<Env> {
    Ok(Env {
        project_root: layout.root().to_path_buf(),
        sandbox: layout.sandbox_dir(),
        ref_name: ref_name.to_owned(),
        sha: sha.to_owned(),
        version: Env::version_from_ref(ref_name),
    })
}

/// The hook environment for a hook that needs the patched tree.
fn sandbox_env(layout: &Layout, ref_name: &str, sha: &str) -> Result<Env> {
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
            err.contains("skot build v1.2.0"),
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
        let err = hook_phase(&layout, &config, "main", Phase::Deps, false, 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no sandbox"), "got {err}");
    }

    #[test]
    fn the_deps_phase_runs_its_hook_once_not_twice() {
        // `Phase::Deps` repeats `HookPoint::Deps` so its `hooks()` array matches the other
        // phases' shape. Taking that literally would double every dependency install.
        assert_eq!(Phase::Deps.hooks(), [HookPoint::Deps, HookPoint::Deps]);
        assert_eq!(unique_points(Phase::Deps), vec![HookPoint::Deps]);
        assert_eq!(
            unique_points(Phase::Build),
            vec![HookPoint::PreBuild, HookPoint::PostBuild]
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
            &[("0001-a.patch", crate::commands::testsupport::patch_one())],
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

        let ran = drive(&layout, &config, "main", &[Phase::Apply], false, 0).unwrap();
        assert_eq!(ran, vec!["apply"]);
        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "first\n"
        );
    }

    #[test]
    fn build_is_usable_more_than_once() {
        // A lock leaked by the first run would make the second one wait and fail.
        let tmp = tempfile::tempdir().unwrap();
        let layout = buildable(tmp.path());
        let (config, _) = layout.project().unwrap();
        drive(&layout, &config, "main", &[Phase::Apply], false, 0).unwrap();
        drive(&layout, &config, "main", &[Phase::Apply], false, 0).unwrap();
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
        assert_eq!(state.ref_name.as_deref(), Some("main"));
        assert_eq!(state.applied, ["0001-a.patch"]);
        assert!(state.sha.as_deref().is_some_and(|s| s.len() == 40));
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
            crate::cli::Cli::try_parse_from(["skot", "build", "--from", "sync", "--only", "deps"])
                .is_err()
        );
    }
}
