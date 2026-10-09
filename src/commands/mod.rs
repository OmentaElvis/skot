//! One module per subcommand.
//!
//! These orchestrate and present. They do not do git work, they do not parse
//! patches, and they do not read paths directly — all three live elsewhere, so that
//! `check`, `patch apply` and `patch push` cannot drift apart.

mod apply;
mod authoring;
mod build;
mod check;
mod clean;
mod drop;
mod hooks;
mod import;
mod init;
mod list;
mod new;
mod pop;
mod push;
mod rebase;
mod refresh;
mod run;
mod status;
mod sync;
#[cfg(test)]
mod testsupport;

use anyhow::Result;

use crate::cli::{Cli, Command, PatchCommand};
use crate::layout::Layout;

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
        Ok(Self {
            layout,
            cli: cli.clone(),
            yes: cli.yes,
        })
    }
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
            PatchCommand::Refresh(args) => refresh::run(&ctx, args),
            PatchCommand::Apply(args) => apply::run(&ctx, args),
            PatchCommand::Unapply => apply::unapply(&ctx),
            PatchCommand::Push(args) => push::run(&ctx, args),
            PatchCommand::Pop(args) => pop::run(&ctx, args),
            PatchCommand::List(args) => list::run(&ctx, args),
            PatchCommand::Drop(args) => drop::run(&ctx, args),
        },
        Command::Check(args) => check::run(&ctx, args),
        Command::Rebase(args) => rebase::run(&ctx, args),
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
