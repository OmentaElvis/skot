//! Running user-authored hooks.
//!
//! A hook is a shell command string. skot never parses one, never rewrites one, and
//! has no idea what it does. Its entire job here is to spawn it with a known working
//! directory, a known environment, and an honest exit code.
//!
//! Two details are load-bearing rather than stylistic:
//!
//! - The working directory is set explicitly on the spawn, not by `cd`-ing in a shell.
//!   An upstream tree containing a file named `install` would otherwise shadow the
//!   `install` subcommand of whatever tool the hook calls.
//! - `SHELL` is not consulted. Hooks run under `sh`, and the config is expected to say
//!   what it needs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use log::{info, warn};

use crate::config::{Config, ExtraHook};
use crate::layout::Layout;
use crate::phase::HookPoint;

/// Values injected into every hook, so a hook never has to be told where it is.
#[derive(Debug, Clone)]
pub struct Env {
    pub project_root: PathBuf,
    pub sandbox: PathBuf,
    pub ref_name: String,
    pub sha: String,
    pub version: String,
}

impl Env {
    /// Version parsed out of a ref name, when it looks like one. Empty otherwise, and
    /// never invented: a hook that wants a version can see whether there is one.
    pub fn version_from_ref(ref_name: &str) -> String {
        let candidate = ref_name.strip_prefix('v').unwrap_or(ref_name);
        let mut parts = candidate.split('.');
        let ok = (0..3).all(|_| {
            parts
                .next()
                .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        }) && parts.next().is_none();
        if ok {
            candidate.to_owned()
        } else {
            String::new()
        }
    }

    fn to_pairs(&self) -> Vec<(String, String)> {
        vec![
            ("SKOT_DIR".into(), self.project_root.display().to_string()),
            ("SKOT_SANDBOX".into(), self.sandbox.display().to_string()),
            ("SKOT_REF".into(), self.ref_name.clone()),
            ("SKOT_SHA".into(), self.sha.clone()),
            ("SKOT_VERSION".into(), self.version.clone()),
        ]
    }
}

/// What a hook did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ran,
    Skipped,
    /// Failed, but `required = false`, so the run continued.
    Failed,
}

/// One hook, resolved and ready to run.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub point: Option<HookPoint>,
    pub name: String,
    pub cmd: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub required: bool,
}

/// Turn `SKOT_VERSION` into a live process environment.
pub fn process_env(
    env: &Env,
    passthrough: &BTreeMap<String, String>,
    extra: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut all = env.to_pairs();
    all.extend(passthrough.iter().map(|(k, v)| (k.clone(), v.clone())));
    all.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    all
}

/// Dry-run listing of everything configured, in lifecycle order.
pub fn plan(config: &Config, extra: Option<&str>) -> Vec<Resolved> {
    let mut out = Vec::new();
    if let Some(name) = extra {
        if let Some(h) = ExtraHook::find(&config.extra, name) {
            out.push(Resolved {
                point: None,
                name: h.name.clone(),
                cmd: h.cmd.clone(),
                cwd: PathBuf::from(h.cwd.clone().unwrap_or_else(|| ".".into())),
                env: h.env.clone(),
                required: h.required,
            });
        }
        return out;
    }
    for (point, cmd) in config.configured_hooks() {
        out.push(Resolved {
            point: Some(point),
            name: point.key().to_owned(),
            cmd: cmd.to_owned(),
            cwd: PathBuf::from("."),
            env: BTreeMap::new(),
            required: true,
        });
    }
    out
}

/// Run one hook. Returns what happened; the caller decides whether it is fatal.
///
/// `json` says the caller is going to write a JSON document to stdout. A hook is a build
/// command, and build commands print: `cargo`, `npm`, `make` and a plain `echo` all write to
/// stdout. Left alone, that output lands in front of the document and `skot build --json`
/// stops being parseable — which is worse than the hook being noisy, because it fails in
/// whoever's script rather than in skot. So under `--json` the hook's stdout goes to stderr,
/// where it stays visible to the person watching, and stdout is left for the one document.
pub fn run_one(hook: &Resolved, env: &Env, config: &Config, json: bool) -> Result<Outcome> {
    let cwd = if hook.cwd.is_absolute() {
        hook.cwd.clone()
    } else {
        env.sandbox.join(&hook.cwd)
    };

    info!("{} {}", hook.name, dim_cmd(&hook.cmd));

    // The parent environment is passed through, not cleared, for the same reason `make`
    // and `npm` pass theirs through: a build command needs PATH to find its compiler,
    // HOME for its caches, and the toolchain's own variables. Skot's variables are
    // then layered on top, and the config's `env` last, so a user can override anything.
    let pairs = process_env(env, &config.env, &hook.env);
    let mut command = Command::new(shell());
    command.arg("-e").arg("-c").arg(&hook.cmd).current_dir(&cwd);
    if json {
        command.stdout(std::process::Stdio::from(std::io::stderr()));
    }
    for (k, v) in pairs {
        command.env(k, v);
    }

    let status = command.status().context(format!(
        "running {} hook `{}` in {}",
        hook.name,
        hook.cmd,
        cwd.display()
    ))?;

    if status.success() {
        info!("{}", hook.name);
        Ok(Outcome::Ran)
    } else {
        let code = status
            .code()
            .map_or_else(|| "signal".to_owned(), |c| c.to_string());
        let message = format!(
            "{} failed with exit status {code} (in {})",
            hook.name,
            cwd.display()
        );
        if hook.required {
            // Not printed here. Every caller of this function propagates the error with `?`,
            // and the top-level handler already reports it, so printing as well means every
            // failed hook is explained twice.
            Err(anyhow::anyhow!("{message}"))
        } else {
            warn!("{message}, continuing because required = false");
            Ok(Outcome::Failed)
        }
    }
}

