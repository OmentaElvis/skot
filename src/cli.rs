//! Command-line surface. The single place subcommands are named.
//!
//! Global flags follow the conventions every developer already expects from `gh`,
//! `kubectl` and `ansible`: `--json` for machines, `--yes` to skip the prompt,
//! `--dir` instead of environment variables for pointing at a project.

use std::path::PathBuf;

use clap::builder::StyledStr;
use clap::builder::styling::{Style, Styles};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::invocation::{skot, skot_args};
use crate::phase::Phase;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "skot",
    version,
    about = "Keep your changes to an upstream repo as a patch series.",
    long_about = "skot keeps your changes as an ordered series of patch files rather than as a \
                  fork, so they stay reviewable, individually droppable, and individually \
                  rebasable when upstream moves.\n\n\
                  skot knows how to manage a sandbox and a patch series. It does not know \
                  your language, your build system, or where your artifact goes: those are \
                  hooks in skot.toml.",
    propagate_version = true,
    disable_help_subcommand = false
)]
pub struct Cli {
    /// Project directory. Defaults to searching upwards for skot.toml.
    #[arg(long, global = true, value_name = "PATH")]
    pub dir: Option<PathBuf>,

    /// Machine-readable output on stdout.
    #[arg(long, global = true)]
    pub json: bool,

    /// Skip the next-step hints on stderr.
    #[arg(long, global = true)]
    pub no_hints: bool,

    /// Do not ask for confirmation.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// More detail. Repeat for more.
    #[arg(long, short = 'v', global = true, action = ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Scaffold skot.toml, series and patches/, asking for what is missing.
    Init(InitArgs),

    /// Build: sandbox, apply, then your hooks.
    Build(BuildArgs),

    /// Where everything is, how big it is, and whether a run was interrupted.
    Status(StatusArgs),

    /// Create, import, apply and inspect patches.
    #[command(subcommand)]
    Patch(PatchCommand),

    /// Check whether the series applies to a ref, without building anything.
    Check(CheckArgs),

    /// Replay the series onto a different ref.
    Rebase(RefArg),

    /// Mirror a remote and keep its refs current.
    Sync(SyncArgs),

    /// Free disk, in levels. Dry run unless --yes.
    Clean(CleanArgs),

    /// List the hooks that are configured, in lifecycle order.
    Hooks,

    /// Run a `[[hook]]` entry by name.
    Run(RunArgs),
}

/// Everything that acts on the series itself, under `skot patch`.
#[derive(Debug, Clone, Subcommand)]
pub enum PatchCommand {
    /// Start a new patch on top of the series.
    New(NewArgs),

    /// Import a commit range, branch or PR as a patch.
    ///
    /// Importing the same thing again updates the patch in place instead of adding
    /// another one.
    Import(ImportArgs),

    /// Re-export your edits to the patch file.
    Refresh(RefreshArgs),

    /// Apply the whole series to the sandbox, or pick patches interactively.
    Apply(RefArg),

    /// Remove the whole series from the sandbox.
    Unapply,

    /// Apply the next patch in the series, or a named one.
    Push(PatchArg),

    /// Undo the top patch, or a named one.
    Pop(PatchArg),

    /// List patches in order (alias: ls).
    #[command(visible_alias = "ls")]
    List(ListArgs),

    /// Take a patch out of the series: its line and its file.
    Drop(DropArgs),
}

// ----- grouped help ------------------------------------------------------------
//
// clap has no notion of command groups: `{subcommands}` is one flat list, and the
// feature has been open upstream since 2019 without a design. So the groups are built
// here from the real command tree and shown through a custom help template, the way
// cargo does it. The one hand-written list is which group each command belongs to;
// everything else (names, descriptions) is read off the enums, and the tests fail if
// the list and the enums ever disagree.

/// Heading style for the groups and section headings: clap's own heading style, applied
/// to `Command` too so one style governs the whole page.
const HEADING: Style = Style::new().bold().underline();

