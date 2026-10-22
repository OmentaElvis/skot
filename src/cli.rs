//! Command-line surface. The single place subcommands are named.
//!
//! Global flags follow the conventions every developer already expects from `gh`,
//! `kubectl` and `ansible`: `--json` for machines, `--yes` to skip the prompt,
//! `--dir` instead of environment variables for pointing at a project.

use std::path::PathBuf;

use clap::builder::styling::{Style, Styles};
use clap::builder::{BoolishValueParser, StyledStr};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::invocation::{skot, skot_args};
use crate::phase::Phase;

pub const ASCII_LOGO: &str = concat!(
    r#"
                              
  ▄▄▄▄▄              ▄▄▄▄▄▄▄  
 ██▀▀▀▀█▄           █▀▀██▀▀▀▀ 
 ▀██▄  ▄▀ ▄▄           ██     
   ▀██▄▄  ██ ▄█▀ ▄███▄ ██     
 ▄   ▀██▄ ████   ██ ██ ██     
 ▀██████▀▄██ ▀█▄▄▀███▀ ▀██▄   
            v"#,
    env!("CARGO_PKG_VERSION"),
    "\n",
    " Maintain your changes as a \n  patch series, not a fork."
);

const PROSE: &str = r#"
skot lets you maintain your changes to someone else's project as
a series of patches instead of a fork. Each patch stays small
enough to review, and you can reorder, disable or rebase any of
them when upstream moves.

Upstream can be a remote repository, a local folder, or any
other git source. You can import patches from a pull request, a
commit range or another checkout. You can also keep several
series side by side, one for each branch, tag or ref you
maintain, and switch between them as needed.

skot handles the mirror, the checkout and the series. Your
language, build system and artifact location stay yours: hooks
in skot.toml let you run your own commands at each stage of the
build, so skot works with any kind of project.
"#;

/// The long help page renders `long_about` instead of `about`, so it carries the
/// logo above the prose.
fn long_about() -> String {
    format!("{ASCII_LOGO}\n\n{PROSE}")
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "skot",
    version,
    about = ASCII_LOGO,
    long_about = long_about(),
    propagate_version = true,
    disable_help_subcommand = false
)]
pub struct Cli {
    /// Project directory. Defaults to searching upwards for skot.toml. Also read from
    /// the SKOT_DIR environment variable, which the flag beats.
    #[arg(long, global = true, value_name = "PATH", env = "SKOT_DIR")]
    pub dir: Option<PathBuf>,

    /// Series to work on for this run: one shot, the active one stays put. Also read
    /// from the SKOT_SERIES environment variable, which the flag beats.
    #[arg(long, global = true, value_name = "NAME", env = "SKOT_SERIES")]
    pub series: Option<String>,

