//! `patch disable`: skip a patch from now on.
//!
//! The series file is the record: `disable` comments the patch's line out, quilt-style,
//! so the choice is trackable in git and lasts. The applied set is untouched, and a
//! patch the sandbox is holding cannot be disabled underneath it: `skot patch pop` puts
//! it down first.

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::DisableArgs;
use crate::commands::authoring;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::state::State;

/// What a disable did, presented by [`run`].
struct Changed {
    /// The patches whose lines were commented out, in series order.
    names: Vec<String>,
    /// The enabled set after the change, in series order.
    enabled: Vec<String>,
    /// What is still disabled after the change.
    disabled: Vec<String>,
}

/// Comment patches' lines out: skipped from now on, persistently.
///
/// Success is [`Exit::Success`] when anything changed. Nothing changing (an empty
/// selection, a patch already disabled) is [`Exit::Failure`], the house rule for a
/// command that did not do the thing asked for.
pub fn run(ctx: &Ctx, args: &DisableArgs) -> Result<Exit> {
    // The question is asked before the lock: a prompt sitting on the lock is one the
    // user can walk away from mid-answer.
    let (_config, series) = ctx.layout.project()?;
    let state = State::load(&ctx.layout).unwrap_or_default();
    let enabled: Vec<String> = series.names().into_iter().map(String::from).collect();
    let disabled = series.disabled(&ctx.layout);

    let wanted: Vec<String> = if args.all {
        // Everything the sandbox is not holding: the applied ones stay enabled, since
        // the applied set is always a subset of the enabled one.
        enabled
            .iter()
            .filter(|n| !state.applied(&ctx.layout).contains(*n))
            .cloned()
            .collect()
    } else if !args.patches.is_empty() {
        let names: Vec<&str> = enabled.iter().map(String::as_str).collect();
        let mut universe: Vec<&str> = names.clone();
        universe.extend(disabled.iter().map(String::as_str));
        let mut targets = Vec::new();
        for want in &args.patches {
            let name = authoring::resolve_patch(&universe, &names, want)?.name(&names);
            if state.applied(&ctx.layout).contains(&name) {
                return Err(refuse_applied(&name));
            }
            targets.push(name);
        }
        targets
    } else if !ctx.cli.json && crate::ui::stdin_is_terminal() {
        pick(&enabled, state.applied(&ctx.layout))?
    } else {
        bail!(
            "cannot ask which patch to disable here.\nName it: `{}`.",
            skot_args("disable", "<patch>")
        )
    };
    if wanted.is_empty() {
        info!(target: "disable", "nothing selected; nothing changed");
        return Ok(Exit::Failure);
    }

    // Already-disabled names are a no-op, not a failure of the rest: a batch disables
    // what is left and says so.
    let mut todo: Vec<String> = Vec::new();
    let mut already: Vec<String> = Vec::new();
    for name in wanted {
        if disabled.contains(&name) {
            if !already.contains(&name) {
                already.push(name);
            }
        } else if !todo.contains(&name) {
            todo.push(name);
        }
    }
    if todo.is_empty() {
        if args.all {
            info!(
                target: "disable",
                "every enabled patch is applied; `{}` puts them down first.",
                skot("pop")
            );
        } else {
            info!(
                target: "disable",
                "{} already disabled; nothing changed",
                already.join(", ")
            );
        }
        return Ok(Exit::Failure);
    }

    let changed = disable(&ctx.layout, &todo)?;
    for name in &already {
        debug!(target: "disable", "  {name} was already disabled; left alone");
    }
    if args.all {
        let kept: Vec<&String> = enabled
            .iter()
            .filter(|n| state.applied(&ctx.layout).contains(*n))
            .collect();
        if !kept.is_empty() {
            info!(
                target: "disable",
                "{} stayed enabled: the sandbox is holding them; `{}` takes them down first",
                super::push::count(kept.len(), "patch", "patches"),
                skot("pop")
            );
        }
    }

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "series_name": ctx.layout.series(),
            "disabled_now": changed.names,
            "enabled": changed.enabled,
            "disabled": changed.disabled,
        }))?;
        return Ok(Exit::Success);
    }

    info!(
        target: "disable",
        "disabled {}",
        super::push::count(changed.names.len(), "patch", "patches")
    );
    for name in &changed.names {
        debug!(target: "disable", "  {name}");
    }
    Hints::new("next")
        .entry(skot("apply"), "reset the sandbox to the smaller set")
        .entry(skot("list"), "the series as it stands")
        .print();
    Ok(Exit::Success)
}

/// Ask an interactive session which patches to disable. Only enabled, not-yet-applied
/// patches are offered: the others cannot go without a pop first, and offering them
/// would only produce the refusal afterwards.
fn pick(enabled: &[String], applied: &[String]) -> Result<Vec<String>> {
    let options: Vec<String> = enabled
        .iter()
        .filter(|n| !applied.contains(*n))
        .cloned()
        .collect();
    if options.is_empty() {
        bail!(
            "every patch in the series is applied, so none can be disabled.\n`{}` puts \
             the top one down.",
            skot("pop")
        );
    }
    inquire::MultiSelect::new("Which patches to disable", options)
        .with_help_message("Space toggles; Enter disables the selection. Esc cancels.")
        .prompt()
        .context("asking which patches to disable")
}

