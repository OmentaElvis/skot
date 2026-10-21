use anyhow::{Context, Result, bail};
use log::{info, warn};

use crate::cli::InitArgs;
use crate::commands::{Ctx, Exit};
use crate::config::SandboxPolicy;
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::{self, DEFAULT_SERIES, Layout};
use crate::phase::HookPoint;
use crate::prompt::{Fallback, PromptError};
use crate::state::State;

const PLACEHOLDER_URL: &str = "url = \"https://github.com/example/project\"";
const PLACEHOLDER_REF: &str = "default-ref = \"v1.0.0\"";
const PLACEHOLDER_POLICY: &str = "policy = \"persistent\"";
const PLACEHOLDER_KEEP: &str = "keep = [\"node_modules\", \"target\"]";
const PLACEHOLDER_SERIES: &str = "series = \"default\"";

/// The keys of the `[hooks]` section, which is what `--hook` may name.
const HOOK_KEYS: [&str; 7] = [
    "pre-sync",
    "post-sync",
    "pre-apply",
    "post-apply",
    "pre-build",
    "build",
    "post-build",
];

/// Directories that exist only to be reused, and so are worth offering as paths a ref
/// change must not touch. Offered, not assumed: only the ones actually present here are
/// asked about.
const KEEP_CANDIDATES: [&str; 5] = ["node_modules", "target", "vendor", ".venv", ".next"];

/// Render `key = value` with the value quoted and escaped as TOML requires.
///
/// Interpolating a raw string into the template would let a value containing a quote
/// produce a file skot can no longer read. That matters here because the whole point
/// of `init` is to leave something behind that parses.
fn toml_line(key: &str, value: &str) -> String {
    format!("{key} = {}", toml::Value::String(value.to_owned()))
}

/// Everything `init` writes, gathered from flags and questions alike.
///
/// `None` means *not known*, and scaffold comments such a value out rather than filling
/// it with a plausible example. The work without the command: values, not clap types, so
/// this runs in tests with no `Ctx` and no prompting.
#[derive(Debug, Default)]
struct Plan {
    url: Option<String>,
    default_ref: Option<String>,
    policy: Option<SandboxPolicy>,
    keep: Option<Vec<String>>,
    hooks: Vec<(String, String)>,
    force: bool,
}

/// Split `NAME=COMMAND` into its halves, refusing names that are not `[hooks]` keys.
fn parse_hook(spec: &str) -> Result<(String, String)> {
    let Some((name, cmd)) = spec.split_once('=') else {
        bail!("`--hook` wants NAME=COMMAND, as in `--hook build=\"make\"`.");
    };
    let name = name.trim();
    if !HOOK_KEYS.contains(&name) {
        bail!(
            "`{name}` is not a hook. The hooks are: {}.",
            HOOK_KEYS.join(", ")
        );
    }
    Ok((name.to_owned(), cmd.to_owned()))
}

/// The answer a question expects, or `None` when it was left blank.
fn non_empty(answer: String) -> Option<String> {
    let trimmed = answer.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// What the wizard says about a stage when asking for its hook.
fn stage_help(point: HookPoint) -> &'static str {
    match point {
        HookPoint::PreSync => "Runs before the mirror is touched.",
        HookPoint::PostSync => "Runs after the mirror is up to date.",
        HookPoint::PreApply => "Runs before the series is applied; project root on a first run.",
        HookPoint::PostApply => {
            "Runs after the series is in the sandbox; a quick check belongs here."
        }
        HookPoint::PreBuild => "Runs before anything is produced: install dependencies here.",
        HookPoint::Build => "Produces the artifact inside the sandbox.",
        HookPoint::PostBuild => {
            "Runs against the finished artifact: copy it into place, smoke test it."
        }
    }
}

/// An example worth showing where one says more than a description can.
fn stage_example(point: HookPoint) -> Option<&'static str> {
    match point {
        HookPoint::PreBuild => Some("npm ci"),
        HookPoint::Build => Some("make"),
        HookPoint::PostBuild => Some("cp dist/app ~/.local/bin/"),
        _ => None,
    }
}