/// Command names inside the groups: clap's literal style.
const LITERAL: Style = Style::new().bold();

/// The root help's groups: heading, then the commands under it in display order.
const ROOT_GROUPS: &[(&str, &[&str])] = &[
    ("Start here", &["init", "build", "status"]),
    ("Patches", &["patch"]),
    ("Upstream", &["check", "rebase", "sync"]),
    ("Housekeeping", &["clean", "hooks", "run"]),
];

/// `skot patch --help`'s groups, same shape as [`ROOT_GROUPS`].
const PATCH_GROUPS: &[(&str, &[&str])] = &[
    ("Author", &["new", "import", "refresh"]),
    ("Apply", &["apply", "unapply", "push", "pop"]),
    ("Inspect and edit", &["list", "drop"]),
];

/// clap's own description of its `help` subcommand, appended after the groups. The test
/// `every_command_is_listed_exactly_once` checks this against the built command tree, so
/// a clap upgrade that rewords it fails rather than drifting.
const HELP_ABOUT: &str = "Print this message or the help of the given subcommand(s)";

/// The command tree with the grouped help applied.
///
/// This, not [`Cli::command`], is what the binary parses with, so `--help` shows the
/// groups. [`crate::invocation`] resolves command paths against the plain tree instead,
/// which is the same tree without the help cosmetics.
pub fn build_cli() -> clap::Command {
    let mut cmd = Cli::command();
    cmd = cmd.styles(Styles::styled().header(HEADING).literal(LITERAL));

    let patch_groups = cmd
        .find_subcommand("patch")
        .map(|patch| grouped_list(patch, PATCH_GROUPS));
    let root_groups = grouped_list(&cmd, ROOT_GROUPS);

    cmd = cmd
        .help_template(help_template("Global options:"))
        .before_help(root_groups)
        .after_help(quick_start());
    if let Some(groups) = patch_groups {
        cmd = cmd.mut_subcommand("patch", |patch| {
            patch
                .help_template(help_template("Options:"))
                .before_help(groups)
        });
    }
    cmd
}

/// The template behind both grouped pages. Positional-less by construction:
/// `every_command_is_listed_exactly_once` asserts that stays true, so no argument
/// section can go missing under a heading the template does not write.
fn help_template(options_heading: &str) -> String {
    let heading = HEADING.render();
    let reset = HEADING.render_reset();
    format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n\
         {{before-help}}{heading}{options_heading}{reset}\n{{options}}{{after-help}}"
    )
}

/// One command list: the groups in order, then clap's `help`, ungrouped.
///
/// `help` is not in the groups on purpose: it is not a command of ours, it is clap's
/// escape hatch, and it is added when clap builds the command, which has not happened
/// yet at this point. Its row is written from [`HELP_ABOUT`] and checked against the
/// built tree by the drift test.
fn grouped_list(cmd: &clap::Command, groups: &[(&str, &[&str])]) -> StyledStr {
    let listed = groups.iter().flat_map(|(_, names)| names.iter().copied());
    let width = listed.clone().map(str::len).max().unwrap_or(0).max(4);
    let mut out = StyledStr::new();
    let mut first = true;
    for (heading, names) in groups.iter().copied() {
        if !first {
            out.push_str("\n");
        }
        first = false;
        out.push_str(&format!(
            "{}{heading}:{}\n",
            HEADING.render(),
            HEADING.render_reset()
        ));
        for name in names {
            row(&mut out, cmd, name, width);
        }
    }
    push_row(&mut out, "help", HELP_ABOUT, width);
    out
}

/// One `  name  description` row, read off the tree so descriptions cannot be copied
/// into a second place and go stale.
fn row(out: &mut StyledStr, cmd: &clap::Command, name: &str, width: usize) {
    let sub = cmd
        .find_subcommand(name)
        .unwrap_or_else(|| panic!("the group table lists `{name}`, which is not a subcommand"));
    let about = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
    // `about` keeps the newlines of a multi-line doc comment; a row wants its first line.
    let about = about.lines().next().unwrap_or("");
    push_row(out, name, about, width);
}