    /// Contact the upstream even when the mirror already has the ref: the fetch phase
    /// of whatever command this run is, not only `sync`'s. Also read from the
    /// SKOT_REFRESH environment variable, which the flag beats.
    #[arg(
        long,
        global = true,
        env = "SKOT_REFRESH",
        value_parser = BoolishValueParser::new()
    )]
    pub refresh: bool,

    /// Never contact the network. The mirror already on disk, and local directories,
    /// are the whole answer; anything else is refused rather than fetched. Also read
    /// from the SKOT_OFFLINE environment variable, which the flag beats.
    #[arg(
        long,
        global = true,
        env = "SKOT_OFFLINE",
        value_parser = BoolishValueParser::new()
    )]
    pub offline: bool,

    /// Machine-readable output on stdout. Also read from the SKOT_JSON environment
    /// variable, which the flag beats.
    #[arg(
        long,
        global = true,
        env = "SKOT_JSON",
        value_parser = BoolishValueParser::new()
    )]
    pub json: bool,

    /// Skip the next-step hints on stderr. Also read from the SKOT_NO_HINTS
    /// environment variable, which the flag beats.
    #[arg(
        long,
        global = true,
        env = "SKOT_NO_HINTS",
        value_parser = BoolishValueParser::new()
    )]
    pub no_hints: bool,

    /// Do not ask: every question takes what Enter would give, a confirmation with
    /// nothing to give is answered yes, and any other question only a flag can answer
    /// is refused with that flag. Also read from the SKOT_YES environment variable,
    /// which the flag beats.
    #[arg(
        long,
        short = 'y',
        global = true,
        env = "SKOT_YES",
        value_parser = BoolishValueParser::new()
    )]
    pub yes: bool,

    /// More detail: `-v` debug, `-vv` trace. Repeat for more. Also read from the
    /// SKOT_VERBOSE environment variable as a number: `SKOT_VERBOSE=2` is `-vv`.
    #[arg(
        long,
        short = 'v',
        global = true,
        action = ArgAction::Count,
        env = "SKOT_VERBOSE"
    )]
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

    /// List, create, switch and copy series.
    ///
    /// A bare `skot series` lists, the way a bare `git branch` lists.
    Series(SeriesArgs),

    /// Check whether the series applies to a ref, without building anything.
    Check(CheckArgs),

    /// Replay the series onto a different ref.
    ///
    /// The series is rebuilt in full and put in place with a rename, so a failure
    /// leaves it untouched. Without `--into` the rebase replaces the series and the
    /// version it replaced is kept for `series undo`; with `--into` the rewritten
    /// series lands under a new name and this one is left alone.
    Rebase(RebaseArgs),

    /// Mirror a remote and keep its refs current.
    ///
    /// Without `--refresh` (a global flag) the mirror's own refs answer when they can:
    /// no connection, no failure because the network happens to be down.
    Sync(RefArg),

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

    /// Save your edits to the patch file.
    #[command(alias = "edit-save")]
    Save(SaveArgs),

    /// Open an existing patch for editing, in a worktree of its own.
    Edit(EditArgs),

    /// Apply the enabled set to the sandbox; choose it with enable and disable.
    Apply(RefArg),

    /// Remove the applied set from the sandbox.
    Unapply,

    /// Apply the next patch in the series, or a named one.
    Push(PatchArg),

    /// Undo the top patch, or a named one.
    Pop(PatchArg),

    /// List patches in order (alias: ls).
    #[command(visible_alias = "ls")]
    List(ListArgs),

    /// Print a patch: a preview on a terminal, the exact patch bytes on a pipe.
    Show(ShowArgs),

    /// Include a patch when applying (picker if none named).
    Enable(EnableArgs),

    /// Skip a patch when applying (picker if none named).
    Disable(DisableArgs),

    /// Move a patch to a different position in the series (alias: mv).
    ///
    /// With no arguments, the patch and its destination are picked from a list.
    #[command(visible_alias = "mv")]
    Move(MoveArgs),

    /// Take a patch out of the series: its line and its file (alias: rm).
    ///
    /// Asks before deleting; `--yes` says so in advance.
    #[command(visible_alias = "rm")]
    Drop(DropArgs),

    /// Copy a patch file between series, in either direction.
    ///
    /// `--into <series>` takes the patch from the active series to another;
    /// `--from <series>` brings one across into the active series. The patch keeps
    /// its name and lands at the end of the series, or wherever `--at` puts it. What
    /// it does on the target's base is `skot check`'s answer to give.
    Copy(PatchCopyArgs),
}