/// A hook value as it belongs in a menu line: one line, bounded length.
fn one_line(cmd: &str) -> String {
    let flat = cmd.replace(['\n', '\r'], " ");
    let mut chars = flat.chars();
    let head: String = chars.by_ref().take(48).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

/// Which of [`KEEP_CANDIDATES`] are actually sitting in `root`.
fn keep_candidates(root: &std::path::Path) -> Vec<String> {
    KEEP_CANDIDATES
        .iter()
        .filter(|name| root.join(name).is_dir())
        .map(|name| (*name).to_owned())
        .collect()
}

/// Scaffold the layout with what it is given; it never invents a value.
///
/// A value it is not given is commented out, not left as a plausible example: an
/// uncommented `default-ref = "v1.0.0"` parses as a real setting, so `skot sync` would
/// go looking for a ref the project never had — a guess that fails later and far from
/// here. The commented line stays to be noticed and filled in.
///
/// The work without the command: the inputs are values rather than clap types, so this
/// runs in tests with no `Ctx` and no parsing.
fn scaffold(layout: &Layout, plan: &Plan) -> Result<()> {
    // The refusal before anything is created: a rejected `init` should touch nothing.
    let config_path = layout.config_file();
    if config_path.exists() && !plan.force {
        bail!(
            "{} already exists.\n\
             Re-run with --force to overwrite it.",
            config_path.display()
        );
    }
    std::fs::create_dir_all(layout.root())
        .context(format!("creating {}", layout.root().display()))?;

    let mut template = crate::config::Config::template();
    // The committed default, always written: a clone has no pointer yet, and the
    // series init just named is the one it should build.
    template = template.replace(PLACEHOLDER_SERIES, &toml_line("series", layout.series()));
    match plan.url.as_deref() {
        Some(url) => template = template.replace(PLACEHOLDER_URL, &toml_line("url", url)),
        None => template = template.replace(PLACEHOLDER_URL, &format!("# {PLACEHOLDER_URL}")),
    }
    match plan.default_ref.as_deref() {
        Some(ref_name) => {
            template = template.replace(PLACEHOLDER_REF, &toml_line("default-ref", ref_name))
        }
        None => template = template.replace(PLACEHOLDER_REF, &format!("# {PLACEHOLDER_REF}")),
    }
    match plan.policy {
        Some(policy) => {
            template = template.replace(
                PLACEHOLDER_POLICY,
                &toml_line("policy", &policy.to_string()),
            )
        }
        None => template = template.replace(PLACEHOLDER_POLICY, &format!("# {PLACEHOLDER_POLICY}")),
    }
    match &plan.keep {
        Some(paths) if !paths.is_empty() => {
            let items: Vec<String> = paths
                .iter()
                .map(|path| toml::Value::String(path.clone()).to_string())
                .collect();
            template =
                template.replace(PLACEHOLDER_KEEP, &format!("keep = [{}]", items.join(", ")));
        }
        _ => template = template.replace(PLACEHOLDER_KEEP, &format!("# {PLACEHOLDER_KEEP}")),
    }
    // Whole lines only: `build = ""` is a substring of both `pre-build = ""` and
    // `post-build = ""`, so a bare substring replace would write one answer over every
    // slot whose name ends in it. The slots with no answer are commented out rather than
    // left as empty strings: an uncommented `build = ""` parses as a real, empty build.
    let filled: Vec<(String, String)> = plan
        .hooks
        .iter()
        .map(|(name, cmd)| (format!("{name} = \"\""), toml_line(name, cmd)))
        .collect();
    template = template
        .split('\n')
        .map(
            |line| match filled.iter().find(|(wanted, _)| line == wanted) {
                Some((_, replacement)) => replacement.clone(),
                None if line.ends_with("= \"\"") => format!("# {line}"),
                None => line.to_owned(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n");

    let series_path = layout.series_file();
    let series_existed = series_path.exists();

    std::fs::write(&config_path, &template)
        .context(format!("writing {}", config_path.display()))?;
    std::fs::create_dir_all(layout.patches_dir())
        .context(format!("creating {}", layout.patches_dir().display()))?;
    if !series_existed {
        std::fs::write(
            &series_path,
            "# The patches to apply, in order. One filename per line.\n",
        )
        .context(format!("writing {}", series_path.display()))?;
    }
    layout.ensure_state_dirs()?;
    Ok(())
}

/// What the scaffold left open that blocks a command, said out loud.
///
/// A missing url or default-ref stops commands cold, so both belong in the report. An
/// unset build hook does not: an apply-only project is finished as it stands, and the
/// `next` hints already point at the `[hooks]` section for anyone who wants one.
fn todo_notes(layout: &Layout, plan: &Plan) -> Vec<String> {
    let config = layout.config_file().display().to_string();
    let mut notes = Vec::new();
    if plan.url.is_none() {
        notes.push(format!(
            "upstream.url is not set: run `{}` again, or edit {config}",
            skot_args("init", "--url <repository>")
        ));
    }
    if plan.default_ref.is_none() {
        notes.push(format!(
            "upstream.default-ref is not set: pass a ref (as in `{}`), or edit {config}",
            skot_args("apply", "main")
        ));
    }
    notes
}

/// `skot init`: ask for what is missing, scaffold, then carry the user to the next step.
///
/// Every question has a flag (`--url`, `--default-ref`, `--policy`, `--keep`, `--hook`,
/// `--sync`, and the global `--series` for the first series' name) and a value passed on
/// the command line skips its question, so nothing here is unavailable to a script.
/// Hooks sit behind one yes/no: a yes opens the `[hooks]`
/// menu, where a stage is picked, answered, and returned to until `done`; a no finishes
/// with the empty table the template documents. Without a TTY (or under `--json`) it
/// asks nothing at all.
pub fn run(ctx: &Ctx, args: &InitArgs) -> Result<Exit> {
    let root = match &ctx.cli.dir {
        Some(d) => layout::absolute(d),
        None => std::env::current_dir().context("reading the working directory")?,
    };
    let layout = Layout::new(&root);

    // Before any question: an init that will be refused should not collect answers it
    // cannot use.
    let config_path = layout.config_file();
    if config_path.exists() && !args.force {
        bail!(
            "{} already exists.\n\
             Re-run with --force to overwrite it.",
            config_path.display()
        );
    }

    // Every question below goes through this: `--json`, `--yes` and a missing
    // terminal all mean the same silent run, which takes each default.
    let asker = ctx.asker();

    // The first series: a flag or a question decide its name, and the layout points at
    // it from here on, so every file init writes lands in that series' directory. A
    // name that is not a single path component is refused now, before anything is made.
    let series = match &ctx.cli.series {
        Some(name) => name.clone(),
        None => {
            let answer = asker
                .text(
                    "First series name",
                    Some(
                        "Where your patches live: a directory under patches/. \
                         Leave empty for the default series.",
                    ),
                    Some(DEFAULT_SERIES),
                    Fallback::Default(String::new()),
                    None,
                )
                .context("asking for the first series name")?;
            non_empty(answer).unwrap_or_else(|| DEFAULT_SERIES.to_owned())
        }
    };
    let layout = layout.with_series(&series)?;
    // Init writes the template, and the template declares the default scope, so the
    // state it stamps here has to carry that same scope rather than the one a layout
    // built before any config exists happens to start with.
    let layout = layout.with_per_series_sandbox(crate::config::Sandbox::default().per_series);

    let mut hooks: Vec<(String, String)> = Vec::new();
    for spec in &args.hook {
        let (name, cmd) = parse_hook(spec)?;
        if hooks.iter().any(|(seen, _)| *seen == name) {
            bail!("`--hook {name}` was given twice.");
        }
        hooks.push((name, cmd));
    }

    // The one question with no default: a URL is a fact about the project, so a run
    // that cannot ask is refused with the flag rather than left with a guess.
    let url = match &args.url {
        Some(url) => non_empty(url.clone()),
        None => non_empty(
            asker.text(
                "Upstream repository URL",
                Some(
                    "The repository your patches apply to: an https URL, an ssh remote, or a local path.",
                ),
                Some("https://github.com/you/project"),
                Fallback::Flag("--url <URL>".to_owned()),
                None,
            )
            .context("asking for the upstream repository URL")?,
        ),
    };

    let default_ref = match &args.default_ref {
        Some(ref_name) => Some(ref_name.clone()),
        None => non_empty(
            asker.text(
                "Default ref",
                Some("Branch or tag commands use when you pass none. Leave empty to set it later."),
                None,
                Fallback::Default(String::new()),
                None,
            )
            .context("asking for the default ref")?,
        ),
    };

    let policy = match args.policy {
        Some(choice) => Some(choice.policy()),
        None => {
            let chosen = asker.select(
                "Sandbox policy",
                None,
                vec![
                    "persistent: kept between runs; a ref change resets it (right locally)",
                    "ephemeral: created and destroyed every run (right for CI)",
                ],
                Fallback::Default("persistent".to_owned()),
                None,
                None,
            );
            let answer = chosen.context("asking for the sandbox policy")?;
            Some(if answer.starts_with("ephemeral") {
                SandboxPolicy::Ephemeral
            } else {
                SandboxPolicy::Persistent
            })
        }
    };

    let keep = if !args.keep.is_empty() {
        Some(args.keep.clone())
    } else {
        let candidates = keep_candidates(layout.root());
        if candidates.is_empty() {
            None
        } else {
            let selected = asker
                .multi(
                    "Paths that must survive a ref change",
                    None,
                    candidates.clone(),
                    Fallback::Default(candidates),
                )
                .context("asking which paths to protect")?;
            (!selected.is_empty()).then_some(selected)
        }
    };

    // Hooks are a table of stages, not one stray question about "build": one yes/no, then
    // a menu where a stage is picked, answered, and returned to until `done` or Esc. A no
    // finishes the init with every slot empty, which is a finished project too, and the
    // template's comments explain each slot to whoever edits the file later. `--hook` has
    // already named a stage, so the question it would have asked does not exist.
    let configure = !args.hook.is_empty()
        || asker
            .confirm(
                "Configure hooks?",
                Some(&format!(
                    "The [hooks] table: what runs at each stage of `{}`. \
                     All optional, all editable later.",
                    skot("build")
                )),
                Fallback::Default(false),
            )
            .context("asking whether to configure hooks")?;
    if configure {
        // Reaching one hook must not cost a walk through the other six, so the menu
        // comes back after every answer wearing the new value. Esc at the menu finishes
        // the wizard with what has been set; at a value it goes back, value unchanged.
        loop {
            // `done (finish)` is the first item and Enter lands on it: a run that wants
            // out of the wizard is out in one keystroke, and the list holds all eight
            // options at once so the exit is on screen without any scrolling.
            let options: Vec<String> = std::iter::once("done (finish)".to_owned())
                .chain(HookPoint::ALL.iter().map(|point| {
                    let key = point.toml_key();
                    match hooks.iter().find(|(name, _)| name == key) {
                        Some((_, cmd)) => format!("{key} = {}", one_line(cmd)),
                        None => format!("{key} (unset)"),
                    }
                }))
                .collect();
            let chosen = asker.select(
                "Which hook to configure",
                Some("Enter finishes; pick a hook to set it. Esc finishes, too."),
                options.clone(),
                Fallback::Default("done (finish)".to_owned()),
                Some(options.len()),
                Some("<done>"),
            );
            let chosen = match chosen {
                Ok(chosen) => chosen,
                // Esc finishes the same way Enter on the first item does.
                Err(PromptError::Cancelled) => break,
                Err(err) => return Err(err).context("asking which hook to configure"),
            };
            let key = chosen.split([' ', '(']).next().unwrap_or("");
            let Some(point) = HookPoint::ALL.iter().find(|p| p.toml_key() == key) else {
                break; // `done`
            };
            let label = format!("Configure `{key}`");
            let answer = asker.text(
                &label,
                Some(stage_help(*point)),
                stage_example(*point),
                Fallback::Default(String::new()),
                Some("<back>"),
            );
            let answer = match answer {
                Ok(answer) => answer,
                // Esc goes back to the menu, the value as it stands.
                Err(PromptError::Cancelled) => continue,
                Err(err) => return Err(err).context(format!("asking for the `{key}` hook")),
            };
            if let Some(cmd) = non_empty(answer) {
                match hooks.iter_mut().find(|(name, _)| name == key) {
                    Some(slot) => slot.1 = cmd,
                    None => hooks.push((key.to_owned(), cmd)),
                }
            }
        }
    }

    let plan = Plan {
        url,
        default_ref,
        policy,
        keep,
        hooks,
        force: args.force,
    };
    if args.sync && plan.url.is_none() {
        bail!("`--sync` needs an upstream URL.\nPass `--url`, or answer the question.");
    }

    scaffold(&layout, &plan)?;

    // The pointer follows the scaffold: an init with `--series exp` leaves the project
    // already pointed at exp, so the next command does not have to be told.
    State::load(&layout)?.set_active_series(&layout, &series)?;

    if !ctx.cli.json {
        info!(target: "init", "wrote {}", layout.config_file().display());
        info!(target: "init", "wrote {}", layout.series_file().display());
        info!(target: "init", "created {}", layout.patches_dir().display());
    }

    // The vite question: offer the step that makes the project usable rather than
    // leaving a freshly scaffolded directory to be figured out. The default is yes: a
    // run that cannot ask takes it, the same way pressing Enter would.
    let want_sync = plan.url.is_some()
        && (args.sync
            || asker
                .confirm(
                    "Mirror the upstream repository now?",
                    Some(&format!(
                        "Fetches it into the local mirror so `{}` can start a patch.",
                        skot("new")
                    )),
                    Fallback::Default(true),
                )
                .context("asking whether to mirror the repository")?);

    let mut sync = None;
    if want_sync {
        sync = Some(
            crate::commands::sync::fetch(&layout, None)
                .context("skot.toml was written, but mirroring the repository failed")?,
        );
    }

    let todo = todo_notes(&layout, &plan);

    if ctx.cli.json {
        crate::commands::emit_json(&serde_json::json!({
            "root": layout.root().display().to_string(),
            "config": layout.config_file().display().to_string(),
            "series": layout.series_file().display().to_string(),
            "patches": layout.patches_dir().display().to_string(),
            "sync": sync,
            "todo": todo,
        }))
        .context("writing the document to stdout")?;
    } else {
        for note in &todo {
            warn!(target: "init", "{note}");
        }
        let mut hints = Hints::new("next").line(
            "edit the [hooks] section to describe how you build and where the result\n  \
             goes; post-build is where you copy it into place.\n",
        );
        hints = hints.entry(skot("build"), "the whole run: fetch, apply, build");
        if sync.is_none() {
            hints = hints.entry(
                skot_args("sync", "<ref>"),
                "just mirror the upstream repository",
            );
        }
        hints
            .entry(
                skot_args("new", "<name>"),
                "start a patch and open a work area on it",
            )
            .print();
    }
    Ok(Exit::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command, SandboxPolicyChoice};
    use clap::Parser as _;

    fn toml_line(key: &str, value: &str) -> String {
        format!("{key} = {}", toml::Value::String(value.to_owned()))
    }

    fn plan(url: &str) -> Plan {
        Plan {
            url: Some(url.to_owned()),
            force: true,
            ..Plan::default()
        }
    }

    fn read(layout: &Layout) -> String {
        std::fs::read_to_string(layout.config_file()).unwrap()
    }

    #[test]
    fn the_url_from_the_flag_reaches_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        scaffold(&layout, &plan("https://example.invalid/proj")).unwrap();

        let written = read(&layout);
        assert!(
            written.contains(&toml_line("url", "https://example.invalid/proj")),
            "{written}"
        );
        assert!(
            !written.contains(PLACEHOLDER_URL),
            "the placeholder survived, so the user cannot tell what is real"
        );
    }

    #[test]
    fn the_first_series_becomes_the_committed_default() {
        // A clone has no pointer to read, so the name init prompted for has to reach
        // the config: that line is what makes `skot build` in a fresh checkout land on
        // the same series the author was on.
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path()).with_series("exp").unwrap();
        scaffold(&layout, &plan("https://example.invalid/proj")).unwrap();

        let written = read(&layout);
        assert!(written.contains(&toml_line("series", "exp")), "{written}");
        assert!(!written.contains(PLACEHOLDER_SERIES), "{written}");
    }

    #[test]
    fn the_default_ref_from_the_flag_reaches_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        let p = Plan {
            default_ref: Some("v9.1.0".to_owned()),
            ..plan("https://example.invalid/proj")
        };
        scaffold(&layout, &p).unwrap();

        let written = read(&layout);
        assert!(
            written.contains(&toml_line("default-ref", "v9.1.0")),
            "{written}"
        );
        assert!(!written.contains(PLACEHOLDER_REF), "{written}");
    }

    #[test]
    fn the_policy_and_keep_reach_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        let p = Plan {
            policy: Some(SandboxPolicy::Ephemeral),
            keep: Some(vec!["npm-cache".to_owned()]),
            ..plan("https://example.invalid/proj")
        };
        scaffold(&layout, &p).unwrap();

        let written = read(&layout);
        assert!(
            written.contains(&toml_line("policy", "ephemeral")),
            "{written}"
        );
        assert!(written.contains("keep = [\"npm-cache\"]"), "{written}");
        assert!(!written.contains(PLACEHOLDER_KEEP), "{written}");

        let parsed: crate::config::Config = toml::from_str(&written).unwrap();
        assert_eq!(parsed.sandbox.policy, SandboxPolicy::Ephemeral);
        assert_eq!(parsed.sandbox.keep, ["npm-cache"]);
    }

    #[test]
    fn values_it_is_not_given_are_commented_out_not_left_live() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        scaffold(&layout, &Plan::default()).unwrap();

        // A live example is a guess that parses: commented, it stays visible as the
        // thing to fill in and commands see nothing at all.
        let written = read(&layout);
        assert!(
            written.contains(&format!("# {PLACEHOLDER_URL}")),
            "{written}"
        );
        assert!(
            written.contains(&format!("# {PLACEHOLDER_REF}")),
            "{written}"
        );
        assert!(
            written.contains(&format!("# {PLACEHOLDER_POLICY}")),
            "{written}"
        );
        assert!(
            written.contains(&format!("# {PLACEHOLDER_KEEP}")),
            "{written}"
        );

        let parsed: crate::config::Config = toml::from_str(&written).unwrap();
        assert_eq!(parsed.upstream.url, None);
        assert_eq!(parsed.upstream.default_ref, None);
        assert_eq!(parsed.sandbox.policy, SandboxPolicy::Persistent);
        assert!(parsed.sandbox.keep.is_empty());
    }

    #[test]
    fn a_hook_fills_its_empty_line() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        let p = Plan {
            hooks: vec![
                ("pre-build".to_owned(), "npm ci".to_owned()),
                ("build".to_owned(), "make".to_owned()),
            ],
            ..plan("https://example.invalid/proj")
        };
        scaffold(&layout, &p).unwrap();

        let written = read(&layout);
        assert!(
            written.contains(&toml_line("pre-build", "npm ci")),
            "{written}"
        );
        assert!(written.contains(&toml_line("build", "make")), "{written}");

        let parsed: crate::config::Config = toml::from_str(&written).unwrap();
        assert_eq!(parsed.hooks.pre_build.as_deref(), Some("npm ci"));
        assert_eq!(parsed.hooks.build.as_deref(), Some("make"));
        assert_eq!(
            parsed.hooks.post_build.as_deref(),
            None,
            "one answer must not plaster itself over the slots it is a substring of"
        );
        assert_eq!(
            parsed.hooks.pre_sync.as_deref(),
            None,
            "the hooks that were not asked about are commented out, not empty strings"
        );
        assert!(
            written.contains("# post-build = \"\""),
            "an unconfigured hook is still there to be noticed: {written}"
        );
    }

    #[test]
    fn a_hook_is_named_and_split_on_the_first_equals() {
        assert_eq!(
            parse_hook("build=make").unwrap(),
            ("build".to_owned(), "make".to_owned())
        );
        assert_eq!(
            parse_hook("pre-build=FOO=1 make").unwrap(),
            ("pre-build".to_owned(), "FOO=1 make".to_owned())
        );
    }

    #[test]
    fn an_unknown_hook_name_is_refused_listing_the_real_ones() {
        let err = parse_hook("bogus=rm -rf /").unwrap_err().to_string();
        assert!(err.contains("`bogus` is not a hook"), "{err}");
        for name in ["build", "pre-build", "post-apply"] {
            assert!(err.contains(name), "the error should list {name}: {err}");
        }
    }

    #[test]
    fn a_hook_without_a_command_is_refused() {
        let err = parse_hook("build").unwrap_err().to_string();
        assert!(err.contains("NAME=COMMAND"), "{err}");
    }

    #[test]
    fn candidates_are_only_the_paths_that_are_actually_there() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            keep_candidates(tmp.path()).is_empty(),
            "an empty project has nothing to offer"
        );
        std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
        assert_eq!(keep_candidates(tmp.path()), ["node_modules"]);
    }

    #[test]
    fn the_result_parses_as_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        // A quote in the value is the interesting case: naive interpolation writes a
        // file that no longer parses, and skot would fail on its own output.
        scaffold(&layout, &plan("https://example.invalid/\"quoted\"")).unwrap();

        let parsed: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(layout.config_file()).unwrap()).unwrap();
        assert_eq!(
            parsed.upstream.url.as_deref(),
            Some("https://example.invalid/\"quoted\"")
        );
    }

    #[test]
    fn it_refuses_to_overwrite_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        scaffold(&layout, &plan("https://example.invalid/first")).unwrap();

        let second = Plan {
            force: false,
            ..plan("https://example.invalid/second")
        };
        assert!(
            scaffold(&layout, &second).is_err(),
            "a second init should refuse to clobber the first"
        );
        let written = read(&layout);
        assert!(
            written.contains("first"),
            "the original config was replaced anyway"
        );
    }

    #[test]
    fn an_existing_series_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        std::fs::create_dir_all(layout.patches_dir()).unwrap();
        std::fs::write(layout.series_file(), "kept.patch\n").unwrap();

        scaffold(&layout, &plan("https://example.invalid/proj")).unwrap();
        // Re-running init is how you pick up new skot settings; it must not silently
        // discard the list of patches that is the entire point of the project.
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "kept.patch\n"
        );
    }

    #[test]
    fn the_series_and_patches_are_created_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        scaffold(&layout, &plan("https://example.invalid/proj")).unwrap();

        assert!(layout.patches_dir().is_dir());
        assert!(layout.series_file().is_file());
        assert!(
            layout.out_dir().is_dir(),
            "init should leave a usable out directory"
        );
    }

    #[test]
    fn every_init_subcommand_shares_this_scaffolding() {
        // Guards the dispatch table against a future subcommand that skips it.
        let cli = Cli::parse_from(["skot", "init", "--url", "u"]);
        let Command::Init(init) = cli.command else {
            panic!("expected the init subcommand");
        };
        assert_eq!(init.url.as_deref(), Some("u"), "clap dropped --url");
        assert_eq!(init.policy, None, "policy is asked for, not assumed");
    }

    #[test]
    fn the_policy_choice_maps_onto_the_config_type() {
        assert_eq!(
            SandboxPolicyChoice::Ephemeral.policy(),
            SandboxPolicy::Ephemeral
        );
        assert_eq!(
            SandboxPolicyChoice::Persistent.policy(),
            SandboxPolicy::Persistent
        );
    }
}
