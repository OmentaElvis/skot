//  ▄▄▄▄▄              ▄▄▄▄▄▄▄
// ██▀▀▀▀█▄           █▀▀██▀▀▀▀
// ▀██▄  ▄▀ ▄▄           ██
//   ▀██▄▄  ██ ▄█▀ ▄███▄ ██
// ▄   ▀██▄ ████   ██ ██ ██
// ▀██████▀▄██ ▀█▄▄▀███▀ ▀██▄
//       Happy hacking!

use std::process::ExitCode;

use anyhow::Result;
use clap::FromArgMatches;

use skot::cli::{Cli, Command, build_cli};
use skot::commands::{self, Exit};
use skot::hints;
use skot::prompt::PromptError;
use skot::ui;

fn main() -> ExitCode {
    // One tree for parsing and for help: `--help` is what `build_cli` dresses up.
    let matches = build_cli().get_matches();
    // Which subcommand this run took: the usage line of a question nobody answered
    // starts at the command that asked it, not at `skot`.
    let path = command_path(&matches);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    ui::set_json(cli.json);
    hints::set_enabled(!cli.no_hints);
    ui::install_logger(cli.json, cli.verbose);

    // The one place a command's verdict becomes a process status. `Err` is a
    // malfunction and says so on stderr; `Exit` is a command that ran and answered.
    // Under `--json` the verdict goes to stdout as one document either way: the
    // logger is off (stdout is carrying a machine's output), so a failure that only
    // logged would vanish, and an agent would learn nothing but the exit code.
    match run(&cli) {
        Ok(Exit::Success) => ExitCode::SUCCESS,
        Ok(Exit::Failure) => ExitCode::FAILURE,
        Err(err) => {
            // A question with nobody to answer it is not a malfunction but a usage
            // error: clap prints it with the command's usage line and exits 2, the
            // way it refuses a missing flag.
            let no_answer =
                err.chain()
                    .find_map(|cause| match cause.downcast_ref::<PromptError>() {
                        Some(PromptError::NoAnswer { question, instead }) => {
                            Some((question, instead))
                        }
                        _ => None,
                    });
            if let Some((question, instead)) = no_answer {
                if cli.json {
                    return json_failure(
                        &format!("cannot ask {question} here; answer it with: {instead}"),
                        2,
                    );
                }
                return usage_error(&path, question, instead);
            }
            if cli.json {
                return json_failure(&format!("{err:?}"), 1);
            }
            log::error!("Error: {:?}", err);
            for cause in err.chain().skip(1) {
                log::debug!("caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

/// A failure under `--json`: one document on stdout, and the exit code the failure
/// would have had in human mode (1 for a malfunction, 2 for a usage error).
///
/// The message is the same text a person would have seen, causes included: an agent
/// is owed the advice, not just the verdict.
fn json_failure(message: &str, code: u8) -> ExitCode {
    use std::io::Write as _;

    let doc = serde_json::json!({ "ok": false, "error": message });
    // Best effort: a stdout that cannot take the document cannot be reported to
    // either, and the exit code is the answer that survives.
    let _ = writeln!(std::io::stdout(), "{doc}");
    ExitCode::from(code)
}

/// A question nobody could answer, printed the way clap refuses a missing flag: the
/// message, the subcommand's usage line, and exit code 2.
fn usage_error(path: &[String], question: &str, instead: &str) -> ExitCode {
    // The same tree the run was parsed from, walked to the command that asked, so
    // the usage line is that command's rather than the root's.
    let mut cmd = build_cli();
    for name in path {
        let Some(sub) = cmd.find_subcommand(name).cloned() else {
            break;
        };
        cmd = sub;
    }
    cmd = cmd.bin_name(if path.is_empty() {
        "skot".to_owned()
    } else {
        format!("skot {}", path.join(" "))
    });
    let err = cmd.error(
        clap::error::ErrorKind::MissingRequiredArgument,
        format!("cannot ask {question} here; answer it with: {instead}"),
    );
    // Swallow broken pipe errors, the way clap's own exit does.
    let _ = err.print();
    ExitCode::from(err.exit_code() as u8)
}

/// The subcommand path this invocation took, exactly as clap parsed it.
fn command_path(matches: &clap::ArgMatches) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, sub)) = current.subcommand() {
        path.push(name.to_owned());
        current = sub;
    }
    path
}

fn run(cli: &Cli) -> Result<Exit> {
    // `init` is the one command that must work before a project exists.
    if matches!(cli.command, Command::Init(_)) {
        let ctx = commands::Ctx::discover_or_init(cli)?;
        return commands::run(ctx);
    }

    let ctx = commands::Ctx::discover(cli)?;
    commands::run(ctx)
}
