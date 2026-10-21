//! One module per subcommand.
//!
//! These orchestrate and present. They do not do git work, they do not parse
//! patches, and they do not read paths directly — all three live elsewhere, so that
//! `check`, `patch apply` and `patch push` cannot drift apart.

mod apply;
pub(crate) mod authoring;
mod build;
mod check;
mod clean;
mod copy;
mod disable;
mod drop;
mod edit;
mod edit_save;
mod enable;
mod hooks;
mod import;
mod init;
mod list;
mod r#move;
mod new;
mod pop;
mod push;
mod rebase;
mod run;
mod series;
mod show;
mod status;
pub(crate) mod sync;
#[cfg(test)]
pub(crate) mod testsupport;

use anyhow::{Result, bail};

use crate::cli::{Cli, Command, PatchCommand, SeriesCommand};
use crate::layout::{DEFAULT_SERIES, Layout};
use crate::state::State;

/// Everything a command needs, resolved once.
pub struct Ctx {
    pub layout: Layout,
    pub cli: Cli,
    pub yes: bool,
}

impl Ctx {
    /// Resolve the project layout, honouring `--dir`.
    pub fn discover(cli: &Cli) -> Result<Self> {
        let layout = match &cli.dir {
            Some(dir) => {
                let start = crate::layout::absolute(dir);
                if start.join(crate::layout::CONFIG_FILE).is_file() {
                    Layout::new(start)
                } else {
                    Layout::discover(&start)?
                }
            }
            None => Layout::discover(&std::env::current_dir()?)?,
        };
        let layout = series_for(cli, layout)?;
        Ok(Self {
            layout,
            cli: cli.clone(),
            yes: cli.yes,
        })
    }

    /// Load `skot.toml`. A missing config is an error, except for `init`.
    pub fn config(&self) -> Result<crate::config::Config> {
        self.layout.config()
    }

    /// Series and config together, which is what most commands want.
    pub fn project(&self) -> Result<(crate::config::Config, crate::series::Series)> {
        self.layout.project()
    }

    /// Who is there to answer this run's questions.
    ///
    /// Built from the invocation itself, once per question: `--json` and `--yes`
    /// both mean nobody is waiting at the prompt, and every prompt in this crate
    /// goes through the answer it hands back rather than probing the terminal again.
    pub fn asker(&self) -> crate::prompt::Asker {
        crate::prompt::Asker::for_run(self.cli.json, self.yes)
    }

    /// Take the sandbox lock for commands that mutate it.
    pub fn lock(&self) -> Result<crate::layout::SandboxLock> {
        self.layout.lock()
    }

    /// Resolve a layout for commands that run before a project exists.
    ///
    /// `init` is the only command that may be run in a directory with no
    /// `skot.toml`, so it uses the directory it was pointed at rather than searching
    /// upwards for something that is not there.
    pub fn discover_or_init(cli: &Cli) -> Result<Self> {
        let layout = match &cli.dir {
            Some(dir) => crate::layout::Layout::new(crate::layout::absolute(dir)),
            None => crate::layout::Layout::new(std::env::current_dir()?),
        };
        let layout = series_for(cli, layout)?;
        Ok(Self {
            layout,
            cli: cli.clone(),
            yes: cli.yes,
        })
    }
}