/// Series management, under `skot series`.
#[derive(Debug, Clone, Args)]
pub struct SeriesArgs {
    /// What to do. A bare `skot series` lists, the way a bare `git branch` lists.
    #[command(subcommand)]
    pub command: Option<SeriesCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum SeriesCommand {
    /// Create a series and switch to it.
    ///
    /// The word is `create` because `new` is already `patch new`: the two would both
    /// answer to a bare `skot new` in advice text, and an ambiguous remedy is worse
    /// than none.
    Create(SeriesCreateArgs),

    /// Switch to an existing series.
    Switch(SeriesSwitchArgs),

    /// Copy a series to a new name, leaving the active one alone.
    ///
    /// A snapshot: every patch file, the list and the base, under a new name, so an
    /// experiment can be split off or a series kept as it stands. The switch is a
    /// separate decision (`skot series switch <name>`).
    Copy(SeriesCopyArgs),

    /// Swap the active series with the version the last successful rebase replaced.
    ///
    /// A rebase keeps one version of the series it replaced, at `.skot/undo/<name>`.
    /// This trades the two, so a second undo puts back what the first one took away.
    /// The sandbox is rebuilt for whichever version ends up active.
    Undo(SeriesUndoArgs),

    /// Remove a series for good: its patches, its list and its base.
    ///
    /// Refuses the active series and any series holding unsaved work. Asks first;
    /// `--yes` says so in advance.
    Drop(SeriesDropArgs),

    /// Rename the active series, pointer, work areas and all.
    ///
    /// A series' sandbox tree is derived state and is rebuilt by the next apply; open
    /// work areas follow the rename, but only ones holding nothing their patch lacks.
    Rename(SeriesRenameArgs),
}

#[derive(Debug, Clone, Args)]
pub struct SeriesCreateArgs {
    /// Name for the new series.
    pub name: String,

    /// Seed the new series from this one instead of the active one.
    #[arg(long, value_name = "NAME")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct SeriesSwitchArgs {
    /// Series to switch to.
    pub name: String,
}

#[derive(Debug, Clone, Args)]
pub struct SeriesCopyArgs {
    /// Name for the copy.
    pub name: String,

    /// Series to copy from, instead of the active one.
    #[arg(long, value_name = "NAME")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct SeriesUndoArgs {
    /// Put the previous series back without asking.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Clone, Args)]
pub struct SeriesDropArgs {
    /// Series to remove.
    pub name: String,
}

#[derive(Debug, Clone, Args)]
pub struct SeriesRenameArgs {
    /// New name for the active series.
    pub name: String,
}

/// `skot patch copy <patch> --into <series>`, or `--from <series>` the other way.
#[derive(Debug, Clone, Args)]
pub struct PatchCopyArgs {
    /// Patch to copy, named as it is in the source series.
    pub patch: String,

    /// Series to copy the patch from; the patch lands in the active series.
    #[arg(
        long,
        value_name = "NAME",
        conflicts_with = "into",
        required_unless_present = "into"
    )]
    pub from: Option<String>,

    /// Series to copy the patch into; the patch comes from the active series.
    #[arg(long, value_name = "NAME")]
    pub into: Option<String>,

    /// Where to put it: a position in the series, counting from 1. The end, by default.
    #[arg(long, value_name = "N")]
    pub at: Option<usize>,
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
    ("Series", &["series"]),
    ("Upstream", &["check", "rebase", "sync"]),
    ("Housekeeping", &["clean", "hooks", "run"]),
];

/// `skot patch --help`'s groups, same shape as [`ROOT_GROUPS`].
const PATCH_GROUPS: &[(&str, &[&str])] = &[
    ("Author", &["new", "edit", "save", "import"]),
    ("Arrange", &["enable", "disable", "move", "drop", "copy"]),
    ("Apply", &["apply", "unapply", "push", "pop"]),
    ("Inspect", &["list", "show"]),
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
    // A gap, so `help` does not read as the last row of the last group: it is clap's
    // escape hatch, not a command of ours.
    out.push_str("\n");
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
        (
            skot_args("new", "my-fix"),
            "work in the worktree skot opens, then:",
        ),
        (skot("save"), "save your changes"),
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
pub struct ShowArgs {
    /// Patches to print. Without names: the one open work area.
    pub patches: Vec<String>,

    /// Only the header and the per-file counts, not the diff itself.
    #[arg(long)]
    pub stat: bool,
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
    #[arg(long, short = 'a', conflicts_with = "first_only")]
    pub all: bool,

    /// Stop at the first failing patch. The default.
    ///
    /// Deliberately not `default_value_t = true`: that makes clap count the flag as
    /// present, so `conflicts_with` would reject `--all` too.
    #[arg(long)]
    pub first_only: bool,
}

/// How much `clean` removes. Ordered, so that each level is a superset of the one
/// before it, which the `clean` tests assert. Work areas with unsaved changes are not part
/// of any level: they are an axis of their own (`--sessions`), never a side effect of
/// tidying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum CleanLevel {
    /// The sandbox. Next build reinstalls dependencies.
    Sandbox,
    /// Also everything skot generated under .skot/out.
    Out,
    /// Also the bare mirror. Next run re-clones.
    Mirror,
    /// All of it. Next run re-clones and reinstalls.
    All,
}

