//! Command-line surface. The single place subcommands are named.
//!
//! Global flags follow the conventions every developer already expects from `gh`,
//! `kubectl` and `ansible`: `--json` for machines, `--yes` to skip the prompt,
//! `--dir` instead of environment variables for pointing at a project.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::phase::Phase;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "skot",
    version,
    about = "Manage a patch series against an upstream repository and build the result.",
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
    /// List the series in order.
    List(ListArgs),

    /// Where everything is, how big it is, and whether a run was interrupted.
    Status(StatusArgs),

    /// Check whether the series applies to a ref, without building anything.
    Check(CheckArgs),

    /// Free disk, in levels. Dry run unless --yes.
    Clean(CleanArgs),

    /// List the hooks that are configured, in lifecycle order.
    Hooks,

    /// Run a `[[hook]]` entry by name.
    Run(RunArgs),

    /// Scaffold skot.toml, series and patches/, asking for what is missing.
    Init(InitArgs),

    // The working set. Every one of these is implemented; they are grouped here because they
    // are the commands people reach for in a loop, not because any of them is a stub.
    /// Mirror a remote and keep its refs current.
    Sync(RefArg),

    /// Apply the whole series to the sandbox.
    Apply(RefArg),

    /// Remove the whole series from the sandbox.
    Unapply,

    /// Apply the next patch in the series, or a named one.
    Push(PatchArg),

    /// Undo the last applied patch, or a named one.
    Pop(PatchArg),

    /// Start a new patch, with the series already applied.
    New(NewArgs),

    /// Re-export your edits as a patch file and register it.
    Refresh(RefreshArgs),

    /// Replay the series onto a different ref.
    Rebase(RefArg),

    /// Build: sandbox, apply, deps, then your hooks.
    Build(BuildArgs),
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

    /// Set a hook: NAME=COMMAND, where NAME is a key of [hooks]. Repeatable.
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

/// A patch name, for the commands that act on the stack rather than on a ref.
///
/// Deliberately not [`RefArg`]: `skot pop 0002-fix` takes a *patch*, and the ref it works
/// against is the one the sandbox is already on. Sharing one argument type between the two
/// would mean `pop main` silently meant "pop the patch called main", which is never what
/// anyone typing that meant.
#[derive(Debug, Clone, Default, Args)]
pub struct PatchArg {
    /// Patch in the series. Defaults to the one on top of the stack.
    #[arg(value_name = "PATCH")]
    pub patch: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct NewArgs {
    /// Short name for the patch. Filenames are derived from it.
    pub name: String,

    /// Base the new patch on this ref instead of the default.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct RefreshArgs {
    /// Patch to refresh. Defaults to the one currently being authored.
    pub name: Option<String>,

    /// Where to insert it in the series.
    #[arg(long, value_name = "INDEX")]
    pub insert_at: Option<usize>,

    /// Do not add it to the series.
    #[arg(long)]
    pub no_register: bool,
}

#[derive(Debug, Clone, Args)]
pub struct BuildArgs {
    /// Ref to build. Defaults to the configured default ref.
    #[arg(value_name = "REF")]
    pub ref_name: Option<String>,

    /// Start at this phase.
    #[arg(long, value_name = "PHASE")]
    pub from: Option<Phase>,

    /// Start at this phase and go to the end.
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
    }

    #[test]
    fn global_flags_are_accepted_after_the_subcommand() {
        // `gh`-style: flags belong anywhere, not only before the subcommand.
        let cli = Cli::try_parse_from(["skot", "list", "--json"]).unwrap();
        assert!(cli.json);
        let cli = Cli::try_parse_from(["skot", "--json", "list"]).unwrap();
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
        for text in [["skot", "check"], ["skot", "apply"], ["skot", "build"]] {
            assert!(Cli::try_parse_from(text).is_ok(), "{text:?} should parse");
        }
    }

    #[test]
    fn from_and_only_cannot_both_be_given() {
        let err = Cli::try_parse_from(["skot", "build", "--from", "deps", "--only", "build"]);
        assert!(err.is_err(), "these two flags contradict each other");
    }

    #[test]
    fn phases_parse_by_name() {
        let cli = Cli::try_parse_from(["skot", "build", "--from", "deps"]).unwrap();
        let Command::Build(args) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(args.from, Some(Phase::Deps));
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
            "deps=make deps",
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
        assert_eq!(args.hook, ["deps=make deps"]);
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
        assert!(Cli::try_parse_from(["skot", "new"]).is_err());
        assert!(Cli::try_parse_from(["skot", "new", "trim-widgets"]).is_ok());
    }
}