/// The series this invocation works on, and the sandbox scope it describes.
///
/// What names it, in order: `--series` (with `SKOT_SERIES` folded into the same flag
/// by clap), the pointer in `.skot/state.json`, then `series = "<name>"` in
/// skot.toml, which `skot init` writes so a fresh checkout is deterministic. With
/// nothing naming it the directories decide: the one series there is wins, none means
/// [`DEFAULT_SERIES`], and several mean the user has to say which line of work they
/// are on, rather than skot guessing.
///
/// The sandbox scope comes from the config, so the paths every later read takes are
/// settled here, once, rather than per command.
///
/// A resolved name does not have to exist on disk for every command: `init` has not
/// created the project yet and names its own series, and `series create` is about to
/// create the series it names. `series list` and `series switch` never read the current
/// series' patches either, so for them a name is bookkeeping, and a lost pointer must
/// not be a wall they cannot open to fix it.
fn series_for(cli: &Cli, layout: Layout) -> Result<Layout> {
    // No config means a project `init` is about to write, and that config declares the
    // default scope, so the state init records has to be stamped with the same one.
    let per_series = layout
        .config()
        .map(|config| config.sandbox.per_series)
        .unwrap_or_else(|_| crate::config::Sandbox::default().per_series);
    let layout = layout.with_per_series_sandbox(per_series);

    // The flag (SKOT_SERIES lands here too), the pointer, then the project default.
    let named = match &cli.series {
        // `check --series all` names every series rather than one; the answer is
        // computed there, and `all` is refused as a series name everywhere else.
        Some(name) if !(name == "all" && matches!(cli.command, Command::Check(_))) => {
            Some(name.clone())
        }
        _ => State::load(&layout)
            .ok()
            .and_then(|state| state.active_series().map(str::to_owned))
            .or_else(|| layout.config().ok().and_then(|config| config.series)),
    };

    // Whether this command reads the current series' patches, and whether it is about
    // to create that series itself.
    let (needs_current, creates_current) = match &cli.command {
        Command::Init(_) => (false, true),
        Command::Series(args) => match &args.command {
            None | Some(SeriesCommand::Switch(_)) | Some(SeriesCommand::Drop(_)) => (false, false),
            Some(SeriesCommand::Create(_)) => (true, true),
            Some(SeriesCommand::Copy(_))
            | Some(SeriesCommand::Rename(_))
            | Some(SeriesCommand::Undo(_)) => (true, false),
        },
        _ => (true, false),
    };

    let name = match named {
        Some(name) => name,
        None => {
            let existing = layout.series_names();
            match existing.len() {
                0 => DEFAULT_SERIES.to_owned(),
                1 => existing[0].clone(),
                _ if needs_current => {
                    let listed = existing
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "several series exist and nothing names the active one: {listed}.\n\
                         Point this run at one with `--series <name>` (or SKOT_SERIES=<name>), \
                         give the project a default with `series = \"<name>\"` in {}, or record \
                         the active series with `{}`.",
                        layout.config_file().display(),
                        crate::invocation::skot_args("series", "switch <name>")
                    );
                }
                // For a command that does not read the series, any valid name will do
                // as bookkeeping; the default is the one that reads least surprising.
                _ => DEFAULT_SERIES.to_owned(),
            }
        }
    };
    let layout = layout.with_series(&name)?;
    // `--refresh` and `--offline` ride on the layout too: they describe this run, not
    // one command's arguments, and the fetch path is deep enough below here that
    // threading a flag down to it would touch every command for one pair of booleans.
    let layout = layout.with_refresh(cli.refresh).with_offline(cli.offline);
    if needs_current && !creates_current && !layout.patches_dir().is_dir() {
        bail!(
            "the series `{name}` does not exist in {}.\n\
             `{}` lists what exists; `{}` creates one.",
            layout.series_root().display(),
            crate::invocation::skot("series"),
            crate::invocation::skot_args("series", "create <name>")
        );
    }
    Ok(layout)
}

/// What a command hands back when it is not an error: it ran, and here is the answer.
///
/// This is the only type that knows a command can fail *without* anything having gone
/// wrong — `check` on a patch that does not apply, `list` with a patch missing from disk,
/// `rebase` onto a base that rejects the series. Those are verdicts, not malfunctions, so
/// they are not `Err`, and they are not `i32` either: a code is a process-boundary detail
/// and belongs to `main`, which is the one place that turns this into an exit status.
/// Nothing below [`run`] ever sees a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did what was asked.
    Success,
    /// The command ran correctly and the answer is negative.
    Failure,
}

/// Dispatch.
pub fn run(ctx: Ctx) -> Result<Exit> {
    match &ctx.cli.command {
        Command::Init(args) => init::run(&ctx, args),
        Command::Build(args) => build::run(&ctx, args),
        Command::Status(args) => status::run(&ctx, args),
        Command::Patch(patch) => match patch {
            PatchCommand::New(args) => new::run(&ctx, args),
            PatchCommand::Import(args) => import::run(&ctx, args),
            PatchCommand::Save(args) => edit_save::run(&ctx, args),
            PatchCommand::Edit(args) => edit::run(&ctx, args),
            PatchCommand::Apply(args) => apply::run(&ctx, args),
            PatchCommand::Unapply => apply::unapply(&ctx),
            PatchCommand::Push(args) => push::run(&ctx, args),
            PatchCommand::Pop(args) => pop::run(&ctx, args),
            PatchCommand::List(args) => list::run(&ctx, args),
            PatchCommand::Show(args) => show::run(&ctx, args),
            PatchCommand::Enable(args) => enable::run(&ctx, args),
            PatchCommand::Disable(args) => disable::run(&ctx, args),
            PatchCommand::Move(args) => r#move::run(&ctx, args),
            PatchCommand::Drop(args) => drop::run(&ctx, args),
            PatchCommand::Copy(args) => copy::run(&ctx, args),
        },
        Command::Check(args) => check::run(&ctx, args),
        Command::Rebase(args) => rebase::run(&ctx, args),
        Command::Series(args) => series::run(&ctx, args),
        Command::Sync(args) => sync::run(&ctx, args),
        Command::Clean(args) => clean::run(&ctx, args),
        Command::Hooks => hooks::run(&ctx),
        Command::Run(args) => run::run(&ctx, args),
    }
}

