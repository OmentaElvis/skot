//! `patch enable`: put a patch back in the applying set.
//!
//! The series file is the record: `enable` drops the `#!` from the patch's line, so the
//! choice is trackable in git and lasts. The applied set is untouched; `skot patch apply`
//! (or a push) realizes the set in the sandbox.

use anyhow::{Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::EnableArgs;
use crate::commands::authoring;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::prompt::{Asker, Fallback};
use crate::series::resolve;

/// What an enable did, presented by [`run`].
struct Changed {
    /// The patches whose `#!` marks were taken off, in series order.
    names: Vec<String>,
    /// The enabled set after the change, in series order.
    enabled: Vec<String>,
    /// What is still disabled after the change.
    disabled: Vec<String>,
}

/// Enable the named patches: enabled from now on, persistently.
///
/// Success is [`Exit::Success`] whenever the named patches end up enabled, even when
/// some already were: the state asked for holds, so the command did its job. Failure
/// is an empty selection, which asked for nothing.
pub fn run(ctx: &Ctx, args: &EnableArgs) -> Result<Exit> {
    // The question is asked before the lock, the way apply's used to be: a prompt
    // sitting on the lock is one the user can walk away from mid-answer.
    let (_config, series) = ctx.layout.project()?;
    let enabled: Vec<String> = series.names().into_iter().map(String::from).collect();
    let disabled = series.disabled();

    let wanted: Vec<String> = if args.all {
        if disabled.is_empty() {
            info!(target: "enable", "every patch is already enabled; nothing changed");
            return Ok(Exit::Success);
        }
        disabled.clone()
    } else if !args.patches.is_empty() {
        let enabled_refs: Vec<&str> = enabled.iter().map(String::as_str).collect();
        let disabled_refs: Vec<&str> = disabled.iter().map(String::as_str).collect();
        let mut universe: Vec<&str> = enabled_refs.clone();
        universe.extend(disabled_refs.iter().copied());
        let mut targets = Vec::new();
        for want in &args.patches {
            // Positions count the disabled pool: those are the patches `enable` can
            // reach, and position 1 among the enabled is enabled already.
            targets.push(
                authoring::resolve_patch(&universe, &disabled_refs, want)?.name(&disabled_refs),
            );
        }
        targets
    } else {
        pick(&ctx.asker(), &disabled)?
    };
    if wanted.is_empty() {
        info!(target: "enable", "nothing selected; nothing changed");
        return Ok(Exit::Failure);
    }

    // Already-enabled names are a no-op, not a failure of the rest: a batch enables
    // what is left and says so.
    let mut todo: Vec<String> = Vec::new();
    let mut already: Vec<String> = Vec::new();
    for name in wanted {
        if enabled.contains(&name) {
            if !already.contains(&name) {
                already.push(name);
            }
        } else if !todo.contains(&name) {
            todo.push(name);
        }
    }
    if todo.is_empty() {
        info!(
            target: "enable",
            "{} already enabled; nothing changed",
            already.join(", ")
        );
        return Ok(Exit::Success);
    }

    let changed = enable(&ctx.layout, &todo)?;
    for name in &already {
        debug!(target: "enable", "  {name} was already enabled; left alone");
    }

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "series_name": ctx.layout.series(),
            "enabled_now": changed.names,
            "enabled": changed.enabled,
            "disabled": changed.disabled,
        }))?;
        return Ok(Exit::Success);
    }

    info!(
        target: "enable",
        "enabled {}",
        super::push::count(changed.names.len(), "patch", "patches")
    );
    for name in &changed.names {
        debug!(target: "enable", "  {name}");
    }
    Hints::new("next")
        .entry(skot("apply"), "realize the set in the sandbox")
        .entry(skot("list"), "the series as it stands")
        .print();
    Ok(Exit::Success)
}

/// Which patches to enable: only the disabled ones are offered, an enabled patch
/// would only produce the no-op. A run that cannot ask is refused with the flag
/// that names the patch instead.
fn pick(asker: &Asker, disabled: &[String]) -> Result<Vec<String>> {
    if disabled.is_empty() {
        bail!(
            "every patch in the series is enabled.\n`{}` skips one.",
            skot("disable")
        );
    }
    Ok(asker.multi(
        "Which patches to enable",
        Some("Space toggles; Enter enables the selection. Esc cancels."),
        disabled.to_vec(),
        Fallback::Flag(skot_args("enable", "<patch>")),
    )?)
}