fn push_row(out: &mut StyledStr, name: &str, about: &str, width: usize) {
    out.push_str(&format!(
        "  {}{name:<width$}{}  {about}\n",
        LITERAL.render(),
        LITERAL.render_reset()
    ));
}

/// The closing block of the root help. Built through [`skot_args`] like any other
/// command string, so a command that moves updates its own example.
fn quick_start() -> StyledStr {
    let mut out = StyledStr::new();
    out.push_str(&format!(
        "{}Quick start:{}\n",
        HEADING.render(),
        HEADING.render_reset()
    ));
    for (command, purpose) in [
        (skot_args("init", "--url <URL>"), "once, per project"),
        (skot("build"), "fetches upstream if needed"),
        (skot_args("new", "my-fix"), "edit the sandbox, then:"),
        (skot("refresh"), "record the edit"),
    ] {
        out.push_str(&format!(
            "  {}{command:<28}{}{purpose}\n",
            LITERAL.render(),
            LITERAL.render_reset()
        ));
    }
    out
}

#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    /// Also show whether each patch file exists on disk.
    #[arg(long)]
    pub paths: bool,
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {}

#[derive(Debug, Clone, Args)]
pub struct CheckArgs {
    /// Ref to check against. Defaults to the ref the sandbox is on, then
    /// `upstream.default-ref`.
    pub ref_name: Option<String>,

    /// Check the applied set instead of the whole series: exactly what `build` would
    /// land. Same ref rules either way.
    #[arg(long)]
    pub applied: bool,

    /// Report every patch, not just the ones that failed.
    #[arg(long, short = 'q', conflicts_with = "first_only")]
    pub all: bool,

    /// Stop at the first failing patch. The default.
    ///
    /// Deliberately not `default_value_t = true`: that makes clap count the flag as
    /// present, so `conflicts_with` would reject `--all` too.
    #[arg(long)]
    pub first_only: bool,
}

/// How much `clean` removes. Ordered, so that each level is a superset of the one
/// before it, which the `clean` tests assert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum CleanLevel {
    /// The sandbox. Next build reinstalls dependencies.
    Build,
    /// The sandbox and everything skot generated under .skot/out.
    Sandbox,
    /// Also the bare mirror. Next run re-clones.
    Mirror,
    /// All of it. Next run re-clones and reinstalls.
    All,
}

#[derive(Debug, Clone, Args)]
pub struct CleanArgs {
    /// How much to remove.
    #[arg(value_enum, default_value_t = CleanLevel::Build)]
    pub level: CleanLevel,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    /// Name of a `[[hook]]` entry.
    pub name: String,
}

#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    /// Upstream repository URL. Asked for interactively if omitted.
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,

    /// Ref commands use when none is given. Asked for interactively if omitted.
    #[arg(long, value_name = "REF")]
    pub default_ref: Option<String>,

    /// Sandbox policy. Asked for interactively if omitted.
    #[arg(long, value_name = "POLICY")]
    pub policy: Option<SandboxPolicyChoice>,

    /// Paths a ref change must not touch. Repeatable. Asked for interactively if omitted.
    #[arg(long, value_name = "PATH")]
    pub keep: Vec<String>,

    /// Set a hook: NAME=COMMAND, where NAME is a key of the `hooks` table. Repeatable.
    /// A hook you name here is not asked about.
    #[arg(long, value_name = "NAME=COMMAND")]
    pub hook: Vec<String>,

    /// Mirror the upstream repository when init finishes.
    #[arg(long)]
    pub sync: bool,

    /// Overwrite an existing skot.toml.
    #[arg(long)]
    pub force: bool,
}

/// The sandbox policy as spelled on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SandboxPolicyChoice {
    /// Keep the sandbox between runs and reset it to a ref. Right locally.
    Persistent,
    /// Create the sandbox, use it, destroy it. Right for CI.
    Ephemeral,
}