/// Run every hook at `point`, if any.
///
/// `json` is passed through to [`run_one`]; see there for why a hook's stdout has to move.
pub fn run_point(point: HookPoint, env: &Env, config: &Config, json: bool) -> Result<Outcome> {
    let Some(cmd) = config.hook(point) else {
        return Ok(Outcome::Skipped);
    };
    let hook = Resolved {
        point: Some(point),
        name: point.key().to_owned(),
        cmd: cmd.to_owned(),
        cwd: PathBuf::from("."),
        env: BTreeMap::new(),
        required: true,
    };
    run_one(&hook, env, config, json)
}

/// Run a user-named hook from `[[hook]]`.
///
/// `json` behaves as in [`run_point`].
pub fn run_named(
    name: &str,
    layout: &Layout,
    env: &Env,
    config: &Config,
    json: bool,
) -> Result<Outcome> {
    let hook = ExtraHook::find(&config.extra, name).ok_or_else(|| {
        let known: Vec<_> = config.extra.iter().map(|h| h.name.as_str()).collect();
        if known.is_empty() {
            anyhow::anyhow!(
                "no `[[hook]]` entries in {}",
                layout.config_file().display()
            )
        } else {
            anyhow::anyhow!(
                "no hook named `{name}`; defined hooks: {}",
                known.join(", ")
            )
        }
    })?;

    run_one(
        &Resolved {
            point: None,
            name: hook.name.clone(),
            cmd: hook.cmd.clone(),
            cwd: PathBuf::from(hook.cwd.clone().unwrap_or_else(|| ".".into())),
            env: hook.env.clone(),
            required: hook.required,
        },
        env,
        config,
        json,
    )
}

fn shell() -> &'static str {
    std::env::var("SKOT_SHELL")
        .unwrap_or_else(|_| "sh".to_owned())
        .leak()
}

fn dim_cmd(cmd: &str) -> String {
    // Long commands would wrap badly, so only the first line is echoed.
    let first = cmd.lines().next().unwrap_or(cmd);
    let elided = if cmd.lines().count() > 1 { " ..." } else { "" };
    format!("{first}{elided}")
}