/// The plain work: lock, take the marks off, record.
fn enable(layout: &Layout, wanted: &[String]) -> Result<Changed> {
    let lock = layout.lock()?;
    let (_config, mut series) = layout.project()?;

    let mut names = Vec::new();
    for want in wanted {
        if !resolve(layout, want).is_file() {
            bail!(
                "`{want}` is disabled, but its file is gone, so there is nothing to put \
                 back.\n`{}` takes the line out.",
                skot_args("drop", want)
            );
        }
        if !series.enable(want) {
            bail!(
                "`{want}` could not be enabled: its line changed underfoot.\n`{}` \
                 lists what is disabled.",
                skot("list")
            );
        }
        names.push(want.clone());
    }
    series.save(layout)?;
    let enabled = series.names().into_iter().map(String::from).collect();
    let disabled = series.disabled();
    drop(lock);

    Ok(Changed {
        names,
        enabled,
        disabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::commands::testsupport;
    use clap::Parser as _;

    fn two_enabled_one_disabled() -> (tempfile::TempDir, Layout) {
        let (tmp, layout) = testsupport::project();
        for name in ["a.patch", "b.patch", "c.patch"] {
            std::fs::write(layout.patches_dir().join(name), "x\n").unwrap();
        }
        std::fs::write(layout.series_file(), "a.patch\n#!b.patch\nc.patch\n").unwrap();
        (tmp, layout)
    }

    #[test]
    fn enabling_puts_the_patch_back_in_the_series() {
        let (_tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "b.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["b.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n"
        );
    }

    #[test]
    fn enabling_an_already_enabled_patch_is_a_successful_no_op() {
        // The state asked for already holds, so the command succeeded: a failure exit
        // here would claim the state was not reached when it plainly is.
        let (_tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "a.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["a.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\n#!b.patch\nc.patch\n",
            "the file is untouched"
        );
    }

    #[test]
    fn a_position_names_a_disabled_patch() {
        // Positions for `enable` count the disabled pool: `patch list` shows those as
        // `#` lines, and position 1 among the enabled is already enabled, so the
        // position must reach the pool the command is about.
        let (_tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "1"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["1".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n",
            "position 1 among the disabled is b.patch"
        );
    }

    #[test]
    fn several_patches_can_be_enabled_at_once_by_name() {
        let (_tmp, layout) = two_enabled_one_disabled();
        // Two disabled this time, so the batch has something to do for both names.
        std::fs::write(layout.series_file(), "a.patch\n#!b.patch\n#!c.patch\n").unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "b", "c"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["b".to_owned(), "c".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n"
        );
    }

    #[test]
    fn all_takes_every_disabled_patch() {
        let (_tmp, layout) = two_enabled_one_disabled();
        std::fs::write(layout.series_file(), "a.patch\n#!b.patch\n#!c.patch\n").unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "--all"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: Vec::new(),
                all: true,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n"
        );
    }

    #[test]
    fn all_with_nothing_disabled_is_a_successful_no_op() {
        let (_tmp, layout) = two_enabled_one_disabled();
        std::fs::write(layout.series_file(), "a.patch\nb.patch\nc.patch\n").unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "--all"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: Vec::new(),
                all: true,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n"
        );
    }

    #[test]
    fn a_batch_with_one_already_enabled_enables_the_rest() {
        let (_tmp, layout) = two_enabled_one_disabled();
        std::fs::write(layout.series_file(), "a.patch\n#!b.patch\n#!c.patch\n").unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "a.patch", "b"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["a.patch".to_owned(), "b".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success, "the patch that could go, went");
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\n#!c.patch\n"
        );
    }

    #[test]
    fn a_script_that_names_nothing_is_told_how() {
        let (tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &EnableArgs {
                patches: Vec::new(),
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("cannot ask"), "{err}");
        assert!(err.contains(&skot_args("enable", "<patch>")), "{err}");
        drop(tmp);
    }

    #[test]
    fn an_unknown_patch_names_what_is_disabled() {
        let (tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "nope.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &EnableArgs {
                patches: vec!["nope.patch".to_owned()],
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("is not in the series"), "{err}");
        assert!(err.contains("b.patch"), "{err}");
        drop(tmp);
    }

    #[test]
    fn json_is_one_document_with_the_sets_after() {
        let (_tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "b.patch", "--json"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        run(
            &ctx,
            &EnableArgs {
                patches: vec!["b.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        // The document itself is stdout's business, checked end to end in tests/cli.rs;
        // here the work is just that the sets after the change are what save wrote.
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n"
        );
    }
}
