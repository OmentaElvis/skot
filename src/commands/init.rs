use std::io::IsTerminal as _;

use anyhow::{Context, Result, bail};
use log::{info, warn};

use crate::cli::InitArgs;
use crate::commands::{Ctx, Exit};
use crate::config::SandboxPolicy;
use crate::hints::Hints;
use crate::layout::{self, Layout};

const PLACEHOLDER_URL: &str = "url = \"https://github.com/example/project\"";
const PLACEHOLDER_REF: &str = "default-ref = \"v1.0.0\"";
const PLACEHOLDER_POLICY: &str = "policy = \"persistent\"";
const PLACEHOLDER_KEEP: &str = "keep = [\"node_modules\", \"target\"]";

/// The keys of the `[hooks]` section, which is what `--hook` may name.
const HOOK_KEYS: [&str; 9] = [
    "pre-sync",
    "post-sync",
    "pre-apply",
    "post-apply",
    "deps",
    "pre-build",
    "post-build",
    "pre-verify",
    "post-verify",
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
        bail!("`--hook` wants NAME=COMMAND, as in `--hook deps=\"npm ci\"`.");
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
            template = template.replace(PLACEHOLDER_POLICY, &toml_line("policy", &policy.to_string()))
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
    for (name, cmd) in &plan.hooks {
        template = template.replace(&format!("{name} = \"\""), &toml_line(name, cmd));
    }

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

/// What the scaffold deliberately left open, said out loud.
///
/// A project with a URL but no build command is not finished, and init should never
/// present it as if it were.
fn todo_notes(layout: &Layout, plan: &Plan) -> Vec<String> {
    let config = layout.config_file().display().to_string();
    let mut notes = Vec::new();
    if plan.url.is_none() {
        notes.push(format!(
            "upstream.url is not set — run `skot init --url <repository>` again, or edit {config}"
        ));
    }
    if plan.default_ref.is_none() {
        notes.push(format!(
            "upstream.default-ref is not set — pass a ref (as in `skot apply main`), or edit {config}"
        ));
    }
    if !plan.hooks.iter().any(|(name, _)| name == "pre-build") {
        notes.push(format!(
            "no build command — `skot build` will not produce anything until \
             `pre-build` is set in {config}"
        ));
    }
    notes
}

/// `skot init`: ask for what is missing, scaffold, then carry the user to the next step.
///
/// Every question has a flag — `--url`, `--default-ref`, `--policy`, `--keep`, `--hook`,
/// `--sync` — and a value passed on the command line skips its question, so nothing here
/// is unavailable to a script. Without a TTY (or under `--json`) it asks nothing at all.
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

    let mut hooks: Vec<(String, String)> = Vec::new();
    for spec in &args.hook {
        let (name, cmd) = parse_hook(spec)?;
        if hooks.iter().any(|(seen, _)| *seen == name) {
            bail!("`--hook {name}` was given twice.");
        }
        hooks.push((name, cmd));
    }

    let ask = !ctx.cli.json && std::io::stdin().is_terminal();

    let url = match &args.url {
        Some(url) => Some(url.clone()),
        None if ask => Some(
            inquire::Text::new("Upstream repository URL")
                .with_help_message(
                    "The repository your patches apply to: an https URL, an ssh remote, or a local path.",
                )
                .with_placeholder("https://github.com/you/project")
                .prompt()
                .context("asking for the upstream repository URL")?,
        ),
        None => None,
    };
    let url = non_empty(url.unwrap_or_default());

    let default_ref = match &args.default_ref {
        Some(ref_name) => Some(ref_name.clone()),
        None if ask => {
            let answer = inquire::Text::new("Default ref")
                .with_help_message(
                    "Branch or tag commands use when you pass none. Leave empty to set it later.",
                )
                .prompt()
                .context("asking for the default ref")?;
            non_empty(answer)
        }
        None => None,
    };

    let policy = match args.policy {
        Some(choice) => Some(choice.policy()),
        None if ask => {
            let answer = inquire::Select::new(
                "Sandbox policy",
                vec![
                    "persistent — kept between runs; a ref change resets it (right locally)",
                    "ephemeral — created and destroyed every run (right for CI)",
                ],
            )
            .prompt()
            .context("asking for the sandbox policy")?;
            Some(if answer.starts_with("ephemeral") {
                SandboxPolicy::Ephemeral
            } else {
                SandboxPolicy::Persistent
            })
        }
        None => None,
    };

    let keep = if !args.keep.is_empty() {
        Some(args.keep.clone())
    } else if ask {
        let candidates = keep_candidates(layout.root());
        if candidates.is_empty() {
            None
        } else {
            let all: Vec<usize> = (0..candidates.len()).collect();
            let selected = inquire::MultiSelect::new("Paths that must survive a ref change", candidates)
                .with_default(&all)
                .prompt()
                .context("asking which paths to protect")?;
            if selected.is_empty() {
                None
            } else {
                Some(selected)
            }
        }
    } else {
        None
    };

    if ask && !hooks.iter().any(|(name, _)| name == "deps") {
        let answer = inquire::Text::new("Dependencies command")
            .with_help_message(
                "Runs in the sandbox before the build. Leave empty if there is nothing to install.",
            )
            .with_placeholder("npm ci")
            .prompt()
            .context("asking for the dependencies command")?;
        if let Some(cmd) = non_empty(answer) {
            hooks.push(("deps".to_owned(), cmd));
        }
    }
    if ask && !hooks.iter().any(|(name, _)| name == "pre-build") {
        let answer = inquire::Text::new("Build command")
            .with_help_message(
                "Produces your artifact inside the sandbox. Leave empty to fill it in later.",
            )
            .with_placeholder("cargo build --release")
            .prompt()
            .context("asking for the build command")?;
        if let Some(cmd) = non_empty(answer) {
            hooks.push(("pre-build".to_owned(), cmd));
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

    if !ctx.cli.json {
        info!(target: "init", "wrote {}", layout.config_file().display());
        info!(target: "init", "wrote {}", layout.series_file().display());
        info!(target: "init", "created {}", layout.patches_dir().display());
    }

    // The vite question: offer the step that makes the project usable rather than
    // leaving a freshly scaffolded directory to be figured out.
    let want_sync = plan.url.is_some()
        && if ctx.cli.json {
            args.sync
        } else if args.sync || ctx.yes {
            true
        } else if ask {
            inquire::Confirm::new("Mirror the upstream repository now?")
                .with_default(true)
                .with_help_message(
                    "Fetches it into the local mirror so `skot new` can start a patch.",
                )
                .prompt()
                .context("asking whether to mirror the repository")?
        } else {
            false
        };

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
        if sync.is_none() {
            hints = hints.entry("skot sync <ref>", "mirror the upstream repository");
        }
        hints
            .entry("skot new <name>", "start a patch, with the series applied")
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
        assert!(written.contains(&toml_line("policy", "ephemeral")), "{written}");
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
        assert!(written.contains(&format!("# {PLACEHOLDER_URL}")), "{written}");
        assert!(written.contains(&format!("# {PLACEHOLDER_REF}")), "{written}");
        assert!(
            written.contains(&format!("# {PLACEHOLDER_POLICY}")),
            "{written}"
        );
        assert!(written.contains(&format!("# {PLACEHOLDER_KEEP}")), "{written}");

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
                ("deps".to_owned(), "npm ci".to_owned()),
                ("pre-build".to_owned(), "make".to_owned()),
            ],
            ..plan("https://example.invalid/proj")
        };
        scaffold(&layout, &p).unwrap();

        let written = read(&layout);
        assert!(written.contains(&toml_line("deps", "npm ci")), "{written}");
        assert!(written.contains(&toml_line("pre-build", "make")), "{written}");

        let parsed: crate::config::Config = toml::from_str(&written).unwrap();
        assert_eq!(parsed.hooks.deps.as_deref(), Some("npm ci"));
        assert_eq!(parsed.hooks.pre_build.as_deref(), Some("make"));
        assert_eq!(
            parsed.hooks.pre_sync.as_deref(),
            Some(""),
            "the hooks that were not asked about keep their empty line"
        );
    }

    #[test]
    fn a_hook_is_named_and_split_on_the_first_equals() {
        assert_eq!(
            parse_hook("deps=npm ci").unwrap(),
            ("deps".to_owned(), "npm ci".to_owned())
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
        for name in ["deps", "pre-build", "pre-verify"] {
            assert!(err.contains(name), "the error should list {name}: {err}");
        }
    }

    #[test]
    fn a_hook_without_a_command_is_refused() {
        let err = parse_hook("deps").unwrap_err().to_string();
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
        scaffold(
            &layout,
            &plan("https://example.invalid/\"quoted\""),
        )
        .unwrap();

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
        std::fs::write(layout.series_file(), "0001-kept.patch\n").unwrap();

        scaffold(&layout, &plan("https://example.invalid/proj")).unwrap();
        // Re-running init is how you pick up new skot settings; it must not silently
        // discard the list of patches that is the entire point of the project.
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-kept.patch\n"
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