#[derive(Debug, Clone, Args)]
pub struct CleanArgs {
    /// How much to remove.
    #[arg(value_enum, default_value_t = CleanLevel::Sandbox)]
    pub level: CleanLevel,

    /// Also drop work areas that hold unsaved changes.
    ///
    /// Work areas are never in a default plan: their directories hold edits that exist
    /// nowhere else until `save` writes them into a patch. `mirror` and `all` refuse
    /// while any remain, and a work area that matches its patch is already gone by then.
    #[arg(long)]
    pub sessions: bool,
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

/// `skot rebase [REF] [--into NAME]`.
#[derive(Debug, Clone, Args)]
pub struct RebaseArgs {
    /// Ref: a tag, a branch, or a commit.
    #[arg(value_name = "REF")]
    pub ref_name: Option<String>,

    /// Put the rebased series under this name instead of replacing the active one.
    ///
    /// The series named by `--series` stays exactly as it is: the rewritten series is
    /// built and installed as a new one, so a rebase can be tried without risking the
    /// series it started from. The new series is not switched to; `series switch` does that.
    #[arg(long, value_name = "NAME")]
    pub into: Option<String>,
}

/// A patch name, for the commands that act on the stack rather than on a ref.
///
/// Deliberately not [`RefArg`]: `skot patch pop fix` takes a *patch*, and the ref it
/// works against is the one the sandbox is already on. Sharing one argument type between
/// the two would mean `pop main` silently meant "pop the patch called main", which is never
/// what anyone typing that meant.
#[derive(Debug, Clone, Default, Args)]
pub struct PatchArg {
    /// Patch in the series: its name, a unique prefix such as `fix`, or its
    /// 1-based position. Defaults to the one on top of the stack.
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

    /// Import a pull request: a number, which is read against the forge's own ref
    /// namespace from the domain of the repository being asked, or the full ref name
    /// as written (for a forge skot does not know).
    #[arg(long, value_name = "N|REF", conflicts_with = "range")]
    pub pr: Option<String>,

    /// Fetch the range from this repository (a URL or a local clone). Refs are looked up
    /// there first, so a range named against that repository resolves as written.
    #[arg(long, value_name = "URL")]
    pub repo: Option<String>,

    /// Name for the new patch. Defaults to the subject of what is being imported: the
    /// commit itself for a one-commit import, `PR N` for a pull request, or the range
    /// as written.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Insert at this position: a 1-based index or an existing patch name. Every other
    /// patch keeps its name and its place. Defaults to appending.
    #[arg(long, value_name = "PATCH|N", conflicts_with = "no_register")]
    pub at: Option<String>,

    /// Write the patch file without adding it to the series.
    #[arg(long)]
    pub no_register: bool,

    /// Extract the range's diff the other way round: the patch undoes the change the
    /// range made, the way `git revert` would, but stays an ordinary patch in the
    /// series. How you put back a feature upstream deleted.
    #[arg(long, short = 'r')]
    pub reverse: bool,
}

#[derive(Debug, Clone, Args)]
pub struct NewArgs {
    /// Short name for the patch. Filenames are derived from it.
    pub name: String,