/// The plain work: lock, comment out, record.
fn disable(layout: &Layout, wanted: &[String]) -> Result<Changed> {
    let lock = layout.lock()?;
    let (_config, mut series) = layout.project()?;
    let current = State::load(layout)?;

    let mut names = Vec::new();
    for want in wanted {
        if current.applied(layout).contains(want) {
            return Err(refuse_applied(want));
        }
        if !series.disable(want) {
            bail!("`{want}` could not be disabled: it is no longer an entry of the series.");
        }
        names.push(want.clone());
    }
    series.save(layout)?;
    let enabled = series.names().into_iter().map(String::from).collect();
    let disabled = series.disabled(layout);
    drop(lock);

    Ok(Changed {
        names,
        enabled,
        disabled,
    })
}

/// The refusal shared by the prompt path and the work itself: a patch the sandbox is
/// holding stays, whatever asked.
fn refuse_applied(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`{name}` is applied, so disabling it would make the sandbox hold a patch the \
         series no longer lists.\n\
         `{}` takes it off (and the patches above it).",
        skot_args("pop", name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::commands::testsupport;
    use clap::Parser as _;

    fn three_enabled() -> (tempfile::TempDir, Layout) {
        let (tmp, layout) = testsupport::project();
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", "a\n"),
                ("0002-b.patch", "b\n"),
                ("0003-c.patch", "c\n"),
            ],
        );
        (tmp, layout)
    }

    #[test]
    fn disabling_comments_the_line_out_and_positions_fold_for_apply() {
        let (_tmp, layout) = three_enabled();
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "0002-b.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DisableArgs {
                patches: vec!["0002-b.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "0001-a.patch\n# 0002-b.patch\n0003-c.patch\n",
            "the choice is in the file, trackable"
        );
    }

    #[test]
    fn disabling_an_already_disabled_patch_is_a_no_op() {
        let (_tmp, layout) = three_enabled();
        std::fs::write(
            layout.series_file(),
            "0001-a.patch\n# 0002-b.patch\n0003-c.patch\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "0002-b.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DisableArgs {
                patches: vec!["0002-b.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Failure);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "0001-a.patch\n# 0002-b.patch\n0003-c.patch\n"
        );
    }

    #[test]
    fn an_applied_patch_is_refused_with_a_way_to_put_it_down() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
            &["0001-a.patch", "0002-b.patch"],
        )
        .0;
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "0001-a.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &DisableArgs {
                patches: vec!["0001-a.patch".to_owned()],
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("is applied"), "{err}");
        assert!(err.contains(&skot_args("pop", "0001-a.patch")), "{err}");
        assert!(
            std::fs::read_to_string(ctx.layout.series_file())
                .unwrap()
                .contains("0001-a.patch\n"),
            "and the line is still an entry"
        );
    }

    #[test]
    fn several_patches_can_be_disabled_at_once_by_prefix() {
        let (_tmp, layout) = three_enabled();
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "0002", "0003"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DisableArgs {
                patches: vec!["0002".to_owned(), "0003".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "0001-a.patch\n# 0002-b.patch\n# 0003-c.patch\n"
        );
    }

    #[test]
    fn all_disables_every_enabled_patch_the_sandbox_is_not_holding() {
        let (_tmp, layout) = three_enabled();
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "--all"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DisableArgs {
                patches: Vec::new(),
                all: true,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "# 0001-a.patch\n# 0002-b.patch\n# 0003-c.patch\n"
        );
    }

    #[test]
    fn all_leaves_the_applied_patches_enabled() {
        // The applied set is always a subset of the enabled one, so `--all` disables
        // what can go and says what stayed, rather than refusing the whole command.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
            &["0001-a.patch", "0002-b.patch"],
        )
        .0;
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "--all"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DisableArgs {
                patches: Vec::new(),
                all: true,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Failure, "nothing could go");
        assert!(
            std::fs::read_to_string(ctx.layout.series_file())
                .unwrap()
                .contains("0001-a.patch\n"),
            "the applied line is still an entry"
        );
    }

    #[test]
    fn a_batch_naming_an_applied_patch_refuses_the_whole_batch() {
        // Check then act: nothing is disabled when one of the names cannot be.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_two()),
                ("0003-c.patch", testsupport::patch_three()),
            ],
            &["0001-a.patch", "0002-b.patch"],
        )
        .0;
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "0003", "0001"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &DisableArgs {
                patches: vec!["0003".to_owned(), "0001".to_owned()],
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("is applied"), "{err}");
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "0001-a.patch\n0002-b.patch\n0003-c.patch\n",
            "the unapplied patch of the batch is untouched"
        );
    }

    #[test]
    fn a_script_that_names_nothing_is_told_how() {
        let (tmp, layout) = three_enabled();
        let cli = Cli::try_parse_from(["skot", "patch", "disable"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &DisableArgs {
                patches: Vec::new(),
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("cannot ask"), "{err}");
        assert!(err.contains(&skot_args("disable", "<patch>")), "{err}");
        drop(tmp);
    }
}
