use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use skot::cli::{Cli, Command};
use skot::commands::{self, Exit};
use skot::hints;
use skot::ui;

fn main() -> ExitCode {
    let cli = Cli::parse();
    ui::set_json(cli.json);
    hints::set_enabled(!cli.no_hints);
    ui::install_logger(cli.json);

    // The one place a command's verdict becomes a process status. `Err` is a
    // malfunction and says so on stderr; `Exit` is a command that ran and answered.
    match run(cli) {
        Ok(Exit::Success) => ExitCode::SUCCESS,
        Ok(Exit::Failure) => ExitCode::FAILURE,
        Err(err) => {
            log::error!("{err}");
            for cause in err.chain().skip(1) {
                log::debug!("caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<Exit> {
    // `init` is the one command that must work before a project exists.
    if matches!(cli.command, Command::Init(_)) {
        let ctx = commands::Ctx::discover_or_init(&cli)?;
        return commands::run(ctx);
    }

    let ctx = commands::Ctx::discover(&cli)?;
    commands::run(ctx)
}