    /// Base the new patch on this ref instead of the default.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Insert at this position: a 1-based index or an existing patch name. Every other
    /// patch keeps its name and its place. Defaults to appending.
    #[arg(long, value_name = "PATCH|N")]
    pub at: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct SaveArgs {
    /// Patch to save into. Defaults to the work area with unsaved changes.
    pub name: Option<String>,

    /// Where to insert it in the series.
    #[arg(long, value_name = "INDEX")]
    pub insert_at: Option<usize>,

    /// Do not add it to the series.
    #[arg(long)]
    pub no_register: bool,

    /// Save even though the patches below changed since the work area opened, against the
    /// base it recorded rather than the one the series now produces.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Args)]
pub struct EditArgs {
    /// Patch to open, as the series lists it.
    #[arg(value_name = "PATCH")]
    pub patch: String,

    /// Open on this ref instead of the recorded one.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Discard the unsaved changes in the work area and start over from the patch.
    ///
    /// Deliberately not `--yes`: the global `-y` skips confirmations, while this one
    /// throws work away, and one flag should not mean both.
    #[arg(long)]
    pub discard: bool,
}

#[derive(Debug, Clone, Args)]
pub struct DropArgs {
    /// Patch to drop: its name, a unique prefix such as `fix`, or its
    /// 1-based position. Refused while it is applied; `skot patch pop` takes it off first.
    #[arg(value_name = "PATCH")]
    pub patch: String,
}

#[derive(Debug, Clone, Args)]
pub struct EnableArgs {
    /// Patches to put back in the applying set: names, unique prefixes such as
    /// `fix`, or 1-based positions among the disabled. Left out, picked from a
    /// list of the disabled ones; `--all` takes every disabled patch.
    #[arg(value_name = "PATCH")]
    pub patches: Vec<String>,
    /// Every disabled patch.
    #[arg(long, conflicts_with = "patches")]
    pub all: bool,
}

#[derive(Debug, Clone, Args)]
pub struct DisableArgs {
    /// Patches to skip from now on: names, unique prefixes such as `fix`,
    /// or 1-based positions among the enabled. Left out, picked from a list of the
    /// enabled, not-yet-applied ones; `--all` takes every one that can go. The line is
    /// marked `#!` in the series, so the choice is trackable and lasts.
    #[arg(value_name = "PATCH")]
    pub patches: Vec<String>,
    /// Every enabled patch the sandbox is not holding.
    #[arg(long, conflicts_with = "patches")]
    pub all: bool,
}

#[derive(Debug, Clone, Args)]
pub struct MoveArgs {
    /// Patch to move: its name, a unique prefix such as `fix`, or its
    /// 1-based position. Left out, it is picked from a list; a patch that is applied
    /// cannot move.
    #[arg(value_name = "PATCH")]
    pub patch: Option<String>,

    /// Where the patch should end up: a 1-based position, or an existing patch to land
    /// before (by name or prefix). Left out, the new position is asked for.
    #[arg(value_name = "PATCH|N", conflicts_with = "to")]
    pub dest: Option<String>,

    /// Same as the destination argument, spelled as a flag.
    #[arg(long, value_name = "PATCH|N", hide = true)]
    pub to: Option<String>,
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
        let cli = Cli::try_parse_from(["skot", "patch", "list", "--series", "exp"]).unwrap();
        assert_eq!(cli.series.as_deref(), Some("exp"));
        let cli = Cli::try_parse_from(["skot", "--series", "exp", "status"]).unwrap();
        assert_eq!(cli.series.as_deref(), Some("exp"));
        let cli = Cli::try_parse_from(["skot", "build", "--refresh"]).unwrap();
        assert!(cli.refresh);
        let cli = Cli::try_parse_from(["skot", "--refresh", "sync"]).unwrap();
        assert!(cli.refresh);
        let cli = Cli::try_parse_from(["skot", "sync", "--offline"]).unwrap();
        assert!(cli.offline);
        let cli = Cli::try_parse_from(["skot", "--offline", "patch", "list"]).unwrap();
        assert!(cli.offline);
    }

    #[test]
    fn the_series_command_creates_switches_and_copies_and_a_bare_one_lists() {
        // A bare `skot series` is the list, the way a bare `git branch` is. There is
        // no `series list` or `series new`: those names belong to `skot patch`, and
        // skot refuses two commands that share a name.
        let cli = Cli::try_parse_from(["skot", "series"]).unwrap();
        let Command::Series(args) = cli.command else {
            panic!("expected series");
        };
        assert!(args.command.is_none(), "bare `series` is the list");

        let cli = Cli::try_parse_from(["skot", "series", "create", "exp"]).unwrap();
        let Command::Series(args) = cli.command else {
            panic!("expected series");
        };
        let Some(SeriesCommand::Create(create)) = args.command else {
            panic!("expected series create");
        };
        assert_eq!(create.name, "exp");

        let cli = Cli::try_parse_from(["skot", "series", "switch", "exp"]).unwrap();
        let Command::Series(args) = cli.command else {
            panic!("expected series");
        };
        let Some(SeriesCommand::Switch(switch)) = args.command else {
            panic!("expected series switch");
        };
        assert_eq!(switch.name, "exp");

        let cli = Cli::try_parse_from(["skot", "series", "copy", "checkpoint"]).unwrap();
        let Command::Series(args) = cli.command else {
            panic!("expected series");
        };
        let Some(SeriesCommand::Copy(copy)) = args.command else {
            panic!("expected series copy");
        };
        assert_eq!(copy.name, "checkpoint");

        for rejected in [
            ["skot", "series", "list"].as_slice(),
            ["skot", "series", "new", "exp"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(rejected).is_err(),
                "{rejected:?} is a patch-level spelling, not a series one"
            );
        }

        for missing in [
            ["skot", "series", "create"],
            ["skot", "series", "switch"],
            ["skot", "series", "copy"],
        ] {
            assert!(
                Cli::try_parse_from(missing).is_err(),
                "{missing:?} needs a name"
            );
        }
    }

    #[test]
    fn clean_defaults_to_the_sandbox_level() {
        let cli = Cli::try_parse_from(["skot", "clean"]).unwrap();
        let Command::Clean(args) = cli.command else {
            panic!("expected clean")
        };
        assert_eq!(args.level, CleanLevel::Sandbox);
        assert!(!args.sessions, "sessions are never in the default plan");
    }

    #[test]
    fn clean_levels_parse_by_name() {
        for (text, expected) in [
            ("sandbox", CleanLevel::Sandbox),
            ("out", CleanLevel::Out),
            ("mirror", CleanLevel::Mirror),
            ("all", CleanLevel::All),
        ] {
            let cli = Cli::try_parse_from(["skot", "clean", text]).unwrap();
            let Command::Clean(args) = cli.command else {
                panic!("expected clean")
            };
            assert_eq!(args.level, expected);
        }
        assert!(
            Cli::try_parse_from(["skot", "clean", "--sessions"]).is_ok(),
            "sessions are an axis flag, not a level"
        );
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
        for level in ["sandbox", "out", "mirror", "all"] {
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
        assert_eq!(args.pr.as_deref(), Some("42"));
        assert_eq!(args.name.as_deref(), Some("fix"));

        let cli = Cli::try_parse_from(["skot", "patch", "import", "--pr", "refs/changes/12/345/1"])
            .unwrap();
        let Command::Patch(PatchCommand::Import(args)) = cli.command else {
            panic!("expected import")
        };
        assert_eq!(args.pr.as_deref(), Some("refs/changes/12/345/1"));

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

        // Each listed command appears exactly once, as one styled row. Matched on a whole
        // word because a substring count would see a longer row twice.
        for name in listed_names(ROOT_GROUPS) {
            assert_eq!(
                root_help
                    .lines()
                    .filter(|l| row_name(l).as_deref() == Some(name.as_str()))
                    .count(),
                1,
                "`{name}` should be rendered as a row exactly once"
            );
        }
        assert_eq!(
            root_help
                .lines()
                .filter(|l| row_name(l).as_deref() == Some("help"))
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
                    .lines()
                    .filter(|l| row_name(l).as_deref() == Some(name.as_str()))
                    .count(),
                1,
                "`{name}` should be rendered as a row exactly once"
            );
        }
    }

    /// The command name at the start of a rendered help row, if the line is one.
    ///
    /// A row is shaped the way `push_row` writes it: the name, then at least two
    /// spaces of name column, then the description. Prose reads `name words` with a
    /// single space, so no sentence in an about or a hint can take a command's place.
    fn row_name(line: &str) -> Option<String> {
        let stripped = strip_ansi(line);
        let rest = stripped.trim_start();
        let name_end = rest.find(char::is_whitespace)?;
        let name = &rest[..name_end];
        let after_name = &rest[name_end..];
        let gap = after_name.len() - after_name.trim_start().len();
        let description = after_name.trim_start();
        (gap >= 2
            && !description.is_empty()
            && name.chars().next().is_some_and(|c| c.is_ascii_lowercase()))
        .then(|| name.to_owned())
    }

    /// Remove ANSI escapes so a row's words can be compared plainly.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
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
        for heading in ["Author:", "Arrange:", "Apply:", "Inspect:", "Options:"] {
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
        let cli = Cli::try_parse_from(["skot", "patch", "move", "2", "--to", "1"]).unwrap();
        assert!(matches!(cli.command, Command::Patch(PatchCommand::Move(_))));
        let cli = Cli::try_parse_from(["skot", "patch", "mv", "2", "--to", "1"]).unwrap();
        assert!(matches!(cli.command, Command::Patch(PatchCommand::Move(_))));
        let cli = Cli::try_parse_from(["skot", "patch", "rm", "1"]).unwrap();
        assert!(matches!(cli.command, Command::Patch(PatchCommand::Drop(_))));
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "1"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Patch(PatchCommand::Enable(_))
        ));
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "1"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Patch(PatchCommand::Disable(_))
        ));
        // Both ends of the batch interface: several patches in one run, or `--all`.
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "fix", "foo"]).unwrap();
        let Command::Patch(PatchCommand::Enable(args)) = cli.command else {
            panic!("expected enable");
        };
        assert_eq!(args.patches, ["fix", "foo"]);
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "--all"]).unwrap();
        let Command::Patch(PatchCommand::Disable(args)) = cli.command else {
            panic!("expected disable");
        };
        assert!(args.all);
        assert!(args.patches.is_empty());
        assert!(
            Cli::try_parse_from(["skot", "patch", "enable", "fix", "--all"]).is_err(),
            "--all and names are two different questions"
        );
        // The reversed extraction of `import`, by either spelling.
        for flag in ["--reverse", "-r"] {
            let cli = Cli::try_parse_from(["skot", "patch", "import", "main..feat", flag]).unwrap();
            let Command::Patch(PatchCommand::Import(args)) = cli.command else {
                panic!("expected import");
            };
            assert!(args.reverse, "{flag} should set reverse");
        }
        let cli = Cli::try_parse_from(["skot", "patch", "import", "main..feat"]).unwrap();
        let Command::Patch(PatchCommand::Import(args)) = cli.command else {
            panic!("expected import");
        };
        assert!(!args.reverse, "reversed is the opt-in");
        // `save` is the command; the old spelling still parses, as a hidden alias.
        let cli = Cli::try_parse_from(["skot", "patch", "save"]).unwrap();
        assert!(matches!(cli.command, Command::Patch(PatchCommand::Save(_))));
        let cli = Cli::try_parse_from(["skot", "patch", "edit-save"]).unwrap();
        assert!(matches!(cli.command, Command::Patch(PatchCommand::Save(_))));
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