/// Print a JSON document on stdout, as the whole output of a command.
///
/// Writes, and nothing else. The exit status is the calling command's verdict to state —
/// `check --json` returns [`Exit::Failure`] for the same report that returns `Exit::Success`
/// when it applies — which is why this does not hand one back.
pub fn emit_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Layout;
    use clap::Parser as _;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn with_series_dirs(layout: &Layout, names: &[&str]) {
        for name in names {
            std::fs::create_dir_all(layout.series_root().join(name)).unwrap();
        }
    }

    #[test]
    fn a_lost_pointer_with_several_series_refuses_to_guess() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        with_series_dirs(&layout, &["alpha", "exp", "work"]);

        let err = series_for(&cli(&["skot", "build"]), layout).unwrap_err();
        let text = format!("{err:#}");
        for name in ["`alpha`", "`exp`", "`work`", "--series", "SKOT_SERIES"] {
            assert!(text.contains(name), "{name} missing from: {text}");
        }
    }

    #[test]
    fn the_one_series_that_exists_is_the_one_that_wins() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        std::fs::remove_dir_all(layout.patches_dir()).unwrap();
        with_series_dirs(&layout, &["alpha"]);

        let resolved = series_for(&cli(&["skot", "build"]), layout).unwrap();
        assert_eq!(resolved.series(), "alpha");
    }

    #[test]
    fn refresh_and_offline_are_run_flags_and_land_on_the_layout() {
        // The flags describe the run, so `series_for` folds them the way it folds the
        // series: once here, and every fetch below reads them off the layout.
        let (_tmp, layout) = crate::commands::testsupport::project();

        let resolved = series_for(
            &cli(&["skot", "--refresh", "--offline", "build"]),
            layout.clone(),
        )
        .unwrap();
        assert!(resolved.refresh());
        assert!(resolved.offline());

        let resolved = series_for(&cli(&["skot", "build", "--refresh"]), layout).unwrap();
        assert!(resolved.refresh(), "after the subcommand counts too");
        assert!(
            !resolved.offline(),
            "and nothing inherits a flag it was not given"
        );
    }

    #[test]
    fn nothing_at_all_still_means_the_default_series() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());

        let resolved = series_for(&cli(&["skot", "init"]), layout).unwrap();
        assert_eq!(resolved.series(), DEFAULT_SERIES);
        // `init` stamps the state it writes with this answer, so a project with no
        // config yet has to resolve to the scope that config will declare.
        assert!(resolved.per_series_sandbox());
    }

    #[test]
    fn the_pointer_beats_the_project_default() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        with_series_dirs(&layout, &["alpha", "stable"]);
        std::fs::write(
            layout.config_file(),
            "series = \"stable\"\n[upstream]\nurl = \"unused\"\n",
        )
        .unwrap();
        State::load(&layout)
            .unwrap()
            .set_active_series(&layout, "alpha")
            .unwrap();

        let resolved = series_for(&cli(&["skot", "build"]), layout).unwrap();
        assert_eq!(resolved.series(), "alpha");
    }

    #[test]
    fn the_project_default_fills_in_where_the_pointer_is_gone() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        with_series_dirs(&layout, &["stable", "exp"]);
        std::fs::write(
            layout.config_file(),
            "series = \"stable\"\n[upstream]\nurl = \"unused\"\n",
        )
        .unwrap();

        let resolved = series_for(&cli(&["skot", "build"]), layout).unwrap();
        assert_eq!(resolved.series(), "stable");
    }

    #[test]
    fn the_flag_beats_the_pointer_the_config_and_the_directories() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        with_series_dirs(&layout, &["alpha", "stable", "exp"]);
        std::fs::write(
            layout.config_file(),
            "series = \"stable\"\n[upstream]\nurl = \"unused\"\n",
        )
        .unwrap();
        State::load(&layout)
            .unwrap()
            .set_active_series(&layout, "alpha")
            .unwrap();

        let resolved = series_for(&cli(&["skot", "--series", "exp", "build"]), layout).unwrap();
        assert_eq!(resolved.series(), "exp");
    }

    #[test]
    fn listing_and_switching_survive_a_lost_pointer() {
        // They never read the current series' patches, so the wall has to come down
        // for them: `series switch` is how a lost pointer gets recorded again.
        let (_tmp, layout) = crate::commands::testsupport::project();
        with_series_dirs(&layout, &["alpha", "exp"]);

        series_for(&cli(&["skot", "series"]), layout.clone()).unwrap();
        series_for(&cli(&["skot", "series", "switch", "exp"]), layout.clone()).unwrap();
        series_for(&cli(&["skot", "init"]), layout).unwrap();
    }

    #[test]
    fn commands_that_read_the_series_still_demand_it_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        std::fs::write(layout.config_file(), "[upstream]\nurl = \"unused\"\n").unwrap();

        let err = series_for(&cli(&["skot", "build"]), layout.clone()).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("does not exist"), "got {text}");

        // Creating is the exemption: the series it is about to make is not there yet.
        series_for(&cli(&["skot", "series", "create", "exp"]), layout).unwrap();
    }
}
