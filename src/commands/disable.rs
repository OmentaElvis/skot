//! `patch disable`: skip a patch from now on.
//!
//! The series file is the record: `disable` marks the patch's line `#!`, so the choice
//! is trackable in git and lasts. The applied set is untouched, and a patch the sandbox
//! is holding cannot be disabled underneath it: `skot patch pop` puts it down first.

use anyhow::{Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::DisableArgs;
use crate::commands::authoring;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::prompt::{Asker, Fallback};
use crate::state::State;

/// What a disable did, presented by [`run`].
struct Changed {
    /// The patches whose lines were marked `#!`, in series order.
    names: Vec<String>,
    /// The enabled set after the change, in series order.
    enabled: Vec<String>,
    /// What is still disabled after the change.
    disabled: Vec<String>,
}

/// Mark the named patches `#!`: skipped from now on, persistently.
///
/// Success is [`Exit::Success`] whenever the named patches end up disabled, even when
/// some already were: the state asked for holds, so the command did its job. Failure
/// is an empty selection, which asked for nothing.
pub fn run(ctx: &Ctx, args: &DisableArgs) -> Result<Exit> {
    // The question is asked before the lock: a prompt sitting on the lock is one the
    // user can walk away from mid-answer.
    let (_config, series) = ctx.layout.project()?;
    let state = State::load(&ctx.layout).unwrap_or_default();
    let enabled: Vec<String> = series.names().into_iter().map(String::from).collect();
    let disabled = series.disabled();

    let wanted: Vec<String> = if args.all {
        // Everything the sandbox is not holding: the applied ones stay enabled, since
        // the applied set is always a subset of the enabled one.
        let candidates: Vec<String> = enabled
            .iter()
            .filter(|n| !state.applied(&ctx.layout).contains(*n))
            .cloned()
            .collect();
        if candidates.is_empty() && !disabled.is_empty() {
            info!(target: "disable", "every patch is already disabled; nothing changed");
            return Ok(Exit::Success);
        }
        candidates
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
    } else {
        pick(&ctx.asker(), &enabled, state.applied(&ctx.layout))?
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
            return Ok(Exit::Failure);
        }
        info!(
            target: "disable",
            "{} already disabled; nothing changed",
            already.join(", ")
        );
        return Ok(Exit::Success);
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

/// Which patches to disable: only enabled, not-yet-applied patches are offered, the
/// others cannot go without a pop first and would only produce the refusal
/// afterwards. A run that cannot ask is refused with the flag that names the patch.
fn pick(asker: &Asker, enabled: &[String], applied: &[String]) -> Result<Vec<String>> {
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
    Ok(asker.multi(
        "Which patches to disable",
        Some("Space toggles; Enter disables the selection. Esc cancels."),
        options,
        Fallback::Flag(skot_args("disable", "<patch>")),
    )?)
}

/// The plain work: lock, mark `#!`, record.
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
    let disabled = series.disabled();
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
            &[("a.patch", "a\n"), ("b.patch", "b\n"), ("c.patch", "c\n")],
        );
        (tmp, layout)
    }

    #[test]
    fn disabling_marks_the_line_and_positions_fold_for_apply() {
        let (_tmp, layout) = three_enabled();
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "b.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DisableArgs {
                patches: vec!["b.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\n#!b.patch\nc.patch\n",
            "the choice is in the file, trackable"
        );
    }

    #[test]
    fn disabling_an_already_disabled_patch_is_a_successful_no_op() {
        // The state asked for already holds, so the command succeeded.
        let (_tmp, layout) = three_enabled();
        std::fs::write(layout.series_file(), "a.patch\n#!b.patch\nc.patch\n").unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "b.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DisableArgs {
                patches: vec!["b.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\n#!b.patch\nc.patch\n"
        );
    }

    #[test]
    fn an_applied_patch_is_refused_with_a_way_to_put_it_down() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("a.patch", testsupport::patch_one()),
                ("b.patch", testsupport::patch_two()),
            ],
            &["a.patch", "b.patch"],
        )
        .0;
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "a.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &DisableArgs {
                patches: vec!["a.patch".to_owned()],
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("is applied"), "{err}");
        assert!(err.contains(&skot_args("pop", "a.patch")), "{err}");
        assert!(
            std::fs::read_to_string(ctx.layout.series_file())
                .unwrap()
                .contains("a.patch\n"),
            "and the line is still an entry"
        );
    }

    #[test]
    fn several_patches_can_be_disabled_at_once_by_position() {
        let (_tmp, layout) = three_enabled();
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "2", "3"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &DisableArgs {
                patches: vec!["2".to_owned(), "3".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\n#!b.patch\n#!c.patch\n"
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
            "#!a.patch\n#!b.patch\n#!c.patch\n"
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
                ("a.patch", testsupport::patch_one()),
                ("b.patch", testsupport::patch_two()),
            ],
            &["a.patch", "b.patch"],
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
                .contains("a.patch\n"),
            "the applied line is still an entry"
        );
    }

    #[test]
    fn all_with_everything_already_disabled_is_a_successful_no_op() {
        // Everything disableable is already disabled: the state holds, so success.
        let (_tmp, layout) = three_enabled();
        std::fs::write(layout.series_file(), "#!a.patch\n#!b.patch\n").unwrap();
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
    }

    #[test]
    fn a_batch_naming_an_applied_patch_refuses_the_whole_batch() {
        // Check then act: nothing is disabled when one of the names cannot be.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("a.patch", testsupport::patch_one()),
                ("b.patch", testsupport::patch_two()),
                ("c.patch", testsupport::patch_three()),
            ],
            &["a.patch", "b.patch"],
        )
        .0;
        let cli = Cli::try_parse_from(["skot", "patch", "disable", "3", "1"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &DisableArgs {
                patches: vec!["3".to_owned(), "1".to_owned()],
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("is applied"), "{err}");
        assert_eq!(
            std::fs::read_to_string(ctx.layout.series_file()).unwrap(),
            "a.patch\nb.patch\nc.patch\n",
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