/// Check that a sandbox directory is safe to run hooks in.
pub fn ensure_sandbox(sandbox: &Path) -> Result<()> {
    if sandbox.is_dir() {
        Ok(())
    } else {
        anyhow::bail!(
            "no sandbox at {}.\n\
             Hooks run with the sandbox as their working directory, so it has to exist first.",
            sandbox.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn env() -> Env {
        Env {
            project_root: PathBuf::from("/proj"),
            sandbox: PathBuf::from("/proj/.skot/sandbox"),
            ref_name: "v1.2.3".into(),
            sha: "abc".into(),
            version: "1.2.3".into(),
        }
    }

    #[test]
    fn version_is_only_extracted_from_something_that_is_one() {
        assert_eq!(Env::version_from_ref("v1.2.3"), "1.2.3");
        assert_eq!(Env::version_from_ref("1.2.3"), "1.2.3");
        assert_eq!(Env::version_from_ref("v1.2.3-rc1"), "");
        assert_eq!(Env::version_from_ref("main"), "");
        assert_eq!(Env::version_from_ref("dev"), "");
        assert_eq!(Env::version_from_ref("v1.2"), "");
        assert_eq!(Env::version_from_ref(""), "");
    }

    #[test]
    fn injected_variables_are_present() {
        let pairs = process_env(&env(), &BTreeMap::new(), &BTreeMap::new());
        let map: BTreeMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map["SKOT_DIR"], "/proj");
        assert_eq!(map["SKOT_SANDBOX"], "/proj/.skot/sandbox");
        assert_eq!(map["SKOT_REF"], "v1.2.3");
        assert_eq!(map["SKOT_VERSION"], "1.2.3");
    }

    #[test]
    fn per_hook_env_overrides_the_passthrough() {
        let passthrough = BTreeMap::from([("CI".to_owned(), "0".to_owned())]);
        let extra = BTreeMap::from([("CI".to_owned(), "1".to_owned())]);
        let map: BTreeMap<_, _> = process_env(&env(), &passthrough, &extra)
            .into_iter()
            .collect();
        assert_eq!(map["CI"], "1");
    }

    #[test]
    fn plan_lists_configured_hooks_in_lifecycle_order() {
        let cfg = Config::parse("[hooks]\npost-build = \"b\"\ndeps = \"d\"\n").unwrap();
        let names: Vec<_> = plan(&cfg, None).into_iter().map(|h| h.name).collect();
        assert_eq!(names, vec!["deps", "post_build"]);
    }

    #[test]
    fn plan_for_an_unconfigured_hook_is_empty() {
        let cfg = Config::default();
        assert!(plan(&cfg, None).is_empty());
    }

    #[test]
    fn plan_of_a_named_hook_ignores_the_lifecycle() {
        let cfg = Config::parse(
            "[hooks]\npost-build = \"b\"\n[[hook]]\nname = \"lint\"\ncmd = \"clippy\"\n",
        )
        .unwrap();
        let planned = plan(&cfg, Some("lint"));
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].cmd, "clippy");
        assert!(planned[0].point.is_none());
    }

    fn layout() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        (tmp, layout)
    }

    /// An `Env` rooted at a real, existing sandbox, so a test that actually spawns a
    /// hook has a working directory to run in. `/proj` does not exist, so it can only
    /// be used for assertions about variable construction.
    fn env_in(sandbox: &std::path::Path) -> Env {
        Env {
            project_root: sandbox.parent().unwrap().parent().unwrap().to_path_buf(),
            sandbox: sandbox.to_path_buf(),
            ref_name: "v1.2.3".into(),
            sha: "abc".into(),
            version: "1.2.3".into(),
        }
    }

    #[test]
    fn an_unset_hook_is_skipped_not_run() {
        let (_tmp, _l) = layout();
        let cfg = Config::default();
        assert_eq!(
            run_point(HookPoint::PreBuild, &env(), &cfg, false).unwrap(),
            Outcome::Skipped
        );
    }

    #[test]
    fn a_hook_runs_and_its_output_is_captured_by_inheriting_stdio() {
        let (_tmp, l) = layout();
        let sandbox = l.sandbox_dir();
        std::fs::create_dir_all(&sandbox).unwrap();
        let cfg = Config::parse("[hooks]\npost-apply = \"echo hi > out.txt\"\n").unwrap();
        assert_eq!(
            run_point(HookPoint::PostApply, &env_in(&sandbox), &cfg, false).unwrap(),
            Outcome::Ran
        );
        assert_eq!(
            std::fs::read_to_string(sandbox.join("out.txt"))
                .unwrap()
                .trim(),
            "hi"
        );
    }

    #[test]
    fn a_failing_required_hook_is_an_error() {
        let (_tmp, l) = layout();
        let sandbox = l.sandbox_dir();
        std::fs::create_dir_all(&sandbox).unwrap();
        let cfg = Config::parse("[hooks]\npre-build = \"exit 3\"\n").unwrap();
        let err = run_point(HookPoint::PreBuild, &env_in(&sandbox), &cfg, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit status 3"), "got {err}");
    }

    #[test]
    fn a_failing_optional_hook_only_warns() {
        let (_tmp, l) = layout();
        let sandbox = l.sandbox_dir();
        std::fs::create_dir_all(&sandbox).unwrap();
        let cfg = Config::parse("[[hook]]\nname = \"lint\"\ncmd = \"exit 1\"\nrequired = false\n")
            .unwrap();
        assert_eq!(
            run_named("lint", &l, &env_in(&sandbox), &cfg, false).unwrap(),
            Outcome::Failed
        );
    }

    #[test]
    fn a_hook_runs_with_the_sandbox_as_its_working_directory() {
        let (_tmp, l) = layout();
        let sandbox = l.sandbox_dir();
        std::fs::create_dir_all(&sandbox).unwrap();
        let cfg = Config::parse("[hooks]\ndeps = \"pwd > where.txt\"\n").unwrap();
        run_point(HookPoint::Deps, &env_in(&sandbox), &cfg, false).unwrap();
        let where_file = std::fs::read_to_string(sandbox.join("where.txt")).unwrap();
        assert_eq!(where_file.trim(), sandbox.display().to_string());
    }

    #[test]
    fn an_unknown_hook_name_lists_the_ones_that_exist() {
        let (_tmp, l) = layout();
        let cfg = Config::parse("[[hook]]\nname = \"lint\"\ncmd = \"true\"\n").unwrap();
        let err = run_named("test", &l, &env(), &cfg, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("lint"), "got {err}");
    }

    #[test]
    fn ensure_sandbox_explains_itself_when_absent() {
        let err = ensure_sandbox(Path::new("/definitely/not/here"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no sandbox"), "got {err}");
    }

    #[test]
    fn long_commands_are_elided_in_the_log() {
        assert_eq!(dim_cmd("one\ntwo"), "one ...");
        assert_eq!(dim_cmd("one"), "one");
    }
}