impl SandboxPolicyChoice {
    /// The config's view of the same choice.
    pub fn policy(self) -> crate::config::SandboxPolicy {
        match self {
            Self::Persistent => crate::config::SandboxPolicy::Persistent,
            Self::Ephemeral => crate::config::SandboxPolicy::Ephemeral,
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct RefArg {
    /// Ref: a tag, a branch, or a commit.
    #[arg(value_name = "REF")]
    pub ref_name: Option<String>,
}

/// Deliberately not [`RefArg`]: `skot sync --refresh` contacts the upstream even when the
/// mirror already has the ref, and putting that flag on the shared type would hand it to
/// `skot patch apply --refresh` too, where it could only mean something the apply path does
/// not do.
#[derive(Debug, Clone, Args)]
pub struct SyncArgs {
    /// Ref: a tag, a branch, or a commit. Defaults to `upstream.default-ref`.
    #[arg(value_name = "REF")]
    pub ref_name: Option<String>,
    /// Contact the upstream even when the mirror already has the ref.
    #[arg(long)]
    pub refresh: bool,
}

/// A patch name, for the commands that act on the stack rather than on a ref.
///
/// Deliberately not [`RefArg`]: `skot patch pop 0002-fix` takes a *patch*, and the ref it
/// works against is the one the sandbox is already on. Sharing one argument type between
/// the two would mean `pop main` silently meant "pop the patch called main", which is never
/// what anyone typing that meant.
#[derive(Debug, Clone, Default, Args)]
pub struct PatchArg {
    /// Patch in the series. Defaults to the one on top of the stack.
    #[arg(value_name = "PATCH")]
    pub patch: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct ImportArgs {
    /// What to import: `FROM..TO`, `FROM...TO` (from their merge base), or just `TO`.
    #[arg(value_name = "RANGE", required_unless_present = "pr")]
    pub range: Option<String>,

    /// Base of the patch. Defaults to the merge base with `upstream.default-ref`.
    #[arg(long, value_name = "REF")]
    pub from: Option<String>,

    /// Import pull request N, by fetching `refs/pull/N/head` from `upstream.url`.
    #[arg(long, value_name = "N", conflicts_with = "range")]
    pub pr: Option<u64>,

    /// Fetch the range from this repository (a URL or a local clone). Refs are looked up
    /// there first, so a range named against that repository resolves as written.
    #[arg(long, value_name = "URL")]
    pub repo: Option<String>,

    /// Name for the new patch. Defaults to the subject of what is being imported: the
    /// commit itself for a one-commit import, `PR N` for a pull request, or the range
    /// as written.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Insert at this position: a 1-based index or an existing patch name. Patches after
    /// it are renumbered so the numbers stay contiguous. Defaults to appending.
    #[arg(long, value_name = "PATCH|N", conflicts_with = "no_register")]
    pub at: Option<String>,

    /// Write the patch file without adding it to the series.
    #[arg(long)]
    pub no_register: bool,
}

#[derive(Debug, Clone, Args)]
pub struct NewArgs {
    /// Short name for the patch. Filenames are derived from it.
    pub name: String,

    /// Base the new patch on this ref instead of the default.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Insert at this position: a 1-based index or an existing patch name. Patches after
    /// it are renumbered so the numbers stay contiguous. Defaults to appending.
    #[arg(long, value_name = "PATCH|N")]
    pub at: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct RefreshArgs {
    /// Patch to refresh. Defaults to the one currently being authored.
    pub name: Option<String>,

    /// Where to insert it in the series. Patches after it are renumbered so the numbers
    /// stay contiguous.
    #[arg(long, value_name = "INDEX")]
    pub insert_at: Option<usize>,

    /// Do not add it to the series.
    #[arg(long)]
    pub no_register: bool,
}

#[derive(Debug, Clone, Args)]
pub struct DropArgs {
    /// Patch to drop: its name in the series, or its 1-based position. Refused while
    /// it is applied; `skot patch pop` takes it off first.
    #[arg(value_name = "PATCH")]
    pub patch: String,
}

#[derive(Debug, Clone, Args)]
pub struct BuildArgs {
    /// Ref to build. Defaults to the configured default ref.
    #[arg(value_name = "REF")]
    pub ref_name: Option<String>,

    /// Start at this phase.
    #[arg(long, value_name = "PHASE")]
    pub from: Option<Phase>,

    /// Run only this phase.
    #[arg(long, value_name = "PHASE", conflicts_with = "from")]
    pub only: Option<Phase>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
        build_cli().debug_assert();
    }

    #[test]
    fn global_flags_are_accepted_after_the_subcommand() {
        // `gh`-style: flags belong anywhere, not only before the subcommand.
        let cli = Cli::try_parse_from(["skot", "patch", "list", "--json"]).unwrap();
        assert!(cli.json);
        let cli = Cli::try_parse_from(["skot", "--json", "patch", "list"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn clean_defaults_to_the_build_level() {
        let cli = Cli::try_parse_from(["skot", "clean"]).unwrap();
        let Command::Clean(args) = cli.command else {
            panic!("expected clean")
        };
        assert_eq!(args.level, CleanLevel::Build);
    }

    #[test]
    fn clean_levels_parse_by_name() {
        for (text, expected) in [
            ("build", CleanLevel::Build),
            ("sandbox", CleanLevel::Sandbox),
            ("mirror", CleanLevel::Mirror),
            ("all", CleanLevel::All),
        ] {
            let cli = Cli::try_parse_from(["skot", "clean", text]).unwrap();
            let Command::Clean(args) = cli.command else {
                panic!("expected clean")
            };
            assert_eq!(args.level, expected);
        }
    }

    #[test]
    fn a_ref_is_optional_everywhere_it_is_optional() {
        for text in [
            ["skot", "check"].as_slice(),
            ["skot", "patch", "apply"].as_slice(),
            ["skot", "build"].as_slice(),
        ] {
            assert!(Cli::try_parse_from(text).is_ok(), "{text:?} should parse");
        }
    }

    #[test]
    fn from_and_only_cannot_both_be_given() {
        let err = Cli::try_parse_from(["skot", "build", "--from", "sync", "--only", "apply"]);
        assert!(err.is_err(), "these two flags contradict each other");
    }

    #[test]
    fn phases_parse_by_name() {
        let cli = Cli::try_parse_from(["skot", "build", "--from", "apply"]).unwrap();
        let Command::Build(args) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(args.from, Some(Phase::Apply));
    }

    #[test]
    fn init_takes_everything_the_prompts_take() {
        // Flag parity: nothing in the onboarding prompts is unavailable to a script.
        let cli = Cli::try_parse_from([
            "skot",
            "init",
            "--url",
            "u",
            "--default-ref",
            "main",
            "--policy",
            "ephemeral",
            "--keep",
            "dist",
            "--keep",
            "vendor",
            "--hook",
            "build=make",
            "--sync",
            "--force",
        ])
        .unwrap();
        let Command::Init(args) = cli.command else {
            panic!("expected the init subcommand");
        };
        assert_eq!(args.url.as_deref(), Some("u"));
        assert_eq!(args.default_ref.as_deref(), Some("main"));
        assert_eq!(args.policy, Some(SandboxPolicyChoice::Ephemeral));
        assert_eq!(args.keep, ["dist", "vendor"]);
        assert_eq!(args.hook, ["build=make"]);
        assert!(args.sync, "--sync should parse");
        assert!(args.force, "--force should parse");
    }

    #[test]
    fn an_unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["skot", "frobnicate"]).is_err());
    }

    #[test]
    fn an_unknown_clean_level_is_rejected_with_the_valid_ones() {
        let err = Cli::try_parse_from(["skot", "clean", "everything"])
            .unwrap_err()
            .to_string();
        for level in ["build", "sandbox", "mirror", "all"] {
            assert!(err.contains(level), "error should list {level}: {err}");
        }
    }

    #[test]
    fn new_requires_a_name() {
        assert!(Cli::try_parse_from(["skot", "patch", "new"]).is_err());
        assert!(Cli::try_parse_from(["skot", "patch", "new", "trim-widgets"]).is_ok());
    }

    #[test]
    fn import_takes_a_range_a_pull_request_and_a_source_repository() {
        let cli = Cli::try_parse_from(["skot", "patch", "import", "main..feat"]).unwrap();
        let Command::Patch(PatchCommand::Import(args)) = cli.command else {
            panic!("expected import")
        };
        assert_eq!(args.range.as_deref(), Some("main..feat"));
        assert!(args.pr.is_none());
        assert!(args.repo.is_none());

        let cli = Cli::try_parse_from(["skot", "patch", "import", "--pr", "42", "--name", "fix"])
            .unwrap();
        let Command::Patch(PatchCommand::Import(args)) = cli.command else {
            panic!("expected import")
        };
        assert_eq!(args.pr, Some(42));
        assert_eq!(args.name.as_deref(), Some("fix"));

        let cli =
            Cli::try_parse_from(["skot", "patch", "import", "--repo", "../fork", "main..feat"])
                .unwrap();
        let Command::Patch(PatchCommand::Import(args)) = cli.command else {
            panic!("expected import")
        };
        assert_eq!(args.repo.as_deref(), Some("../fork"));
        assert_eq!(args.range.as_deref(), Some("main..feat"));
    }

    #[test]
    fn import_cannot_take_both_a_pull_request_and_a_range() {
        let err = Cli::try_parse_from(["skot", "patch", "import", "--pr", "7", "main..feat"])
            .unwrap_err();
        let err = err.to_string();
        assert!(
            err.contains("--pr"),
            "the conflicting flag should be named: {err}"
        );
        assert!(
            err.contains("RANGE"),
            "the other side should be named: {err}"
        );
    }

    #[test]
    fn import_with_nothing_to_import_is_rejected() {
        assert!(Cli::try_parse_from(["skot", "patch", "import"]).is_err());
        assert!(Cli::try_parse_from(["skot", "patch", "import", "--no-register"]).is_err());
    }

    /// The anti-drift test. The group tables and the enums are two descriptions of the
    /// same surface; this holds them to each other. A subcommand nobody put in a group
    /// fails here, a group entry for a command that no longer exists fails here, and
    /// `help` is the only name allowed outside the tables.
    #[test]
    fn every_command_is_listed_exactly_once_in_the_grouped_help() {
        let mut cmd = build_cli();
        let root_help = cmd.render_help().ansi().to_string();

        let actual: Vec<String> = cmd
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(|sub| sub.get_name().to_string())
            .collect();
        assert_matches_tables(ROOT_GROUPS, &actual, "root");
        assert!(
            cmd.get_arguments().all(|arg| !arg.is_positional()),
            "the root template writes no positional section, so the root must have no positionals"
        );

        // clap's own row: the hand-written text must still be what clap will show.
        let help_about = cmd
            .find_subcommand("help")
            .and_then(|help| help.get_about())
            .map(|about| about.to_string());
        assert_eq!(help_about.as_deref(), Some(HELP_ABOUT));

        // Each listed command appears exactly once, as one styled row.
        for name in listed_names(ROOT_GROUPS) {
            assert_eq!(
                root_help
                    .matches(&format!("{}{name}", LITERAL.render()))
                    .count(),
                1,
                "`{name}` should be rendered as a row exactly once"
            );
        }
        assert_eq!(
            root_help
                .matches(&format!("{}help", LITERAL.render()))
                .count(),
            1,
            "clap's help should be rendered as a row exactly once"
        );

        let patch = cmd
            .find_subcommand_mut("patch")
            .expect("the root has a patch subcommand");
        let patch_help = patch.render_help().ansi().to_string();
        let patch_actual: Vec<String> = patch
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(|sub| sub.get_name().to_string())
            .collect();
        assert_matches_tables(PATCH_GROUPS, &patch_actual, "patch");
        assert!(
            patch.get_arguments().all(|arg| !arg.is_positional()),
            "the patch template writes no positional section, so patch must have no positionals"
        );
        for name in listed_names(PATCH_GROUPS) {
            assert_eq!(
                patch_help
                    .matches(&format!("{}{name}", LITERAL.render()))
                    .count(),
                1,
                "`{name}` should be rendered as a row exactly once"
            );
        }
    }

    #[test]
    fn the_grouped_help_renders_its_sections() {
        let mut cmd = build_cli();
        let root_help = cmd.render_help().ansi().to_string();
        for heading in [
            "Start here:",
            "Patches:",
            "Upstream:",
            "Housekeeping:",
            "Global options:",
            "Quick start:",
        ] {
            assert!(
                root_help.contains(&format!(
                    "{}{heading}{}",
                    HEADING.render(),
                    HEADING.render_reset()
                )),
                "root help is missing the styled heading {heading:?}"
            );
        }
        for command in [skot_args("init", "--url <URL>"), skot("build")] {
            assert!(
                root_help.contains(&format!("{}{command}", LITERAL.render())),
                "quick start is missing {command:?}"
            );
        }

        let patch = cmd.find_subcommand_mut("patch").unwrap();
        let patch_help = patch.render_help().ansi().to_string();
        for heading in ["Author:", "Apply:", "Inspect and edit:", "Options:"] {
            assert!(
                patch_help.contains(&format!(
                    "{}{heading}{}",
                    HEADING.render(),
                    HEADING.render_reset()
                )),
                "patch help is missing the styled heading {heading:?}"
            );
        }
    }

    #[test]
    fn series_commands_live_under_the_patch_subcommand() {
        for old in [
            ["skot", "new"],
            ["skot", "import"],
            ["skot", "refresh"],
            ["skot", "apply"],
            ["skot", "unapply"],
            ["skot", "push"],
            ["skot", "pop"],
            ["skot", "list"],
            ["skot", "remove"],
        ] {
            assert!(Cli::try_parse_from(old).is_err(), "{old:?} should be gone");
        }
        let cli = Cli::try_parse_from(["skot", "patch", "push"]).unwrap();
        assert!(matches!(cli.command, Command::Patch(PatchCommand::Push(_))));
        let cli = Cli::try_parse_from(["skot", "patch", "ls"]).unwrap();
        assert!(matches!(cli.command, Command::Patch(PatchCommand::List(_))));
        let cli = Cli::try_parse_from(["skot", "patch", "drop", "1"]).unwrap();
        assert!(matches!(cli.command, Command::Patch(PatchCommand::Drop(_))));
    }

    /// Every name in the tables exists in the tree, appears once across the tables,
    /// and the tree has no names besides the tables and clap's `help`.
    fn assert_matches_tables(groups: &[(&str, &[&str])], actual: &[String], page: &str) {
        let listed = listed_names(groups);
        let mut seen = std::collections::BTreeSet::new();
        for name in &listed {
            assert!(
                seen.insert(name.clone()),
                "{page}: `{name}` is listed twice"
            );
            assert!(
                actual.contains(name),
                "{page}: the table lists `{name}`, which is not a subcommand"
            );
        }
        let extras: Vec<&str> = actual
            .iter()
            .map(String::as_str)
            .filter(|name| !seen.contains(*name))
            .collect();
        assert_eq!(
            extras,
            ["help"],
            "{page}: commands outside the group tables must be exactly clap's help"
        );
    }

    fn listed_names(groups: &[(&str, &[&str])]) -> Vec<String> {
        groups
            .iter()
            .flat_map(|(_, names)| names.iter().map(|name| name.to_string()))
            .collect()
    }
}
