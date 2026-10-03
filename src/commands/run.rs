use anyhow::Result;

use crate::cli::RunArgs;
use crate::commands::{Ctx, Exit};
use crate::hooks::{self, Env};

pub fn run(ctx: &Ctx, args: &RunArgs) -> Result<Exit> {
    let config = ctx.config()?;
    let _lock = ctx.lock()?;

    // A named hook may run against the sandbox or against the project. It runs against
    // the sandbox, because that is where a build's inputs live.
    let sandbox = ctx.layout.sandbox_dir();
    hooks::ensure_sandbox(&sandbox)?;

    let state = crate::state::State::load(&ctx.layout)?;
    let env = Env {
        project_root: ctx.layout.root().to_path_buf(),
        sandbox,
        ref_name: state.ref_name.clone().unwrap_or_default(),
        sha: state.sha.clone().unwrap_or_default(),
        version: state
            .ref_name
            .as_deref()
            .map(Env::version_from_ref)
            .unwrap_or_default(),
    };

    // The verdict is the hook's, not an error: a build hook that fails is the answer to
    // "did it work", and `skot run` reports it the way `check` reports a conflict.
    Ok(
        match hooks::run_named(&args.name, &ctx.layout, &env, &config, ctx.cli.json)? {
            hooks::Outcome::Ran => Exit::Success,
            hooks::Outcome::Failed => Exit::Failure,
            hooks::Outcome::Skipped => Exit::Success,
        },
    )
}
